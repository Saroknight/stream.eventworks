//! DXGI Desktop Duplication: works on every Windows 10/11 version and keeps up with the
//! monitor's refresh rate for full-screen capture. The captured image stays on the GPU.

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::{
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO,
};

pub struct Duplicator {
    output: IDXGIOutput1,
    device: ID3D11Device,
    dup: Option<IDXGIOutputDuplication>,
    holding: bool,
    pub width: u32,
    pub height: u32,
}

pub enum Grab {
    /// A new desktop image. Valid until the next call to `acquire` or `release`.
    Frame(ID3D11Texture2D),
    /// Nothing changed on screen within the timeout.
    Nothing,
    /// Duplication was interrupted (resolution change, UAC prompt, full-screen switch). Retrying.
    Lost,
}

impl Duplicator {
    pub fn new(output: &IDXGIOutput1, device: &ID3D11Device) -> Result<Self> {
        let mut d = Self { output: output.clone(), device: device.clone(), dup: None, holding: false, width: 0, height: 0 };
        d.recreate().context("start desktop duplication")?;
        Ok(d)
    }

    fn recreate(&mut self) -> Result<()> {
        self.dup = None;
        self.holding = false;
        unsafe {
            let dup = self.output.DuplicateOutput(&self.device)?;
            let desc = dup.GetDesc();
            self.width = desc.ModeDesc.Width;
            self.height = desc.ModeDesc.Height;
            self.dup = Some(dup);
        }
        Ok(())
    }

    pub fn release(&mut self) {
        if self.holding {
            if let Some(dup) = &self.dup {
                unsafe {
                    let _ = dup.ReleaseFrame();
                }
            }
            self.holding = false;
        }
    }

    pub fn acquire(&mut self, timeout_ms: u32) -> Result<Grab> {
        self.release();
        if self.dup.is_none() {
            // Re-creation fails while the secure desktop (UAC) is up; keep trying.
            if self.recreate().is_err() {
                std::thread::sleep(std::time::Duration::from_millis(50));
                return Ok(Grab::Lost);
            }
        }
        let dup = self.dup.as_ref().unwrap();
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {
                self.holding = true;
                // LastPresentTime == 0 means only the mouse moved, not a new image.
                if info.LastPresentTime == 0 || resource.is_none() {
                    return Ok(Grab::Nothing);
                }
                let tex: ID3D11Texture2D = resource.unwrap().cast().context("desktop image as texture")?;
                Ok(Grab::Frame(tex))
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(Grab::Nothing),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                self.dup = None;
                Ok(Grab::Lost)
            }
            Err(e) => Err(e).context("AcquireNextFrame"),
        }
    }
}

impl Drop for Duplicator {
    fn drop(&mut self) {
        self.release();
    }
}
