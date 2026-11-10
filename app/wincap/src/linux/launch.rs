//! GStreamer plumbing shared by the video and sound pipelines: build one from text, shut it
//! down however it ends, and read errors off its bus.

use std::ops::Deref;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::Control;

/// A pipeline that is set back to Null when dropped, however the code using it exits. A
/// pipeline dropped in any other state keeps its threads and PipeWire streams alive, so every
/// failed start used to leak one.
pub struct Pipeline {
    pipeline: gst::Pipeline,
    control: Arc<Control>,
    /// "video" or "audio", for the error message.
    what: &'static str,
}

impl Deref for Pipeline {
    type Target = gst::Pipeline;
    fn deref(&self) -> &gst::Pipeline {
        &self.pipeline
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        if let Err(e) = self.pipeline.set_state(gst::State::Null) {
            self.control.report_error(format!("stopping the {} pipeline: {e}", self.what));
        }
    }
}

/// Builds the pipeline described by `text`, which ends in an appsink named `sink`.
pub fn launch(text: &str, what: &'static str, control: &Arc<Control>) -> Result<(Pipeline, gst_app::AppSink)> {
    let pipeline = gst::parse::launch(text)
        .with_context(|| format!("building the {what} pipeline"))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("the {what} pipeline isn't a pipeline"))?;
    let pipeline = Pipeline { pipeline, control: control.clone(), what };
    let sink =
        pipeline.by_name("sink").and_then(|e| e.downcast::<gst_app::AppSink>().ok()).ok_or_else(|| anyhow!("the {what} pipeline has no output"))?;
    Ok((pipeline, sink))
}

/// An error a pipeline element posted.
#[derive(Debug, thiserror::Error)]
#[error("{element}: {message} ({debug})")]
pub struct PipelineError {
    /// Which element, e.g. "/GstPipeline:pipeline0/GstPipeWireSrc:src".
    pub element: String,
    /// The error itself, in plain words.
    pub message: String,
    /// The element's technical detail (may be empty).
    pub debug: String,
}

impl PipelineError {
    /// PipeWire couldn't find the stream it was pointed at. The text comes from the session
    /// manager (WirePlumber), which is why it's matched as text.
    pub fn target_not_found(&self) -> bool {
        self.message.contains("target not found") || self.debug.contains("target not found")
    }

    pub fn from_message(msg: &gst::Message) -> Option<Self> {
        let gst::MessageView::Error(e) = msg.view() else { return None };
        Some(Self {
            element: e.src().map(|s| s.path_string().to_string()).unwrap_or_default(),
            message: e.error().to_string(),
            debug: e.debug().map(|d| d.to_string()).unwrap_or_default(),
        })
    }
}

/// The next error on the bus, waiting up to `wait` (zero: don't wait).
pub fn next_error(bus: &gst::Bus, wait: gst::ClockTime) -> Option<PipelineError> {
    bus.timed_pop_filtered(wait, &[gst::MessageType::Error]).as_ref().and_then(PipelineError::from_message)
}

/// Sets `pipeline` playing. When an element refuses, `set_state` only says "failed": the real
/// reason (which element, and why) is on the bus, and is the error returned (a
/// [`PipelineError`]).
pub fn play(pipeline: &gst::Pipeline) -> Result<()> {
    if pipeline.set_state(gst::State::Playing).is_ok() {
        return Ok(());
    }
    match pipeline.bus().and_then(|bus| next_error(&bus, gst::ClockTime::from_seconds(1))) {
        Some(why) => Err(why.into()),
        None => Err(anyhow!("an element refused to start (no details)")),
    }
}

/// The GStreamer element `name` is installed.
pub fn available(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_state` only says "failed"; which element refused, and why, is on the bus.
    #[test]
    fn a_refused_start_says_which_element_and_why() {
        gst::init().unwrap();
        let pipeline = gst::parse::launch("filesrc name=missing location=/nonexistent/streamroom-test ! fakesink").unwrap();
        let pipeline = pipeline.downcast::<gst::Pipeline>().unwrap();
        let e = play(&pipeline).unwrap_err();
        pipeline.set_state(gst::State::Null).unwrap();
        let why = e.downcast_ref::<PipelineError>().expect("the element's own error");
        assert!(why.element.ends_with("missing"), "{why}");
        assert!(!why.message.is_empty());
    }
}
