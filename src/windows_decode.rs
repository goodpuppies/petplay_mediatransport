//! Windows inbox H.264 MFT, D3D11 decode surfaces, and GPU RGB conversion.
use anyhow::{Context, Result, ensure};
use std::mem::ManuallyDrop;
use windows::{
    Win32::{
        Foundation::{HMODULE, RECT},
        Graphics::{
            Direct3D::*,
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
        Media::MediaFoundation::*,
        System::Com::*,
    },
    core::{GUID, Interface},
};
const H264_DECODER: GUID = GUID::from_u128(0x62ce7e72_4c71_4d20_b15d_452831a87d9d);
pub struct Decoder {
    transform: IMFTransform,
    _manager: IMFDXGIDeviceManager,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    width: u32,
    height: u32,
    rgb: Option<Rgb>,
    _platform: Platform,
}
struct Rgb {
    texture: ID3D11Texture2D,
    _shared: ID3D11Texture2D,
    query: ID3D11Query,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    view: ID3D11VideoProcessorOutputView,
    handle: usize,
}
struct Platform;
impl Platform {
    fn new() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
            if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                CoUninitialize();
                return Err(e.into());
            }
            Ok(Self)
        }
    }
}
impl Drop for Platform {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            CoUninitialize();
        }
    }
}
impl Decoder {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        unsafe {
            ensure!(
                (16..=8192).contains(&width)
                    && (16..=8192).contains(&height)
                    && width % 2 == 0
                    && height % 2 == 0,
                "invalid decoder geometry"
            );
            let platform = Platform::new()?;
            let (mut device, mut context) = (None, None);
            let adapter = std::env::var("PETPLAY_DXGI_ADAPTER")
                .ok()
                .map(|s| -> Result<IDXGIAdapter> {
                    let index: u32 = s.parse().context("PETPLAY_DXGI_ADAPTER")?;
                    Ok(CreateDXGIFactory1::<IDXGIFactory1>()?
                        .EnumAdapters1(index)?
                        .cast()?)
                })
                .transpose()?;
            D3D11CreateDevice(
                adapter.as_ref(),
                if adapter.is_some() {
                    D3D_DRIVER_TYPE_UNKNOWN
                } else {
                    D3D_DRIVER_TYPE_HARDWARE
                },
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .context("D3D11 hardware device")?;
            let device = device.unwrap();
            let adapter_desc = device.cast::<IDXGIDevice>()?.GetAdapter()?.GetDesc()?;
            let name = String::from_utf16_lossy(&adapter_desc.Description)
                .trim_end_matches('\0')
                .to_owned();
            println!(
                "WINDOWS_GPU adapter={name:?} vendor={:04x} luid={}:{}",
                adapter_desc.VendorId,
                adapter_desc.AdapterLuid.HighPart,
                adapter_desc.AdapterLuid.LowPart
            );
            let context = context.unwrap();
            let _ = context
                .cast::<ID3D11Multithread>()?
                .SetMultithreadProtected(true);
            let (mut token, mut manager) = (0, None);
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(&device, token)?;
            let transform: IMFTransform =
                CoCreateInstance(&H264_DECODER, None, CLSCTX_INPROC_SERVER)?;
            let attributes = transform.GetAttributes()?;
            ensure!(
                attributes.GetUINT32(&MF_SA_D3D11_AWARE)? != 0,
                "decoder is not D3D11 aware"
            );
            attributes.SetUINT32(&MF_LOW_LATENCY, 1)?;
            transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, ((width as u64) << 32) | height as u64)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &input, 0)?;
            let mut result = Self {
                transform,
                _manager: manager,
                device,
                context,
                width,
                height,
                rgb: None,
                _platform: platform,
            };
            result.output_type()?;
            result
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            result
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            println!("WINDOWS_DECODER_READY width={width} height={height} d3d11=1");
            Ok(result)
        }
    }
    unsafe fn output_type(&mut self) -> Result<()> {
        unsafe {
            for index in 0..100 {
                let ty = self.transform.GetOutputAvailableType(0, index)?;
                if ty.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    self.transform.SetOutputType(0, &ty, 0)?;
                    return Ok(());
                }
            }
            anyhow::bail!("MFT has no NV12 output")
        }
    }
    /// Explicit diagnostic readback, never used by live reception.
    pub fn diagnostic(&self, path: &std::path::Path) -> Result<()> {
        let rgb = self.rgb.as_ref().context("no RGB frame")?;
        readback(
            &self.device,
            &self.context,
            &rgb._shared,
            self.width,
            self.height,
            path,
        )
    }
    pub fn reset(&mut self) -> Result<()> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(())
        }
    }
    pub fn decode(&mut self, bytes: &[u8], timestamp: u64) -> Result<Vec<usize>> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes.len().try_into()?)?;
            let mut pointer = std::ptr::null_mut();
            buffer.Lock(&mut pointer, None, None)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime((timestamp.saturating_mul(10)).try_into()?)?;
            sample.SetSampleDuration(166667)?;
            self.transform
                .ProcessInput(0, &sample, 0)
                .context("MFT ProcessInput")?;
            let mut frames = Vec::new();
            loop {
                let mut output = MFT_OUTPUT_DATA_BUFFER::default();
                let mut status = 0;
                let result =
                    self.transform
                        .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status);
                let sample = ManuallyDrop::take(&mut output.pSample);
                let _events = ManuallyDrop::take(&mut output.pEvents);
                match result {
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                        self.output_type()?;
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                    Ok(()) => {}
                }
                let sample = sample.context("decoder returned no sample")?;
                let buffer = sample.GetBufferByIndex(0)?;
                let output_type = self.transform.GetOutputCurrentType(0)?;
                let size = output_type.GetUINT64(&MF_MT_FRAME_SIZE)?;
                ensure!(
                    (size >> 32) as u32 == self.width && size as u32 == self.height,
                    "decoded geometry differs from catalog"
                );
                let dxgi: IMFDXGIBuffer = buffer
                    .cast()
                    .context("decoder returned CPU storage; refusing fallback")?;
                let mut resource = std::ptr::null_mut();
                dxgi.GetResource(&ID3D11Texture2D::IID, &mut resource)?;
                let texture = ID3D11Texture2D::from_raw(resource);
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                ensure!(
                    desc.Format == DXGI_FORMAT_NV12
                        && desc.BindFlags & D3D11_BIND_DECODER.0 as u32 != 0,
                    "decoder did not return a hardware decode surface"
                );
                let slice = dxgi.GetSubresourceIndex()?;
                frames.push(self.convert(&texture, slice)?);
            }
            Ok(frames)
        }
    }
    unsafe fn convert(&mut self, texture: &ID3D11Texture2D, slice: u32) -> Result<usize> {
        unsafe {
            let video: ID3D11VideoDevice = self.device.cast()?;
            let context: ID3D11VideoContext = self.context.cast()?;
            if self.rgb.is_none() {
                let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                    InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                    InputFrameRate: DXGI_RATIONAL {
                        Numerator: 60,
                        Denominator: 1,
                    },
                    InputWidth: self.width,
                    InputHeight: self.height,
                    OutputFrameRate: DXGI_RATIONAL {
                        Numerator: 60,
                        Denominator: 1,
                    },
                    OutputWidth: self.width,
                    OutputHeight: self.height,
                    Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                };
                let enumerator = video.CreateVideoProcessorEnumerator(&desc)?;
                let processor = video.CreateVideoProcessor(&enumerator, 0)?;
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: self.width,
                    Height: self.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
                };
                let mut rgb = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut rgb))?;
                let rgb = rgb.unwrap();
                let mut shared = None;
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut shared))?;
                let shared = shared.unwrap();
                let handle = shared.cast::<IDXGIResource>()?.GetSharedHandle()?.0 as usize;
                let mut query = None;
                self.device.CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: D3D11_QUERY_EVENT,
                        MiscFlags: 0,
                    },
                    Some(&mut query),
                )?;
                let desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                };
                let mut view = None;
                video.CreateVideoProcessorOutputView(&rgb, &enumerator, &desc, Some(&mut view))?;
                self.rgb = Some(Rgb {
                    texture: rgb,
                    _shared: shared,
                    query: query.unwrap(),
                    enumerator,
                    processor,
                    view: view.unwrap(),
                    handle,
                });
            }
            let rgb = self.rgb.as_ref().unwrap();
            let rect = RECT {
                left: 0,
                top: 0,
                right: self.width as i32,
                bottom: self.height as i32,
            };
            context.VideoProcessorSetStreamSourceRect(&rgb.processor, 0, true, Some(&rect));
            context.VideoProcessorSetStreamDestRect(&rgb.processor, 0, true, Some(&rect));
            context.VideoProcessorSetOutputTargetRect(&rgb.processor, true, Some(&rect));
            context.VideoProcessorSetStreamAutoProcessingMode(&rgb.processor, 0, false);
            context.VideoProcessorSetStreamColorSpace(
                &rgb.processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: 16 | if self.height >= 720 { 4 } else { 0 },
                },
            );
            context.VideoProcessorSetOutputColorSpace(
                &rgb.processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 },
            );
            let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: slice,
                    },
                },
            };
            let mut view = None;
            video.CreateVideoProcessorInputView(
                texture,
                &rgb.enumerator,
                &desc,
                Some(&mut view),
            )?;
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(view),
                ..Default::default()
            };
            let result = context.VideoProcessorBlt(
                &rgb.processor,
                &rgb.view,
                0,
                std::slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result?;
            // OpenVR reads a DXGISharedHandle directly: only atomic CopyResource updates are allowed.
            self.context.CopyResource(&rgb._shared, &rgb.texture);
            self.context.End(&rgb.query);
            self.context.Flush();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut done = 0u32;
            while done == 0 {
                self.context
                    .GetData(&rgb.query, Some((&mut done as *mut u32).cast()), 4, 0)?;
                ensure!(
                    std::time::Instant::now() < deadline,
                    "D3D11 completion stalled"
                );
                if done == 0 {
                    std::thread::yield_now();
                }
            }
            Ok(rgb.handle)
        }
    }
}
pub fn test(args: &[std::ffi::OsString]) -> Result<()> {
    ensure!(
        (3..=6).contains(&args.len()),
        "decode-test LENGTH_PREFIXED_FIXTURE WIDTH HEIGHT [DIAGNOSTIC.bgra] [--hold] [--reset-test]"
    );
    let mut decoder = Decoder::new(
        args[1].to_str().unwrap().parse()?,
        args[2].to_str().unwrap().parse()?,
    )?;
    let data = std::fs::read(&args[0])?;
    let mut count = 0;
    let mut shared_handle = None;
    let passes = if args.iter().skip(4).any(|a| a == "--reset-test") {
        2
    } else {
        1
    };
    for pass in 0..passes {
        if pass > 0 {
            decoder.reset()?;
            println!("WINDOWS_DECODER_RESET");
        }
        let mut offset = 0;
        let mut input_count = 0;
        while offset < data.len() {
            ensure!(offset + 4 <= data.len(), "truncated length");
            let size = u32::from_le_bytes(data[offset..offset + 4].try_into()?) as usize;
            offset += 4;
            ensure!(
                size <= 4 * 1024 * 1024 && offset + size <= data.len(),
                "invalid access unit"
            );
            for handle in decoder.decode(&data[offset..offset + size], input_count * 16667)? {
                ensure!(
                    shared_handle.is_none() || shared_handle == Some(handle),
                    "reset changed shared texture handle"
                );
                shared_handle = Some(handle);
                count += 1;
                println!("WINDOWS_GPU_FRAME frame={count} handle={handle} raw_cpu_bytes=0");
            }
            input_count += 1;
            offset += size;
        }
    }
    ensure!(count > 0, "no decoded frames");
    if args.len() >= 4 {
        decoder.diagnostic(std::path::Path::new(&args[3]))?;
    }
    println!("WINDOWS_DECODE_OK frames={count}");
    if args.iter().skip(4).any(|a| a == "--hold") {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            if line? == "STOP" {
                break;
            }
        }
    }
    Ok(())
}

fn readback(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    width: u32,
    height: u32,
    path: &std::path::Path,
) -> Result<()> {
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.MiscFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.unwrap();
        context.CopyResource(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let mut bytes = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            bytes.extend_from_slice(std::slice::from_raw_parts(
                (mapped.pData as *const u8).add((y * mapped.RowPitch) as usize),
                (width * 4) as usize,
            ));
        }
        context.Unmap(&staging, 0);
        std::fs::write(path, &bytes)?;
        println!("WINDOWS_DIAGNOSTIC_READBACK bytes={}", bytes.len());
        Ok(())
    }
}
pub fn shared_test(args: &[std::ffi::OsString]) -> Result<()> {
    unsafe {
        ensure!(
            args.len() == 4,
            "shared-test HANDLE WIDTH HEIGHT DIAGNOSTIC.bgra"
        );
        let width = args[1].to_str().unwrap().parse()?;
        let height = args[2].to_str().unwrap().parse()?;
        let decoder = Decoder::new(width, height)?;
        let handle: usize = args[0].to_str().unwrap().parse()?;
        let mut texture: Option<ID3D11Texture2D> = None;
        decoder.device.OpenSharedResource(
            windows::Win32::Foundation::HANDLE(handle as *mut _),
            &mut texture,
        )?;
        readback(
            &decoder.device,
            &decoder.context,
            &texture.unwrap(),
            width,
            height,
            std::path::Path::new(&args[3]),
        )?;
        println!("WINDOWS_SHARED_TEXTURE_OK handle={handle}");
        Ok(())
    }
}

pub fn gpu_info() -> Result<()> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        for i in 0..16 {
            let adapter = match factory.EnumAdapters1(i) {
                Ok(a) => a,
                Err(_) => break,
            };
            let desc = adapter.GetDesc1()?;
            let name = String::from_utf16_lossy(&desc.Description)
                .trim_end_matches('\0')
                .to_owned();
            println!("GPU_INFO adapter={i} name={name:?}");
            for j in 0..16 {
                let output = match adapter.EnumOutputs(j) {
                    Ok(o) => o,
                    Err(_) => break,
                };
                let desc = output.GetDesc()?;
                let device = String::from_utf16_lossy(&desc.DeviceName)
                    .trim_end_matches('\0')
                    .to_owned();
                println!(
                    "GPU_OUTPUT adapter={i} output={j} name={device:?} attached={} width={} height={}",
                    desc.AttachedToDesktop.as_bool(),
                    desc.DesktopCoordinates.right - desc.DesktopCoordinates.left,
                    desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top
                );
            }
        }
        Ok(())
    }
}
