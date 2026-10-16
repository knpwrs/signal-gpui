//! Audio attachments and voice messages, played as Signal Desktop plays them
//! (`MessageAudio`): one at a time, at 1×, 1.5×, 2× or 0.5×, drawn as a
//! waveform of 47 bars. AVFoundation decodes AAC (what phones send), MP3
//! (what Desktop sends), WAV and the rest macOS knows.

use std::path::Path;

/// Bars in a waveform (Desktop's `BAR_COUNT`).
pub const BARS: usize = 47;

/// Desktop's playback rates, in the order its button steps through them.
pub const RATES: [f32; 4] = [1., 1.5, 2., 0.5];

/// The rate after `rate` (Desktop's `PlaybackRateButton.nextPlaybackRate`).
pub fn next_rate(rate: f32) -> f32 {
    let at = RATES.iter().position(|r| (r - rate).abs() < 0.01).unwrap_or(0);
    RATES[(at + 1) % RATES.len()]
}

/// "1×", "1.5×": the rate button's label.
pub fn rate_label(rate: f32) -> String {
    let text = format!("{rate}");
    format!("{text}×")
}

/// The loudest sample in each of `bars` equal stretches of `samples`, as a
/// share of the loudest overall (Desktop's peaks, normalised).
pub fn peaks(samples: impl IntoIterator<Item = f32>, total: usize, bars: usize) -> Vec<f32> {
    let mut peaks = vec![0f32; bars];
    if total == 0 || bars == 0 {
        return peaks;
    }
    for (ix, sample) in samples.into_iter().enumerate().take(total) {
        let bar = ix * bars / total;
        peaks[bar] = peaks[bar].max(sample.abs());
    }
    let loudest = peaks.iter().copied().fold(0f32, f32::max);
    if loudest > 0. {
        for peak in &mut peaks {
            *peak /= loudest;
        }
    }
    peaks
}

/// Something playing an audio file: [`Player`], or in tests a silent
/// stand-in (tests never play sound).
pub trait Playback {
    fn play(&self);
    fn pause(&self);
    fn is_playing(&self) -> bool;
    /// Where it is, in seconds.
    fn position(&self) -> f64;
    fn duration(&self) -> f64;
    fn seek(&self, seconds: f64);
    fn set_rate(&self, rate: f32);
}

/// Opens a file to play.
pub type Opener = fn(&Path) -> anyhow::Result<Box<dyn Playback>>;

/// The real player.
pub fn open(path: &Path) -> anyhow::Result<Box<dyn Playback>> {
    Ok(Box::new(Player::open(path)?))
}

impl Playback for Player {
    fn play(&self) {
        Player::play(self)
    }
    fn pause(&self) {
        Player::pause(self)
    }
    fn is_playing(&self) -> bool {
        Player::is_playing(self)
    }
    fn position(&self) -> f64 {
        Player::position(self)
    }
    fn duration(&self) -> f64 {
        Player::duration(self)
    }
    fn seek(&self, seconds: f64) {
        Player::seek(self, seconds)
    }
    fn set_rate(&self, rate: f32) {
        Player::set_rate(self, rate)
    }
}

/// A silent player for tests: it "plays" for a moment of real time, at its
/// last second, then stops and rewinds as `AVAudioPlayer` does at the end.
#[cfg(test)]
pub mod silent {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    #[derive(Default)]
    pub struct Silent {
        started: Cell<Option<Instant>>,
    }

    pub const LENGTH: f64 = 2.;
    /// How long it plays, in real time.
    const PLAYS_FOR: Duration = Duration::from_millis(300);

    pub fn open(_: &std::path::Path) -> anyhow::Result<Box<dyn super::Playback>> {
        Ok(Box::new(Silent::default()))
    }

    impl super::Playback for Silent {
        fn play(&self) {
            if self.started.get().is_none() {
                self.started.set(Some(Instant::now()));
            }
        }
        fn pause(&self) {
            self.started.set(None);
        }
        fn is_playing(&self) -> bool {
            self.started.get().is_some_and(|at| at.elapsed() < PLAYS_FOR)
        }
        fn position(&self) -> f64 {
            if self.is_playing() { LENGTH - 0.1 } else { 0. }
        }
        fn duration(&self) -> f64 {
            LENGTH
        }
        fn seek(&self, _: f64) {}
        fn set_rate(&self, _: f32) {}
    }
}

/// An audio file being played (or paused); dropping it stops it.
pub struct Player {
    #[cfg(target_os = "macos")]
    player: objc2::rc::Retained<objc2_avf_audio::AVAudioPlayer>,
}

impl Player {
    /// Opens `path`, ready to play.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        #[cfg(target_os = "macos")]
        {
            use objc2::AnyThread as _;
            use objc2_avf_audio::AVAudioPlayer;
            let url = file_url(path)?;
            let player = unsafe { AVAudioPlayer::initWithContentsOfURL_error(AVAudioPlayer::alloc(), &url) }
                .map_err(|e| anyhow::anyhow!("{}", e.localizedDescription()))?;
            unsafe {
                player.setEnableRate(true);
                player.prepareToPlay();
            }
            Ok(Self { player })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = path;
            anyhow::bail!("audio playback isn't available here")
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
        return unsafe { self.player.isPlaying() };
        #[cfg(not(target_os = "macos"))]
        false
    }

    /// Where it is, in seconds.
    pub fn position(&self) -> f64 {
        #[cfg(target_os = "macos")]
        return unsafe { self.player.currentTime() };
        #[cfg(not(target_os = "macos"))]
        0.
    }

    pub fn duration(&self) -> f64 {
        #[cfg(target_os = "macos")]
        return unsafe { self.player.duration() };
        #[cfg(not(target_os = "macos"))]
        0.
    }

    pub fn seek(&self, seconds: f64) {
        #[cfg(target_os = "macos")]
        unsafe {
            self.player.setCurrentTime(seconds.clamp(0., self.duration()));
        }
        #[cfg(not(target_os = "macos"))]
        let _ = seconds;
    }

    pub fn set_rate(&self, rate: f32) {
        #[cfg(target_os = "macos")]
        unsafe {
            self.player.setRate(rate);
        }
        #[cfg(not(target_os = "macos"))]
        let _ = rate;
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        unsafe {
            self.player.stop();
        }
    }
}

#[cfg(target_os = "macos")]
fn file_url(path: &Path) -> anyhow::Result<objc2::rc::Retained<objc2_foundation::NSURL>> {
    let path = path.to_str().ok_or_else(|| anyhow::anyhow!("not a UTF-8 path"))?;
    Ok(objc2_foundation::NSURL::fileURLWithPath(&objc2_foundation::NSString::from_str(path)))
}

/// The file's waveform: [`BARS`] peaks from 0 to 1, and its length in
/// seconds. Decoding takes a moment, so this belongs off the main thread.
pub fn waveform(path: &Path) -> anyhow::Result<(Vec<f32>, f64)> {
    #[cfg(target_os = "macos")]
    {
        use objc2::AnyThread as _;
        use objc2_avf_audio::{AVAudioFile, AVAudioPCMBuffer};
        let url = file_url(path)?;
        let file = unsafe { AVAudioFile::initForReading_error(AVAudioFile::alloc(), &url) }
            .map_err(|e| anyhow::anyhow!("{}", e.localizedDescription()))?;
        let format = unsafe { file.processingFormat() };
        let total = unsafe { file.length() }.max(0) as usize;
        let rate = unsafe { format.sampleRate() };
        const CHUNK: u32 = 1 << 16;
        let buffer = unsafe { AVAudioPCMBuffer::initWithPCMFormat_frameCapacity(AVAudioPCMBuffer::alloc(), &format, CHUNK) }
            .ok_or_else(|| anyhow::anyhow!("no buffer for that format"))?;
        let mut samples = Vec::with_capacity(total);
        // (Reading at the end fails without saying why: stop before it.)
        while (unsafe { file.framePosition() }.max(0) as usize) < total {
            unsafe { file.readIntoBuffer_error(&buffer) }.map_err(|e| anyhow::anyhow!("{}", e.localizedDescription()))?;
            let frames = unsafe { buffer.frameLength() } as usize;
            if frames == 0 {
                break;
            }
            // The first channel is enough for the shape (Desktop mixes
            // down; the peaks come out much the same).
            let channels = unsafe { buffer.floatChannelData() };
            if channels.is_null() {
                anyhow::bail!("not floating-point audio");
            }
            let first = unsafe { *channels };
            let data = unsafe { std::slice::from_raw_parts(first.as_ptr(), frames) };
            samples.extend_from_slice(data);
        }
        let total = samples.len();
        Ok((peaks(samples, total, BARS), if rate > 0. { total as f64 / rate } else { 0. }))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        anyhow::bail!("audio isn't available here")
    }
}

/// "0:07", "1:05": a time as Desktop shows it beside the waveform.
pub fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.).round() as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_step_as_desktop_does() {
        assert_eq!(next_rate(1.), 1.5);
        assert_eq!(next_rate(2.), 0.5);
        assert_eq!(next_rate(0.5), 1.);
        assert_eq!((rate_label(1.5), rate_label(1.)), ("1.5×".into(), "1×".into()));
        assert_eq!((clock(7.4), clock(65.)), ("0:07".into(), "1:05".into()));
    }

    #[test]
    fn peaks_are_the_loudest_per_bar_normalised() {
        // Quiet, then loud, then nothing.
        let samples = [0.1, -0.2, 0.8, -0.4, 0., 0.];
        assert_eq!(peaks(samples, 6, 3), vec![0.25, 1., 0.]);
        assert_eq!(peaks([0f32; 4], 4, 2), vec![0., 0.], "silence stays flat");
        assert_eq!(peaks([], 0, 3), vec![0., 0., 0.]);
    }

    /// Decodes a generated WAV (no sound is played).
    #[cfg(target_os = "macos")]
    #[test]
    fn waveforms_come_from_the_decoded_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        // A second of 8 kHz mono 16-bit: silent, then a loud half.
        let rate = 8000u32;
        let samples: Vec<i16> = (0..rate).map(|i| if i < rate / 2 { 0 } else { if i % 2 == 0 { 20000 } else { -20000 } }).collect();
        let mut wav = Vec::new();
        let data_len = samples.len() as u32 * 2;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&rate.to_le_bytes());
        wav.extend_from_slice(&(rate * 2).to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            wav.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(&path, wav).unwrap();
        let (peaks, seconds) = waveform(&path).unwrap();
        assert_eq!(peaks.len(), BARS);
        assert!((seconds - 1.).abs() < 0.01, "{seconds}");
        assert_eq!(peaks[0], 0., "the silent half");
        assert_eq!(peaks[BARS - 1], 1., "the loud half");
    }
}
