//! `petplay-media link`: an iroh endpoint that carries stageforge hub-to-hub links.
//!
//! A PetPlay hub starts this as a child process and drives it over stdio with newline-delimited
//! JSON, so no native code loads into the hub and the same protocol works on every platform.
//! Each link is one iroh connection with ALPN [`ALPN`] and one bidirectional stream of
//! length-prefixed UTF-8 frames (the hub's link frames, already JSON text).
//!
//! Hub → link (stdin):
//! - `{"op":"connect","req":N,"endpoint":{"id":…,"relay":…?,"addrs":[…]?}}`
//! - `{"op":"send","conn":C,"data":"…"}`
//! - `{"op":"close","conn":C,"reason":"…"}`
//!
//! Link → hub (stdout):
//! - `{"ev":"ready","id":…,"relay":…|null,"addrs":[…]}` once, when the endpoint can be reached
//! - `{"ev":"connected","req":N,"conn":C}` / `{"ev":"failed","req":N,"error":…}`
//! - `{"ev":"incoming","conn":C,"peer":…}`
//! - `{"ev":"data","conn":C,"data":"…"}`
//! - `{"ev":"closed","conn":C,"reason":…}`
//!
//! The endpoint id plus relay URL is what a hub publishes in a topic record: it identifies and
//! authenticates the peer and reveals no IP address. iroh holepunches to a direct path through
//! the relay when it can; until then (and always, for browsers) frames go through the relay,
//! end-to-end encrypted. Better transports are negotiated over the link afterwards.
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
    endpoint::{Connection, RelayMode, default_relay_mode, presets},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
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

struct State {
    endpoint: Endpoint,
    lookup: MemoryLookup,
    out: Out,
    next_conn: AtomicU64,
    conns: Mutex<HashMap<u64, mpsc::UnboundedSender<ConnCommand>>>,
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
        // `Minimal` publishes nothing about this endpoint; peers reach it by the id and relay
        // URL a hub hands them. The relay coordinates holepunching.
        Endpoint::builder(presets::Minimal).relay_mode(match relay {
            Some(url) => RelayMode::custom([url]),
            None => default_relay_mode(),
        })
    };
    let endpoint = builder
        .address_lookup(lookup.clone())
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
        .context("binding the link endpoint")?;
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
        endpoint: endpoint.clone(),
        lookup,
        out: out.clone(),
        next_conn: AtomicU64::new(1),
        conns: Mutex::new(HashMap::new()),
    });
    out.send(ready_event(&endpoint))?;

    let acceptor = tokio::spawn(accept_loop(state.clone()));
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    // The hub owns this process: stdin closing means the hub is gone.
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
    endpoint.close().await;
    drop(state);
    drop(out);
    let _ = writer.await;
    Ok(())
}

fn ready_event(endpoint: &Endpoint) -> Value {
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
    json!({ "ev": "ready", "id": endpoint.id().to_string(), "relay": relay, "addrs": addrs })
}

fn handle(state: &Arc<State>, command: Value) {
    match command["op"].as_str() {
        Some("connect") => {
            let req = command["req"].clone();
            let state = state.clone();
            tokio::spawn(async move {
                if let Err(error) = connect(&state, &command["endpoint"], req.clone()).await {
                    let _ = state
                        .out
                        .send(json!({ "ev": "failed", "req": req, "error": format!("{error:#}") }));
                }
            });
        }
        Some(op @ ("send" | "close")) => {
            let Some(conn) = command["conn"].as_u64() else {
                return;
            };
            let sender = state.conns.lock().unwrap().get(&conn).cloned();
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

async fn connect(state: &Arc<State>, endpoint: &Value, req: Value) -> Result<u64> {
    let addr = parse_endpoint(endpoint)?;
    state.lookup.add_endpoint_info(addr.clone());
    let connection =
        tokio::time::timeout(Duration::from_secs(20), state.endpoint.connect(addr, ALPN))
            .await
            .context("timed out connecting")??;
    let (send, recv) = connection.open_bi().await?;
    Ok(start_conn(
        state,
        connection,
        send,
        recv,
        |conn| json!({ "ev": "connected", "req": req, "conn": conn }),
    ))
}

async fn accept_loop(state: Arc<State>) {
    while let Some(incoming) = state.endpoint.accept().await {
        let state = state.clone();
        tokio::spawn(async move {
            let accepted = async {
                let connection = incoming.await?;
                let (send, recv) = connection.accept_bi().await?;
                anyhow::Ok((connection, send, recv))
            };
            match accepted.await {
                Ok((connection, send, recv)) => {
                    let peer = connection.remote_id().to_string();
                    start_conn(
                        &state,
                        connection,
                        send,
                        recv,
                        |conn| json!({ "ev": "incoming", "conn": conn, "peer": peer }),
                    );
                }
                Err(error) => eprintln!("[link] incoming connection failed: {error:#}"),
            }
        });
    }
}

fn start_conn(
    state: &Arc<State>,
    connection: Connection,
    mut send: impl tokio::io::AsyncWrite + Unpin + Send + 'static,
    mut recv: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    announce: impl FnOnce(u64) -> Value,
) -> u64 {
    let conn = state.next_conn.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = mpsc::unbounded_channel::<ConnCommand>();
    state.conns.lock().unwrap().insert(conn, tx);
    // Announced before the reader starts, so the hub knows the connection before its data.
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
