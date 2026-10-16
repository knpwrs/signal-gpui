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
        BadgeCheck,
        ArrowDown,
        Ban,
        Bell,
        BellOff,
        ChevronLeft,
        ChevronRight,
        Hand,
        LayoutGrid,
        SquareUser,
        ChartBar,
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
        LoaderCircle,
        RotateCcw,
        Image,
        Link,
        ListFilter,
        Lock,
        LogOut,
        Maximize2,
        MessageCircle,
        Mic,
        MicOff,
        Minimize2,
        AtSign,
        MessageSquareCheck,
        MessageSquareDot,
        MessageSquareShare,
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
        Share2,
        SquareCheck,
        SquarePen,
        Timer,
        Trash,
        TriangleAlert,
        User,
        Video,
        VideoOff,
        Users,
        WifiOff,
        X
    ]
);

/// Signal Desktop's own icons (AGPL-3.0-only, like this app), where Lucide
/// has no match: the message status circles, the Signal chat's logo, and
/// the navigation tabs' icons.
const SIGNAL_ICONS: &[(&str, &[u8])] = &[
    ("icons/signal/messagestatus-sending.svg", include_bytes!("../assets/icons/signal/messagestatus-sending.svg")),
    ("icons/signal/messagestatus-sent.svg", include_bytes!("../assets/icons/signal/messagestatus-sent.svg")),
    ("icons/signal/messagestatus-delivered.svg", include_bytes!("../assets/icons/signal/messagestatus-delivered.svg")),
    ("icons/signal/messagestatus-read.svg", include_bytes!("../assets/icons/signal/messagestatus-read.svg")),
    ("icons/signal/error-circle.svg", include_bytes!("../assets/icons/signal/error-circle.svg")),
    ("icons/signal/profile-avatar.svg", include_bytes!("../assets/icons/signal/profile-avatar.svg")),
    ("icons/signal/nav-chat.svg", include_bytes!("../assets/icons/signal/nav-chat.svg")),
    ("icons/signal/nav-chat-fill.svg", include_bytes!("../assets/icons/signal/nav-chat-fill.svg")),
    ("icons/signal/nav-phone.svg", include_bytes!("../assets/icons/signal/nav-phone.svg")),
    ("icons/signal/nav-phone-fill.svg", include_bytes!("../assets/icons/signal/nav-phone-fill.svg")),
    ("icons/signal/nav-stories.svg", include_bytes!("../assets/icons/signal/nav-stories.svg")),
    ("icons/signal/nav-stories-fill.svg", include_bytes!("../assets/icons/signal/nav-stories-fill.svg")),
    ("icons/signal/nav-settings.svg", include_bytes!("../assets/icons/signal/nav-settings.svg")),
    ("icons/signal/nav-settings-fill.svg", include_bytes!("../assets/icons/signal/nav-settings-fill.svg")),
    ("icons/signal/nav-menu.svg", include_bytes!("../assets/icons/signal/nav-menu.svg")),
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
