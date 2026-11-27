//! Forward a native iroh MoQ source to browser WebTransport, without decoding.
use anyhow::Result;
#[cfg(feature = "iroh")]
pub fn run(args: &[std::ffi::OsString]) -> Result<()> {
    use anyhow::{Context, ensure};
    ensure!(
        (2..=3).contains(&args.len()),
        "usage: petplay-media gateway SHARE_CODE QUIC_LISTEN_ADDR\n            or: petplay-media gateway IROH_URL DIRECT_ADDRESS QUIC_LISTEN_ADDR"
    );
    // A share code replaces the loopback form's URL and address, and is the only form
    // that reaches a source across the internet.
    let shared = (args.len() == 2)
        .then(|| crate::peer::decode(args[0].to_str().context("invalid share code")?))
        .transpose()?;
    let (url, direct) = match &shared {
        Some(addr) => (
            format!("iroh://{}/", addr.id).parse::<url::Url>()?,
            crate::peer::direct_addresses(addr),
        ),
        None => (
            args[0]
                .to_str()
                .context("invalid URL")?
                .parse::<url::Url>()?,
            vec![
                args[1]
                    .to_str()
                    .context("invalid direct address")?
                    .parse()?,
            ],
        ),
    };
    let bind = args[args.len() - 1]
        .to_str()
        .context("invalid listen address")?
        .parse()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let _enter = runtime.enter();
    let _ = moq_tokio::crypto::install_default();
    let origin = moq_tokio::origin::spawn();
    let browser_origin = origin.consume();
    runtime.block_on(async {
        let endpoint = match shared.clone() {
            Some(addr) => crate::peer::endpoint(Some(addr)).await?,
            None => crate::publish::local_endpoint().await?,
        };
        let mut config = moq_tokio::connect::Config::default();
        config.once = Some(true);
        let client = config
            .init(Default::default())?
            .with_iroh(endpoint.clone())
            .with_iroh_addrs(direct)
            .with_subscriber(origin);
        let session = client.connect(url);
        // Give holepunching a short head start before exposing the consuming origin to
        // browsers; MoQ requests tracks lazily, so nothing flows before this. If no direct
        // path appears, the stream continues over the relay.
        if let Some(addr) = &shared {
            let path = tokio::select! {
                path = crate::peer::prefer_direct(&endpoint, addr.id, crate::peer::DIRECT_GRACE) => path,
                result = session.closed() => {
                    result?;
                    anyhow::bail!("source closed before the media path was established");
                }
            };
            match path {
                crate::peer::MediaPath::Direct => println!("GATEWAY_DIRECT peer={}", addr.id),
                crate::peer::MediaPath::Relayed => println!("GATEWAY_RELAYED peer={}", addr.id),
            }
        }
        let (fingerprint, _) = crate::publish::serve(
            &runtime, browser_origin, bind, crate::publish::Transport::Browser,
        )?;
        crate::web::start(bind, &fingerprint)?;
        println!("GATEWAY_READY transport=iroh browser_fingerprint={fingerprint}");
        session.closed().await?;
        Ok(())
    })
}
#[cfg(not(feature = "iroh"))]
pub fn run(_args: &[std::ffi::OsString]) -> Result<()> {
    anyhow::bail!("build with --features iroh for the gateway")
}
