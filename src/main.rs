//! Native media worker. Only DMA-BUF descriptors and encoded bytes cross CPU memory.
mod actor;
#[cfg(feature = "publish")]
mod annexb_source;
#[cfg(all(feature = "encode", target_os = "linux"))]
mod atlas;
#[cfg(feature = "audio")]
mod audio;
#[cfg(all(feature = "capture-windows", target_os = "windows"))]
mod audio_windows;
#[cfg(all(feature = "publish", target_os = "windows"))]
mod capture_source;
mod gateway;
#[cfg(feature = "iroh")]
mod link;
#[cfg(feature = "iroh")]
mod peer;
mod publish;
#[cfg(all(feature = "decode", target_os = "linux"))]
mod receive;
#[cfg(any(
    all(feature = "decode", target_os = "linux"),
    all(feature = "decode-windows", target_os = "windows"),
    feature = "audio"
))]
mod receive_input;
#[cfg(all(feature = "decode", target_os = "linux"))]
mod receive_output;
#[cfg(feature = "encode")]
mod source;
mod web;
#[cfg(any(feature = "encode", feature = "publish"))]
mod webrtc;
#[cfg(all(feature = "decode-windows", target_os = "windows"))]
mod windows_decode;
#[cfg(all(feature = "capture-windows", target_os = "windows"))]
mod windows_encode;
#[cfg(all(feature = "capture-windows", target_os = "windows"))]
mod wgc_source;
#[cfg(all(feature = "decode-windows", target_os = "windows"))]
mod windows_receive;
use anyhow::Result;
/// Set by a parent that holds this process's stdin open for its own lifetime (PetPlay's
/// `petplayMediaCommand`). EOF on stdin then means the parent is gone, however it went
/// (Ctrl-C mid-teardown, crash, SIGKILL), so exit instead of holding ports and the GPU.
/// Modes that read data from stdin (`atlas`, `capture-audio --stdin`, `receive --stdio`)
/// must not be spawned with it.
const EXIT_WITH_PARENT_ENV: &str = "PETPLAY_MEDIA_EXIT_WITH_PARENT";

fn exit_with_parent() {
    if std::env::var_os(EXIT_WITH_PARENT_ENV).is_none() {
        return;
    }
    std::thread::spawn(|| {
        use std::io::{Read, Write};
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 64];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => {
                    // Not eprintln!: stderr is usually a pipe into the dead parent, and
                    // eprintln! panics on EPIPE, which would kill only this thread.
                    let _ = writeln!(
                        std::io::stderr(),
                        "petplay-media: parent gone (stdin closed), exiting"
                    );
                    std::process::exit(0);
                }
                Ok(_) => {}
            }
        }
    });
}

fn main() -> Result<()> {
    exit_with_parent();
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
    if args.get(1).is_some_and(|a| a == "capture-wgc") {
        #[cfg(all(feature = "capture-windows", target_os = "windows"))]
        return wgc_source::run(&args[2..]);
        #[cfg(not(all(feature = "capture-windows", target_os = "windows")))]
        anyhow::bail!("capture-wgc needs Windows and --features capture-windows");
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
    if args.get(1).is_some_and(|a| a == "capture-audio") {
        #[cfg(feature = "audio")]
        if args[2..].iter().any(|a| a == "--stdin") {
            return audio::capture_stdin(&args[2..]);
        }
        #[cfg(all(feature = "capture-windows", target_os = "windows"))]
        return audio_windows::run(&args[2..]);
        #[cfg(not(all(feature = "capture-windows", target_os = "windows")))]
        anyhow::bail!(
            "capture-audio captures on Windows (--features capture-windows); elsewhere pipe PCM to --stdin"
        );
    }
    if args.get(1).is_some_and(|a| a == "receive-audio") {
        #[cfg(feature = "audio")]
        return audio::receive(&args[2..]);
        #[cfg(not(feature = "audio"))]
        anyhow::bail!("receive-audio needs --features audio");
    }
    if args.get(1).is_some_and(|a| a == "link") {
        #[cfg(feature = "iroh")]
        return link::run(&args[2..]);
        #[cfg(not(feature = "iroh"))]
        anyhow::bail!("link needs --features iroh");
    }
    if args.get(1).is_some_and(|a| a == "atlas") {
        #[cfg(all(feature = "encode", target_os = "linux"))]
        return atlas::run(&args[2..]);
        #[cfg(not(all(feature = "encode", target_os = "linux")))]
        anyhow::bail!("atlas needs Linux and --features encode");
    }
    if args.get(1).is_some_and(|a| a == "actor") {
        return actor::run();
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
