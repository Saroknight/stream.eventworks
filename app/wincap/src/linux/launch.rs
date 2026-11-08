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

/// The GStreamer element `name` is installed.
pub fn available(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}
