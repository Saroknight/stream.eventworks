//! Media Foundation hardware H.264 encoder (NVENC, AMF or QuickSync, whichever the GPU has),
//! fed directly with GPU textures, configured for low latency.

use std::mem::ManuallyDrop;

use anyhow::{anyhow, bail, Context, Result};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;

pub struct EncodedOut {
    pub data: Vec<u8>,
    pub keyframe: bool,
    /// Sample time (100 ns units) of the input frame this came from.
    pub sample_time: i64,
}

pub struct Encoder {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Option<ICodecAPI>,
    _manager: IMFDXGIDeviceManager,
    pub name: String,
    pub unsupported: Vec<String>,
    fps: u32,
    /// Encoder asked for this many more input frames.
    pub need_input: u32,
    sequence_header: Vec<u8>,
}

fn activate_name(a: &IMFActivate) -> String {
    unsafe {
        let mut p = PWSTR::null();
        let mut len = 0u32;
        if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() && !p.is_null() {
            let s = p.to_string().unwrap_or_default();
            CoTaskMemFree(Some(p.0 as *const _));
            return s;
        }
    }
    "hardware H.264 encoder".into()
}

/// Hardware H.264 encoders for NV12 input, best first. Prefers the GPU that drives the monitor.
fn find_encoders(luid: LUID) -> Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let flags = MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER;
    unsafe {
        let take = |ptr: *mut Option<IMFActivate>, count: u32| -> Vec<IMFActivate> {
            let mut out = Vec::new();
            if !ptr.is_null() {
                let slice = std::slice::from_raw_parts_mut(ptr, count as usize);
                for item in slice.iter_mut() {
                    if let Some(a) = item.take() {
                        out.push(a);
                    }
                }
                CoTaskMemFree(Some(ptr as *const _));
            }
            out
        };

        // MFTEnum2 can filter by GPU (Windows 10 1703+).
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1)?;
        if let Some(attrs) = &attrs {
            let packed = ((luid.HighPart as u32 as u64) << 32) | luid.LowPart as u64;
            attrs.SetUINT64(&MFT_ENUM_ADAPTER_LUID, packed)?;
            let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0u32;
            if MFTEnum2(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), attrs, &mut ptr, &mut count).is_ok() {
                let list = take(ptr, count);
                if !list.is_empty() {
                    return Ok(list);
                }
            }
        }
        let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut ptr, &mut count).context("MFTEnumEx")?;
        Ok(take(ptr, count))
    }
}

impl Encoder {
    pub fn new(device: &ID3D11Device, luid: LUID, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        let candidates = find_encoders(luid)?;
        if candidates.is_empty() {
            bail!("no hardware H.264 encoder found. Update your graphics driver.");
        }
        let mut errors = Vec::new();
        for activate in candidates {
            let name = activate_name(&activate);
            match Self::open(&activate, &name, device, width, height, fps, bitrate) {
                Ok(enc) => return Ok(enc),
                Err(e) => {
                    errors.push(format!("{name}: {e:#}"));
                    unsafe {
                        let _ = activate.ShutdownObject();
                    }
                }
            }
        }
        bail!("no hardware encoder could be started: {}", errors.join("; "))
    }

    fn open(activate: &IMFActivate, name: &str, device: &ID3D11Device, width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        unsafe {
            let mft: IMFTransform = activate.ActivateObject().context("ActivateObject")?;

            // Hardware encoders are asynchronous and must be unlocked before use.
            let attrs = mft.GetAttributes().context("GetAttributes")?;
            if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 0 {
                bail!("encoder is not asynchronous (not a hardware encoder)");
            }
            attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;

            // Hand it our D3D11 device so it reads GPU textures directly (zero copy).
            let mut token = 0u32;
            let mut manager: Option<IMFDXGIDeviceManager> = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.ok_or_else(|| anyhow!("no DXGI device manager"))?;
            manager.ResetDevice(device, token)?;
            mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize).context("encoder rejected the D3D11 device")?;

            // Low-latency settings. Rate control must be set before the output type.
            let codec = mft.cast::<ICodecAPI>().ok();
            let mut unsupported = Vec::new();
            if let Some(c) = &codec {
                let mut set = |api: &GUID, v: VARIANT, label: &str| {
                    if c.SetValue(api, &v).is_err() {
                        unsupported.push(label.to_string());
                    }
                };
                set(&CODECAPI_AVEncCommonRateControlMode, VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32), "CBR");
                set(&CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(bitrate), "bitrate");
                set(&CODECAPI_AVLowLatencyMode, VARIANT::from(true), "low-latency mode");
                set(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32), "no B-frames");
                // Keyframes come mainly on request (new viewer / packet loss); this is just a safety net.
                set(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(fps * 10), "GOP size");
            } else {
                unsupported.push("ICodecAPI".into());
            }

            let size = ((width as u64) << 32) | height as u64;
            let rate = ((fps as u64) << 32) | 1;

            let out_type = MFCreateMediaType()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            // Baseline profile: decodable everywhere WebRTC H.264 is supported.
            out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
            mft.SetOutputType(0, &out_type, 0).context("SetOutputType (H.264)")?;

            let mut set_input = false;
            for i in 0.. {
                let t = match mft.GetInputAvailableType(0, i) {
                    Ok(t) => t,
                    Err(_) => break,
                };
                if t.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFVideoFormat_NV12) {
                    t.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
                    t.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
                    mft.SetInputType(0, &t, 0).context("SetInputType (NV12)")?;
                    set_input = true;
                    break;
                }
            }
            if !set_input {
                bail!("encoder does not accept NV12 at {width}x{height}");
            }

            let info = mft.GetOutputStreamInfo(0)?;
            if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
                bail!("encoder expects caller-allocated output (unsupported)");
            }

            let events: IMFMediaEventGenerator = mft.cast().context("IMFMediaEventGenerator")?;
            mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            let mut enc =
                Self { mft, events, codec, _manager: manager, name: name.to_string(), unsupported, fps, need_input: 0, sequence_header: Vec::new() };
            enc.refresh_sequence_header();
            Ok(enc)
        }
    }

    fn refresh_sequence_header(&mut self) {
        unsafe {
            if let Ok(t) = self.mft.GetOutputCurrentType(0) {
                if let Ok(n) = t.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) {
                    let mut buf = vec![0u8; n as usize];
                    if t.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut buf, None).is_ok() {
                        self.sequence_header = buf;
                    }
                }
            }
        }
    }

    /// Handles all pending encoder events without blocking.
    pub fn poll(&mut self, out: &mut Vec<EncodedOut>) -> Result<()> {
        loop {
            let event = match unsafe { self.events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(e) => e,
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(e) => return Err(e).context("encoder GetEvent"),
            };
            let kind = unsafe { event.GetType()? };
            if kind == METransformNeedInput.0 as u32 {
                self.need_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                self.take_output(out)?;
            }
        }
    }

    fn take_output(&mut self, out: &mut Vec<EncodedOut>) -> Result<()> {
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(None), dwStatus: 0, pEvents: ManuallyDrop::new(None) }];
        let mut status = 0u32;
        let result = unsafe { self.mft.ProcessOutput(0, &mut buffers, &mut status) };
        let sample = unsafe { ManuallyDrop::take(&mut buffers[0].pSample) };
        let _events = unsafe { ManuallyDrop::take(&mut buffers[0].pEvents) };
        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                unsafe {
                    let t = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &t, 0)?;
                }
                self.refresh_sequence_header();
                return Ok(());
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
            Err(e) => return Err(e).context("ProcessOutput"),
        }
        let Some(sample) = sample else { return Ok(()) };
        unsafe {
            let sample_time = sample.GetSampleTime().unwrap_or(0);
            let clean_point = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 1;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let mut data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;

            let nals = nal_types(&data);
            let keyframe = clean_point || nals.contains(&5);
            // Every keyframe must carry SPS/PPS so viewers who join late can decode it.
            if keyframe && !nals.contains(&7) {
                if self.sequence_header.is_empty() {
                    self.refresh_sequence_header();
                }
                if !self.sequence_header.is_empty() {
                    let mut with_header = self.sequence_header.clone();
                    with_header.extend_from_slice(&data);
                    data = with_header;
                }
            }
            out.push(EncodedOut { data, keyframe, sample_time });
        }
        Ok(())
    }

    /// Feeds one NV12 GPU texture. Call only when `need_input > 0`.
    pub fn submit(&mut self, texture: &ID3D11Texture2D, sample_time: i64, keyframe: bool) -> Result<()> {
        unsafe {
            if keyframe {
                if let Some(c) = &self.codec {
                    let _ = c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                }
            }
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(sample_time)?;
            sample.SetSampleDuration(10_000_000 / self.fps as i64)?;
            self.mft.ProcessInput(0, &sample, 0).context("ProcessInput")?;
        }
        self.need_input = self.need_input.saturating_sub(1);
        Ok(())
    }

    pub fn set_bitrate(&self, bps: u32) {
        if let Some(c) = &self.codec {
            unsafe {
                let _ = c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bps));
            }
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.mft.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
        }
    }
}

/// NAL unit types present in an Annex B buffer.
pub fn nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut i = 0;
    while i + 3 < data.len() {
        let (is_start, len) = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            (true, 3)
        } else if i + 4 < data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
            (true, 4)
        } else {
            (false, 1)
        };
        if is_start {
            if let Some(b) = data.get(i + len) {
                types.push(b & 0x1f);
            }
        }
        i += len;
    }
    types
}
