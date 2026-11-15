//! Native media worker. Only DMA-BUF descriptors and encoded bytes cross CPU memory.
#[cfg(feature = "publish")]
mod annexb_source;
#[cfg(all(feature = "publish", target_os = "windows"))]
mod capture_source;
mod gateway;
#[cfg(feature = "iroh")]
mod peer;
mod publish;
#[cfg(all(feature = "decode", target_os = "linux"))]
mod receive;
#[cfg(any(
    all(feature = "decode", target_os = "linux"),
    all(feature = "decode-windows", target_os = "windows")
))]
mod receive_input;
#[cfg(all(feature = "decode", target_os = "linux"))]
mod receive_output;
#[cfg(feature = "encode")]
mod source;
mod web;
#[cfg(all(feature = "decode-windows", target_os = "windows"))]
mod windows_decode;
#[cfg(all(feature = "decode-windows", target_os = "windows"))]
mod windows_receive;
use anyhow::Result;
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    #[cfg(all(feature = "decode-windows", target_os = "windows"))]
    if args.get(1).is_some_and(|a| a == "decode-test") {
        return windows_decode::test(&args[2..]);
    }
    #[cfg(all(feature = "decode-windows", target_os = "windows"))]
    if args.get(1).is_some_and(|a| a == "shared-test") {
        return windows_decode::shared_test(&args[2..]);
    }
    #[cfg(feature = "publish")]
    if args.get(1).is_some_and(|a| a == "publish-annexb") {
        return annexb_source::run(&args[2..]);
    }
    #[cfg(all(feature = "decode-windows", target_os = "windows"))]
    if args.get(1).is_some_and(|a| a == "gpu-info") {
        return windows_decode::gpu_info();
    }
    #[cfg(all(feature = "publish", target_os = "windows"))]
    if args.get(1).is_some_and(|a| a == "capture-tcp") {
        return capture_source::run(&args[2..]);
    }
    if args.get(1).is_some_and(|a| a == "receive") {
        #[cfg(all(feature = "decode", target_os = "linux"))]
        return receive::run(&args[2..]);
        #[cfg(all(feature = "decode-windows", target_os = "windows"))]
        return windows_receive::run(&args[2..]);
        #[cfg(not(any(
            all(feature = "decode", target_os = "linux"),
            all(feature = "decode-windows", target_os = "windows")
        )))]
        anyhow::bail!(
            "native receive needs Linux --features decode or Windows --features decode-windows"
        );
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
