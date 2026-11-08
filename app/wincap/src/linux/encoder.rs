//! The H.264 encoders the Linux video pipeline can use, and how to set each one up.

use super::launch::available;

/// The H.264 encoders we know how to drive, best first in `Encoder::ALL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoder {
    VaH264,
    VaH264LowPower,
    /// The older gstreamer-vaapi plugin.
    Vaapi,
    Nvenc,
    X264,
    OpenH264,
}

impl Encoder {
    pub const ALL: [Encoder; 6] = [Self::VaH264, Self::VaH264LowPower, Self::Vaapi, Self::Nvenc, Self::X264, Self::OpenH264];

    pub fn element(self) -> &'static str {
        match self {
            Self::VaH264 => "vah264enc",
            Self::VaH264LowPower => "vah264lpenc",
            Self::Vaapi => "vaapih264enc",
            Self::Nvenc => "nvh264enc",
            Self::X264 => "x264enc",
            Self::OpenH264 => "openh264enc",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::VaH264 => "VA-API H.264 (graphics card)",
            Self::VaH264LowPower => "VA-API H.264 low power (graphics card)",
            Self::Vaapi => "VA-API H.264 (graphics card, legacy plugin)",
            Self::Nvenc => "NVIDIA NVENC H.264 (graphics card)",
            Self::X264 => "x264 (processor)",
            Self::OpenH264 => "OpenH264 (processor)",
        }
    }

    pub fn on_gpu(self) -> bool {
        match self {
            Self::VaH264 | Self::VaH264LowPower | Self::Vaapi | Self::Nvenc => true,
            Self::X264 | Self::OpenH264 => false,
        }
    }

    /// Runs on VA surfaces, which `vapostproc` makes.
    fn wants_va_surfaces(self) -> bool {
        match self {
            Self::VaH264 | Self::VaH264LowPower => true,
            Self::Vaapi | Self::Nvenc | Self::X264 | Self::OpenH264 => false,
        }
    }

    /// The encoder and everything its part of the pipeline needs are installed.
    pub fn available(self) -> bool {
        available(self.element()) && (!self.wants_va_surfaces() || available("vapostproc"))
    }

    /// Constant bitrate, no B-frames, a keyframe at least every `gop` frames, low latency.
    pub fn settings(self, kbps: u32, gop: u32) -> String {
        match self {
            Self::VaH264 | Self::VaH264LowPower => format!("rate-control=cbr bitrate={kbps} key-int-max={gop} b-frames=0 target-usage=7"),
            Self::Vaapi => format!("rate-control=cbr bitrate={kbps} keyframe-period={gop} max-bframes=0 tune=low-power"),
            Self::Nvenc => format!("rc-mode=cbr bitrate={kbps} gop-size={gop} bframes=0 zerolatency=true preset=low-latency-hq"),
            Self::X264 => format!("tune=zerolatency speed-preset=veryfast bitrate={kbps} key-int-max={gop} bframes=0 vbv-buf-capacity=500"),
            Self::OpenH264 => format!("bitrate={} gop-size={gop} rate-control=bitrate", self.bitrate_value(kbps * 1000)),
        }
    }

    /// The encoder's `bitrate` property for `bps` bits per second.
    pub fn bitrate_value(self, bps: u32) -> u32 {
        match self {
            Self::OpenH264 => bps,
            Self::VaH264 | Self::VaH264LowPower | Self::Vaapi | Self::Nvenc | Self::X264 => bps / 1000,
        }
    }

    /// Scales the frames to `size` (caps fields) in the format the encoder takes.
    pub fn convert(self, size: &str) -> String {
        match self {
            Self::VaH264 | Self::VaH264LowPower => format!("vapostproc ! video/x-raw(memory:VAMemory),format=NV12{size}"),
            Self::Vaapi | Self::Nvenc => format!("videoconvert ! videoscale ! video/x-raw,format=NV12{size}"),
            Self::X264 | Self::OpenH264 => format!("videoconvert ! videoscale ! video/x-raw,format=I420{size}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Encoder;

    #[test]
    fn bitrate_units_match_each_encoder() {
        // OpenH264 takes bits per second, the others kilobits.
        assert!(Encoder::OpenH264.settings(8000, 120).starts_with("bitrate=8000000 "));
        assert_eq!(Encoder::OpenH264.bitrate_value(8_000_000), 8_000_000);
        for e in [Encoder::VaH264, Encoder::VaH264LowPower, Encoder::Vaapi, Encoder::Nvenc, Encoder::X264] {
            assert!(e.settings(8000, 120).contains("bitrate=8000 "), "{e:?}");
            assert_eq!(e.bitrate_value(8_000_000), 8000, "{e:?}");
        }
    }
}
