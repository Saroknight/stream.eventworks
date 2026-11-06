//! D3D11 device on the GPU that drives the chosen monitor.

use anyhow::{anyhow, Context, Result};
use windows::core::Interface;
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1, DXGI_OUTPUT_DESC};

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub output: IDXGIOutput1,
    pub adapter_name: String,
    pub luid: LUID,
}

fn wide(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end])
}

pub fn open(monitor: u32) -> Result<Gpu> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;

        // Every monitor attached to the desktop, on every GPU. Primary monitor (at 0,0) first.
        let mut outputs: Vec<(IDXGIAdapter1, IDXGIOutput, DXGI_OUTPUT_DESC)> = Vec::new();
        let mut ai = 0;
        while let Ok(adapter) = factory.EnumAdapters1(ai) {
            let mut oi = 0;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                if let Ok(desc) = output.GetDesc() {
                    if desc.AttachedToDesktop.as_bool() {
                        outputs.push((adapter.clone(), output, desc));
                    }
                }
                oi += 1;
            }
            ai += 1;
        }
        if outputs.is_empty() {
            return Err(anyhow!("no monitors found"));
        }
        outputs.sort_by_key(|(_, _, d)| {
            let r = d.DesktopCoordinates;
            (!(r.left == 0 && r.top == 0), r.left, r.top)
        });
        let index = (monitor as usize).min(outputs.len() - 1);
        let (adapter, output, _desc) = outputs.swap_remove(index);
        let adesc = adapter.GetDesc1().context("adapter GetDesc1")?;

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice (video support)")?;
        let device = device.ok_or_else(|| anyhow!("no D3D11 device"))?;
        let context = context.ok_or_else(|| anyhow!("no D3D11 context"))?;

        // The Media Foundation encoder uses this device from its own threads.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }

        Ok(Gpu {
            device,
            context,
            output: output.cast::<IDXGIOutput1>().context("IDXGIOutput1 (Windows 8+ required)")?,
            adapter_name: wide(&adesc.Description),
            luid: adesc.AdapterLuid,
        })
    }
}
