//! Shared contacts, payment notifications and gift badges in the timeline,
//! as Signal Desktop shows them: `EmbeddedContact` and its `ContactDetail`,
//! `Message`'s `renderPayment` and `renderGiftBadge`, and the
//! `OutgoingGiftBadgeModal`. Read only: payments and redeeming happen on
//! the phone.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Disableable as _, Sizable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, FontWeight, Hsla, SharedString, TestSupportExt as _, Window, div, img, px};
use signal_store::{
    AddressKind, ContactMethod, ContactMethodKind, ConversationId, ConversationKind, DirectoryEntry, GiftBadge, GiftBadgeState, PostalAddress,
    SharedContact,
};

use crate::state::AppState;
use crate::theme::Palette;

/// What a button in the contact details does to the app.
type ChatAction = dyn Fn(&mut AppState, &mut gpui_kit::Context<AppState>);

/// Desktop's `ContactAvatar` color for every shared contact (`A100`).
const CONTACT_AVATAR_COLOR: u8 = 0;

/// Who a shared contact is on Signal, as Desktop's `resolveSharedContact`
/// finds them: their account when the message names it, else a chat with
/// one of their numbers. `None`: not on Signal (as far as we know).
pub fn resolve(contact: &SharedContact, state: &AppState) -> Option<DirectoryEntry> {
    let name = contact.display_name().unwrap_or_else(|| "Unknown contact".into());
    if let Some(aci) = &contact.aci {
        let id = ConversationId::direct(aci);
        let title = state.conversation(&id).map_or(name, |c| c.title.clone());
        return Some(DirectoryEntry { id, kind: ConversationKind::Direct, title, phone: None });
    }
    contact.numbers.iter().find_map(|number| {
        let wanted = e164_digits(&number.value)?;
        state
            .conversations
            .iter()
            .chain(&state.archived)
            .find(|c| c.kind == ConversationKind::Direct && c.phone.as_deref().and_then(e164_digits).as_deref() == Some(&wanted))
            .map(|c| DirectoryEntry { id: c.id.clone(), kind: c.kind, title: c.title.clone(), phone: c.phone.clone() })
    })
}

/// A number written as E.164 (a `+` and digits, formatting aside). Desktop
/// normalizes others with our region (`parsePhoneItem`); those aren't
/// matched here.
fn e164_digits(value: &str) -> Option<String> {
    let value = value.trim();
    let rest = value.strip_prefix('+')?;
    let digits: String = rest.chars().filter(char::is_ascii_digit).collect();
    let clean = rest.chars().all(|c| c.is_ascii_digit() || " -().".contains(c));
    (clean && (7..=15).contains(&digits.len())).then_some(digits)
}

fn contact_avatar(contact: &SharedContact, size: f32) -> AnyElement {
    let name = contact.display_name().unwrap_or_default();
    let photo = contact.avatar.as_ref().and_then(|a| a.local_path.as_deref()).map(std::path::Path::new);
    crate::views::common::person_avatar(photo, &name, CONTACT_AVATAR_COLOR, px(size))
}

/// Desktop's `EmbeddedContact`: photo, name and a chevron, opening the
/// details, and "Message" when they're on Signal.
pub fn render_contact(
    message_id: i64,
    contact: &SharedContact,
    outgoing: bool,
    fg: Hsla,
    secondary: Hsla,
    state: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let name = contact.display_name().or_else(|| contact.organization.clone()).unwrap_or_else(|| "Unknown contact".into());
    let entry = resolve(contact, state.read(cx));
    let (open_state, open_contact) = (state.clone(), contact.clone());
    div()
        .flex()
        .flex_col()
        .gap_3()
        .pt_1()
        .pb_1p5()
        .min_w(px(220.))
        .child(
            div()
                .id(SharedString::from(format!("shared-contact-{message_id}")))
                .flex()
                .items_center()
                .gap_3()
                .cursor_pointer()
                .child(contact_avatar(contact, 52.))
                .child(div().flex_1().min_w_0().truncate().text_color(fg).font_weight(FontWeight::MEDIUM).child(name))
                .child(div().flex_none().text_color(secondary).child("›"))
                .on_click(move |_, window, cx| open_detail(open_state.clone(), open_contact.clone(), window, cx))
                .test_support(),
        )
        .when_some(entry, |d, entry| {
            let state = state.clone();
            d.child(
                Button::new(SharedString::from(format!("shared-contact-message-{message_id}")))
                    .label("Message")
                    .w_full()
                    .map(|b| if outgoing { b.outline() } else { b.primary() })
                    .on_click(move |_, _, cx| state.update(cx, |s, cx| s.start_conversation(entry.clone(), cx))),
            )
        })
        .into_any_element()
}

/// Desktop's `getLabelForPhone` / `getLabelForEmail`.
fn method_label(method: &ContactMethod, custom: &'static str) -> String {
    match method.kind {
        ContactMethodKind::Home => "Home".into(),
        ContactMethodKind::Mobile => "Mobile".into(),
        ContactMethodKind::Work => "Work".into(),
        ContactMethodKind::Custom => method.label.clone().unwrap_or_else(|| custom.into()),
    }
}

/// Desktop's `getLabelForAddress`.
fn address_label(address: &PostalAddress) -> String {
    match address.kind {
        AddressKind::Home => "Home".into(),
        AddressKind::Work => "Work".into(),
        AddressKind::Custom => address.label.clone().unwrap_or_else(|| "Address".into()),
    }
}

/// An address's lines, as Desktop's `ContactDetail` writes them.
pub fn address_lines(address: &PostalAddress) -> Vec<String> {
    let mut lines = Vec::new();
    lines.extend(address.street.clone());
    lines.extend(address.pobox.as_ref().map(|p| format!("PO Box {p}")));
    lines.extend(address.neighborhood.clone());
    let place: Vec<&str> = [&address.city, &address.region, &address.postcode].into_iter().flatten().map(String::as_str).collect();
    if !place.is_empty() {
        lines.push(place.join(" "));
    }
    lines.extend(address.country.clone());
    lines
}

fn detail_item(label: String, value: impl IntoElement, palette: Palette) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .py_1p5()
        .child(div().text_xs().text_color(palette.text_secondary).child(label))
        .child(div().text_sm().child(value))
        .into_any_element()
}

/// Desktop's `ContactDetail`: the photo, name and organization; Message,
/// Video and Voice when they're on Signal; then their numbers, emails,
/// addresses, nickname and note.
pub fn open_detail(state: Entity<AppState>, contact: SharedContact, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let name = contact.display_name().unwrap_or_default();
        let entry = resolve(&contact, state.read(cx));
        let in_call = state.read(cx).call.is_some();
        let mut fields: Vec<AnyElement> = Vec::new();
        for number in &contact.numbers {
            fields.push(detail_item(method_label(number, "Phone"), SharedString::from(number.value.clone()), palette));
        }
        for email in &contact.emails {
            fields.push(detail_item(method_label(email, "Email"), SharedString::from(email.value.clone()), palette));
        }
        for address in &contact.addresses {
            let lines = div().flex().flex_col().children(address_lines(address).into_iter().map(|l| div().child(l)));
            fields.push(detail_item(address_label(address), lines, palette));
        }
        if let Some(nickname) = contact.display_nickname() {
            fields.push(detail_item("Nickname".into(), SharedString::from(nickname), palette));
        }
        if let Some(note) = contact.note.clone() {
            fields.push(detail_item("Notes".into(), SharedString::from(note), palette));
        }
        let buttons = entry.map(|entry| {
            let button = |id: &'static str, label: &'static str, action: Box<ChatAction>| {
                let state = state.clone();
                Button::new(id).label(label).outline().on_click(move |_, window, cx| {
                    window.close_all_dialogs(cx);
                    state.update(cx, |s, cx| action(s, cx));
                })
            };
            let (message, video, voice) = (entry.clone(), entry.id.clone(), entry.id.clone());
            div()
                .flex()
                .justify_center()
                .gap_2()
                .child(button("shared-contact-detail-message", "Message", Box::new(move |s, cx| s.start_conversation(message.clone(), cx))))
                .child(button("shared-contact-detail-video", "Video", Box::new(move |s, cx| s.open_call_lobby(video.clone(), true, cx))).disabled(in_call))
                .child(button("shared-contact-detail-voice", "Voice", Box::new(move |s, cx| s.open_call_lobby(voice.clone(), false, cx))).disabled(in_call))
        });
        dialog.w(px(400.)).child(
            div()
                .debug_selector(|| "shared-contact-detail".into())
                .flex()
                .flex_col()
                .gap_4()
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_1()
                        .child(contact_avatar(&contact, 80.))
                        .child(div().mt_2().text_lg().font_weight(FontWeight::SEMIBOLD).child(name))
                        .when_some(contact.organization.clone(), |d, org| d.child(div().text_color(palette.text_secondary).child(org))),
                )
                .children(buttons)
                .when(!fields.is_empty(), |d| {
                    d.child(div().flex().flex_col().px_3().py_1().rounded_lg().border_1().border_color(palette.panel_border).children(fields))
                }),
        )
    });
}

/// Desktop's `renderPayment`: what happened, that the phone has its
/// status, and the note.
pub fn render_payment(description: String, note: Option<String>, fg: Hsla, secondary: Hsla) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .max_w(px(300.))
        .text_color(fg)
        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(description))
        .child(
            div()
                .px_2()
                .py_1p5()
                .rounded_md()
                .border_1()
                .border_color(secondary)
                .text_xs()
                .text_color(secondary)
                .child("Check your primary device for this payment’s status"),
        )
        .when_some(note, |d, note| d.child(div().text_sm().child(note)))
        .into_any_element()
}

/// Desktop's remaining time on a gift badge: days, hours or minutes left,
/// or "Expired".
pub fn gift_remaining(expiration: i64, now: i64) -> String {
    const MINUTE: i64 = 60_000;
    let duration = expiration - now;
    let (days, hours, minutes) = (duration.div_euclid(24 * 60 * MINUTE), duration.div_euclid(60 * MINUTE), duration.div_euclid(MINUTE));
    let plural = |n: i64, one: &str, other: &str| format!("{n} {} remaining", if n == 1 { one } else { other });
    if days > 1 {
        plural(days, "day", "days")
    } else if hours > 1 {
        plural(hours, "hour", "hours")
    } else if minutes > 0 {
        plural(minutes, "minute", "minutes")
    } else {
        "Expired".into()
    }
}

/// Desktop's `renderGiftBadge`. Unopened, a wrapped gift and where to open
/// it (clicking says so again, Desktop's toast); opened, the badge, the
/// time left, and "Redeemed" (theirs) or "View" (ours).
#[allow(clippy::too_many_arguments)]
pub fn render_gift_badge(
    message_id: i64,
    gift: &GiftBadge,
    outgoing: bool,
    chat_title: String,
    fg: Hsla,
    secondary: Hsla,
    state: &Entity<AppState>,
    cx: &App,
) -> AnyElement {
    let id = SharedString::from(format!("gift-badge-{message_id}"));
    if matches!(gift.state, GiftBadgeState::Unopened | GiftBadgeState::Failed) {
        let (text, toast) = if outgoing {
            ("Tap this message on mobile to view your donation", "Check your phone to view your donation")
        } else {
            ("View this message on mobile to open it", "Check your phone to open this donation")
        };
        return div()
            .id(id)
            .flex()
            .flex_col()
            .gap_2()
            .w(px(200.))
            .cursor_pointer()
            .child(
                div()
                    .h(px(120.))
                    .rounded_lg()
                    .bg(gpui_kit::rgb(0xe5_4d_5d))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(56.))
                    .child("🎁"),
            )
            .child(div().text_sm().text_color(fg).child(text))
            .on_click(move |_, window, cx| window.push_notification(Notification::new().message(toast), cx))
            .into_any_element();
    }
    let s = state.read(cx);
    let badge = gift.badge_id.as_ref().and_then(|b| s.badges.badges.get(b)).cloned();
    let dark = gpui_kit::component::ActiveTheme::theme(cx).is_dark();
    let image = badge.as_ref().and_then(|b| s.badges.image(b, 64., dark));
    let badge_element = match image {
        Some(path) => img(path).size(px(64.)).flex_none().into_any_element(),
        // Desktop's `donation--missing` placeholder.
        None => div().size(px(64.)).flex_none().rounded_full().border_2().border_color(secondary).into_any_element(),
    };
    let now = chrono::Utc::now().timestamp_millis();
    let view = outgoing.then(|| {
        let (state, badge_id) = (state.clone(), gift.badge_id.clone());
        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
            open_outgoing(state.clone(), badge_id.clone(), chat_title.clone(), window, cx)
        }
    });
    div()
        .flex()
        .flex_col()
        .gap_2()
        .w(px(240.))
        .child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(badge_element)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(fg).child("Donation"))
                        .child(div().text_xs().text_color(secondary).child(gift_remaining(gift.expiration, now))),
                ),
        )
        .child(
            Button::new(id)
                .label(if outgoing { "View" } else { "✓ Redeemed" })
                .outline()
                .small()
                .w_full()
                .disabled(!outgoing)
                .when_some(view, |b, view| b.on_click(view)),
        )
        .into_any_element()
}

/// Desktop's `OutgoingGiftBadgeModal`.
pub fn open_outgoing(state: Entity<AppState>, badge_id: Option<String>, recipient: String, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, cx| {
        let s = state.read(cx);
        let dark = gpui_kit::component::ActiveTheme::theme(cx).is_dark();
        let badge = badge_id.as_ref().and_then(|b| s.badges.badges.get(b));
        let image = badge.and_then(|b| s.badges.image(b, 140., dark));
        dialog.w(px(400.)).child(
            div()
                .debug_selector(|| "outgoing-gift-badge".into())
                .flex()
                .flex_col()
                .items_center()
                .gap_3()
                .text_center()
                .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Thanks for your support!"))
                .child(div().text_sm().child(format!(
                    "You've made a donation to Signal on behalf of {recipient}. They'll be given the option to show their support on their profile."
                )))
                .child(match image {
                    Some(path) => img(path).size(px(140.)).into_any_element(),
                    None => div().size(px(140.)).rounded_full().bg(Palette::get(cx).row_hover).into_any_element(),
                })
                .child(div().font_weight(FontWeight::SEMIBOLD).child("Donation")),
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_time_reads_as_desktops() {
        let day = 24 * 60 * 60_000;
        assert_eq!(gift_remaining(3 * day + 5, 0), "3 days remaining");
        assert_eq!(gift_remaining(day + 60_000, 0), "24 hours remaining");
        assert_eq!(gift_remaining(90 * 60_000, 0), "90 minutes remaining");
        assert_eq!(gift_remaining(60_000, 0), "1 minute remaining");
        assert_eq!(gift_remaining(-5, 0), "Expired");
    }

    #[test]
    fn only_e164_numbers_match() {
        assert_eq!(e164_digits("+1 (555) 010-0199").as_deref(), Some("15550100199"));
        assert_eq!(e164_digits("555-0199"), None);
        assert_eq!(e164_digits("+1 555 ext 3"), None);
    }

    #[test]
    fn addresses_read_as_desktop_writes_them() {
        let address = PostalAddress {
            street: Some("1 Main St".into()),
            pobox: Some("12".into()),
            city: Some("Springfield".into()),
            postcode: Some("00001".into()),
            country: Some("Nowhere".into()),
            ..Default::default()
        };
        assert_eq!(address_lines(&address), ["1 Main St", "PO Box 12", "Springfield 00001", "Nowhere"]);
    }
}
