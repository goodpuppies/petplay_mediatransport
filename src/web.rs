//! Local viewer assets and certificate pin discovery, separate from media transport.
use anyhow::{Context, Result};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener},
    path::PathBuf,
    time::Duration,
};
pub fn start(quic: SocketAddr, fingerprint: &str) -> Result<()> {
    let http = SocketAddr::new(
        quic.ip(),
        quic.port().checked_add(1).context("HTTP port overflow")?,
    );
    let listener = TcpListener::bind(http)?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web");
    // Checked once so a missing build fails at startup; served fresh on every request, so a
    // rebuilt viewer reaches browsers without restarting the gateway.
    std::fs::metadata(root.join("index.html"))?;
    std::fs::metadata(root.join("dist/viewer.js"))
        .context("run bun install and bun run build in the mediatransport web/ directory")?;
    let config =
        format!("{{\"url\":\"https://{quic}/\",\"fingerprint\":\"{fingerprint}\"}}").into_bytes();
    std::thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(mut client) = client else { break };
            let _ = client.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = client.set_write_timeout(Some(Duration::from_secs(2)));
            let mut input = [0u8; 8192];
            let Ok(n) = client.read(&mut input) else {
                continue;
            };
            let line = String::from_utf8_lossy(&input[..n]);
            let mut parts = line.lines().next().unwrap_or("").split_whitespace();
            let method = parts.next().unwrap_or("");
            let path = parts.next().unwrap_or("").split('?').next().unwrap_or("");
            let file = |name: &str| std::fs::read(root.join(name)).ok();
            let modified = |name: &str| {
                std::fs::metadata(root.join(name))
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis().to_string().into_bytes())
            };
            let found = |mime, body: Option<Vec<u8>>| match body {
                Some(body) => ("200 OK", mime, body),
                None => ("404 Not Found", "text/plain", b"Not found".to_vec()),
            };
            let (code, mime, body) = if method != "GET" {
                (
                    "405 Method Not Allowed",
                    "text/plain",
                    b"GET required".to_vec(),
                )
            } else {
                match path {
                    "/" => found("text/html; charset=utf-8", file("index.html")),
                    "/viewer.js" => found("text/javascript; charset=utf-8", file("dist/viewer.js")),
                    // A hot-swappable XR presenter, and its build stamp for change polling.
                    "/presenter.js" => {
                        found("text/javascript; charset=utf-8", file("dist/presenter.js"))
                    }
                    "/build" => found("text/plain", modified("dist/presenter.js")),
                    "/config.json" => ("200 OK", "application/json", config.clone()),
                    _ => found("text/plain", None),
                }
            };
            let header = format!(
                "HTTP/1.1 {code}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = client
                .write_all(header.as_bytes())
                .and_then(|()| client.write_all(&body));
        }
    });
    println!("VIEWER_READY http://{http}/");
    Ok(())
}
