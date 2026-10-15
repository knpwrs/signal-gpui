//! Playing a video attachment, with its sound, as Desktop's lightbox does:
//! AVFoundation plays it, and its frames are read out to draw in the window.

use std::path::Path;

/// One frame: BGRA rows, `width * 4` bytes each.
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

/// A video being played (or paused); dropping it stops it.
pub struct VideoPlayer {
    #[cfg(target_os = "macos")]
    player: objc2::rc::Retained<objc2_av_foundation::AVPlayer>,
    #[cfg(target_os = "macos")]
    item: objc2::rc::Retained<objc2_av_foundation::AVPlayerItem>,
    #[cfg(target_os = "macos")]
    output: objc2::rc::Retained<objc2_av_foundation::AVPlayerItemVideoOutput>,
}

impl VideoPlayer {
    /// Opens `path`, paused at the start. Must be called on the main thread.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        #[cfg(target_os = "macos")]
        {
            use objc2::AnyThread as _;
            use objc2::rc::Retained;
            use objc2_av_foundation::{AVPlayer, AVPlayerItem, AVPlayerItemVideoOutput};
            use objc2_foundation::{MainThreadMarker, NSDictionary, NSNumber, NSString, NSURL};
            let mtm = MainThreadMarker::new().ok_or_else(|| anyhow::anyhow!("not on the main thread"))?;
            let path = path.to_str().ok_or_else(|| anyhow::anyhow!("not a UTF-8 path"))?;
            let url = NSURL::fileURLWithPath(&NSString::from_str(path));
            let item = unsafe { AVPlayerItem::playerItemWithURL(&url, mtm) };
            // Frames as BGRA, which is what gpui draws.
            let key: &NSString = unsafe {
                let key: &objc2_core_foundation::CFString = objc2_core_video::kCVPixelBufferPixelFormatTypeKey;
                &*(key as *const objc2_core_foundation::CFString).cast::<NSString>()
            };
            let format = NSNumber::new_u32(objc2_core_video::kCVPixelFormatType_32BGRA);
            let attributes: Retained<NSDictionary<NSString, objc2::runtime::AnyObject>> =
                NSDictionary::from_slices(&[key], &[&*format as &objc2::runtime::AnyObject]);
            let output = unsafe {
                AVPlayerItemVideoOutput::initWithPixelBufferAttributes(AVPlayerItemVideoOutput::alloc(), Some(&attributes))
            };
            unsafe { item.addOutput(&output) };
            let player = unsafe { AVPlayer::playerWithPlayerItem(Some(&item), mtm) };
            Ok(Self { player, item, output })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = path;
            anyhow::bail!("video playback isn't available here")
        }
    }

    pub fn play(&self) {
        #[cfg(target_os = "macos")]
        unsafe {
            self.player.play();
        }
    }

    pub fn pause(&self) {
        #[cfg(target_os = "macos")]
        unsafe {
            self.player.pause();
        }
    }

    pub fn is_playing(&self) -> bool {
        #[cfg(target_os = "macos")]
        return unsafe { self.player.rate() } > 0.;
        #[cfg(not(target_os = "macos"))]
        false
    }

    /// Where it is, in seconds.
    pub fn position(&self) -> f64 {
        #[cfg(target_os = "macos")]
        return finite(unsafe { self.player.currentTime().seconds() });
        #[cfg(not(target_os = "macos"))]
        0.
    }

    /// Its length in seconds (0 until it's loaded).
    pub fn duration(&self) -> f64 {
        #[cfg(target_os = "macos")]
        return finite(unsafe { self.item.duration().seconds() });
        #[cfg(not(target_os = "macos"))]
        0.
    }

    pub fn seek(&self, seconds: f64) {
        #[cfg(target_os = "macos")]
        unsafe {
            let time = objc2_core_media::CMTime::with_seconds(seconds.max(0.), 600);
            self.player.seekToTime(time);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = seconds;
    }

    /// The frame to show now, if it changed since the last one.
    pub fn frame(&self) -> Option<VideoFrame> {
        #[cfg(target_os = "macos")]
        {
            use objc2_core_video::{
                CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
                CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
                CVPixelBufferUnlockBaseAddress,
            };
            let time = unsafe { self.player.currentTime() };
            if !unsafe { self.output.hasNewPixelBufferForItemTime(time) } {
                return None;
            }
            let buffer =
                unsafe { self.output.copyPixelBufferForItemTime_itemTimeForDisplay(time, std::ptr::null_mut()) }?;
            if unsafe { CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly) } != 0 {
                return None;
            }
            let (width, height) = (CVPixelBufferGetWidth(&buffer), CVPixelBufferGetHeight(&buffer));
            let stride = CVPixelBufferGetBytesPerRow(&buffer);
            let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
            let frame = (!base.is_null() && stride >= width * 4).then(|| {
                // Rows without their padding.
                let mut bgra = Vec::with_capacity(width * height * 4);
                for row in 0..height {
                    let line = unsafe { std::slice::from_raw_parts(base.add(row * stride), width * 4) };
                    bgra.extend_from_slice(line);
                }
                VideoFrame { width: width as u32, height: height as u32, bgra }
            });
            unsafe { CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags::ReadOnly) };
            frame
        }
        #[cfg(not(target_os = "macos"))]
        None
    }
}

#[cfg(target_os = "macos")]
fn finite(seconds: f64) -> f64 {
    if seconds.is_finite() { seconds.max(0.) } else { 0. }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        unsafe {
            self.player.pause();
        }
    }
}
