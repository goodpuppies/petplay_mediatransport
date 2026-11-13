//! Small MoQ broadcast: config JSON and timestamped Annex-B access units.
use anyhow::{Context, Result};
use moq_net::{Timescale, Timestamp, broadcast, group, origin, track};
use std::{net::SocketAddr, time::Duration};
pub struct Publisher {
    pub fingerprint: String,
    _runtime: tokio::runtime::Runtime,
    _origin: origin::Producer,
    _broadcast: broadcast::Producer,
    _config: track::Producer,
    video: track::Producer,
    group: Option<group::Producer>,
    sequence: u64,
    start: Option<u64>,
}
impl Publisher {
    pub fn new(bind: SocketAddr, iroh_local: bool) -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let _enter = runtime.enter();
        let _ = moq_tokio::crypto::install_default();
        let origin = moq_tokio::origin::spawn();
        let broadcast = origin.create_broadcast("petplay")?;
        let config = broadcast.create_track("config", None)?;
        let video = broadcast.create_track(
            "video",
            track::Info::default()
                .with_timescale(Timescale::MICRO)
                .with_max_age(Duration::from_secs(1)),
        )?;
        broadcast.announce(Default::default())?;
        let fingerprint = serve(&runtime, origin.consume(), bind, iroh_local)?;
        drop(_enter);
        Ok(Self {
            fingerprint,
            _runtime: runtime,
            _origin: origin,
            _broadcast: broadcast,
            _config: config,
            video,
            group: None,
            sequence: 0,
            start: None,
        })
    }
    pub fn frame(&mut self, bytes: &[u8], key: bool, timestamp: u64) -> Result<()> {
        let start = *self.start.get_or_insert(timestamp);
        let micros = (timestamp - start) / 1000;
        if self.group.is_none() {
            // Annex-B SPS is carried in-band. Discover profile/compatibility/level
            // from the actual encoder output rather than guessing a codec string.
            let codec = codec(bytes).context("first frame has no SPS")?;
            let json = format!(
                "{{\"version\":1,\"codec\":\"{codec}\",\"width\":1920,\"height\":1080,\"framerate\":75,\"stereo\":\"side-by-side\",\"timestamp\":\"microseconds\",\"format\":\"annexb\"}}"
            );
            let mut group = self._config.append_group()?;
            group.write_frame(Timestamp::now(), json)?;
            group.finish()?;
        }
        if key || self.group.is_none() {
            if let Some(g) = self.group.take() {
                g.finish()?;
            }
            self.group = Some(self.video.create_group(self.sequence.into())?);
            self.sequence += 1;
        }
        let mut data = Vec::with_capacity(bytes.len() + 9);
        data.push(u8::from(key));
        data.extend_from_slice(&micros.to_be_bytes());
        data.extend_from_slice(bytes);
        self.group
            .as_mut()
            .unwrap()
            .write_frame(Timestamp::from_scale(micros, 1_000_000)?, data)?;
        Ok(())
    }
}
pub fn serve(
    runtime: &tokio::runtime::Runtime,
    origin: origin::Consumer,
    bind: SocketAddr,
    iroh_local: bool,
) -> Result<String> {
    let mut listen = moq_tokio::listen::Config::default();
    listen.bind = Some(moq_tokio::listen::Bind::Addr(bind));
    listen.tls.generate = vec!["localhost".into(), "127.0.0.1".into()];
    let mut config = moq_tokio::server::Config::default();
    config.listen = listen;
    if iroh_local {
        #[cfg(feature = "iroh")]
        {
            let endpoint = runtime.block_on(local_endpoint())?;
            println!(
                "IROH_READY id={} address={}",
                endpoint.id(),
                endpoint.bound_sockets()[0]
            );
            config.iroh = Some(endpoint);
        }
        #[cfg(not(feature = "iroh"))]
        anyhow::bail!("build with --features iroh for the iroh transport");
    }
    let server = config.init()?;
    let fingerprint = server
        .certificates()
        .fingerprints()
        .into_iter()
        .next()
        .context("no server certificate")?;
    runtime.spawn(async move {
        if let Err(err) = server.serve_publish(origin).await {
            eprintln!("MoQ server stopped: {err}");
        }
    });
    Ok(fingerprint)
}
#[cfg(feature = "iroh")]
pub async fn local_endpoint() -> Result<moq_tokio::iroh::Endpoint> {
    use moq_tokio::iroh::{Endpoint, web_transport_iroh::iroh::endpoint::presets};
    // Explicit loopback-only transport: no public relays, address publication or DNS.
    Ok(Endpoint::builder(presets::Minimal)
        .clear_address_lookup()
        .clear_relay_transports()
        .clear_ip_transports()
        .bind_addr("127.0.0.1:0".parse::<SocketAddr>()?)?
        .bind()
        .await?)
}
fn codec(data: &[u8]) -> Option<String> {
    for i in 0..data.len() {
        let p = if data[i..].starts_with(&[0, 0, 0, 1]) {
            i + 4
        } else if data[i..].starts_with(&[0, 0, 1]) {
            i + 3
        } else {
            continue;
        };
        let Some(nal) = data.get(p..p + 4) else {
            continue;
        };
        if nal[0] & 31 == 7 {
            return Some(format!(
                "avc1.{:02X}{:02X}{:02X}",
                data[p + 1],
                data[p + 2],
                data[p + 3]
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::codec;
    #[test]
    fn codec_uses_complete_sps_and_accepts_both_annexb_prefixes() {
        assert_eq!(
            codec(&[0, 0, 1, 0x67, 77, 0, 51]).as_deref(),
            Some("avc1.4D0033")
        );
        assert_eq!(
            codec(&[0, 0, 0, 1, 0x67, 77, 64, 42]).as_deref(),
            Some("avc1.4D402A")
        );
        assert_eq!(codec(&[0, 0, 1, 0x67, 77]), None);
        assert_eq!(codec(&[0, 0, 1, 0x65, 77, 64, 42]), None);
    }
}
