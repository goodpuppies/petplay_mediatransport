//! Stdio is control/metadata only; a persistent DXGI texture owns the pixels.
use anyhow::{Result, ensure};
use std::{
    io::{BufRead, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::RecvTimeoutError,
    },
    time::Duration,
};
pub fn run(args: &[std::ffi::OsString]) -> Result<()> {
    ensure!(
        (2..=3).contains(&args.len()) && args.last().is_some_and(|s| s == "--stdio"),
        "receive SHARE_CODE --stdio, or receive IROH_URL ADDRESS --stdio"
    );
    let stop = Arc::new(AtomicBool::new(false));
    let control = stop.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            match line {
                Ok(line) if line == "STOP" => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        control.store(true, Ordering::Relaxed);
    });
    let input = crate::receive_input::Input::connect(args)?;
    let mut decoder = None;
    let mut geometry = None;
    let mut count = 0u64;
    let stdout = std::io::stdout();
    while !stop.load(Ordering::Relaxed) {
        let unit = match input.rx.recv_timeout(Duration::from_millis(50)) {
            Ok(u) => u,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        ensure!(
            geometry.is_none() || geometry == Some((unit.width, unit.height)),
            "resize requires a fresh session"
        );
        if decoder.is_none() {
            decoder = Some(crate::windows_decode::Decoder::new(
                unit.width,
                unit.height,
            )?);
            geometry = Some((unit.width, unit.height));
        }
        if unit.reset {
            decoder.as_mut().unwrap().reset()?;
        }
        for handle in decoder
            .as_mut()
            .unwrap()
            .decode(&unit.bytes, unit.timestamp)?
        {
            count += 1;
            writeln!(
                stdout.lock(),
                "WINDOWS_MEDIA_FRAME {}",
                serde_json::json!({"version":1,"handle":handle.to_string(),"width":unit.width,"height":unit.height,"sequence":count,"timestampUs":unit.timestamp})
            )?;
            stdout.lock().flush()?;
        }
    }
    input.finish()?;
    println!("RECEIVER_STOPPED decoded={count} raw_cpu_bytes=0");
    Ok(())
}
