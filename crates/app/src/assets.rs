//! Icons: GPUI Kit's default set plus the Lucide icons this app uses.

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(
    ExtraIcons,
    [
        Album,
        AppWindow,
        Archive,
        ArchiveRestore,
        ArrowDown,
        Ban,
        Bell,
        BellOff,
        ChevronLeft,
        CheckCheck,
        CircleAlert,
        Clock,
        Download,
        Ellipsis,
        EllipsisVertical,
        FaceSlightlySmiling,
        FaceSlightlySmilingPlus,
        File,
        Forward,
        Info,
        RotateCcw,
        Image,
        Lock,
        LogOut,
        Maximize2,
        MessageCircle,
        Mic,
        MicOff,
        Minimize2,
        MessageSquareDot,
        NotebookText,
        Paperclip,
        Pause,
        Phone,
        PhoneIncoming,
        PhoneMissed,
        PhoneOff,
        PhoneOutgoing,
        Pencil,
        Pin,
        PinOff,
        Play,
        Reply,
        ScreenShare,
        ScreenShareOff,
        Send,
        SquareCheck,
        SquarePen,
        Timer,
        Trash,
        User,
        Video,
        VideoOff,
        Users,
        WifiOff,
        X
    ]
);

/// Signal Desktop's own icons (AGPL-3.0-only, like this app), where Lucide
/// has no match: the message status circles.
const SIGNAL_ICONS: &[(&str, &[u8])] = &[
    ("icons/signal/messagestatus-sending.svg", include_bytes!("../assets/icons/signal/messagestatus-sending.svg")),
    ("icons/signal/messagestatus-sent.svg", include_bytes!("../assets/icons/signal/messagestatus-sent.svg")),
    ("icons/signal/messagestatus-delivered.svg", include_bytes!("../assets/icons/signal/messagestatus-delivered.svg")),
    ("icons/signal/messagestatus-read.svg", include_bytes!("../assets/icons/signal/messagestatus-read.svg")),
    ("icons/signal/error-circle.svg", include_bytes!("../assets/icons/signal/error-circle.svg")),
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, bytes)) = SIGNAL_ICONS.iter().find(|(name, _)| *name == path) {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        if let Some(bytes) = ExtraIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.extend(SIGNAL_ICONS.iter().map(|(name, _)| *name).filter(|name| name.starts_with(path)).map(Into::into));
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
