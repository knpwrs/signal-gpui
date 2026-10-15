//! The camera, on its own thread: each frame goes to RingRTC's video source
//! (what the other side sees) and, mirrored, to the self-view.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use nokhwa::pixel_format::RgbAFormat;
use nokhwa::utils::{CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution};
use ringrtc::webrtc::media::{VideoFrame, VideoPixelFormat, VideoSource};

use crate::FrameSlot;

/// Frames in a row that may fail (100 ms apart) before the camera stops.
const MAX_FAILURES: u32 = 50;

/// How long to wait for the user to answer macOS's camera prompt.
const PERMISSION_WAIT: Duration = Duration::from_secs(60);

/// A running camera; dropping it stops it.
pub(crate) struct Camera {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Camera {
    /// `choice`: the camera's name (else the first).
    pub(crate) fn start(source: VideoSource, preview: FrameSlot, choice: Option<String>) -> anyhow::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("camera".into())
            .spawn(move || {
                if let Err(error) = run(&flag, &source, &preview, choice.as_deref()) {
                    tracing::warn!(error = %format!("{error:#}"), "camera stopped");
                }
            })
            .context("starting the camera thread")?;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The cameras there are, by name.
pub(crate) fn list() -> Vec<crate::Device> {
    nokhwa::query(nokhwa::utils::ApiBackend::Auto)
        .unwrap_or_default()
        .into_iter()
        .map(|c| crate::Device { id: c.human_name(), name: c.human_name() })
        .collect()
}

fn run(stop: &AtomicBool, source: &VideoSource, preview: &FrameSlot, choice: Option<&str>) -> anyhow::Result<()> {
    // macOS asks for camera access the first time.
    nokhwa::nokhwa_initialize(|granted| tracing::info!(granted, "camera access"));
    let asked = Instant::now();
    while !nokhwa::nokhwa_check() {
        if stop.load(Ordering::Relaxed) || asked.elapsed() > PERMISSION_WAIT {
            return Err(anyhow!("no camera access"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // 720p at 30 fps, as Desktop sends at most.
    let wanted = CameraFormat::new(Resolution::new(1280, 720), FrameFormat::NV12, 30);
    let format = RequestedFormat::new::<RgbAFormat>(RequestedFormatType::Closest(wanted));
    let index = choice
        .and_then(|name| {
            nokhwa::query(nokhwa::utils::ApiBackend::Auto)
                .ok()?
                .into_iter()
                .find(|c| c.human_name() == name)
                .map(|c| c.index().clone())
        })
        .unwrap_or(CameraIndex::Index(0));
    let mut camera = nokhwa::Camera::new(index, format).map_err(|e| anyhow!("{e}"))?;
    camera.open_stream().map_err(|e| anyhow!("{e}"))?;
    let mut failures = 0;
    while !stop.load(Ordering::Relaxed) {
        let frame = match camera.frame() {
            Ok(frame) => {
                failures = 0;
                frame
            }
            // Unplugged, or taken by another app: wait, then give up.
            Err(error) => {
                failures += 1;
                if failures >= MAX_FAILURES {
                    let _ = camera.stop_stream();
                    return Err(anyhow!("{error}"));
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let Ok(image) = frame.decode_image::<RgbAFormat>() else { continue };
        let (width, height) = image.dimensions();
        let rgba = image.into_raw();
        source.push_frame(VideoFrame::copy_from_slice(width, height, VideoPixelFormat::Rgba, &rgba));
        preview.put_rgba(width, height, &rgba, true);
    }
    let _ = camera.stop_stream();
    Ok(())
}
