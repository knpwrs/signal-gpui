//! Small shared widgets and formatting.

use chrono::{DateTime, Datelike as _, Local, TimeZone as _};
use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, Div, ObjectFit, Pixels, SharedString, div, img, px};
use signal_store::{Conversation, ConversationKind, DeliveryStatus};

use crate::theme::{Palette, avatar_colors, initials};

/// A round avatar with initials.
pub fn avatar(name: &str, color: u8, size: Pixels) -> Div {
    let (bg, fg) = avatar_colors(color);
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(size)
        .rounded_full()
        .bg(bg)
        .text_color(fg)
        .text_size(size * 0.4)
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .child(SharedString::from(initials(name)))
}

/// A person's avatar: their profile photo once downloaded, else initials.
pub fn person_avatar(photo: Option<&std::path::Path>, name: &str, color: u8, size: Pixels) -> AnyElement {
    match photo {
        Some(path) => img(path.to_path_buf())
            .flex_none()
            .size(size)
            .rounded_full()
            .object_fit(ObjectFit::Cover)
            .into_any_element(),
        None => avatar(name, color, size).into_any_element(),
    }
}

/// A conversation's avatar: its downloaded photo, else (like Signal Desktop)
/// a people icon for groups, a note icon for Note to Self, or initials.
pub fn conversation_avatar(conversation: &Conversation, size: Pixels) -> AnyElement {
    if let Some(path) = &conversation.avatar_path {
        return img(std::path::PathBuf::from(path))
            .flex_none()
            .size(size)
            .rounded_full()
            .object_fit(ObjectFit::Cover)
            .into_any_element();
    }
    let icon = match conversation.kind {
        ConversationKind::Group => Some(IconName::Users),
        ConversationKind::NoteToSelf => Some(IconName::NotebookText),
        ConversationKind::Direct => None,
    };
    match icon {
        Some(icon) => {
            let (bg, fg) = avatar_colors(conversation.color);
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .size(size)
                .rounded_full()
                .bg(bg)
                .child(Icon::new(icon).size(size * 0.5).text_color(fg))
                .into_any_element()
        }
        None => avatar(&conversation.title, conversation.color, size).into_any_element(),
    }
}

/// Delivery-status icon for one of our messages. `read` is the color for
/// read receipts, `muted` for everything else.
/// Where a delivery status shows, which decides how failures look.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum StatusPlace {
    /// Message metadata: a failed send says "Not sent" instead of an icon.
    Bubble,
    /// Chat list: a failed send shows a red error circle.
    List,
}

/// Signal Desktop's delivery status icons: a dotted circle that turns while
/// sending, one outlined circle with a check once sent, two once delivered,
/// two filled ones once read. All in the surrounding text colour (read
/// isn't highlighted), 12pt tall; the double ones are 18pt wide.
pub fn status_icon(
    status: DeliveryStatus,
    color: gpui_kit::Hsla,
    palette: Palette,
    place: StatusPlace,
    key: impl Into<SharedString>,
) -> Option<AnyElement> {
    use gpui_kit::{Animation, AnimationExt as _, Transformation, percentage, svg};
    let icon = |name: &str, width: f32| {
        svg().path(SharedString::from(format!("icons/signal/{name}.svg"))).w(px(width)).h(px(12.)).flex_none()
    };
    Some(match status {
        DeliveryStatus::Sending => icon("messagestatus-sending", 12.)
            .text_color(color)
            .with_animation(
                gpui_kit::ElementId::Name(format!("sending-{}", key.into()).into()),
                Animation::new(std::time::Duration::from_secs(4)).repeat(),
                |icon, delta| icon.with_transformation(Transformation::rotate(percentage(delta))),
            )
            .into_any_element(),
        DeliveryStatus::Sent => icon("messagestatus-sent", 12.).text_color(color).into_any_element(),
        DeliveryStatus::Delivered => icon("messagestatus-delivered", 18.).text_color(color).into_any_element(),
        DeliveryStatus::Read => icon("messagestatus-read", 18.).text_color(color).into_any_element(),
        DeliveryStatus::Failed => match place {
            StatusPlace::Bubble => return None,
            StatusPlace::List => icon("error-circle", 12.).text_color(palette.danger).into_any_element(),
        },
    })
}

fn local(millis: i64) -> Option<DateTime<Local>> {
    Local.timestamp_millis_opt(millis).single()
}

/// Conversation list timestamp: "now", "5m", "3:41 PM", "Mon", "Mar 3", "3/3/24".
pub fn short_timestamp(millis: i64) -> String {
    let Some(time) = local(millis) else { return String::new() };
    let now = Local::now();
    let age = now.signed_duration_since(time);
    if age.num_seconds() < 60 {
        "now".into()
    } else if age.num_minutes() < 60 {
        format!("{}m", age.num_minutes())
    } else if time.date_naive() == now.date_naive() {
        time.format("%-I:%M %p").to_string()
    } else if (now.date_naive() - time.date_naive()).num_days() < 7 {
        time.format("%a").to_string()
    } else if time.year() == now.year() {
        time.format("%b %-d").to_string()
    } else {
        time.format("%-m/%-d/%y").to_string()
    }
}

/// Message footer time.
pub fn message_time(millis: i64) -> String {
    let Some(time) = local(millis) else { return String::new() };
    let age = Local::now().signed_duration_since(time);
    if age.num_seconds() < 60 {
        "Now".into()
    } else if age.num_minutes() < 60 {
        format!("{}m", age.num_minutes())
    } else {
        time.format("%-I:%M %p").to_string()
    }
}

/// Day separator: "Today", "Yesterday", "Monday", "March 3", "March 3, 2024".
pub fn day_label(millis: i64) -> String {
    let Some(time) = local(millis) else { return String::new() };
    let today = Local::now().date_naive();
    let day = time.date_naive();
    match (today - day).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => time.format("%A").to_string(),
        _ if day.year() == today.year() => time.format("%B %-d").to_string(),
        _ => time.format("%B %-d, %Y").to_string(),
    }
}

pub fn same_day(a: i64, b: i64) -> bool {
    match (local(a), local(b)) {
        (Some(a), Some(b)) => a.date_naive() == b.date_naive(),
        _ => false,
    }
}

/// Joins lines and runs of spaces with single spaces, for one-paragraph
/// previews (draft rows, quotes).
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Web links in `text`: each byte range and the URL to open. Bare domains
/// (`signal.org`, `www.example.com/x`) open over https; other schemes are
/// left as plain text so a message can't make a `file:` or `javascript:`
/// link clickable.
pub fn find_links(text: &str) -> Vec<(std::ops::Range<usize>, String)> {
    let mut finder = linkify::LinkFinder::new();
    finder.kinds(&[linkify::LinkKind::Url]).url_must_have_scheme(false);
    finder
        .links(text)
        .filter_map(|link| {
            let found = link.as_str();
            let url = match found.split_once("://") {
                Some((scheme, _)) if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") => {
                    found.to_owned()
                }
                Some(_) => return None,
                None if found.contains(':') && !found.contains('.') => return None,
                None => format!("https://{found}"),
            };
            Some((link.start()..link.end(), url))
        })
        .collect()
}

pub fn format_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.0} KB", b as f64 / 1024.),
        b if b < 1024 * 1024 * 1024 => format!("{:.1} MB", b as f64 / (1024. * 1024.)),
        b => format!("{:.2} GB", b as f64 / (1024. * 1024. * 1024.)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_web_links_only() {
        let text = "See https://signal.org/blog/. Or www.example.com/a?b=1, or signal.org! Not ftp://x.org or javascript:alert(1)";
        let links = find_links(text);
        let found: Vec<(&str, &str)> = links.iter().map(|(r, url)| (&text[r.clone()], url.as_str())).collect();
        assert_eq!(
            found,
            [
                ("https://signal.org/blog/", "https://signal.org/blog/"),
                ("www.example.com/a?b=1", "https://www.example.com/a?b=1"),
                ("signal.org", "https://signal.org"),
            ]
        );
        assert!(find_links("no links here").is_empty());
    }
}


/// Where a mouse-down outside a popover closed it. Popovers close on mouse
/// down outside, as menus do, but the button that toggles one acts on the
/// click that follows, so clicking it while open would close the popover and
/// then reopen it. The button skips the click that began where this recorded.
#[derive(Default)]
pub(crate) struct ClosedByClick(Option<gpui_kit::Point<Pixels>>);

impl ClosedByClick {
    pub(crate) fn record(&mut self, event: &gpui_kit::MouseDownEvent) {
        self.0 = Some(event.position);
    }

    /// Whether `click` is the one that closed the popover. Forgets it either
    /// way.
    pub(crate) fn take(&mut self, click: &gpui_kit::ClickEvent) -> bool {
        let at = self.0.take();
        matches!(click, gpui_kit::ClickEvent::Mouse(m) if at == Some(m.down.position))
    }
}
