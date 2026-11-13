//! Subscribe natively, decode on VA-API, and export RGB through the local GPU slot contract.
use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use moq_vaapi::{
    decode::{Config, Decoder},
    dmabuf::{DmaBuf, Plane},
    vpp::Processor,
};
use std::{
    path::Path,
    sync::mpsc::{RecvTimeoutError, TrySendError, sync_channel},
    time::{Duration, Instant},
};
struct Unit {
    bytes: Bytes,
    timestamp: u64,
    reset: bool,
    width: u32,
    height: u32,
}

pub fn run(args: &[std::ffi::OsString]) -> Result<()> {
    ensure!(
        (2..=3).contains(&args.len()),
        "usage: petplay-media receive SHARE_CODE SOCKET\n or: petplay-media receive IROH_URL DIRECT_ADDRESS SOCKET"
    );
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
    let mut output = crate::receive_output::Output::bind(Path::new(&args[args.len() - 1]))?;
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
                crate::peer::require_direct(&endpoint, addr.id, Duration::from_secs(15)).await?;
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
                crate::peer::require_direct(&endpoint, addr.id, Duration::from_secs(15)).await?;
                crate::peer::watch_direct(&endpoint, addr.id).await
            };
            tokio::select! {r=&mut stream=>r,r=session.closed()=>{r?;Ok(())},r=policy=>r}
        } else {
            tokio::select! {r=&mut stream=>r,r=session.closed()=>{r?;Ok(())}}
        }
    });
    let make_decoder = || {
        let mut config = Config::new();
        config.device = Some("/dev/dri/renderD128".into());
        Decoder::new(config)
    };
    let mut decoder = make_decoder()?;
    let processor = Processor::new("/dev/dri/renderD128")?;
    let mut decoded = 0u64;
    let mut geometry = None;
    loop {
        if !output.collect()? {
            break;
        }
        let unit = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(unit) => unit,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        if unit.reset {
            decoder = make_decoder()?;
        }
        for frame in decoder
            .decode_exported(&unit.bytes, unit.timestamp)
            .context("hardware H.264 decode")?
        {
            ensure!(
                frame.width == unit.width && frame.height == unit.height,
                "decoded geometry differs from catalog"
            );
            ensure!(
                (16..=8192).contains(&frame.width) && (16..=8192).contains(&frame.height),
                "decoded geometry out of bounds"
            );
            ensure!(
                geometry.is_none() || geometry == Some((frame.width, frame.height)),
                "resize requires a new receiver session"
            );
            geometry = Some((frame.width, frame.height));
            let descriptor = &frame.descriptor;
            ensure!(
                descriptor.objects.len() == 1 && descriptor.layers.len() == 1,
                "unsupported NV12 export layout"
            );
            let object = &descriptor.objects[0];
            let layer = &descriptor.layers[0];
            ensure!(
                layer.num_planes == 2
                    && layer.drm_format == u32::from_le_bytes(*b"NV12")
                    && layer.object_index[..2] == [0, 0],
                "unsupported NV12 planes"
            );
            let input = processor.import(DmaBuf {
                drm_format: layer.drm_format,
                modifier: object.drm_format_modifier,
                width: frame.width,
                height: frame.height,
                planes: (0..layer.num_planes as usize)
                    .map(|i| Plane {
                        offset: layer.offset[i],
                        pitch: layer.pitch[i],
                    })
                    .collect(),
                fd: object.fd.try_clone()?,
                color: None,
            })?;
            output.present(
                &processor,
                &input,
                frame.width,
                frame.height,
                frame.timestamp,
            )?;
            decoded += 1;
            if decoded == 1 || decoded % 60 == 0 {
                println!(
                    "RECEIVER_FRAME decoded={decoded} sent={} dropped={} width={} height={} raw_cpu_bytes=0",
                    output.sent, output.dropped, frame.width, frame.height
                );
            }
        }
    }
    if network.is_finished() {
        runtime.block_on(network)??;
    } else {
        network.abort();
    }
    println!(
        "RECEIVER_STOPPED decoded={decoded} sent={} dropped={}",
        output.sent, output.dropped
    );
    Ok(())
}
