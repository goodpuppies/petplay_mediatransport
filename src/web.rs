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
    let html = std::fs::read(root.join("index.html"))?;
    let js = std::fs::read(root.join("dist/viewer.js"))
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
            let (code, mime, body) = if method != "GET" {
                (
                    "405 Method Not Allowed",
                    "text/plain",
                    b"GET required".as_slice(),
                )
            } else {
                match path {
                    "/" => ("200 OK", "text/html; charset=utf-8", html.as_slice()),
                    "/viewer.js" => ("200 OK", "text/javascript; charset=utf-8", js.as_slice()),
                    "/config.json" => ("200 OK", "application/json", config.as_slice()),
                    _ => ("404 Not Found", "text/plain", b"Not found".as_slice()),
                }
            };
            let header = format!(
                "HTTP/1.1 {code}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = client
                .write_all(header.as_bytes())
                .and_then(|()| client.write_all(body));
        }
    });
    println!("VIEWER_READY http://{http}/");
    Ok(())
}
