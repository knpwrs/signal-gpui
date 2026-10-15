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
    /// `choice`: the camera's name (else the first). `failed` hears why
    /// it stopped, if it stops on its own.
    pub(crate) fn start(
        source: VideoSource,
        preview: FrameSlot,
        choice: Option<String>,
        failed: impl FnOnce(String) + Send + 'static,
    ) -> anyhow::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("camera".into())
            .spawn(move || {
                if let Err(error) = run(&flag, &source, &preview, choice.as_deref()) {
                    let error = format!("{error:#}");
                    tracing::warn!(%error, "camera stopped");
                    // (Not when it was stopped; it was meant to.)
                    if !flag.load(Ordering::Relaxed) {
                        failed(error);
                    }
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

/// The formats to ask for, best first: 720p at 30 fps (as Desktop sends
/// at most), then whatever the camera has. On macOS nokhwa lists the
/// camera's NV12 formats as YUYV, so asking for NV12 never matches there;
/// and `Closest` only finds a frame rate at exactly the size asked for.
fn requests() -> Vec<RequestedFormatType> {
    let hd = |format| RequestedFormatType::Closest(CameraFormat::new(Resolution::new(1280, 720), format, 30));
    vec![
        hd(FrameFormat::YUYV),
        hd(FrameFormat::NV12),
        hd(FrameFormat::MJPEG),
        RequestedFormatType::HighestResolution(Resolution::new(1280, 720)),
        RequestedFormatType::AbsoluteHighestFrameRate,
        RequestedFormatType::None,
    ]
}

/// Opens the camera with the first format it can give.
fn open(index: &CameraIndex) -> anyhow::Result<nokhwa::Camera> {
    let mut last = anyhow!("no camera formats");
    for request in requests() {
        let opened = nokhwa::Camera::new(index.clone(), RequestedFormat::new::<RgbAFormat>(request))
            .and_then(|mut camera| camera.open_stream().map(|()| camera));
        match opened {
            Ok(camera) => {
                tracing::info!(format = %camera.camera_format(), "camera started");
                return Ok(camera);
            }
            Err(error) => last = anyhow!("{error}"),
        }
    }
    Err(last)
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
    let index = choice
        .and_then(|name| {
            nokhwa::query(nokhwa::utils::ApiBackend::Auto)
                .ok()?
                .into_iter()
                .find(|c| c.human_name() == name)
                .map(|c| c.index().clone())
        })
        .unwrap_or(CameraIndex::Index(0));
    let mut camera = open(&index)?;
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
