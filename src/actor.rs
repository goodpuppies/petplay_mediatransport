//! StageForge native actor mode (`petplay-media actor`), speaking `protocol: 1` NDJSON over stdio.
//!
//! Manages media encoder feeds inside one long-lived process without stdout regex scraping or
//! loopback HTTP SDP servers. Control RPCs go over stdin/stdout (logs to stderr), while pixel
//! data stays on native DMA-BUF Unix seqpacket sockets / QUIC / WebRTC.

use anyhow::Result;
use serde_json::{Value, json};
use stageforge_actor::Actor;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;

#[cfg(all(feature = "encode", target_os = "linux"))]
use crate::{atlas, publish, source, webrtc};

#[cfg(all(feature = "encode", target_os = "linux"))]
struct SocketFeedEntry {
    id: String,
    socket_path: String,
    host: String,
    port: u16,
    fingerprint: String,
    share_code: Option<String>,
    webrtc_hub: webrtc::WebRtcHub,
    runtime: tokio::runtime::Handle,
    stop: Arc<AtomicBool>,
    stats: Arc<source::FeedStats>,
    join: Option<JoinHandle<()>>,
}

#[cfg(all(feature = "encode", target_os = "linux"))]
impl SocketFeedEntry {
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(all(feature = "encode", target_os = "linux"))]
struct AtlasEntry {
    id: String,
    host: String,
    port: u16,
    width: u32,
    height: u32,
    fingerprint: String,
    share_code: Option<String>,
    webrtc_hub: webrtc::WebRtcHub,
    runtime: tokio::runtime::Handle,
    cmd_tx: mpsc::Sender<atlas::AtlasCmd>,
    join: Option<JoinHandle<()>>,
}

#[cfg(all(feature = "encode", target_os = "linux"))]
impl AtlasEntry {
    fn stop_and_join(&mut self) {
        let _ = self.cmd_tx.send(atlas::AtlasCmd::Quit);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub fn run() -> Result<()> {
    let mut actor = Actor::new("petplayMedia");

    #[cfg(all(feature = "encode", target_os = "linux"))]
    {
        let feeds: Arc<Mutex<HashMap<String, SocketFeedEntry>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let atlases: Arc<Mutex<HashMap<String, AtlasEntry>>> =
            Arc::new(Mutex::new(HashMap::new()));

        {
            let feeds = Arc::clone(&feeds);
            actor.on("START_SOCKET_FEED", move |ctx, payload| {
                let id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "START_SOCKET_FEED needs {id}".to_owned())?
                    .to_owned();
                let socket_path = payload
                    .get("socketPath")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "START_SOCKET_FEED needs {socketPath}".to_owned())?
                    .to_owned();
                let host = payload
                    .get("host")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "START_SOCKET_FEED needs {host}".to_owned())?
                    .to_owned();
                let port = payload
                    .get("port")
                    .and_then(Value::as_u64)
                    .and_then(|p| u16::try_from(p).ok())
                    .ok_or_else(|| "START_SOCKET_FEED needs valid {port}".to_owned())?;
                let layout_str = payload
                    .get("layout")
                    .and_then(Value::as_str)
                    .unwrap_or("mono");
                let layout = publish::Layout::parse(layout_str).map_err(|e| format!("{e:#}"))?;
                let fps = payload
                    .get("fps")
                    .and_then(Value::as_u64)
                    .unwrap_or(60)
                    .clamp(1, 240) as u32;
                let max_fps = payload
                    .get("maxFps")
                    .and_then(Value::as_u64)
                    .map(|v| v.clamp(1, 240) as u32)
                    .or(Some(fps));
                let bitrate = payload
                    .get("bitrate")
                    .and_then(Value::as_u64)
                    .unwrap_or(12_000_000)
                    .clamp(100_000, 200_000_000) as u32;
                let keyframe_seconds = payload
                    .get("keyframeSeconds")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .clamp(1, 60) as u32;
                let persist = payload
                    .get("persist")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let share = payload
                    .get("share")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let subscriber = payload
                    .get("subscriber")
                    .and_then(Value::as_str)
                    .unwrap_or(ctx.sender)
                    .to_owned();

                // If a feed with this id is already running, return its info directly.
                {
                    let map = feeds.lock().unwrap();
                    if let Some(existing) = map.get(&id) {
                        return Ok(json!({
                            "id": existing.id,
                            "socketPath": existing.socket_path,
                            "host": existing.host,
                            "port": existing.port,
                            "fingerprint": existing.fingerprint,
                            "code": existing.share_code,
                            "webrtc": true,
                        }));
                    }
                }

                let bind: SocketAddr = format!("{host}:{port}")
                    .parse()
                    .map_err(|e| format!("invalid bind address {host}:{port}: {e}"))?;
                let transport = if share {
                    publish::Transport::Peer
                } else {
                    publish::Transport::Browser
                };
                let publisher = publish::Publisher::new_for_actor(bind, transport)
                    .map_err(|e| format!("publisher init failed: {e:#}"))?;
                let fingerprint = publisher.fingerprint.clone();
                let share_code = publisher.share_code.clone();
                let webrtc_hub = publisher.webrtc_hub();
                let runtime = publisher.runtime_handle();

                let path_buf = PathBuf::from(&socket_path);
                let (listener, socket_guard) = source::bind_socket_feed(&path_buf)
                    .map_err(|e| format!("bind socket {socket_path}: {e:#}"))?;

                let stop = Arc::new(AtomicBool::new(false));
                let stats = Arc::new(source::FeedStats::default());
                let stop_thread = Arc::clone(&stop);
                let stats_thread = Arc::clone(&stats);
                let feeds_thread = Arc::clone(&feeds);
                let handle_thread = ctx.handle.clone();
                let id_thread = id.clone();

                let cfg = source::SocketFeedConfig {
                    socket_path: path_buf,
                    layout,
                    framerate: fps,
                    bitrate,
                    keyframe_seconds,
                    persist,
                    max_fps,
                };

                let join = std::thread::spawn(move || {
                    let result = source::run_socket_feed(
                        listener,
                        socket_guard,
                        cfg,
                        publisher,
                        Arc::clone(&stop_thread),
                        stats_thread,
                    );
                    if let Err(err) = result {
                        eprintln!("[petplayMedia] feed {id_thread} exited with error: {err:#}");
                    }
                    if !stop_thread.load(Ordering::Relaxed) {
                        feeds_thread.lock().unwrap().remove(&id_thread);
                        let _ = handle_thread.post(
                            &subscriber,
                            "FEED_ENDED",
                            json!({ "id": id_thread }),
                        );
                    }
                });

                feeds.lock().unwrap().insert(
                    id.clone(),
                    SocketFeedEntry {
                        id: id.clone(),
                        socket_path: socket_path.clone(),
                        host: host.clone(),
                        port,
                        fingerprint: fingerprint.clone(),
                        share_code: share_code.clone(),
                        webrtc_hub,
                        runtime,
                        stop,
                        stats,
                        join: Some(join),
                    },
                );

                let mut resp = json!({
                    "id": id,
                    "socketPath": socket_path,
                    "host": host,
                    "port": port,
                    "fingerprint": fingerprint,
                    "webrtc": true,
                });
                if let Some(code) = share_code {
                    resp["code"] = Value::String(code);
                }
                Ok(resp)
            });
        }

        {
            let atlases = Arc::clone(&atlases);
            actor.on("ATLAS_START", move |ctx, payload| {
                let id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("atlas")
                    .to_owned();
                let host = payload
                    .get("host")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "ATLAS_START needs {host}".to_owned())?
                    .to_owned();
                let port = payload
                    .get("port")
                    .and_then(Value::as_u64)
                    .and_then(|p| u16::try_from(p).ok())
                    .ok_or_else(|| "ATLAS_START needs valid {port}".to_owned())?;
                let width = ((payload.get("width").and_then(Value::as_u64).unwrap_or(2560) as u32)
                    & !1)
                    .max(320);
                let height = ((payload.get("height").and_then(Value::as_u64).unwrap_or(1440)
                    as u32)
                    & !1)
                    .max(240);
                let fps = payload
                    .get("fps")
                    .and_then(Value::as_u64)
                    .unwrap_or(30)
                    .clamp(1, 240) as u32;
                let bitrate = payload
                    .get("bitrate")
                    .and_then(Value::as_u64)
                    .unwrap_or(12_000_000)
                    .clamp(100_000, 200_000_000) as u32;
                let share = payload
                    .get("share")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let subscriber = payload
                    .get("subscriber")
                    .and_then(Value::as_str)
                    .unwrap_or(ctx.sender)
                    .to_owned();

                {
                    let map = atlases.lock().unwrap();
                    if let Some(existing) = map.get(&id) {
                        let mut resp = json!({
                            "id": existing.id,
                            "host": existing.host,
                            "port": existing.port,
                            "fingerprint": existing.fingerprint,
                            "webrtc": true,
                        });
                        if let Some(code) = &existing.share_code {
                            resp["code"] = Value::String(code.clone());
                        }
                        return Ok(resp);
                    }
                }

                let bind: SocketAddr = format!("{host}:{port}")
                    .parse()
                    .map_err(|e| format!("invalid bind address {host}:{port}: {e}"))?;
                let transport = if share {
                    publish::Transport::Peer
                } else {
                    publish::Transport::Browser
                };
                let publisher = publish::Publisher::new_for_actor(bind, transport)
                    .map_err(|e| format!("atlas publisher init failed: {e:#}"))?;
                let fingerprint = publisher.fingerprint.clone();
                let share_code = publisher.share_code.clone();
                let webrtc_hub = publisher.webrtc_hub();
                let runtime = publisher.runtime_handle();

                let (cmd_tx, cmd_rx) = mpsc::channel::<atlas::AtlasCmd>();
                let handle_thread = ctx.handle.clone();
                let id_thread = id.clone();
                let sub_thread = subscriber.clone();

                let join = std::thread::spawn(move || {
                    let result = atlas::run_atlas_loop(
                        width,
                        height,
                        fps,
                        bitrate,
                        publisher,
                        cmd_rx,
                        false,
                        |layout_json| {
                            if let Ok(layout) = serde_json::from_str::<Value>(layout_json) {
                                let _ = handle_thread.post(
                                    &sub_thread,
                                    "ATLAS_LAYOUT",
                                    json!({ "id": id_thread, "layout": layout }),
                                );
                            }
                        },
                    );
                    if let Err(err) = result {
                        eprintln!("[petplayMedia] atlas {id_thread} exited with error: {err:#}");
                    }
                });

                atlases.lock().unwrap().insert(
                    id.clone(),
                    AtlasEntry {
                        id: id.clone(),
                        host: host.clone(),
                        port,
                        width,
                        height,
                        fingerprint: fingerprint.clone(),
                        share_code: share_code.clone(),
                        webrtc_hub,
                        runtime,
                        cmd_tx,
                        join: Some(join),
                    },
                );

                let mut resp = json!({
                    "id": id,
                    "host": host,
                    "port": port,
                    "fingerprint": fingerprint,
                    "webrtc": true,
                });
                if let Some(code) = share_code {
                    resp["code"] = Value::String(code);
                }
                Ok(resp)
            });
        }

        {
            let atlases = Arc::clone(&atlases);
            actor.on("ATLAS_ADD", move |_ctx, payload| {
                let id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("atlas");
                let slot_id = payload
                    .get("slotId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "ATLAS_ADD needs {slotId, socketPath}".to_owned())?
                    .to_owned();
                let socket_path = payload
                    .get("socketPath")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "ATLAS_ADD needs {slotId, socketPath}".to_owned())?;
                let map = atlases.lock().unwrap();
                let entry = map
                    .get(id)
                    .ok_or_else(|| format!("unknown atlas {id}"))?;
                entry
                    .cmd_tx
                    .send(atlas::AtlasCmd::Add {
                        id: slot_id,
                        socket_path: PathBuf::from(socket_path),
                    })
                    .map_err(|_| format!("atlas {id} channel closed"))?;
                Ok(Value::Bool(true))
            });
        }

        {
            let atlases = Arc::clone(&atlases);
            actor.on("ATLAS_REMOVE", move |_ctx, payload| {
                let id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("atlas");
                let slot_id = payload
                    .get("slotId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "ATLAS_REMOVE needs {slotId}".to_owned())?
                    .to_owned();
                let map = atlases.lock().unwrap();
                if let Some(entry) = map.get(id) {
                    let _ = entry.cmd_tx.send(atlas::AtlasCmd::Remove { id: slot_id });
                }
                Ok(Value::Bool(true))
            });
        }

        {
            let feeds = Arc::clone(&feeds);
            let atlases = Arc::clone(&atlases);
            actor.on("STOP_FEED", move |_ctx, payload| {
                let id = payload.get("id").and_then(Value::as_str);
                let mut to_stop = Vec::new();
                let mut atlases_to_stop = Vec::new();
                {
                    let mut map = feeds.lock().unwrap();
                    let mut amap = atlases.lock().unwrap();
                    if let Some(id) = id {
                        if let Some(entry) = map.remove(id) {
                            to_stop.push(entry);
                        }
                        if let Some(entry) = amap.remove(id) {
                            atlases_to_stop.push(entry);
                        }
                    } else {
                        for (_, entry) in map.drain() {
                            to_stop.push(entry);
                        }
                        for (_, entry) in amap.drain() {
                            atlases_to_stop.push(entry);
                        }
                    }
                }
                for mut entry in to_stop {
                    entry.stop_and_join();
                }
                for mut entry in atlases_to_stop {
                    entry.stop_and_join();
                }
                Ok(Value::Bool(true))
            });
        }

        {
            let feeds = Arc::clone(&feeds);
            let atlases = Arc::clone(&atlases);
            actor.on("WEBRTC_OFFER", move |_ctx, payload| {
                let id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "WEBRTC_OFFER needs {id, sdp}".to_owned())?;
                let sdp = payload
                    .get("sdp")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| "WEBRTC_OFFER needs {id, sdp}".to_owned())?;
                let found = {
                    let map = feeds.lock().unwrap();
                    map.get(id)
                        .map(|e| (e.webrtc_hub.clone(), e.runtime.clone()))
                }
                .or_else(|| {
                    let amap = atlases.lock().unwrap();
                    amap.get(id)
                        .map(|e| (e.webrtc_hub.clone(), e.runtime.clone()))
                });
                let (hub, runtime) = found.ok_or_else(|| format!("unknown feed {id}"))?;
                let answer = runtime
                    .block_on(hub.accept_offer(sdp))
                    .map_err(|e| format!("WebRTC offer failed: {e:#}"))?;
                Ok(json!({ "sdp": answer }))
            });
        }

        {
            let feeds = Arc::clone(&feeds);
            let atlases = Arc::clone(&atlases);
            actor.on("LIST_FEEDS", move |_ctx, _payload| {
                let mut list: Vec<Value> = feeds
                    .lock()
                    .unwrap()
                    .values()
                    .map(|e| {
                        json!({
                            "id": e.id,
                            "kind": "socket",
                            "socketPath": e.socket_path,
                            "host": e.host,
                            "port": e.port,
                            "fingerprint": e.fingerprint,
                            "code": e.share_code,
                            "connected": e.stats.connected.load(Ordering::Relaxed),
                            "frames": e.stats.frames.load(Ordering::Relaxed),
                        })
                    })
                    .collect();
                for e in atlases.lock().unwrap().values() {
                    list.push(json!({
                        "id": e.id,
                        "kind": "atlas",
                        "host": e.host,
                        "port": e.port,
                        "width": e.width,
                        "height": e.height,
                        "fingerprint": e.fingerprint,
                        "code": e.share_code,
                    }));
                }
                Ok(Value::Array(list))
            });
        }

        {
            let feeds = Arc::clone(&feeds);
            let atlases = Arc::clone(&atlases);
            actor.on_shutdown(move || {
                let drained: Vec<SocketFeedEntry> =
                    feeds.lock().unwrap().drain().map(|(_, v)| v).collect();
                for mut entry in drained {
                    entry.stop_and_join();
                }
                let drained_atlases: Vec<AtlasEntry> =
                    atlases.lock().unwrap().drain().map(|(_, v)| v).collect();
                for mut entry in drained_atlases {
                    entry.stop_and_join();
                }
            });
        }
    }

    actor.run_stdio()?;
    Ok(())
}
