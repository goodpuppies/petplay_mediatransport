//! `petplay-media link`: a process's iroh node for stageforge remote actors.
//!
//! A PetPlay process starts this as a child and drives it over stdio with newline-delimited
//! JSON, so no native code loads into PetPlay and the same protocol works on every platform.
//! Each connection is one iroh connection with ALPN [`ALPN`] and one bidirectional stream of
//! length-prefixed UTF-8 frames (actor messages, already JSON text). The dialer writes first.
//!
//! Two endpoints separate untrusted discovery from trusted direct P2P (mitigating iroh#3535):
//! - **Discovery endpoint** (published on the topic server): when not `--local`, built with
//!   `clear_ip_transports()` so it is strictly relay-only and never binds UDP or leaks direct IP
//!   addresses to untrusted peers.
//! - **Direct endpoint** (never published on the topic server): created lazily on demand with full
//!   IP + relay NAT traversal and an `EndpointHooks::after_handshake` allowlist of trusted peer
//!   endpoint ids. Peers only learn this endpoint id over the encrypted discovery link after the
//!   user trusts them.
//!
//! PetPlay → link (stdin):
//! - `{"op":"connect","req":N,"endpoint":{"id":…,"relay":…?,"addrs":[…]?},"tier":"discovery"|"direct"?}`
//! - `{"op":"direct_endpoint","req":N}`
//! - `{"op":"allow_direct","peer":"<EndpointId>"}`
//! - `{"op":"revoke_direct","peer":"<EndpointId>"}`
//! - `{"op":"send","conn":C,"data":"…"}`
//! - `{"op":"close","conn":C,"reason":"…"}`
//!
//! Link → PetPlay (stdout):
//! - `{"ev":"ready","id":…,"relay":…|null,"addrs":[…]}` once, when the discovery endpoint is ready
//! - `{"ev":"direct_ready","req":N,"id":…,"relay":…|null,"addrs":[…]}`
//! - `{"ev":"connected","req":N,"conn":C,"peer":…,"tier":"discovery"|"direct"}` / `{"ev":"failed","req":N,"error":…}`
//! - `{"ev":"incoming","conn":C,"peer":…,"tier":"discovery"|"direct"}`
//! - `{"ev":"data","conn":C,"data":"…"}`
//! - `{"ev":"closed","conn":C,"reason":…}`
//!
//! `--relay URL` uses that relay (a self-hosted `iroh-relay`, or `iroh-relay --dev` for tests)
//! instead of n0's public ones. `--local` binds loopback only with no relay, for tests on one
//! machine; its `addrs` are loopback addresses and must not be published.
//!
//! A frame is a big-endian u32 length and that many bytes of UTF-8. A length of `u32::MAX`
//! marks the last frame, carrying the close reason (a QUIC close could overtake unread data).

use anyhow::{Context, Result, bail};
use moq_tokio::iroh::web_transport_iroh::iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayUrl, TransportAddr, Watcher,
    address_lookup::MemoryLookup,
    endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, RelayMode, default_relay_mode, presets},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::mpsc,
};

pub const ALPN: &[u8] = b"petplay/link/1";
/// One frame is one actor message; anything larger belongs on a negotiated transport.
const MAX_FRAME: usize = 16 * 1024 * 1024;
/// A length of this value marks the last frame: the close reason, then the stream ends.
const CLOSE_MARKER: u32 = u32::MAX;

type Out = mpsc::UnboundedSender<Value>;

enum ConnCommand {
    Send(String),
    Close(String),
}

struct ConnEntry {
    tx: mpsc::UnboundedSender<ConnCommand>,
    peer: EndpointId,
    tier: &'static str,
}

#[derive(Debug)]
struct DirectAllowlistHook {
    allowed: Arc<Mutex<HashSet<EndpointId>>>,
}

impl EndpointHooks for DirectAllowlistHook {
    async fn after_handshake(&self, conn: &Connection) -> AfterHandshakeOutcome {
        if self.allowed.lock().unwrap().contains(&conn.remote_id()) {
            AfterHandshakeOutcome::accept()
        } else {
            AfterHandshakeOutcome::Reject {
                error_code: 403u32.into(),
                reason: b"untrusted peer".to_vec(),
            }
        }
    }
}

struct DirectNode {
    endpoint: Endpoint,
    lookup: MemoryLookup,
    acceptor: tokio::task::JoinHandle<()>,
}

struct State {
    local: bool,
    relay: Option<RelayUrl>,
    endpoint: Endpoint,
    lookup: MemoryLookup,
    direct: tokio::sync::Mutex<Option<Arc<DirectNode>>>,
    allowed_direct: Arc<Mutex<HashSet<EndpointId>>>,
    out: Out,
    next_conn: AtomicU64,
    conns: Mutex<HashMap<u64, ConnEntry>>,
}

pub fn run(args: &[std::ffi::OsString]) -> Result<()> {
    let local = args.iter().any(|a| a == "--local");
    let relay = args
        .iter()
        .position(|a| a == "--relay")
        .map(|i| {
            args.get(i + 1)
                .and_then(|url| url.to_str())
                .context("--relay needs a URL")?
                .parse::<RelayUrl>()
                .context("--relay")
        })
        .transpose()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(local, relay))
}

async fn serve(local: bool, relay: Option<RelayUrl>) -> Result<()> {
    let lookup = MemoryLookup::new();
    let builder = if local {
        Endpoint::builder(presets::Minimal)
            .clear_relay_transports()
            .clear_address_lookup()
            .bind_addr("127.0.0.1:0".parse::<SocketAddr>()?)?
    } else {
        // Discovery endpoint is strictly relay-only (`clear_ip_transports`): it binds no UDP
        // socket and has zero direct IP candidates, preventing iroh#3535 IP leaks to untrusted
        // peers on the topic server. Trusted peers upgrade to the lazy direct endpoint.
        Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .relay_mode(match relay.clone() {
                Some(url) => RelayMode::custom([url]),
                None => default_relay_mode(),
            })
    };
    let endpoint = builder
        .address_lookup(lookup.clone())
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .context("binding the link discovery endpoint")?;
    if !local {
        tokio::time::timeout(Duration::from_secs(15), endpoint.online())
            .await
            .context("no relay connection: this endpoint cannot be reached by id")?;
    }

    let (out, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(value) = out_rx.recv().await {
            let mut line = value.to_string();
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    let state = Arc::new(State {
        local,
        relay,
        endpoint: endpoint.clone(),
        lookup,
        direct: tokio::sync::Mutex::new(None),
        allowed_direct: Arc::new(Mutex::new(HashSet::new())),
        out: out.clone(),
        next_conn: AtomicU64::new(1),
        conns: Mutex::new(HashMap::new()),
    });
    out.send(endpoint_event("ready", None, &endpoint))?;

    let acceptor = tokio::spawn(accept_loop(state.clone(), endpoint.clone(), "discovery"));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    // PetPlay owns this process: stdin closing means it is gone.
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(command) => handle(&state, command),
            Err(error) => eprintln!("[link] bad command: {error}"),
        }
    }
    acceptor.abort();
    if let Some(direct) = state.direct.lock().await.take() {
        direct.acceptor.abort();
        direct.endpoint.close().await;
    }
    endpoint.close().await;
    drop(state);
    drop(out);
    let _ = writer.await;
    Ok(())
}

fn endpoint_event(ev: &str, req: Option<Value>, endpoint: &Endpoint) -> Value {
    let addr = endpoint.watch_addr().get();
    let relay = addr.addrs.iter().find_map(|entry| match entry {
        TransportAddr::Relay(url) => Some(url.to_string()),
        _ => None,
    });
    let addrs: Vec<String> = addr
        .addrs
        .iter()
        .filter_map(|entry| match entry {
            TransportAddr::Ip(socket) => Some(socket.to_string()),
            _ => None,
        })
        .collect();
    match req {
        Some(req) => json!({
            "ev": ev,
            "req": req,
            "id": endpoint.id().to_string(),
            "relay": relay,
            "addrs": addrs,
        }),
        None => json!({
            "ev": ev,
            "id": endpoint.id().to_string(),
            "relay": relay,
            "addrs": addrs,
        }),
    }
}

async fn ensure_direct(state: &Arc<State>) -> Result<Arc<DirectNode>> {
    let mut guard = state.direct.lock().await;
    if let Some(existing) = guard.as_ref() {
        return Ok(existing.clone());
    }
    let lookup = MemoryLookup::new();
    let hook = DirectAllowlistHook {
        allowed: state.allowed_direct.clone(),
    };
    let builder = if state.local {
        Endpoint::builder(presets::Minimal)
            .clear_relay_transports()
            .clear_address_lookup()
            .bind_addr("127.0.0.1:0".parse::<SocketAddr>()?)?
    } else {
        // Direct P2P endpoint: keeps IP transports enabled for holepunching, guarded by
        // `DirectAllowlistHook` and never published to the topic server.
        Endpoint::builder(presets::Minimal).relay_mode(match state.relay.clone() {
            Some(url) => RelayMode::custom([url]),
            None => default_relay_mode(),
        })
    };
    let endpoint = builder
        .address_lookup(lookup.clone())
        .alpns(vec![ALPN.to_vec()])
        .hooks(hook)
        .bind()
        .await
        .context("binding the link direct endpoint")?;
    if !state.local {
        tokio::time::timeout(Duration::from_secs(15), endpoint.online())
            .await
            .context("no relay connection for direct endpoint")?;
    }
    let acceptor = tokio::spawn(accept_loop(state.clone(), endpoint.clone(), "direct"));
    let node = Arc::new(DirectNode {
        endpoint,
        lookup,
        acceptor,
    });
    *guard = Some(node.clone());
    Ok(node)
}

fn handle(state: &Arc<State>, command: Value) {
    match command["op"].as_str() {
        Some("connect") => {
            let req = command["req"].clone();
            let tier = match command["tier"].as_str() {
                Some("direct") => "direct",
                _ => "discovery",
            };
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(error) = connect(&state, &command["endpoint"], req.clone(), tier).await {
                    let _ = state
                        .out
                        .send(json!({ "ev": "failed", "req": req, "error": format!("{error:#}") }));
                }
            });
        }
        Some("direct_endpoint") => {
            let req = command["req"].clone();
            let state = state.clone();
            tokio::spawn(async move {
                match ensure_direct(&state).await {
                    Ok(direct) => {
                        let _ = state
                            .out
                            .send(endpoint_event("direct_ready", Some(req), &direct.endpoint));
                    }
                    Err(error) => {
                        let _ = state.out.send(json!({
                            "ev": "failed",
                            "req": req,
                            "error": format!("{error:#}"),
                        }));
                    }
                }
            });
        }
        Some("allow_direct") => {
            if let Some(peer) = command["peer"].as_str() {
                match peer.parse::<EndpointId>() {
                    Ok(id) => {
                        state.allowed_direct.lock().unwrap().insert(id);
                    }
                    Err(error) => eprintln!("[link] bad allow_direct peer {peer:?}: {error}"),
                }
            }
        }
        Some("revoke_direct") => {
            if let Some(peer) = command["peer"].as_str() {
                match peer.parse::<EndpointId>() {
                    Ok(id) => {
                        state.allowed_direct.lock().unwrap().remove(&id);
                        let to_close: Vec<_> = state
                            .conns
                            .lock()
                            .unwrap()
                            .values()
                            .filter(|entry| entry.tier == "direct" && entry.peer == id)
                            .map(|entry| entry.tx.clone())
                            .collect();
                        for tx in to_close {
                            let _ = tx.send(ConnCommand::Close("peer untrusted".to_owned()));
                        }
                    }
                    Err(error) => eprintln!("[link] bad revoke_direct peer {peer:?}: {error}"),
                }
            }
        }
        Some(op @ ("send" | "close")) => {
            let Some(conn) = command["conn"].as_u64() else {
                return;
            };
            let sender = state
                .conns
                .lock()
                .unwrap()
                .get(&conn)
                .map(|entry| entry.tx.clone());
            let Some(sender) = sender else { return };
            let _ = sender.send(if op == "send" {
                ConnCommand::Send(command["data"].as_str().unwrap_or_default().to_owned())
            } else {
                ConnCommand::Close(command["reason"].as_str().unwrap_or("closed").to_owned())
            });
        }
        other => eprintln!("[link] unknown op {other:?}"),
    }
}

fn parse_endpoint(value: &Value) -> Result<EndpointAddr> {
    let id: EndpointId = value["id"]
        .as_str()
        .context("endpoint.id")?
        .parse()
        .context("endpoint.id is not an iroh endpoint id")?;
    let mut addr = EndpointAddr::new(id);
    if let Some(relay) = value["relay"].as_str() {
        addr = addr.with_relay_url(relay.parse::<RelayUrl>().context("endpoint.relay")?);
    }
    for entry in value["addrs"].as_array().into_iter().flatten() {
        let socket: SocketAddr = entry.as_str().context("endpoint.addrs")?.parse()?;
        addr = addr.with_ip_addr(socket);
    }
    if addr.addrs.is_empty() {
        bail!(
            "an endpoint needs a relay URL or an address; this link publishes and looks up nothing"
        );
    }
    Ok(addr)
}

async fn connect(state: &Arc<State>, endpoint: &Value, req: Value, tier: &'static str) -> Result<u64> {
    let addr = parse_endpoint(endpoint)?;
    let peer_id = addr.id;
    let connection = if tier == "direct" {
        state.allowed_direct.lock().unwrap().insert(peer_id);
        let direct = ensure_direct(state).await?;
        direct.lookup.add_endpoint_info(addr.clone());
        tokio::time::timeout(Duration::from_secs(20), direct.endpoint.connect(addr, ALPN))
            .await
            .context("timed out connecting (direct)")??
    } else {
        state.lookup.add_endpoint_info(addr.clone());
        tokio::time::timeout(Duration::from_secs(20), state.endpoint.connect(addr, ALPN))
            .await
            .context("timed out connecting")??
    };
    let (send, recv) = connection.open_bi().await?;
    let peer = peer_id.to_string();
    Ok(start_conn(
        state,
        connection,
        peer_id,
        tier,
        send,
        recv,
        |conn| json!({ "ev": "connected", "req": req, "conn": conn, "peer": peer, "tier": tier }),
    ))
}

async fn accept_loop(state: Arc<State>, endpoint: Endpoint, tier: &'static str) {
    while let Some(incoming) = endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            let accepted = async {
                let connection = incoming.await?;
                let (send, recv) = connection.accept_bi().await?;
                anyhow::Ok((connection, send, recv))
            };
            match accepted.await {
                Ok((connection, send, recv)) => {
                    let peer_id = connection.remote_id();
                    let peer = peer_id.to_string();
                    start_conn(
                        &state,
                        connection,
                        peer_id,
                        tier,
                        send,
                        recv,
                        |conn| json!({ "ev": "incoming", "conn": conn, "peer": peer, "tier": tier }),
                    );
                }
                Err(error) => eprintln!("[link] incoming ({tier}) connection failed: {error:#}"),
            }
        });
    }
}

fn start_conn(
    state: &Arc<State>,
    connection: Connection,
    peer: EndpointId,
    tier: &'static str,
    mut send: impl tokio::io::AsyncWrite + Unpin + Send + 'static,
    mut recv: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    announce: impl FnOnce(u64) -> Value,
) -> u64 {
    let conn = state.next_conn.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::unbounded_channel::<ConnCommand>();
    state
        .conns
        .lock()
        .unwrap()
        .insert(conn, ConnEntry { tx, peer, tier });
    // Announced before the reader starts, so PetPlay knows the connection before its data.
    let _ = state.out.send(announce(conn));

    let writer_connection = connection.clone();
    tokio::spawn(async move {
        while let Some(command) = rx.recv().await {
            match command {
                ConnCommand::Send(text) => {
                    let bytes = text.as_bytes();
                    if send
                        .write_all(&(bytes.len() as u32).to_be_bytes())
                        .await
                        .is_err()
                        || send.write_all(bytes).await.is_err()
                    {
                        break;
                    }
                }
                ConnCommand::Close(reason) => {
                    // The reason travels in-band: a QUIC close can overtake unread stream data.
                    let reason_bytes = &reason.as_bytes()[..reason.len().min(1024)];
                    let _ = send.write_all(&CLOSE_MARKER.to_be_bytes()).await;
                    let _ = send
                        .write_all(&(reason_bytes.len() as u32).to_be_bytes())
                        .await;
                    let _ = send.write_all(reason_bytes).await;
                    let _ = send.shutdown().await;
                    // Let the peer read what was written before the close arrives.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    writer_connection.close(0u32.into(), reason.as_bytes());
                    break;
                }
            }
        }
    });

    let state = state.clone();
    tokio::spawn(async move {
        let reason = loop {
            let mut header = [0u8; 4];
            if let Err(error) = recv.read_exact(&mut header).await {
                break closed_reason(&connection, error);
            }
            if u32::from_be_bytes(header) == CLOSE_MARKER {
                let mut size = [0u8; 4];
                let mut reason = String::from("closed");
                if recv.read_exact(&mut size).await.is_ok() {
                    let mut bytes = vec![0u8; (u32::from_be_bytes(size) as usize).min(1024)];
                    if recv.read_exact(&mut bytes).await.is_ok() {
                        reason = String::from_utf8_lossy(&bytes).into_owned();
                    }
                }
                break reason;
            }
            let length = u32::from_be_bytes(header) as usize;
            if length > MAX_FRAME {
                connection.close(1u32.into(), b"frame too large");
                break "frame too large".to_owned();
            }
            let mut body = vec![0u8; length];
            if let Err(error) = recv.read_exact(&mut body).await {
                break closed_reason(&connection, error);
            }
            let Ok(text) = String::from_utf8(body) else {
                connection.close(1u32.into(), b"frame is not UTF-8");
                break "frame is not UTF-8".to_owned();
            };
            let _ = state
                .out
                .send(json!({ "ev": "data", "conn": conn, "data": text }));
        };
        state.conns.lock().unwrap().remove(&conn);
        let _ = state
            .out
            .send(json!({ "ev": "closed", "conn": conn, "reason": reason }));
    });
    conn
}

fn closed_reason(connection: &Connection, error: std::io::Error) -> String {
    match connection.close_reason() {
        Some(reason) => reason.to_string(),
        None => format!("stream ended: {error}"),
    }
}
