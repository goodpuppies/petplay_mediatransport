//! Platform-independent native subscription and bounded encoded-frame queue.
use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use std::{
    sync::mpsc::{Receiver, TrySendError, sync_channel},
    time::{Duration, Instant},
};
pub struct Unit {
    pub bytes: Bytes,
    pub timestamp: u64,
    pub reset: bool,
    pub width: u32,
    pub height: u32,
}

pub struct Input {
    pub runtime: tokio::runtime::Runtime,
    pub network: tokio::task::JoinHandle<Result<()>>,
    pub rx: Receiver<Unit>,
}
impl Input {
    pub fn connect(args: &[std::ffi::OsString]) -> Result<Self> {
        let shared = if args.len() == 2 {
            Some(crate::peer::decode(
                args[0].to_str().context("share code")?,
            )?)
        } else {
            None
        };
        let (url, direct) = if let Some(addr) = &shared {
            (
                format!("iroh://{}/", addr.id).parse()?,
                crate::peer::direct_addresses(addr),
            )
        } else {
            (
                args[0].to_str().context("URL")?.parse::<url::Url>()?,
                vec![args[1].to_str().context("address")?.parse()?],
            )
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let (tx, rx) = sync_channel::<Unit>(3);
        let network = runtime.spawn(async move {
            let _ = moq_tokio::crypto::install_default();
            let endpoint = if let Some(addr) = shared.clone() {
                crate::peer::endpoint(Some(addr)).await?
            } else {
                crate::publish::local_endpoint().await?
            };
            let origin = moq_tokio::origin::spawn();
            let consumer = origin.consume();
            let mut config = moq_tokio::connect::Config::default();
            config.once = Some(true);
            let client = config
                .init(Default::default())?
                .with_iroh(endpoint.clone())
                .with_iroh_addrs(direct)
                .with_subscriber(origin);
            let session = client.connect(url);
            let stream = async {
                if let Some(addr) = &shared {
                    crate::peer::require_direct(&endpoint, addr.id, Duration::from_secs(15))
                        .await?;
                    println!("RECEIVER_DIRECT");
                }
                let deadline = Instant::now() + Duration::from_secs(15);
                let broadcast = loop {
                    match consumer.request_broadcast("petplay").await {
                        Ok(b) => break b,
                        Err(moq_net::Error::Unroutable) if Instant::now() < deadline => {
                            tokio::time::sleep(Duration::from_millis(50)).await
                        }
                        Err(e) => return Err(e.into()),
                    }
                };
                let mut catalog = broadcast.track("config")?.subscribe(None).await?;
                let mut group = catalog
                    .recv_group()
                    .await?
                    .context("missing config group")?;
                let frame = group.read_frame().await?.context("missing config frame")?;
                ensure!(frame.payload.len() <= 65536, "config too large");
                let config: serde_json::Value = serde_json::from_slice(&frame.payload)?;
                ensure!(
                    config["version"] == 1
                        && config["format"] == "annexb"
                        && config["stereo"] == "none"
                        && config["codec"]
                            .as_str()
                            .is_some_and(|s| s.starts_with("avc1.")),
                    "receiver needs a mono Annex-B H.264 source"
                );
                let dimension = |name: &str| -> Result<u32> {
                    let n = config[name].as_u64().context("missing geometry")?;
                    ensure!(
                        (16..=8192).contains(&n) && n % 2 == 0,
                        "invalid source geometry"
                    );
                    Ok(n as u32)
                };
                let width = dimension("width")?;
                let height = dimension("height")?;
                let mut video = broadcast.track("video")?.subscribe(None).await?;
                let mut need_key = true;
                let mut skipped = false;
                loop {
                    let Some(mut group) = video.recv_group().await? else {
                        break;
                    };
                    loop {
                        let frame = match group.read_frame().await {
                            Ok(Some(f)) => f,
                            Ok(None) => break,
                            Err(_) => {
                                need_key = true;
                                skipped = true;
                                break;
                            }
                        };
                        let payload = frame.payload;
                        ensure!(
                            (10..=4 * 1024 * 1024).contains(&payload.len()) && payload[0] <= 1,
                            "invalid encoded access unit"
                        );
                        let key = payload[0] == 1;
                        if need_key && !key {
                            continue;
                        }
                        let timestamp = u64::from_be_bytes(payload[1..9].try_into()?);
                        let unit = Unit {
                            bytes: payload.slice(9..),
                            timestamp,
                            reset: skipped,
                            width,
                            height,
                        };
                        match tx.try_send(unit) {
                            Ok(()) => {
                                need_key = false;
                                skipped = false;
                            }
                            Err(TrySendError::Full(_)) => {
                                need_key = true;
                                skipped = true;
                            }
                            Err(TrySendError::Disconnected(_)) => return Ok(()),
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            };
            tokio::pin!(stream);
            if let Some(addr) = shared.clone() {
                let policy = async {
                    crate::peer::require_direct(&endpoint, addr.id, Duration::from_secs(15))
                        .await?;
                    crate::peer::watch_direct(&endpoint, addr.id).await
                };
                tokio::select! {r=&mut stream=>r,r=session.closed()=>{r?;Ok(())},r=policy=>r}
            } else {
                tokio::select! {r=&mut stream=>r,r=session.closed()=>{r?;Ok(())}}
            }
        });
        Ok(Self {
            runtime,
            network,
            rx,
        })
    }
    pub fn finish(self) -> Result<()> {
        if self.network.is_finished() {
            self.runtime.block_on(self.network)??;
        } else {
            self.network.abort();
        }
        Ok(())
    }
}
