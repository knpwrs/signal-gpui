//! Signal Desktop's in-chat sounds (`ts/util/Sound.std.ts`): `pop` when a
//! message arrives in the open chat, `whoosh` when we send one. Desktop's
//! `sounds/pop.ogg` and `sounds/whoosh.ogg` (AGPL-3.0-only like this app),
//! converted to AAC, which macOS plays. Tests never play sound.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    Pop,
    Whoosh,
}

const POP: &[u8] = include_bytes!("../assets/sounds/pop.m4a");
const WHOOSH: &[u8] = include_bytes!("../assets/sounds/whoosh.m4a");

impl Sound {
    fn bytes(self) -> &'static [u8] {
        match self {
            Sound::Pop => POP,
            Sound::Whoosh => WHOOSH,
        }
    }
}

/// Plays `sound` once, on the main thread.
pub fn play(sound: Sound) {
    #[cfg(all(target_os = "macos", not(test)))]
    macos::play(sound.bytes());
    #[cfg(any(not(target_os = "macos"), test))]
    let _ = sound.bytes();
}

#[cfg(all(target_os = "macos", not(test)))]
mod macos {
    use std::cell::RefCell;

    use objc2::AnyThread as _;
    use objc2::rc::Retained;
    use objc2_app_kit::NSSound;
    use objc2_foundation::NSData;

    thread_local! {
        /// The sounds playing, kept until they're done.
        static PLAYING: RefCell<Vec<Retained<NSSound>>> = const { RefCell::new(Vec::new()) };
    }

    pub fn play(bytes: &'static [u8]) {
        let data = NSData::with_bytes(bytes);
        let Some(sound) = NSSound::initWithData(NSSound::alloc(), &data) else { return };
        sound.play();
        PLAYING.with(|playing| {
            let mut playing = playing.borrow_mut();
            playing.retain(|s| s.isPlaying());
            playing.push(sound);
        });
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn macos_can_decode_the_sounds() {
        use objc2::AnyThread as _;
        use objc2_app_kit::NSSound;
        for sound in [super::Sound::Pop, super::Sound::Whoosh] {
            let data = objc2_foundation::NSData::with_bytes(sound.bytes());
            let decoded = NSSound::initWithData(NSSound::alloc(), &data).expect("decodable");
            assert!(decoded.duration() > 0.05, "{sound:?}: {}", decoded.duration());
        }
    }
}
