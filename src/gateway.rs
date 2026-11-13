//! Forward a native iroh MoQ source to browser WebTransport, without decoding.
use anyhow::Result;
#[cfg(feature = "iroh")]
pub fn run(args: &[std::ffi::OsString]) -> Result<()> {
    use anyhow::{Context, ensure};
    ensure!(
        args.len() == 3,
        "usage: petplay-media gateway IROH_URL DIRECT_ADDRESS QUIC_LISTEN_ADDR"
    );
    let url = args[0]
        .to_str()
        .context("invalid URL")?
        .parse::<url::Url>()?;
    let direct = args[1]
        .to_str()
        .context("invalid direct address")?
        .parse()?;
    let bind = args[2]
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
    let fingerprint = crate::publish::serve(&runtime, origin.consume(), bind, false)?;
    crate::web::start(bind, &fingerprint)?;
    runtime.block_on(async move {
        let endpoint = crate::publish::local_endpoint().await?;
        let mut config = moq_tokio::connect::Config::default();
        config.once = Some(true);
        let client = config
            .init(Default::default())?
            .with_iroh(endpoint)
            .with_iroh_addrs(vec![direct])
            .with_subscriber(origin);
        println!("GATEWAY_READY transport=iroh browser_fingerprint={fingerprint}");
        client.connect(url).closed().await?;
        Ok(())
    })
}
#[cfg(not(feature = "iroh"))]
pub fn run(_args: &[std::ffi::OsString]) -> Result<()> {
    anyhow::bail!("build with --features iroh for the gateway")
}
