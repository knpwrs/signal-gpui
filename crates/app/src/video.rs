//! Looping silent video (Signal's GIFs are MP4s) decoded into an animated
//! image with the `ffmpeg` command-line tool. Frames are scaled to display
//! size and capped in rate and length, so memory stays modest.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, bail};
use gpui_kit::RenderImage;

/// GIFs are short; this bounds memory for unusually long ones.
const MAX_SECONDS: u32 = 15;
const MAX_FPS: f32 = 30.;

/// `ffmpeg`/`ffprobe`, also where Homebrew puts them (an app started from
/// Finder doesn't get the shell's PATH).
fn tool(name: &str) -> PathBuf {
    ["/opt/homebrew/bin", "/usr/local/bin"]
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Width, height and frame rate of the first video stream.
fn probe(path: &Path) -> anyhow::Result<(u32, u32, f32)> {
    let output = Command::new(tool("ffprobe"))
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height,avg_frame_rate", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .context("running ffprobe (install ffmpeg: brew install ffmpeg)")?;
    if !output.status.success() {
        bail!("ffprobe failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    parse_probe(&String::from_utf8_lossy(&output.stdout))
}

fn parse_probe(text: &str) -> anyhow::Result<(u32, u32, f32)> {
    let mut parts = text.trim().split(',');
    let width: u32 = parts.next().context("no width")?.trim().parse()?;
    let height: u32 = parts.next().context("no height")?.trim().parse()?;
    let rate = parts.next().unwrap_or("25/1").trim();
    let fps = match rate.split_once('/') {
        Some((n, d)) => n.parse::<f32>().unwrap_or(25.) / d.parse::<f32>().unwrap_or(1.).max(1.),
        None => rate.parse().unwrap_or(25.),
    };
    if width == 0 || height == 0 {
        bail!("video has no size");
    }
    Ok((width, height, if fps.is_finite() && fps > 0. { fps } else { 25. }))
}

/// Tallest output: a very tall video still fits a sane buffer.
const MAX_HEIGHT: u32 = 1200;

/// Output size: at most `max_width` wide and `MAX_HEIGHT` tall, keeping the
/// aspect ratio, even dimensions (as encoders like).
fn output_size(width: u32, height: u32, max_width: u32) -> (u32, u32) {
    let scale = (max_width as f32 / width as f32).min(MAX_HEIGHT as f32 / height as f32).min(1.);
    let w = ((width as f32 * scale).round() as u32).max(2) & !1;
    let h = ((height as f32 * scale).round() as u32).max(2) & !1;
    (w, h)
}

/// Decodes the video at `path` into an animated image `max_width` wide at
/// most. Blocking: run it off the UI thread.
pub fn decode_loop(path: &Path, max_width: u32) -> anyhow::Result<RenderImage> {
    let (width, height, fps) = probe(path)?;
    let (w, h) = output_size(width, height, max_width);
    let fps = fps.min(MAX_FPS);
    let mut child = Command::new(tool("ffmpeg"))
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-t", &MAX_SECONDS.to_string(), "-an"])
        .args(["-vf", &format!("fps={fps},scale={w}:{h}")])
        // gpui's images are BGRA, so ask for that directly.
        .args(["-f", "rawvideo", "-pix_fmt", "bgra", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running ffmpeg (install ffmpeg: brew install ffmpeg)")?;
    let mut raw = Vec::new();
    // No more than the frames we asked for, whatever ffmpeg sends.
    let max_bytes = (w * h * 4) as u64 * (MAX_SECONDS as f32 * fps).ceil() as u64;
    let mut stdout = child.stdout.take().context("no ffmpeg output")?;
    let read = (&mut stdout).take(max_bytes).read_to_end(&mut raw);
    drop(stdout);
    let capped = raw.len() as u64 >= max_bytes;
    if capped || read.is_err() {
        // It may still be writing; never wait on a blocked pipe.
        let _ = child.kill();
    }
    let status = child.wait()?;
    read?;
    // A decode that failed partway isn't a loop to show (a capped one, which
    // we stopped, is).
    if !capped && !status.success() {
        bail!("ffmpeg failed: {status}");
    }

    let frame_bytes = (w * h * 4) as usize;
    // The exact rate ffmpeg resampled to (29.97 isn't 30).
    let delay = image::Delay::from_numer_denom_ms((1_000_000. / fps.max(1.)) as u32, 1000);
    let frames: Vec<image::Frame> = raw
        .chunks_exact(frame_bytes)
        .filter_map(|chunk| image::RgbaImage::from_raw(w, h, chunk.to_vec()))
        .map(|buffer| image::Frame::from_parts(buffer, 0, 0, delay))
        .collect();
    if frames.is_empty() {
        bail!("no frames decoded");
    }
    Ok(RenderImage::new(frames))
}

/// A still of the video's first frame (at most `MAX_POSTER_WIDTH` wide),
/// saved beside it, for showing it in a message. Made once; blocking.
pub fn poster(path: &Path) -> anyhow::Result<PathBuf> {
    let out = path.with_extension("poster.png");
    if out.exists() {
        return Ok(out);
    }
    // Made under another name, and renamed once whole: a run that dies
    // partway leaves no poster to be reused.
    let partial = path.with_extension("poster.partial.png");
    let made = Command::new(tool("ffmpeg"))
        .args(["-v", "error", "-y", "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-vf", &format!("scale='min({MAX_POSTER_WIDTH},iw)':-2")])
        .arg(&partial)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running ffmpeg (install ffmpeg: brew install ffmpeg)")?;
    if !made.success() || !partial.exists() {
        let _ = std::fs::remove_file(&partial);
        bail!("ffmpeg made no still: {made}");
    }
    std::fs::rename(&partial, &out)?;
    Ok(out)
}

/// A poster is shown at most this wide (Desktop's media width, at 2×).
const MAX_POSTER_WIDTH: u32 = 600;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_output_and_sizes() {
        assert_eq!(parse_probe("480,270,30000/1001\n").unwrap().0, 480);
        assert!((parse_probe("480,270,30000/1001").unwrap().2 - 29.97).abs() < 0.01);
        assert_eq!(parse_probe("100,50,0/0").unwrap().2, 25.);
        assert!(parse_probe("").is_err());
        assert_eq!(output_size(480, 270, 300), (300, 168));
        assert_eq!(output_size(201, 101, 400), (200, 100));
        assert_eq!(output_size(2, 20_000, 300), (2, 1200), "tall videos are capped");
    }

    /// Needs ffmpeg installed.
    #[test]
    fn decodes_a_generated_clip() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mp4");
        let made = Command::new(tool("ffmpeg"))
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=size=320x180:rate=10:duration=1", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("ffmpeg not available; skipping");
            return;
        }
        let image = decode_loop(&clip, 160).unwrap();
        assert_eq!(image.frame_count(), 10);
        assert_eq!((image.size(0).width.0, image.size(0).height.0), (160, 90));
    }

    /// Needs ffmpeg installed.
    #[test]
    fn a_video_gets_a_poster_once() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("wide.mp4");
        let made = Command::new(tool("ffmpeg"))
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=size=1280x720:rate=10:duration=1", "-pix_fmt", "yuv420p"])
            .arg(&clip)
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("ffmpeg not available; skipping");
            return;
        }
        let poster = poster(&clip).unwrap();
        assert_eq!(poster, dir.path().join("wide.poster.png"));
        let size = image::image_dimensions(&poster).unwrap();
        assert_eq!(size, (600, 338), "at most 600 wide, its shape kept");
        let made_at = std::fs::metadata(&poster).unwrap().modified().unwrap();
        assert_eq!(super::poster(&clip).unwrap(), poster);
        assert_eq!(std::fs::metadata(&poster).unwrap().modified().unwrap(), made_at, "made once");
    }
}
