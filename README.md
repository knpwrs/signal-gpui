# Ziggy

An unofficial native Signal client for macOS, built with Rust and [GPUI](https://gpui-kit.com). It links to your phone as a secondary device, the way Signal Desktop does. It was called Signal GPUI until October 2026; the first launch under the new name moves that app's data over.

![Ziggy in demo mode: the chat list and a group chat](docs/screenshot.png)

## A fraction of the memory

Signal Desktop is an Electron app: a main process, a renderer and four helpers. Ziggy is one native process. In an Activity Monitor spot check (Memory column, same account, both apps open), Ziggy used 121 MB, about 12% of the 1,038 MB that Signal Desktop's six processes used.

It stays small as you use it:

- The timeline and chat list only lay out and draw the rows on screen.
- A chat loads 60 messages at a time as you scroll back, and drops them when you switch chats.
- Decoded photos live in a per-chat cache with a 256 MB budget, freed when you switch. GPUI's default would keep every decoded image until the app quits.
- Looping GIFs share a 256 MB budget of their own.

## Feature parity with Signal Desktop

The goal is to do what Signal Desktop does, the way it does it. When behavior is in question, the answer comes from reading Desktop's source, and the code cites the file it follows. That covers the everyday things (messages, replies, reactions, edits, deletes, mentions, formatting, stickers, GIFs, link previews, voice messages, disappearing messages, pins, groups) and the less visible ones:

- **Voice and video calls**, 1:1 and in groups, with screen sharing, through RingRTC, the calling library Desktop uses.
- **Stories**: viewing, posting text, photo and video stories, replies, and story privacy from your phone.
- **Message history when linking**: like Desktop's "link and sync", your phone sends your history, which is downloaded, decrypted and imported.
- **Sync with your other devices**: pins, archived, muted and unread chats, blocks, hidden stories and settings go through Signal's Storage Service, editing only the fields that changed and keeping the ones this client doesn't know.

Where Signal's server turns features on or sets limits through remote config, this client reads the same config and follows it as Desktop 8.34 would. `todo.md` lists what's left.

## Beyond Signal Desktop

- **Lossless image optimization.** Signal Desktop redraws every image except GIFs, and past 0.2 MiB sends it as a JPEG at quality 0.7 scaled to at most 1600 pixels (`ts/util/scaleImageToLevel`). Here, images are shrunk without changing a pixel: PNG with [oxipng](https://github.com/shssoichiro/oxipng) at its maximum preset, JPEG with mozjpeg's lossless transcode, location metadata removed, colour profile and rotation kept. TIFF and BMP, which macOS often puts on the clipboard, become PNG losslessly.
- **Reply from a notification.** Notifications have a reply field and Mark as Read. Desktop's only open the chat.
- **Emoji search that tolerates typos.** An [`fst`](https://docs.rs/fst) index over names, keywords and shortcodes answers in 2 to 45 µs, matches words in any order, allows one typo (`smlie`), and ranks by your own use.
- **Developer conveniences.** An offline demo mode, separate profiles, and opening without taking focus (see Running).

## How it's built

| Crate | Role |
| --- | --- |
| `crates/app` | The GPUI interface |
| `crates/backend` | Signal networking on its own Tokio thread, through [presage](https://github.com/whisperfish/presage), libsignal-service-rs and [libsignal](https://github.com/signalapp/libsignal). The interface talks to it with commands and events |
| `crates/store` | The app's SQLCipher database: chats, messages, full-text search, stories, settings |
| `crates/calls` | Calls, on RingRTC with Signal's prebuilt WebRTC |
| `crates/emoji` | The emoji table and search index, generated at build time from emojibase |
| `vendor/` | presage and libsignal-service-rs with the changes this app needs, such as receiving and sending stories (`vendor/README.md`) |

## Running

Building needs `cmake` (BoringSSL, for contact discovery) and `protoc` (RingRTC and libsignal's `spqr`), and GIFs and video posters need `ffmpeg`: `brew install cmake ffmpeg`, then `mise install` for the `protoc` pinned in `mise.toml`. mise's `protoc` is the upstream release binary; Homebrew's links Homebrew's abseil and stops starting when abseil upgrades ahead of it. The first build downloads Signal's prebuilt WebRTC (about 28 MB) into `target/webrtc`.

```sh
cargo run --release                                              # your account: scan the QR code with Signal on your phone
SIGNAL_GPUI_DATA_DIR=/tmp/sg cargo run                           # a separate profile, its key in a file rather than the Keychain
SIGNAL_GPUI_DEMO=1 SIGNAL_GPUI_DATA_DIR=/tmp/sg-demo cargo run   # made-up demo data; never connects to Signal
SIGNAL_GPUI_BACKGROUND=1 …                                       # open without taking keyboard focus
```

The database key lives in the login keychain, and Keychain access depends on the app's code signature: unsigned builds ask for your password after every rebuild. A free "Apple Development" certificate from Xcode stops that, and `scripts/bundle.sh` signs with it. [docs/signing.md](docs/signing.md) has the steps.

macOS only shows notifications, and only grants camera and microphone access, to an app bundle. `cargo run` runs the app from `target/<profile>/Ziggy.app`; for a build you start yourself, `scripts/bundle.sh` makes the bundle. `mise run bundle` does both: it builds release and makes `target/release/Ziggy.app`.

GIF search needs your own Giphy API key (free at developers.giphy.com), pasted in the GIFs tab or set as `GIPHY_API_KEY`. Signal Desktop's key belongs to Signal, so it isn't used here.

## Tests

```sh
cargo test --workspace        # store, mapping and backend tests, and headless GPUI tests against the demo
cargo bench -p signal-emoji
```

## License

AGPL-3.0-only (see `LICENSE`), as libsignal, libsignal-service-rs and presage require. Third-party notices are in `NOTICE`. This is an unofficial client, not affiliated with Signal.
