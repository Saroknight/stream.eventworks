//! Linux capture: screen via the Wayland screen-share portal (or X11), GStreamer encoding,
//! PipeWire sound. Same interface as the Windows side.
mod audio;
mod encoder;
mod gate;
mod launch;
mod portal;
pub(crate) mod record;
mod video;

pub use audio::{audio_apps, start_audio};
pub use record::{recording, start_recording, stop_recording};
pub use video::{list_monitors, screen_dialog, start_video};
