//! Screen sharing (Desktop's "Present"): a display or window, through
//! ScreenCaptureKit, sent in place of the camera. macOS asks for screen
//! recording access the first time.

use ringrtc::webrtc::media::{VideoFrame, VideoPixelFormat, VideoSource};

use crate::FrameSlot;

/// Something to share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareSource {
    /// `display:<id>` or `window:<id>`.
    pub id: String,
    pub name: String,
    pub is_display: bool,
}

/// How wide a shared frame may be (taller screens keep their shape).
const MAX_WIDTH: u32 = 1920;
/// Frames per second; screens change slowly, as Desktop assumes.
const FPS: i32 = 15;

#[cfg(target_os = "macos")]
mod imp {
    use screencapturekit::cm::CMSampleBufferExt as _;
    use screencapturekit::cv::CVPixelBufferLockFlags;
    use screencapturekit::prelude::*;

    use super::*;

    pub fn sources() -> anyhow::Result<Vec<ShareSource>> {
        let content = SCShareableContent::get().map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let displays = content.displays();
        let mut out: Vec<ShareSource> = displays
            .iter()
            .enumerate()
            .map(|(ix, d)| ShareSource {
                id: format!("display:{}", d.display_id()),
                name: if displays.len() == 1 { "Entire screen".into() } else { format!("Screen {}", ix + 1) },
                is_display: true,
            })
            .collect();
        for window in content.windows() {
            let title = window.title().unwrap_or_default();
            let app = window.owning_application().map(|a| a.application_name()).unwrap_or_default();
            // Only real windows: on screen, titled, not tiny.
            if !window.is_on_screen() || title.is_empty() || window.frame().size.width < 100. {
                continue;
            }
            let name = if app.is_empty() { title } else { format!("{app}: {title}") };
            out.push(ShareSource { id: format!("window:{}", window.window_id()), name, is_display: false });
        }
        Ok(out)
    }

    struct Output {
        source: VideoSource,
        preview: FrameSlot,
    }

    impl SCStreamOutputTrait for Output {
        fn did_output_sample_buffer(&self, sample: CMSampleBuffer, kind: SCStreamOutputType) {
            if !matches!(kind, SCStreamOutputType::Screen) {
                return;
            }
            let Some(buffer) = sample.pixel_buffer() else { return };
            let Ok(guard) = buffer.lock(CVPixelBufferLockFlags::READ_ONLY) else { return };
            let (width, height, stride) = (guard.width(), guard.height(), guard.bytes_per_row());
            let Some(bgra) = (unsafe { guard.as_slice() }) else { return };
            // Rows without their padding, as RGBA (what RingRTC takes).
            let mut rgba = Vec::with_capacity(width * height * 4);
            for row in bgra.chunks(stride).take(height) {
                for px in row[..width * 4].as_chunks::<4>().0 {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
            }
            self.source.push_frame(VideoFrame::copy_from_slice(
                width as u32,
                height as u32,
                VideoPixelFormat::Rgba,
                &rgba,
            ));
            self.preview.put_rgba(width as u32, height as u32, &rgba, false);
        }
    }

    /// A running share; dropping it stops it.
    pub struct Share {
        stream: SCStream,
    }

    impl Share {
        pub fn start(id: &str, source: VideoSource, preview: FrameSlot) -> anyhow::Result<Self> {
            let content = SCShareableContent::get().map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let (filter, width, height) = match id.split_once(':') {
                Some(("display", id)) => {
                    let display = content
                        .displays()
                        .into_iter()
                        .find(|d| d.display_id().to_string() == id)
                        .ok_or_else(|| anyhow::anyhow!("that screen is gone"))?;
                    let filter = SCContentFilter::create().with_display(&display).with_excluding_windows(&[]).build()?;
                    (filter, display.width(), display.height())
                }
                Some(("window", id)) => {
                    let window = content
                        .windows()
                        .into_iter()
                        .find(|w| w.window_id().to_string() == id)
                        .ok_or_else(|| anyhow::anyhow!("that window is gone"))?;
                    let frame = window.frame();
                    let filter = SCContentFilter::create().with_window(&window).build()?;
                    (filter, frame.size.width as u32, frame.size.height as u32)
                }
                _ => anyhow::bail!("unknown source {id}"),
            };
            let (width, height) = fit(width, height);
            let config = SCStreamConfiguration::new()
                .with_width(width)
                .with_height(height)
                .with_pixel_format(PixelFormat::BGRA)
                .with_shows_cursor(true)
                .with_minimum_frame_interval(&CMTime::new(1, FPS));
            let mut stream = SCStream::new(&filter, &config)?;
            stream.add_output_handler(Output { source, preview }, SCStreamOutputType::Screen)?;
            stream.start_capture()?;
            Ok(Self { stream })
        }
    }

    impl Drop for Share {
        fn drop(&mut self) {
            let _ = self.stream.stop_capture();
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;

    pub fn sources() -> anyhow::Result<Vec<ShareSource>> {
        Ok(Vec::new())
    }

    pub struct Share;

    impl Share {
        pub fn start(_: &str, _: VideoSource, _: FrameSlot) -> anyhow::Result<Self> {
            anyhow::bail!("screen sharing isn't available here")
        }
    }
}

pub use imp::{Share, sources};

/// At most `MAX_WIDTH` wide, keeping the shape; even sizes, as encoders like.
fn fit(width: u32, height: u32) -> (u32, u32) {
    let scale = (MAX_WIDTH as f64 / width.max(1) as f64).min(1.);
    let even = |v: f64| ((v as u32) & !1).max(2);
    (even(width as f64 * scale), even(height as f64 * scale))
}

#[cfg(test)]
mod tests {
    #[test]
    fn shares_fit_full_hd_width() {
        assert_eq!(super::fit(3840, 2160), (1920, 1080));
        assert_eq!(super::fit(1280, 801), (1280, 800));
        assert_eq!(super::fit(1, 1), (2, 2));
    }
}
