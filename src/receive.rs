//! Subscribe natively, decode on VA-API, and export RGB through the local GPU slot contract.
use anyhow::{Context, Result, ensure};
use moq_vaapi::{
    decode::{Config, Decoder},
    dmabuf::{DmaBuf, Plane},
    vpp::Processor,
};
use std::{path::Path, sync::mpsc::RecvTimeoutError, time::Duration};

pub fn run(args: &[std::ffi::OsString]) -> Result<()> {
    ensure!(
        (2..=3).contains(&args.len()),
        "usage: petplay-media receive SHARE_CODE SOCKET\n or: petplay-media receive IROH_URL DIRECT_ADDRESS SOCKET\n or: petplay-media receive https://HOST:PORT/ CERT_SHA256 SOCKET"
    );
    let mut output = crate::receive_output::Output::bind(Path::new(&args[args.len() - 1]))?;
    let input = crate::receive_input::Input::connect(args)?;
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
        let unit = match input.rx.recv_timeout(Duration::from_millis(50)) {
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
    input.finish()?;
    println!(
        "RECEIVER_STOPPED decoded={decoded} sent={} dropped={}",
        output.sent, output.dropped
    );
    Ok(())
}
