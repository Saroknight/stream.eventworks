//! GPU color conversion and scaling with the D3D11 video processor:
//! desktop BGRA (any size) -> NV12 at the output size, which is what hardware encoders take.

use std::mem::ManuallyDrop;

use crate::cursor::{CursorPainter, CursorState};
use anyhow::{anyhow, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12,
    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};

pub struct Converter {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vctx: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    /// Clean copy of the latest desktop image.
    staging: ID3D11Texture2D,
    /// The image actually converted: staging plus the mouse cursor.
    composite: ID3D11Texture2D,
    input_view: ID3D11VideoProcessorInputView,
    painter: CursorPainter,
    outputs: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
    pub src_width: u32,
    pub src_height: u32,
    pub src_format: DXGI_FORMAT,
}

fn texture(device: &ID3D11Device, w: u32, h: u32, format: DXGI_FORMAT, bind: D3D11_BIND_FLAG) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.context("CreateTexture2D")?;
    tex.ok_or_else(|| anyhow!("no texture"))
}

impl Converter {
    /// `ring` = how many output textures to rotate through (the async encoder may still be
    /// reading an earlier one while we write the next).
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src: (u32, u32, DXGI_FORMAT),
        dst: (u32, u32),
        fps: u32,
        ring: usize,
    ) -> Result<Self> {
        let (sw, sh, sfmt) = src;
        let (dw, dh) = dst;
        unsafe {
            let vdev: ID3D11VideoDevice = device.cast().context("ID3D11VideoDevice")?;
            let vctx: ID3D11VideoContext = context.cast().context("ID3D11VideoContext")?;
            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: sw,
                InputHeight: sh,
                OutputFrameRate: rate,
                OutputWidth: dw,
                OutputHeight: dh,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            };
            let enumerator = vdev.CreateVideoProcessorEnumerator(&content).context("CreateVideoProcessorEnumerator")?;
            let processor = vdev.CreateVideoProcessor(&enumerator, 0).context("CreateVideoProcessor")?;

            // Our own copy of the desktop image, so the duplication frame can be released immediately.
            let staging = texture(device, sw, sh, sfmt, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE)?;
            let composite = texture(device, sw, sh, sfmt, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE)?;
            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
            };
            let mut input_view = None;
            vdev.CreateVideoProcessorInputView(&composite, &enumerator, &in_desc, Some(&mut input_view)).context("CreateVideoProcessorInputView")?;
            let input_view = input_view.ok_or_else(|| anyhow!("no input view"))?;

            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut outputs = Vec::with_capacity(ring);
            for _ in 0..ring {
                let tex = texture(device, dw, dh, DXGI_FORMAT_NV12, D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE)?;
                let mut view = None;
                vdev.CreateVideoProcessorOutputView(&tex, &enumerator, &out_desc, Some(&mut view))
                    .context("CreateVideoProcessorOutputView (NV12)")?;
                outputs.push((tex, view.ok_or_else(|| anyhow!("no output view"))?));
            }

            // Full-range RGB in, limited-range BT.709 YUV out: what browsers expect from H.264.
            if let Ok(vctx1) = vctx.cast::<ID3D11VideoContext1>() {
                vctx1.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
                vctx1.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            }
            vctx.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            vctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);

            // Fit the source inside the output, keeping its aspect ratio (black bars if they differ).
            let src_rect = RECT { left: 0, top: 0, right: sw as i32, bottom: sh as i32 };
            let scale = f64::min(dw as f64 / sw as f64, dh as f64 / sh as f64);
            let (fw, fh) = ((sw as f64 * scale).round() as i32, (sh as f64 * scale).round() as i32);
            let (ox, oy) = ((dw as i32 - fw) / 2, (dh as i32 - fh) / 2);
            let dst_rect = RECT { left: ox, top: oy, right: ox + fw, bottom: oy + fh };
            let target = RECT { left: 0, top: 0, right: dw as i32, bottom: dh as i32 };
            vctx.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&src_rect));
            vctx.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dst_rect));
            vctx.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target));
            let black =
                D3D11_VIDEO_COLOR { Anonymous: D3D11_VIDEO_COLOR_0 { YCbCr: D3D11_VIDEO_COLOR_YCbCrA { Y: 0.0625, Cb: 0.5, Cr: 0.5, A: 1.0 } } };
            vctx.VideoProcessorSetOutputBackgroundColor(&processor, true, &black);

            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                vctx,
                processor,
                staging,
                composite,
                input_view,
                painter: CursorPainter::default(),
                outputs,
                next: 0,
                src_width: sw,
                src_height: sh,
                src_format: sfmt,
            })
        }
    }

    /// Copies the desktop image into our own texture so the duplication frame can be released.
    pub fn stage(&mut self, desktop: &ID3D11Texture2D) {
        unsafe { self.context.CopyResource(&self.staging, desktop) };
    }

    /// Converts the staged image (plus the cursor, if given) into the next NV12 output texture.
    pub fn convert_staged(&mut self, cursor: Option<&CursorState>) -> Result<ID3D11Texture2D> {
        unsafe { self.context.CopyResource(&self.composite, &self.staging) };
        if let Some(c) = cursor {
            // BGRA only (desktop duplication always gives BGRA). A cursor failure never stops the stream.
            if self.src_format == DXGI_FORMAT_B8G8R8A8_UNORM {
                let _ = self.painter.draw(&self.device, &self.context, &self.composite, (self.src_width, self.src_height), c);
            }
        }
        let (out_tex, out_view) = &self.outputs[self.next];
        self.next = (self.next + 1) % self.outputs.len();
        unsafe {
            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: ManuallyDrop::new(Some(self.input_view.clone())),
                ..Default::default()
            }];
            let result = self.vctx.VideoProcessorBlt(&self.processor, out_view, 0, &streams);
            // Release the reference we handed to the stream description.
            ManuallyDrop::drop(&mut streams[0].pInputSurface);
            result.context("VideoProcessorBlt")?;
        }
        Ok(out_tex.clone())
    }

    pub fn output_size(&self) -> (u32, u32) {
        let d = texture_desc(&self.outputs[0].0);
        (d.Width, d.Height)
    }
}

pub fn texture_desc(tex: &ID3D11Texture2D) -> D3D11_TEXTURE2D_DESC {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { tex.GetDesc(&mut desc) };
    desc
}
