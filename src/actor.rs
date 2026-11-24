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
    slot_sockets: HashMap<String, String>,
    slot_seq: u64,
    join: Option<JoinHandle<()>>,
}

#[cfg(all(feature = "encode", target_os = "linux"))]
struct TransportBinding {
    peer: String,
    feed_id: Option<String>,
    owns_feed: bool,
}

#[cfg(all(feature = "encode", target_os = "linux"))]
fn runtime_socket_dir() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::geteuid() }));
    PathBuf::from(base).join(format!("petplay-media-{}", std::process::id()))
}

#[cfg(all(feature = "encode", target_os = "linux"))]
fn sanitize_filename(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(48)
        .collect()
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

#[cfg(all(feature = "encode", target_os = "linux"))]
fn start_socket_feed_from_params(
    params: &Value,
    subscriber: String,
    handle: stageforge_actor::ActorHandle,
    feeds: &Arc<Mutex<HashMap<String, SocketFeedEntry>>>,
    transports: &Arc<Mutex<HashMap<String, TransportBinding>>>,
    sock_dir: &Arc<PathBuf>,
) -> std::result::Result<(Value, bool), String> {
    let id = params
        .get("feedId")
        .or_else(|| params.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| "socket feed needs {id|feedId}".to_owned())?
        .to_owned();
    let socket_path = params
        .get("socketPath")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            sock_dir
                .join(format!("feed-{}.sock", sanitize_filename(&id)))
                .to_string_lossy()
                .into_owned()
        });
    let host = params
        .get("host")
        .and_then(Value::as_str)
        .ok_or_else(|| "socket feed needs {host}".to_owned())?
        .to_owned();
    let port = params
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(|| "socket feed needs valid {port}".to_owned())?;
    let layout_str = params
        .get("layout")
        .and_then(Value::as_str)
        .unwrap_or("mono");
    let layout = publish::Layout::parse(layout_str).map_err(|e| format!("{e:#}"))?;
    let fps = params
        .get("fps")
        .and_then(Value::as_u64)
        .unwrap_or(60)
        .clamp(1, 240) as u32;
    let max_fps = params
        .get("maxFps")
        .and_then(Value::as_u64)
        .map(|v| v.clamp(1, 240) as u32)
        .or(Some(fps));
    let bitrate = params
        .get("bitrate")
        .and_then(Value::as_u64)
        .unwrap_or(12_000_000)
        .clamp(100_000, 200_000_000) as u32;
    let keyframe_seconds = params
        .get("keyframeSeconds")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 60) as u32;
    let persist = params
        .get("persist")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let share = params
        .get("share")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    {
        let map = feeds.lock().unwrap();
        if let Some(existing) = map.get(&id) {
            let mut resp = json!({
                "id": existing.id,
                "feedId": existing.id,
                "socketPath": existing.socket_path,
                "host": existing.host,
                "port": existing.port,
                "fingerprint": existing.fingerprint,
                "webrtc": true,
            });
            if let Some(code) = &existing.share_code {
                resp["code"] = Value::String(code.clone());
            }
            return Ok((resp, false));
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
    let feeds_thread = Arc::clone(feeds);
    let transports_thread = Arc::clone(transports);
    let handle_thread = handle;
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
            let _ = handle_thread.post(&subscriber, "FEED_ENDED", json!({ "id": id_thread }));
            let ended_transports: Vec<(String, String)> = {
                let mut tmap = transports_thread.lock().unwrap();
                let ids: Vec<String> = tmap
                    .iter()
                    .filter(|(_, b)| b.feed_id.as_deref() == Some(id_thread.as_str()))
                    .map(|(tid, _)| tid.clone())
                    .collect();
                ids.into_iter()
                    .filter_map(|tid| tmap.remove(&tid).map(|b| (tid, b.peer)))
                    .collect()
            };
            for (tid, peer) in ended_transports {
                let _ = handle_thread.post(
                    &peer,
                    "TRANSPORTSIGNAL",
                    json!({ "id": tid, "data": { "closed": true, "reason": "feed ended" } }),
                );
            }
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
        "feedId": id,
        "socketPath": socket_path,
        "host": host,
        "port": port,
        "fingerprint": fingerprint,
        "webrtc": true,
    });
    if let Some(code) = share_code {
        resp["code"] = Value::String(code);
    }
    Ok((resp, true))
}

pub fn run() -> Result<()> {
    let mut actor = Actor::new("petplayMedia");

    #[cfg(all(feature = "encode", target_os = "linux"))]
    {
        let feeds: Arc<Mutex<HashMap<String, SocketFeedEntry>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let atlases: Arc<Mutex<HashMap<String, AtlasEntry>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let transports: Arc<Mutex<HashMap<String, TransportBinding>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let sock_dir = Arc::new(runtime_socket_dir());
        let _ = std::fs::create_dir_all(&*sock_dir);

        {
            let feeds = Arc::clone(&feeds);
            let transports = Arc::clone(&transports);
            let sock_dir = Arc::clone(&sock_dir);
            actor.on("START_SOCKET_FEED", move |ctx, payload| {
                let subscriber = payload
                    .get("subscriber")
                    .and_then(Value::as_str)
                    .unwrap_or(ctx.sender)
                    .to_owned();
                let (resp, _) = start_socket_feed_from_params(
                    &payload,
                    subscriber,
                    ctx.handle.clone(),
                    &feeds,
                    &transports,
                    &sock_dir,
                )?;
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
                        slot_sockets: HashMap::new(),
                        slot_seq: 0,
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
            let sock_dir = Arc::clone(&sock_dir);
            actor.on("ATLAS_ADD", move |_ctx, payload| {
                let id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("atlas");
                let slot_id = payload
                    .get("slotId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "ATLAS_ADD needs {slotId}".to_owned())?
                    .to_owned();
                let requested_path = payload
                    .get("socketPath")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned);
                let mut map = atlases.lock().unwrap();
                let entry = map
                    .get_mut(id)
                    .ok_or_else(|| format!("unknown atlas {id}"))?;
                if let Some(existing) = entry.slot_sockets.get(&slot_id) {
                    return Ok(json!({ "slotId": slot_id, "socketPath": existing }));
                }
                entry.slot_seq += 1;
                let socket_path = requested_path.unwrap_or_else(|| {
                    sock_dir
                        .join(format!("atlas-{}-{}.sock", sanitize_filename(id), entry.slot_seq))
                        .to_string_lossy()
                        .into_owned()
                });
                // Bind synchronously before replying so producers can connect immediately with no sleep.
                let (listener, guard) = atlas::bind_slot_socket(&PathBuf::from(&socket_path))
                    .map_err(|e| format!("bind atlas slot socket {socket_path}: {e:#}"))?;
                entry
                    .cmd_tx
                    .send(atlas::AtlasCmd::AddBound {
                        id: slot_id.clone(),
                        listener,
                        guard,
                    })
                    .map_err(|_| format!("atlas {id} channel closed"))?;
                entry.slot_sockets.insert(slot_id.clone(), socket_path.clone());
                Ok(json!({ "slotId": slot_id, "socketPath": socket_path }))
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
                let mut map = atlases.lock().unwrap();
                if let Some(entry) = map.get_mut(id) {
                    entry.slot_sockets.remove(&slot_id);
                    let _ = entry.cmd_tx.send(atlas::AtlasCmd::Remove { id: slot_id });
                }
                Ok(Value::Bool(true))
            });
        }

        // StageForge Transport protocol: negotiate `dmabuf-seqpacket` for `purpose: "media.frames"`.
        // Opening with `{feedId, host, port, ...}` starts and owns the feed (closing the transport
        // stops and unlinks it); opening with `{feedId}` or `{atlasId, slotId}` attaches to an
        // existing feed without owning its lifecycle.
        {
            let feeds = Arc::clone(&feeds);
            let atlases = Arc::clone(&atlases);
            let transports = Arc::clone(&transports);
            let sock_dir = Arc::clone(&sock_dir);
            actor.on("TRANSPORTOFFER", move |ctx, payload| {
                let transport_id = payload
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "TRANSPORTOFFER missing id".to_owned())?
                    .to_owned();
                let from = payload
                    .get("from")
                    .and_then(Value::as_str)
                    .unwrap_or(ctx.sender)
                    .to_owned();
                let purpose = payload
                    .get("purpose")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if purpose != "media.frames" {
                    return Err(format!("unsupported transport purpose: {purpose}"));
                }
                let offers_seqpacket = payload
                    .get("kinds")
                    .and_then(Value::as_array)
                    .is_some_and(|arr| {
                        arr.iter().any(|k| {
                            k.get("kind").and_then(Value::as_str) == Some("dmabuf-seqpacket")
                        })
                    });
                if !offers_seqpacket {
                    return Err("no compatible transport kind (expected dmabuf-seqpacket)".to_owned());
                }
                let params = payload.get("params").unwrap_or(&Value::Null);
                let (signal_data, binding) = if params.get("host").is_some() && params.get("port").is_some() {
                    let (resp, created) = start_socket_feed_from_params(
                        params,
                        from.clone(),
                        ctx.handle.clone(),
                        &feeds,
                        &transports,
                        &sock_dir,
                    )?;
                    let feed_id = resp
                        .get("feedId")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    (
                        resp,
                        TransportBinding {
                            peer: from.clone(),
                            feed_id,
                            owns_feed: created,
                        },
                    )
                } else if let Some(feed_id) = params.get("feedId").and_then(Value::as_str) {
                    let map = feeds.lock().unwrap();
                    let entry = map
                        .get(feed_id)
                        .ok_or_else(|| format!("unknown feedId {feed_id}"))?;
                    let mut resp = json!({
                        "feedId": entry.id,
                        "socketPath": entry.socket_path,
                        "host": entry.host,
                        "port": entry.port,
                        "fingerprint": entry.fingerprint,
                        "webrtc": true,
                    });
                    if let Some(code) = &entry.share_code {
                        resp["code"] = Value::String(code.clone());
                    }
                    (
                        resp,
                        TransportBinding {
                            peer: from.clone(),
                            feed_id: Some(entry.id.clone()),
                            owns_feed: false,
                        },
                    )
                } else if let (Some(atlas_id), Some(slot_id)) = (
                    params.get("atlasId").and_then(Value::as_str),
                    params.get("slotId").and_then(Value::as_str),
                ) {
                    let socket_path = atlases
                        .lock()
                        .unwrap()
                        .get(atlas_id)
                        .and_then(|a| a.slot_sockets.get(slot_id).cloned())
                        .ok_or_else(|| format!("unknown atlas slot {atlas_id}/{slot_id}"))?;
                    (
                        json!({ "socketPath": socket_path }),
                        TransportBinding {
                            peer: from.clone(),
                            feed_id: None,
                            owns_feed: false,
                        },
                    )
                } else {
                    return Err("dmabuf-seqpacket requires params.{feedId,host,port}, params.feedId, or params.{atlasId,slotId}".to_owned());
                };

                transports.lock().unwrap().insert(transport_id.clone(), binding);

                // Emit the bound socket descriptor over TRANSPORTSIGNAL immediately (StageForge
                // buffers signals before TRANSPORTOFFER resolves, so zero extra RTT).
                let _ = ctx.handle.post(
                    &from,
                    "TRANSPORTSIGNAL",
                    json!({
                        "id": transport_id,
                        "data": signal_data,
                    }),
                );
                Ok(json!({
                    "kind": "dmabuf-seqpacket",
                    "listener": "responder",
                }))
            });
        }

        {
            let feeds = Arc::clone(&feeds);
            let transports = Arc::clone(&transports);
            actor.on("TRANSPORTSIGNAL", move |_ctx, payload| {
                let id = payload.get("id").and_then(Value::as_str);
                let closed = payload
                    .get("data")
                    .and_then(|d| d.get("closed"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if closed {
                    if let Some(tid) = id {
                        if let Some(binding) = transports.lock().unwrap().remove(tid) {
                            if binding.owns_feed {
                                if let Some(feed_id) = binding.feed_id {
                                    let entry = feeds.lock().unwrap().remove(&feed_id);
                                    if let Some(mut entry) = entry {
                                        entry.stop_and_join();
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(Value::Bool(true))
            });
        }

        {
            let feeds = Arc::clone(&feeds);
            let transports = Arc::clone(&transports);
            actor.on("TRANSPORTCANCEL", move |_ctx, payload| {
                if let Some(tid) = payload.get("id").and_then(Value::as_str) {
                    if let Some(binding) = transports.lock().unwrap().remove(tid) {
                        if binding.owns_feed {
                            if let Some(feed_id) = binding.feed_id {
                                let entry = feeds.lock().unwrap().remove(&feed_id);
                                if let Some(mut entry) = entry {
                                    entry.stop_and_join();
                                }
                            }
                        }
                    }
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
            let sock_dir = Arc::clone(&sock_dir);
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
                let _ = std::fs::remove_dir_all(&*sock_dir);
            });
        }
    }

    actor.run_stdio()?;
    Ok(())
}
