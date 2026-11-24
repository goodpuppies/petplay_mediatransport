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
};
use std::thread::JoinHandle;

#[cfg(all(feature = "encode", target_os = "linux"))]
use crate::{publish, source, webrtc};

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

pub fn run() -> Result<()> {
    let mut actor = Actor::new("petplayMedia");

    #[cfg(all(feature = "encode", target_os = "linux"))]
    {
        let feeds: Arc<Mutex<HashMap<String, SocketFeedEntry>>> =
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
            let feeds = Arc::clone(&feeds);
            actor.on("STOP_FEED", move |_ctx, payload| {
                let id = payload.get("id").and_then(Value::as_str);
                let mut to_stop = Vec::new();
                {
                    let mut map = feeds.lock().unwrap();
                    if let Some(id) = id {
                        if let Some(entry) = map.remove(id) {
                            to_stop.push(entry);
                        }
                    } else {
                        for (_, entry) in map.drain() {
                            to_stop.push(entry);
                        }
                    }
                }
                for mut entry in to_stop {
                    entry.stop_and_join();
                }
                Ok(Value::Bool(true))
            });
        }

        {
            let feeds = Arc::clone(&feeds);
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
                let (hub, runtime) = {
                    let map = feeds.lock().unwrap();
                    let entry = map
                        .get(id)
                        .ok_or_else(|| format!("unknown feed {id}"))?;
                    (entry.webrtc_hub.clone(), entry.runtime.clone())
                };
                let answer = runtime
                    .block_on(hub.accept_offer(sdp))
                    .map_err(|e| format!("WebRTC offer failed: {e:#}"))?;
                Ok(json!({ "sdp": answer }))
            });
        }

        {
            let feeds = Arc::clone(&feeds);
            actor.on("LIST_FEEDS", move |_ctx, _payload| {
                let map = feeds.lock().unwrap();
                let list: Vec<Value> = map
                    .values()
                    .map(|e| {
                        json!({
                            "id": e.id,
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
                Ok(Value::Array(list))
            });
        }

        {
            let feeds = Arc::clone(&feeds);
            actor.on_shutdown(move || {
                let drained: Vec<SocketFeedEntry> =
                    feeds.lock().unwrap().drain().map(|(_, v)| v).collect();
                for mut entry in drained {
                    entry.stop_and_join();
                }
            });
        }
    }

    actor.run_stdio()?;
    Ok(())
}
