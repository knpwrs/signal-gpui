//! Signal Desktop's ringtone (`sounds/ringtone_minimal.ogg`, AGPL-3.0-only
//! like this app; converted to AAC, which macOS plays), looped while a call
//! rings.

/// The ringtone, playing until dropped.
pub struct Ringtone {
    #[cfg(target_os = "macos")]
    sound: objc2::rc::Retained<objc2_app_kit::NSSound>,
}

const RINGTONE: &[u8] = include_bytes!("../assets/sounds/ringtone.m4a");

impl Ringtone {
    /// Starts ringing; `None` where sound isn't available.
    pub fn start() -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            use objc2::AnyThread as _;
            use objc2_app_kit::NSSound;
            use objc2_foundation::NSData;
            let data = NSData::with_bytes(RINGTONE);
            let sound = NSSound::initWithData(NSSound::alloc(), &data)?;
            sound.setLoops(true);
            sound.play();
            Some(Self { sound })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = RINGTONE;
            None
        }
    }
}

impl Drop for Ringtone {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        self.sound.stop();
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn macos_can_play_the_ringtone() {
        use objc2::AnyThread as _;
        use objc2_app_kit::NSSound;
        let data = objc2_foundation::NSData::with_bytes(super::RINGTONE);
        let sound = NSSound::initWithData(NSSound::alloc(), &data).expect("decodable");
        assert!(sound.duration() > 7.0, "{}", sound.duration());
    }
}
