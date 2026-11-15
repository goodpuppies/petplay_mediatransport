//! Publish an already encoded Annex-B stream. No raw frames enter this process.
use crate::publish::{Format, Layout, Publisher, Transport};
use anyhow::{Result, ensure};
use std::{io::Read, time::Instant};
fn nals(bytes: &[u8]) -> Vec<(usize, u8)> {
    let mut result = Vec::new();
    let mut i = 0;
    while i + 3 < bytes.len() {
        let prefix = if bytes[i..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if bytes[i..].starts_with(&[0, 0, 1]) {
            3
        } else {
            i += 1;
            continue;
        };
        if i + prefix < bytes.len() {
            result.push((i, bytes[i + prefix] & 31));
        }
        i += prefix;
    }
    result
}
fn emit(publisher: &mut Publisher, unit: &[u8], start: Instant, count: &mut u64) -> Result<()> {
    let types = nals(unit);
    ensure!(
        types.first().is_some_and(|n| n.1 == 9),
        "encoder must emit an AUD before each picture"
    );
    if !types.iter().any(|n| n.1 == 1 || n.1 == 5) {
        return Ok(());
    }
    let key = types.iter().any(|n| n.1 == 5);
    if *count == 0 && !key {
        return Ok(());
    }
    publisher.frame(
        unit,
        key,
        start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
    )?;
    *count += 1;
    if *count == 1 || *count % 60 == 0 {
        println!("ANNEXB_PUBLISHED frames={count} encoded_input=1");
    }
    Ok(())
}
pub fn run(args: &[std::ffi::OsString]) -> Result<()> {
    ensure!(
        args.len() == 3,
        "publish-annexb WIDTH HEIGHT FPS (AUD-delimited H.264 on stdin)"
    );
    let width: u32 = args[0].to_str().unwrap_or("").parse()?;
    let height: u32 = args[1].to_str().unwrap_or("").parse()?;
    let framerate: u32 = args[2].to_str().unwrap_or("").parse()?;
    ensure!(
        (16..=8192).contains(&width)
            && (16..=8192).contains(&height)
            && width % 2 == 0
            && height % 2 == 0
            && (1..=240).contains(&framerate),
        "invalid encoded source format"
    );
    publish_stream(std::io::stdin().lock(), width, height, framerate)
}
pub fn publish_stream(mut input: impl Read, width: u32, height: u32, framerate: u32) -> Result<()> {
    let mut publisher = Publisher::new("127.0.0.1:4540".parse()?, Transport::Peer)?;
    publisher.describe(Format {
        width,
        height,
        framerate,
        layout: Layout::Mono,
    });
    let start = Instant::now();
    let mut count = 0;
    let mut pending = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        let n = input.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&chunk[..n]);
        ensure!(
            pending.len() <= 8 * 1024 * 1024,
            "encoded access unit too large or missing AUDs"
        );
        loop {
            let auds: Vec<_> = nals(&pending)
                .into_iter()
                .filter(|n| n.1 == 9)
                .map(|n| n.0)
                .collect();
            if auds.len() < 2 {
                break;
            }
            let split = auds[1];
            emit(&mut publisher, &pending[..split], start, &mut count)?;
            pending.drain(..split);
        }
    }
    if !pending.is_empty() {
        emit(&mut publisher, &pending, start, &mut count)?;
    }
    println!("ANNEXB_STOPPED frames={count}");
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detects_both_prefixes_and_ignores_payload() {
        assert_eq!(
            nals(&[0, 0, 0, 1, 9, 240, 0, 0, 1, 0x65, 1, 2]),
            vec![(0, 9), (6, 5)]
        );
    }
}
