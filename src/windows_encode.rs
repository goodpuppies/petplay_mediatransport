//! Windows GPU encoding: a captured BGRA texture becomes NV12 on D3D11's video processor and
//! H.264 in the GPU's Media Foundation hardware encoder. Pixels stay on the GPU throughout; the
//! only bytes this process reads are the encoded stream.
//!
//! The encoder is the GPU vendor's hardware MFT (NVENC, AMF or Quick Sync behind the same
//! interface), picked on the capture device's adapter. Hardware MFTs are asynchronous: they ask
//! for input and announce output through events, which [`Encoder::poll_event`] returns.
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
        System::{Com::*, Variant::VARIANT},
    },
    core::{GUID, Interface, PWSTR},
};

/// COM and Media Foundation, for the life of the encoder.
pub struct Platform;
impl Platform {
    pub fn new() -> Result<Self> {
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

/// A hardware D3D11 device for capture, conversion and encoding: `PETPLAY_DXGI_ADAPTER`'s
/// adapter, else the default one. Capture only works on the adapter driving the monitor (or one
/// Windows can copy to), which on a desktop is the one GPU.
pub fn device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let adapter = std::env::var("PETPLAY_DXGI_ADAPTER")
            .ok()
            .map(|s| -> Result<IDXGIAdapter> {
                let index: u32 = s.parse().context("PETPLAY_DXGI_ADAPTER")?;
                Ok(CreateDXGIFactory1::<IDXGIFactory1>()?.EnumAdapters1(index)?.cast()?)
            })
            .transpose()?;
        let (mut device, mut context) = (None, None);
        D3D11CreateDevice(
            adapter.as_ref(),
            if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE },
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
        let context = context.unwrap();
        // The encoder works on this device from its own threads.
        let _ = context.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
        let desc = device.cast::<IDXGIDevice>()?.GetAdapter()?.GetDesc()?;
        let name = String::from_utf16_lossy(&desc.Description).trim_end_matches('\0').to_owned();
        println!(
            "WINDOWS_GPU adapter={name:?} vendor={:04x} luid={}:{}",
            desc.VendorId, desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart
        );
        Ok((device, context))
    }
}

/// BGRA → NV12 (BT.709, studio range) into a small ring of encoder input surfaces.
pub struct Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    slots: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    /// A BGRA texture the processor can read, for capture textures it cannot bind directly.
    staging: Option<ID3D11Texture2D>,
    done: ID3D11Query,
    pub width: u32,
    pub height: u32,
}

/// Surfaces the encoder may still hold while the next frame converts.
const SLOTS: usize = 4;

impl Converter {
    pub fn new(device: &ID3D11Device, context: &ID3D11DeviceContext, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let video: ID3D11VideoDevice = device.cast()?;
            let video_context: ID3D11VideoContext = context.cast()?;
            let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
            let enumerator = video.CreateVideoProcessorEnumerator(&D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: width,
                InputHeight: height,
                OutputFrameRate: rate,
                OutputWidth: width,
                OutputHeight: height,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            })?;
            let processor = video.CreateVideoProcessor(&enumerator, 0)?;
            let mut slots = Vec::with_capacity(SLOTS);
            for _ in 0..SLOTS {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut texture = None;
                device.CreateTexture2D(&desc, None, Some(&mut texture))?;
                let texture = texture.unwrap();
                let mut view = None;
                video.CreateVideoProcessorOutputView(
                    &texture,
                    &enumerator,
                    &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                        },
                    },
                    Some(&mut view),
                )?;
                slots.push((texture, view.unwrap()));
            }
            let rect = RECT { left: 0, top: 0, right: width as i32, bottom: height as i32 };
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&rect));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&rect));
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&rect));
            video_context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            // Input: full-range RGB. Output: BT.709 YCbCr (bit 2), studio range (nominal 16-235,
            // bits 4-5 = 1), as the SPS's VUI and every receiver assume.
            video_context.VideoProcessorSetStreamColorSpace(
                &processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 },
            );
            video_context.VideoProcessorSetOutputColorSpace(
                &processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 4 | (1 << 4) },
            );
            let mut done = None;
            device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut done))?;
            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                video,
                video_context,
                enumerator,
                processor,
                slots,
                next: 0,
                staging: None,
                done: done.unwrap(),
                width,
                height,
            })
        }
    }

    /// Convert `source` (BGRA, at least `width`×`height`) into the next input surface, and wait
    /// until the GPU has read it: the capture's texture goes back to Windows right after.
    pub fn convert(&mut self, source: &ID3D11Texture2D) -> Result<ID3D11Texture2D> {
        unsafe {
            let view = match self.input_view(source) {
                Ok(view) => view,
                Err(_) => {
                    // Not bindable as processor input: one GPU copy into a texture that is.
                    let staging = self.staging()?;
                    let region = D3D11_BOX {
                        left: 0,
                        top: 0,
                        front: 0,
                        right: self.width,
                        bottom: self.height,
                        back: 1,
                    };
                    self.context.CopySubresourceRegion(&staging, 0, 0, 0, 0, source, 0, Some(&region));
                    self.input_view(&staging)?
                }
            };
            let (texture, output) = &self.slots[self.next];
            self.next = (self.next + 1) % self.slots.len();
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(view)),
                ..Default::default()
            };
            let result = self.video_context.VideoProcessorBlt(
                &self.processor,
                output,
                0,
                std::slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result.context("VideoProcessorBlt")?;
            self.context.End(&self.done);
            self.context.Flush();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            let mut finished = 0u32;
            while finished == 0 {
                self.context.GetData(&self.done, Some((&mut finished as *mut u32).cast()), 4, 0)?;
                ensure!(std::time::Instant::now() < deadline, "GPU conversion stalled");
                if finished == 0 {
                    std::thread::yield_now();
                }
            }
            Ok(texture.clone())
        }
    }

    unsafe fn input_view(&self, texture: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        unsafe {
            let mut view = None;
            self.video.CreateVideoProcessorInputView(
                texture,
                &self.enumerator,
                &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                    },
                },
                Some(&mut view),
            )?;
            view.context("no processor input view")
        }
    }

    unsafe fn staging(&mut self) -> Result<ID3D11Texture2D> {
        unsafe {
            if let Some(staging) = &self.staging {
                return Ok(staging.clone());
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width,
                Height: self.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut texture = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut texture))?;
            let texture = texture.unwrap();
            println!("WINDOWS_CONVERT staging=1 (capture texture is not processor input)");
            self.staging = Some(texture.clone());
            Ok(texture)
        }
    }
}

pub struct Settings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    /// Frames between keyframes.
    pub gop: u32,
}

/// What the encoder asks for next.
pub enum Event {
    NeedInput,
    HaveOutput,
    Other,
}

/// One encoded access unit: Annex-B, with SPS/PPS in front of every keyframe.
pub struct Unit {
    pub bytes: Vec<u8>,
    pub key: bool,
    /// The input's sample time, in 100 ns units.
    pub time: i64,
}

pub struct Encoder {
    transform: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    _manager: IMFDXGIDeviceManager,
    provides_samples: bool,
    output_size: u32,
    frame_duration: i64,
}

impl Encoder {
    pub fn new(device: &ID3D11Device, settings: &Settings) -> Result<Self> {
        unsafe {
            let Settings { width, height, fps, bitrate, gop } = *settings;
            let (mut token, mut manager) = (0, None);
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(device, token)?;

            // The hardware encoder on this device's adapter.
            let luid = device.cast::<IDXGIDevice>()?.GetAdapter()?.GetDesc()?.AdapterLuid;
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 1)?;
            let attributes = attributes.unwrap();
            attributes.SetBlob(
                &MFT_ENUM_ADAPTER_LUID,
                std::slice::from_raw_parts(
                    &luid as *const _ as *const u8,
                    std::mem::size_of_val(&luid),
                ),
            )?;
            let input_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_NV12,
            };
            let output_info = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnum2(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input_info),
                Some(&output_info),
                &attributes,
                &mut activates,
                &mut count,
            )
            .context("enumerate hardware H.264 encoders")?;
            let found: Vec<IMFActivate> = if activates.is_null() {
                Vec::new()
            } else {
                let list = std::slice::from_raw_parts_mut(activates, count as usize);
                let found = list.iter_mut().filter_map(|a| a.take()).collect();
                CoTaskMemFree(Some(activates as *const _));
                found
            };
            let activate = found.into_iter().next().context("no hardware H.264 encoder on this GPU")?;
            let name = friendly_name(&activate).unwrap_or_else(|| "hardware H.264 encoder".into());
            let transform: IMFTransform = activate.ActivateObject().context("activate the encoder")?;

            let attributes = transform.GetAttributes()?;
            attributes
                .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                .context("unlock the asynchronous encoder")?;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)
                .context("give the encoder the D3D11 device")?;

            // Rate control before the media types: some encoders size their pipeline from it.
            let codec: Option<ICodecAPI> = transform.cast().ok();
            if let Some(codec) = &codec {
                set(codec, &CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32), "CBR");
                set(codec, &CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate), "bitrate");
                set(codec, &CODECAPI_AVLowLatencyMode, VARIANT::from(true), "low latency");
                set(codec, &CODECAPI_AVEncMPVGOPSize, VARIANT::from(gop), "GOP size");
                set(codec, &CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32), "no B-frames");
            }

            let size = ((width as u64) << 32) | height as u64;
            let rate = ((fps as u64) << 32) | 1;
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            output.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            color(&output);
            transform.SetOutputType(0, &output, 0).context("H.264 output type")?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            input.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            color(&input);
            transform.SetInputType(0, &input, 0).context("NV12 input type")?;

            let info = transform.GetOutputStreamInfo(0)?;
            let provides_samples = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32
                != 0;
            let events: IMFMediaEventGenerator = transform.cast().context("the encoder is not asynchronous")?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            println!("WINDOWS_ENCODER name={name:?} width={width} height={height} fps={fps} bitrate={bitrate}");
            Ok(Self {
                transform,
                events,
                codec,
                _manager: manager,
                provides_samples,
                output_size: info.cbSize.max(1 << 20),
                frame_duration: 10_000_000 / fps.max(1) as i64,
            })
        }
    }

    /// The encoder's next request, if it has one.
    pub fn poll_event(&self) -> Result<Option<Event>> {
        unsafe {
            let event = match self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(event) => event,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(None),
                Err(e) => return Err(e).context("encoder event"),
            };
            let kind = event.GetType()?;
            Ok(Some(if kind == METransformNeedInput.0 as u32 {
                Event::NeedInput
            } else if kind == METransformHaveOutput.0 as u32 {
                Event::HaveOutput
            } else {
                Event::Other
            }))
        }
    }

    /// Hand `surface` (an NV12 surface from [`Converter::convert`]) to the encoder. Only after a
    /// [`Event::NeedInput`].
    pub fn submit(&self, surface: &ID3D11Texture2D, time: i64, key: bool) -> Result<()> {
        unsafe {
            if key {
                if let Some(codec) = &self.codec {
                    set(codec, &CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32), "keyframe");
                }
            }
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, surface, 0, false)?;
            if let Ok(planar) = buffer.cast::<IMF2DBuffer>() {
                buffer.SetCurrentLength(planar.GetContiguousLength()?)?;
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(self.frame_duration)?;
            self.transform.ProcessInput(0, &sample, 0).context("encoder ProcessInput")?;
            Ok(())
        }
    }

    /// Take the encoded unit after an [`Event::HaveOutput`]. `None` when the encoder only changed
    /// its output format.
    pub fn take(&mut self) -> Result<Option<Unit>> {
        unsafe {
            let mut output = MFT_OUTPUT_DATA_BUFFER::default();
            if !self.provides_samples {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(self.output_size)?)?;
                output.pSample = ManuallyDrop::new(Some(sample));
            }
            let mut status = 0;
            let result = self.transform.ProcessOutput(0, std::slice::from_mut(&mut output), &mut status);
            let sample = ManuallyDrop::take(&mut output.pSample);
            let _events = ManuallyDrop::take(&mut output.pEvents);
            match result {
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let format = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &format, 0)?;
                    return Ok(None);
                }
                Err(e) => return Err(e).context("encoder ProcessOutput"),
                Ok(()) => {}
            }
            let sample = sample.context("the encoder returned no sample")?;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let (mut pointer, mut length) = (std::ptr::null_mut(), 0u32);
            buffer.Lock(&mut pointer, None, Some(&mut length))?;
            let mut bytes = std::slice::from_raw_parts(pointer, length as usize).to_vec();
            buffer.Unlock()?;
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let time = sample.GetSampleTime().unwrap_or(0);
            // Late joiners start at a keyframe and need the parameter sets with it.
            if key && !nal_types(&bytes).contains(&7) {
                let header = self.sequence_header()?;
                ensure!(!header.is_empty(), "keyframe without SPS/PPS, and none in the output type");
                bytes.splice(0..0, header);
            }
            Ok(Some(Unit { bytes, key, time }))
        }
    }

    unsafe fn sequence_header(&self) -> Result<Vec<u8>> {
        unsafe {
            let format = self.transform.GetOutputCurrentType(0)?;
            let (mut data, mut size) = (std::ptr::null_mut(), 0u32);
            if format.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut data, &mut size).is_err() {
                return Ok(Vec::new());
            }
            let header = std::slice::from_raw_parts(data, size as usize).to_vec();
            CoTaskMemFree(Some(data as *const _));
            Ok(header)
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
        }
    }
}

/// BT.709, studio range: what the converter writes and the VUI should say.
unsafe fn color(format: &IMFMediaType) {
    unsafe {
        let _ = format.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32);
        let _ = format.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32);
        let _ = format.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32);
        let _ = format.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32);
    }
}

/// Encoder options are best effort: report the ones a driver refuses.
unsafe fn set(codec: &ICodecAPI, key: &GUID, value: VARIANT, what: &str) {
    if let Err(error) = unsafe { codec.SetValue(key, &value) } {
        println!("WINDOWS_ENCODER_OPTION {what} unsupported: {error}");
    }
}

unsafe fn friendly_name(activate: &IMFActivate) -> Option<String> {
    unsafe {
        let mut name = PWSTR::null();
        let mut length = 0u32;
        activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut length).ok()?;
        let text = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        text
    }
}

/// NAL unit types in an Annex-B unit.
pub fn nal_types(bytes: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
            types.push(bytes[i + 3] & 31);
            i += 3;
        } else {
            i += 1;
        }
    }
    types
}
