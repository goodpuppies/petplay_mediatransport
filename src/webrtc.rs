//! WebRTC H.264 publisher backed by `str0m` (pure Rust, ICE-lite, DTLS-SRTP).
//!
//! Runs alongside the MoQ/WebTransport server in `Publisher` so browsers on plain HTTP
//! (e.g. a direct Tailscale URL without `tailscale serve`, where `WebTransport` and
//! `VideoDecoder` are disabled) can still receive hardware-decoded H.264 video.
//!
//! Signaling is a loopback-only `POST http://127.0.0.1:<port>/offer` endpoint accepting a raw
//! browser SDP offer (with a `recvonly` video transceiver) and returning the SDP answer.
//! Only the local `desktopSource` actor talks to this endpoint; remote browsers exchange SDP
//! over StageForge's encrypted actor route.

use anyhow::{Context, Result};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, Once, RwLock},
    time::Instant,
};
use str0m::{
    Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcConfig,
    change::SdpOffer,
    format::Codec,
    media::{MediaKind, MediaTime, Mid, Pt},
    net::{Protocol, Receive},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    sync::broadcast,
};

static CRYPTO_INIT: Once = Once::new();

fn ensure_crypto() {
    CRYPTO_INIT.call_once(|| {
        str0m::crypto::from_feature_flags().install_process_default();
    });
}

#[derive(Clone)]
pub struct EncodedFrame {
    pub bytes: Arc<[u8]>,
    pub key: bool,
    pub micros: u64,
}

#[derive(Clone)]
pub struct WebRtcHub {
    bind_ip: IpAddr,
    tx: broadcast::Sender<EncodedFrame>,
    latest_key: Arc<RwLock<Option<EncodedFrame>>>,
}

impl WebRtcHub {
    /// Start a loopback-only SDP signaling listener and return the hub to push frames into.
    /// Announces `WEBRTC_READY http://127.0.0.1:<port>/offer` on stdout.
    pub fn start(runtime: &tokio::runtime::Runtime, bind_ip: IpAddr) -> Result<(Self, SocketAddr)> {
        ensure_crypto();
        let (tx, _) = broadcast::channel(64);
        let hub = Self {
            bind_ip,
            tx,
            latest_key: Arc::new(RwLock::new(None)),
        };
        let listener = runtime.block_on(TcpListener::bind(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
        )))?;
        let signal_addr = listener.local_addr()?;
        let server_hub = hub.clone();
        runtime.spawn(async move {
            serve_signaling(listener, server_hub).await;
        });
        println!("WEBRTC_READY http://{signal_addr}/offer");
        Ok((hub, signal_addr))
    }

    /// Push one encoded Annex-B H.264 access unit to all active WebRTC viewers.
    pub fn push(&self, bytes: &[u8], key: bool, micros: u64) {
        let has_viewers = self.tx.receiver_count() > 0;
        if !has_viewers && !key {
            return;
        }
        let frame = EncodedFrame {
            bytes: Arc::from(bytes),
            key,
            micros,
        };
        if key {
            if let Ok(mut slot) = self.latest_key.write() {
                *slot = Some(frame.clone());
            }
        }
        if has_viewers {
            let _ = self.tx.send(frame);
        }
    }

    /// Accept one browser SDP offer and spawn its ICE-lite UDP peer loop.
    pub async fn accept_offer(&self, offer_sdp: &str) -> Result<String> {
        ensure_crypto();
        let offer = SdpOffer::from_sdp_string(offer_sdp).context("invalid SDP offer")?;
        let udp = UdpSocket::bind(SocketAddr::new(self.bind_ip, 0))
            .await
            .with_context(|| format!("bind WebRTC UDP on {}", self.bind_ip))?;
        let local_addr = udp.local_addr()?;

        let mut rtc = RtcConfig::new()
            .set_ice_lite(true)
            .clear_codecs()
            .enable_h264(true)
            .build(Instant::now());
        rtc.add_local_candidate(Candidate::host(local_addr, "udp")?)
            .context("add local WebRTC candidate")?;

        let answer = rtc
            .sdp_api()
            .accept_offer(offer)
            .context("accept WebRTC SDP offer")?;
        let answer_sdp = answer.to_sdp_string();

        let rx = self.tx.subscribe();
        let latest_key = self.latest_key.clone();
        tokio::spawn(async move {
            if let Err(err) = run_peer(rtc, udp, local_addr, rx, latest_key).await {
                eprintln!("webrtc peer ended: {err}");
            }
        });
        Ok(answer_sdp)
    }
}

async fn run_peer(
    mut rtc: Rtc,
    udp: UdpSocket,
    local_addr: SocketAddr,
    mut rx: broadcast::Receiver<EncodedFrame>,
    latest_key: Arc<RwLock<Option<EncodedFrame>>>,
) -> Result<()> {
    let mut video_mid: Option<Mid> = None;
    let mut video_pt: Option<Pt> = None;
    let mut sent_keyframe = false;
    let mut buf = vec![0u8; 2048];

    loop {
        // Drain all pending str0m outputs until it asks to wait for a timeout/input.
        let timeout = loop {
            match rtc.poll_output()? {
                Output::Timeout(at) => break at,
                Output::Transmit(tx) => {
                    let _ = udp.send_to(&tx.contents, tx.destination).await;
                }
                Output::Event(event) => match event {
                    Event::MediaAdded(m) if m.kind == MediaKind::Video => {
                        video_mid = Some(m.mid);
                        video_pt = rtc.writer(m.mid).and_then(|w| {
                            w.payload_params()
                                .find(|p| p.spec().codec == Codec::H264)
                                .map(|p| p.pt())
                        });
                    }
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                        return Ok(());
                    }
                    Event::KeyframeRequest(_) => {
                        sent_keyframe = false;
                        if let Some(frame) = latest_key.read().ok().and_then(|g| g.clone()) {
                            send_frame(&mut rtc, video_mid, video_pt, &frame, &mut sent_keyframe);
                        }
                    }
                    _ => {}
                },
            }
        };

        // Once ICE + DTLS are up, prime the viewer immediately with the latest cached keyframe
        // so it doesn't wait up to a full GOP interval on a still desktop.
        if !sent_keyframe && rtc.is_connected() && video_mid.is_some() && video_pt.is_some() {
            if let Some(frame) = latest_key.read().ok().and_then(|g| g.clone()) {
                send_frame(&mut rtc, video_mid, video_pt, &frame, &mut sent_keyframe);
                continue;
            }
        }

        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(timeout));
        tokio::pin!(sleep);

        tokio::select! {
            _ = &mut sleep => {
                rtc.handle_input(Input::Timeout(Instant::now()))?;
            }
            recv = udp.recv_from(&mut buf) => {
                let (n, source) = recv?;
                if let Ok(contents) = (&buf[..n]).try_into() {
                    rtc.handle_input(Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: local_addr,
                            contents,
                        },
                    ))?;
                }
            }
            frame = rx.recv() => {
                match frame {
                    Ok(frame) => {
                        if rtc.is_connected() {
                            send_frame(&mut rtc, video_mid, video_pt, &frame, &mut sent_keyframe);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        sent_keyframe = false;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
}

fn send_frame(
    rtc: &mut Rtc,
    mid: Option<Mid>,
    pt: Option<Pt>,
    frame: &EncodedFrame,
    sent_keyframe: &mut bool,
) {
    let (Some(mid), Some(pt)) = (mid, pt) else {
        return;
    };
    if !*sent_keyframe && !frame.key {
        return;
    }
    if let Some(writer) = rtc.writer(mid) {
        if writer
            .write(
                pt,
                Instant::now(),
                MediaTime::from_micros(frame.micros),
                frame.bytes.clone(),
            )
            .is_ok()
        {
            if frame.key {
                *sent_keyframe = true;
            }
        }
    }
}

async fn serve_signaling(listener: TcpListener, hub: WebRtcHub) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            break;
        };
        let hub = hub.clone();
        tokio::spawn(async move {
            let _ = handle_http(&mut stream, &hub).await;
        });
    }
}

async fn handle_http(stream: &mut tokio::net::TcpStream, hub: &WebRtcHub) -> Result<()> {
    let mut raw = Vec::with_capacity(8192);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        raw.extend_from_slice(&chunk[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if raw.len() > 64 * 1024 {
            return write_response(stream, "413 Payload Too Large", "headers too large").await;
        }
    };

    let headers = String::from_utf8_lossy(&raw[..header_end]);
    let mut first = headers.lines().next().unwrap_or("").split_whitespace();
    let method = first.next().unwrap_or("");
    let path = first.next().unwrap_or("").split('?').next().unwrap_or("");
    if method != "POST" || path != "/offer" {
        return write_response(stream, "404 Not Found", "POST /offer required").await;
    }

    let content_length = headers
        .lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    if content_length > 256 * 1024 {
        return write_response(stream, "413 Payload Too Large", "offer too large").await;
    }

    while raw.len() < header_end + content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&raw[header_end..raw.len().min(header_end + content_length)]);
    match hub.accept_offer(&body).await {
        Ok(answer) => write_response(stream, "200 OK", &answer).await,
        Err(err) => write_response(stream, "400 Bad Request", &format!("{err:#}")).await,
    }
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    body: &str,
) -> Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use str0m::media::Direction;

    #[test]
    fn webrtc_hub_accepts_h264_recvonly_offer() {
        ensure_crypto();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (hub, _addr) =
            WebRtcHub::start(&rt, IpAddr::V4(Ipv4Addr::LOCALHOST)).expect("start hub");

        // Build a synthetic browser-like recvonly H.264 offer with str0m.
        let mut client = RtcConfig::new()
            .clear_codecs()
            .enable_h264(true)
            .build(Instant::now());
        client
            .add_local_candidate(
                Candidate::host("127.0.0.1:50000".parse().unwrap(), "udp").unwrap(),
            )
            .unwrap();
        let mut changes = client.sdp_api();
        changes.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let (offer, _pending) = changes.apply().unwrap();

        let answer = rt
            .block_on(hub.accept_offer(&offer.to_sdp_string()))
            .expect("accept offer");
        assert!(answer.contains("a=sendonly"), "expected sendonly in {answer}");
        assert!(answer.contains("H264"), "expected H264 in {answer}");
        assert!(answer.contains("a=ice-lite"), "expected ice-lite in {answer}");
    }
}
