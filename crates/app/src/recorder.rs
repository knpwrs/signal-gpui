//! Recording a voice message, as Signal Desktop does (`AudioRecorder`, the
//! `audioRecorder` duck): from the microphone to a file, with the level
//! drawn as it goes, for up to an hour. Desktop encodes MP3; this records
//! AAC in an M4A file, which is what Signal's phones send.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// Desktop's limit (`CompositionRecording`: `DurationInSeconds.HOUR`).
pub const MAX_SECONDS: f64 = 60. * 60.;

/// Levels kept for the bars (Desktop's `MAX_PEAKS`).
pub const MAX_LEVELS: usize = 400;

/// Something recording: the microphone, or a stand-in in tests.
pub trait Recorder {
    /// Seconds recorded so far.
    fn elapsed(&self) -> f64;
    /// How loud it is now, from 0 to 1.
    fn level(&mut self) -> f32;
    /// Stops recording, leaving the file complete.
    fn stop(&mut self);
}

/// Starts recording into `path`.
pub type StartRecorder = fn(&Path) -> anyhow::Result<Box<dyn Recorder>>;

/// A recording in the composer: where it goes, its recorder, and the
/// levels so far (newest last).
pub struct Recording {
    pub path: PathBuf,
    recorder: Box<dyn Recorder>,
    pub levels: VecDeque<f32>,
    /// Stopped at the time limit, waiting to be sent or discarded.
    pub stopped: bool,
}

impl Recording {
    pub fn new(path: PathBuf, recorder: Box<dyn Recorder>) -> Self {
        Self { path, recorder, levels: VecDeque::new(), stopped: false }
    }

    pub fn elapsed(&self) -> f64 {
        self.recorder.elapsed()
    }

    /// Takes a level reading. Returns true when the time limit was just
    /// reached, which stops the recording.
    pub fn tick(&mut self) -> bool {
        if self.stopped {
            return false;
        }
        let level = self.recorder.level();
        if self.levels.len() == MAX_LEVELS {
            self.levels.pop_front();
        }
        self.levels.push_back(level);
        if self.recorder.elapsed() >= MAX_SECONDS {
            self.stop();
            return true;
        }
        false
    }

    pub fn stop(&mut self) {
        if !self.stopped {
            self.recorder.stop();
            self.stopped = true;
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A file to record the next voice message into, never one in use.
pub fn new_path() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    std::env::temp_dir().join(format!("ziggy-voice-{}-{millis}-{n}.m4a", std::process::id()))
}

/// Records from the default microphone. macOS asks for permission the first
/// time; if it's refused, recording fails to start.
pub fn start_microphone(path: &Path) -> anyhow::Result<Box<dyn Recorder>> {
    #[cfg(target_os = "macos")]
    {
        Microphone::start(path).map(|m| Box::new(m) as Box<dyn Recorder>)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        anyhow::bail!("recording isn't available here")
    }
}

#[cfg(target_os = "macos")]
struct Microphone {
    recorder: objc2::rc::Retained<objc2_avf_audio::AVAudioRecorder>,
    /// `currentTime` reads 0 once stopped; the length is kept from before.
    stopped_at: Option<f64>,
}

#[cfg(target_os = "macos")]
impl Microphone {
    fn start(path: &Path) -> anyhow::Result<Self> {
        use objc2::AnyThread as _;
        use objc2::runtime::AnyObject;
        use objc2_avf_audio::{AVAudioRecorder, AVEncoderBitRateKey, AVFormatIDKey, AVNumberOfChannelsKey, AVSampleRateKey};
        use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
        let path = path.to_str().ok_or_else(|| anyhow::anyhow!("not a UTF-8 path"))?;
        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        // kAudioFormatMPEG4AAC ('aac '); mono speech at 44.1 kHz, 64 kbit/s.
        const MPEG4_AAC: u32 = u32::from_be_bytes(*b"aac ");
        let keys = unsafe { [AVFormatIDKey, AVSampleRateKey, AVNumberOfChannelsKey, AVEncoderBitRateKey] };
        let keys: Vec<&NSString> = keys.into_iter().collect::<Option<_>>().ok_or_else(|| anyhow::anyhow!("no audio settings keys"))?;
        let values = [NSNumber::new_u32(MPEG4_AAC), NSNumber::new_f64(44_100.), NSNumber::new_u32(1), NSNumber::new_u32(64_000)];
        let values: Vec<&AnyObject> = values.iter().map(|v| &**v as &AnyObject).collect();
        let settings = NSDictionary::from_slices(&keys, &values);
        let recorder = unsafe { AVAudioRecorder::initWithURL_settings_error(AVAudioRecorder::alloc(), &url, &settings) }
            .map_err(|e| anyhow::anyhow!("{}", e.localizedDescription()))?;
        unsafe {
            recorder.setMeteringEnabled(true);
            if !recorder.record() {
                anyhow::bail!("the microphone didn't start (is access to it allowed?)");
            }
        }
        Ok(Self { recorder, stopped_at: None })
    }
}

#[cfg(target_os = "macos")]
impl Recorder for Microphone {
    fn elapsed(&self) -> f64 {
        self.stopped_at.unwrap_or_else(|| unsafe { self.recorder.currentTime() })
    }

    fn level(&mut self) -> f32 {
        unsafe {
            self.recorder.updateMeters();
            // The loudness since the last reading, as Desktop's encoder
            // measures each 100 ms (a mean square, in decibels).
            decibels_to_level(self.recorder.averagePowerForChannel(0))
        }
    }

    fn stop(&mut self) {
        if self.stopped_at.is_none() {
            self.stopped_at = Some(self.elapsed());
            unsafe { self.recorder.stop() };
        }
    }
}

/// Below this, a bar is empty (Desktop's `NOISE_BED`).
const NOISE_BED: f32 = -60.;

/// A meter reading (decibels below full scale, -160 for silence) as a bar's
/// height from 0 to 1: Desktop's `toPeak` (`ts/util/waveformBuilder`),
/// linear in decibels from the noise bed to full scale, so speech fills
/// half the bar or more rather than a few percent.
pub fn decibels_to_level(decibels: f32) -> f32 {
    if decibels.is_finite() { ((decibels - NOISE_BED) / -NOISE_BED).clamp(0., 1.) } else { 0. }
}

#[cfg(test)]
pub mod fake {
    //! A recorder that records nothing: time and levels are set by the test.

    use std::cell::Cell;
    use std::rc::Rc;

    use super::Recorder;

    #[derive(Clone, Default)]
    pub struct FakeMic {
        pub seconds: Rc<Cell<f64>>,
        pub level: Rc<Cell<f32>>,
        pub stopped: Rc<Cell<bool>>,
    }

    impl Recorder for FakeMic {
        fn elapsed(&self) -> f64 {
            self.seconds.get()
        }

        fn level(&mut self) -> f32 {
            self.level.get()
        }

        fn stop(&mut self) {
            self.stopped.set(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeMic;
    use super::*;

    #[test]
    fn levels_are_kept_newest_last_up_to_the_limit() {
        let mic = FakeMic::default();
        let mut recording = Recording::new(PathBuf::from("x.m4a"), Box::new(mic.clone()));
        for i in 0..MAX_LEVELS + 5 {
            mic.level.set(i as f32);
            assert!(!recording.tick());
        }
        assert_eq!(recording.levels.len(), MAX_LEVELS);
        assert_eq!(recording.levels.back(), Some(&((MAX_LEVELS + 4) as f32)));
    }

    #[test]
    fn an_hour_stops_it() {
        let mic = FakeMic::default();
        let mut recording = Recording::new(PathBuf::from("x.m4a"), Box::new(mic.clone()));
        mic.seconds.set(MAX_SECONDS - 1.);
        assert!(!recording.tick());
        mic.seconds.set(MAX_SECONDS);
        assert!(recording.tick(), "the limit");
        assert!(mic.stopped.get() && recording.stopped);
        assert!(!recording.tick(), "only once");
    }

    #[test]
    fn dropping_it_stops_the_microphone() {
        let mic = FakeMic::default();
        drop(Recording::new(PathBuf::from("x.m4a"), Box::new(mic.clone())));
        assert!(mic.stopped.get());
    }

    #[test]
    fn meter_readings_become_levels() {
        assert_eq!(decibels_to_level(0.), 1.);
        assert!((decibels_to_level(-20.) - 2. / 3.).abs() < 1e-6, "speech is most of the bar");
        assert_eq!(decibels_to_level(-60.), 0., "Desktop's noise bed");
        assert_eq!(decibels_to_level(-160.), 0., "silence");
        assert_eq!(decibels_to_level(f32::NEG_INFINITY), 0.);
    }
}
