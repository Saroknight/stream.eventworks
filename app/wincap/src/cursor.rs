//! Mouse cursor: DXGI Desktop Duplication captures the screen *without* the cursor, and reports
//! the cursor's shape and position separately. We draw it into each frame ourselves.
//!
//! Drawing: copy the small rectangle under the cursor to a CPU-readable texture, blend the cursor
//! in on the CPU (handles all three Windows cursor types, including "invert" pixels like the
//! text I-beam), and copy it back. A few KB per frame.

use anyhow::{anyhow, Result};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_POINTER_SHAPE_INFO, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};

#[derive(Clone, Copy)]
enum Op {
    /// Alpha blend (0 = transparent, 255 = opaque).
    Blend(u8),
    /// XOR the screen pixel with the color (Windows "invert" cursor pixels).
    Xor,
}

#[derive(Clone)]
pub struct Shape {
    w: u32,
    h: u32,
    /// BGR color and operation per pixel, row-major.
    px: Vec<([u8; 3], Op)>,
}

/// Cursor position and shape, updated from duplication frame info.
#[derive(Default)]
pub struct CursorState {
    pub visible: bool,
    /// Top-left of the cursor image, relative to the captured monitor.
    pub x: i32,
    pub y: i32,
    pub shape: Option<Shape>,
}

impl Shape {
    pub fn from_dxgi(info: &DXGI_OUTDUPL_POINTER_SHAPE_INFO, buf: &[u8]) -> Option<Shape> {
        let (w, pitch) = (info.Width, info.Pitch as usize);
        let kind = info.Type as i32;
        let mut px = Vec::new();
        if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 {
            // 1 bit per pixel: AND mask on top, XOR mask below (so the buffer is twice as tall).
            let h = info.Height / 2;
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let bit = 0x80u8 >> (x % 8);
                    let and = buf.get(y * pitch + x / 8)? & bit != 0;
                    let xor = buf.get((y + h as usize) * pitch + x / 8)? & bit != 0;
                    px.push(match (and, xor) {
                        (false, false) => ([0, 0, 0], Op::Blend(255)),
                        (false, true) => ([255, 255, 255], Op::Blend(255)),
                        (true, false) => ([0, 0, 0], Op::Blend(0)),
                        (true, true) => ([255, 255, 255], Op::Xor),
                    });
                }
            }
            return Some(Shape { w, h, px });
        }
        let h = info.Height;
        for y in 0..h as usize {
            for x in 0..w as usize {
                let i = y * pitch + x * 4;
                let p = buf.get(i..i + 4)?;
                let bgr = [p[0], p[1], p[2]];
                px.push(if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 {
                    (bgr, Op::Blend(p[3]))
                } else if kind == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 {
                    // Alpha byte is a mask: 0 = use the color, 0xFF = XOR with the screen.
                    if p[3] == 0 {
                        (bgr, Op::Blend(255))
                    } else {
                        (bgr, Op::Xor)
                    }
                } else {
                    return None;
                });
            }
        }
        Some(Shape { w, h, px })
    }
}

/// Draws the cursor into a BGRA texture.
#[derive(Default)]
pub struct CursorPainter {
    scratch: Option<(ID3D11Texture2D, u32, u32)>,
}

impl CursorPainter {
    pub fn draw(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        target: &ID3D11Texture2D,
        (tw, th): (u32, u32),
        cursor: &CursorState,
    ) -> Result<()> {
        let Some(shape) = cursor.shape.as_ref().filter(|_| cursor.visible) else { return Ok(()) };
        // Clip the cursor rectangle to the frame.
        let x0 = cursor.x.max(0);
        let y0 = cursor.y.max(0);
        let x1 = (cursor.x + shape.w as i32).min(tw as i32);
        let y1 = (cursor.y + shape.h as i32).min(th as i32);
        if x1 <= x0 || y1 <= y0 {
            return Ok(());
        }
        let (rw, rh) = ((x1 - x0) as u32, (y1 - y0) as u32);
        let scratch = self.scratch(device, rw, rh)?;
        unsafe {
            let area = D3D11_BOX { left: x0 as u32, top: y0 as u32, front: 0, right: x1 as u32, bottom: y1 as u32, back: 1 };
            context.CopySubresourceRegion(&scratch, 0, 0, 0, 0, target, 0, Some(&area));
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(&scratch, 0, D3D11_MAP_READ_WRITE, 0, Some(&mut map))?;
            let (sx, sy) = ((x0 - cursor.x) as u32, (y0 - cursor.y) as u32);
            for row in 0..rh {
                let line = std::slice::from_raw_parts_mut((map.pData as *mut u8).add((row * map.RowPitch) as usize), (rw * 4) as usize);
                for col in 0..rw {
                    let (bgr, op) = shape.px[((sy + row) * shape.w + sx + col) as usize];
                    let d = &mut line[(col * 4) as usize..(col * 4 + 3) as usize];
                    match op {
                        Op::Xor => {
                            for k in 0..3 {
                                d[k] ^= bgr[k];
                            }
                        }
                        Op::Blend(255) => d.copy_from_slice(&bgr),
                        Op::Blend(0) => {}
                        Op::Blend(a) => {
                            let a = a as u16;
                            for k in 0..3 {
                                d[k] = ((bgr[k] as u16 * a + d[k] as u16 * (255 - a) + 127) / 255) as u8;
                            }
                        }
                    }
                }
            }
            context.Unmap(&scratch, 0);
            let back = D3D11_BOX { left: 0, top: 0, front: 0, right: rw, bottom: rh, back: 1 };
            context.CopySubresourceRegion(target, 0, x0 as u32, y0 as u32, 0, &scratch, 0, Some(&back));
        }
        Ok(())
    }

    /// A CPU-readable/writable texture at least w x h (grown as needed, never shrunk).
    fn scratch(&mut self, device: &ID3D11Device, w: u32, h: u32) -> Result<ID3D11Texture2D> {
        if let Some((t, sw, sh)) = &self.scratch {
            if *sw >= w && *sh >= h {
                return Ok(t.clone());
            }
        }
        let (w, h) = (w.max(64), h.max(64));
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: (D3D11_CPU_ACCESS_READ | D3D11_CPU_ACCESS_WRITE).0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
        let tex = tex.ok_or_else(|| anyhow!("no cursor texture"))?;
        self.scratch = Some((tex.clone(), w, h));
        Ok(tex)
    }
}
