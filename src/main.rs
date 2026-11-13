//! Native media worker. Only DMA-BUF descriptors and encoded bytes cross CPU memory.
mod gateway;
#[cfg(feature = "iroh")]
mod peer;
mod publish;
#[cfg(all(feature = "decode", target_os = "linux"))]
mod receive;
#[cfg(all(feature = "decode", target_os = "linux"))]
mod receive_output;
#[cfg(feature = "encode")]
mod source;
mod web;
use anyhow::Result;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "receive") {
        #[cfg(all(feature = "decode", target_os = "linux"))]
        return receive::run(&args[2..]);
        #[cfg(not(all(feature = "decode", target_os = "linux")))]
        anyhow::bail!("native receive needs Linux and a build with --features decode");
    }
    if args.get(1).is_some_and(|a| a == "gateway") {
        return gateway::run(&args[2..]);
    }
    #[cfg(feature = "encode")]
    return source::run(&args);
    #[cfg(not(feature = "encode"))]
    anyhow::bail!(
        "this build has no encoder; use gateway, or receive in a Linux --features decode build"
    )
}
