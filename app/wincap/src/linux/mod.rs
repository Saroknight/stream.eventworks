//! Linux capture: screen via the Wayland screen-share portal (or X11), GStreamer encoding,
//! PipeWire sound. Same interface as the Windows side.
mod audio;
mod launch;
mod portal;
mod video;

pub use audio::{audio_apps, start_audio};
pub use video::{list_monitors, start_video};
