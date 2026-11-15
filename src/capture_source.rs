//! Consume screen-streamer's existing localhost BGRA frame protocol and hardware-encode it.
use anyhow::{Context, Result, ensure};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Stdio},
};
fn frame(input: &mut TcpStream) -> Result<(u32, u32, Vec<u8>)> {
    let mut header = [0u8; 16];
    input.read_exact(&mut header)?;
    let u = |at| u32::from_le_bytes(header[at..at + 4].try_into().unwrap());
    let (width, height, size, chunks) = (u(0), u(4), u(8), u(12));
    ensure!(
        (16..=8192).contains(&width)
            && (16..=8192).contains(&height)
            && size as u64 == width as u64 * height as u64 * 4
            && chunks > 0
            && chunks <= 4096,
        "invalid BGRA frame header"
    );
    let mut bytes = vec![0u8; size as usize];
    let mut offset = 0;
    for _ in 0..chunks {
        let mut length = [0u8; 4];
        input.read_exact(&mut length)?;
        let n = u32::from_le_bytes(length) as usize;
        ensure!(
            n > 0 && n <= 256 * 1024 && offset + n <= bytes.len(),
            "invalid BGRA chunk"
        );
        input.read_exact(&mut bytes[offset..offset + n])?;
        offset += n;
    }
    ensure!(offset == bytes.len(), "incomplete BGRA frame");
    Ok((width, height, bytes))
}
pub fn run(args: &[std::ffi::OsString]) -> Result<()> {
    ensure!(args.len() == 3, "capture-tcp LOCAL_PORT FFMPEG_EXE FPS");
    let port: u16 = args[0].to_str().context("port")?.parse()?;
    let fps: u32 = args[2].to_str().context("fps")?.parse()?;
    ensure!((1..=120).contains(&fps), "invalid fps");
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    println!("CAPTURE_LISTEN address=127.0.0.1:{port}");
    std::io::stdout().flush()?;
    let (mut capture, _) = listener.accept()?;
    let (width, height, first) = frame(&mut capture)?;
    println!("CAPTURE_FORMAT width={width} height={height} encoding=nvenc output=1280x720");
    let mut encoder = Command::new(&args[1])
        .args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-f",
            "rawvideo",
            "-pixel_format",
            "bgra",
            "-video_size",
            &format!("{width}x{height}"),
            "-framerate",
            &fps.to_string(),
            "-i",
            "pipe:0",
            "-vf",
            "scale=1280:720:force_original_aspect_ratio=decrease:force_divisible_by=2,pad=1280:720:(ow-iw)/2:(oh-ih)/2",
            "-c:v",
            "h264_nvenc",
            "-preset",
            "p1",
            "-tune",
            "ull",
            "-zerolatency",
            "1",
            "-rc",
            "cbr",
            "-b:v",
            "8M",
            "-maxrate",
            "8M",
            "-bufsize",
            "1M",
            "-bf",
            "0",
            "-g",
            &fps.to_string(),
            "-bsf:v",
            "h264_metadata=aud=insert",
            "-f",
            "h264",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("start hardware encoder")?;
    let output = encoder.stdout.take().unwrap();
    let publish =
        std::thread::spawn(move || crate::annexb_source::publish_stream(output, 1280, 720, fps));
    let result = (|| -> Result<()> {
        let mut sink = encoder.stdin.take().unwrap();
        sink.write_all(&first)?;
        loop {
            if publish.is_finished() {
                break;
            }
            let (w, h, bytes) = match frame(&mut capture) {
                Ok(f) => f,
                Err(e) => {
                    if encoder.try_wait()?.is_some() {
                        return Err(e);
                    }
                    break;
                }
            };
            ensure!((w, h) == (width, height), "capture resize requires restart");
            sink.write_all(&bytes)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = encoder.kill();
    }
    let status = encoder.wait()?;
    let published = publish
        .join()
        .map_err(|_| anyhow::anyhow!("publisher panicked"))?;
    result?;
    published?;
    ensure!(status.success(), "hardware encoder exited {status}");
    Ok(())
}
