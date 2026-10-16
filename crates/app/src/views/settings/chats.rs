//! Desktop's Chats page: address book photos, keeping muted chats
//! archived, link previews, emoticons, chat folders, and importing the
//! phone's contacts. (Its spell check, text formatting, skin tone and chat
//! export aren't here.)

use gpui_kit::component::button::Button;
use gpui_kit::component::{Disableable as _, Sizable as _};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, SharedString, div};
use signal_backend::AccountSetting;

use super::{Settings, list, switch};
use crate::state::ContactSync;
use crate::theme::Palette;

pub(super) fn render(settings: &Entity<Settings>, palette: Palette, cx: &mut App) -> AnyElement {
    let state = settings.read(cx).state.clone();
    let (prefs, sync) = {
        let s = state.read(cx);
        (s.prefs.clone(), s.contact_sync)
    };
    let flag = |id: &'static str, label: &'static str, description: &'static str, setting: AccountSetting| {
        let state = state.clone();
        switch(id, label, Some(description.into()), prefs.flag(setting), false, palette, move |on, _, cx| {
            state.update(cx, |s, cx| s.set_account_setting(setting, on, cx))
        })
    };
    let emoji_state = state.clone();
    let sync_state = state.clone();
    let pending = matches!(sync, ContactSync::Pending(_));
    div()
        .flex()
        .flex_col()
        .gap_5()
        .child(list(
            None,
            None,
            [
                flag(
                    "address-book-photos",
                    "Use address book photos",
                    "Display contact photos from your address book if available.",
                    AccountSetting::PreferContactAvatars,
                ),
                flag(
                    "keep-muted-chats-archived",
                    "Keep muted chats archived",
                    "Muted chats that are archived will remain archived when a new message arrives.",
                    AccountSetting::KeepMutedChatsArchived,
                ),
            ],
            palette,
        ))
        .child(list(
            Some("Text input"),
            None,
            [
                flag(
                    "link-previews",
                    "Generate link previews",
                    "Retrieve link previews directly from websites for messages you send.",
                    AccountSetting::LinkPreviews,
                ),
                switch(
                    "auto-convert-emoji",
                    "Convert typed emoticons to emoji",
                    Some("For example, :-) will be converted to 🙂".into()),
                    prefs.auto_convert_emoji,
                    false,
                    palette,
                    move |on, _, cx| emoji_state.update(cx, |s, cx| s.set_auto_convert_emoji(on, cx)),
                ),
            ],
            palette,
        ))
        .child(list(Some("Chat folders"), None, [super::chat_folders::row(settings, palette, cx)], palette))
        .child(list(
            None,
            None,
            [div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(div().text_sm().child("Import contacts"))
                        .child(div().text_xs().text_color(palette.text_secondary).child(import_description(prefs.contacts_synced_at)))
                        .when(sync == ContactSync::Failed, |d| {
                            d.child(div().text_xs().text_color(palette.danger).child(
                                "Import failed. Make sure your computer and your phone are connected to the internet.",
                            ))
                        }),
                )
                .child(
                    Button::new("import-contacts")
                        .outline()
                        .small()
                        .label("Import now")
                        .loading(pending)
                        .disabled(pending)
                        .on_click(move |_, window, cx| {
                            sync_state.update(cx, |s, cx| s.sync_contacts(cx));
                            window.refresh();
                        }),
                )
                .into_any_element()],
            palette,
        ))
        .into_any_element()
}

/// Desktop's `syncExplanation` and `Preferences--lastSynced` ("Last import
/// at {date} {time}"; left out before the first, where Desktop shows 1970).
fn import_description(synced_at: Option<i64>) -> SharedString {
    use chrono::TimeZone as _;
    let at = synced_at.and_then(|at| chrono::Local.timestamp_millis_opt(at).single());
    let last = at.map(|t| format!(" Last import at {} {}", t.format("%-m/%-d/%Y"), t.format("%-I:%M:%S %p"))).unwrap_or_default();
    format!("Import all Signal groups and contacts from your mobile device.{last}").into()
}
