//! Our profile, as Signal Desktop edits it (`ts/components/ProfileEditor.dom.tsx`
//! and the modal around it): the photo (`AvatarEditor`, with Desktop's
//! presets), name, about and its emoji, the username (`UsernameEditor`)
//! and its link and QR code (`UsernameLinkEditor`).

use std::sync::Arc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, ClipboardItem, Context, Entity, FontWeight, Image, ImageFormat, ObjectFit, SharedString, Subscription, Task, Window,
    deferred, div, img, px,
};
use signal_backend::profiles::{ABOUT_BYTES, ABOUT_GRAPHEMES, AvatarChange, NAME_BYTES, NAME_GRAPHEMES, ProfileEdit};
use signal_backend::usernames::{self, DISCRIMINATOR_MAX_LENGTH, Reservation, UsernameProblem};
use signal_backend::{Command, OwnProfile};
use signal_store::{remaining_count, truncate_text};

use crate::state::{AppEvent, AppState};
use crate::theme::Palette;
use crate::views::emoji_picker::{EmojiPicker, PickerEvent};

/// Desktop's preset abouts (`getDefaultBios`).
const DEFAULT_BIOS: [(&str, &str); 5] = [
    ("👋", "Speak Freely"),
    ("🤐", "Encrypted"),
    ("👍", "Free to chat"),
    ("☕", "Coffee lover"),
    ("📵", "Taking a break"),
];

/// Desktop waits this long after typing before reserving (`INPUT_DELAY_MS`).
const RESERVE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// Desktop's username link colors (`COLOR_MAP` in `UsernameLinkEditor`):
/// `UsernameLink.Color` with its (QR code, card, tint) colors.
pub const LINK_COLORS: [(i32, u32, u32, u32); 8] = [
    (1, 0x2449c0, 0x506ecd, 0xecf0fb),
    (2, 0x000000, 0xffffff, 0xf5f5f5),
    (3, 0x464852, 0x6a6c75, 0xf0f0f1),
    (4, 0x73694f, 0xaa9c7c, 0xf6f5f2),
    (5, 0x55733f, 0x7c9b69, 0xf1f5f0),
    (6, 0xd96b2d, 0xee691a, 0xfef1ea),
    (7, 0xbb617b, 0xf77099, 0xfef1f5),
    (8, 0x7651c5, 0xa183d4, 0xf5f3fb),
];

/// A link color's (QR code, card) colors; unknown is blue (Desktop's
/// `DEFAULT_PRESET`).
pub fn link_colors(color: i32) -> (u32, u32) {
    let (_, fg, bg, _) = LINK_COLORS.iter().find(|(id, ..)| *id == color).copied().unwrap_or(LINK_COLORS[0]);
    (fg, bg)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Main,
    Name,
    About,
    Avatar,
    Username,
    UsernameLink,
}

impl Page {
    /// Desktop's titles (`ProfileEditorModal--*`).
    fn title(self) -> &'static str {
        match self {
            Page::Main => "Profile",
            Page::Name => "Your Name",
            Page::About => "About",
            Page::Avatar => "Your Avatar",
            Page::Username => "Username",
            Page::UsernameLink => "Sharing",
        }
    }
}

/// The photo staged on the avatar page.
#[derive(Clone)]
enum StagedPhoto {
    /// What we have.
    Current,
    Remove,
    /// A processed JPEG, with its image for the preview.
    New(Arc<Vec<u8>>, Arc<Image>),
}

/// The username editor's state (Desktop's `usernameReservation`).
#[derive(Default)]
struct UsernameState {
    reservation: Option<Reservation>,
    problem: Option<UsernameProblem>,
    reserving: bool,
    confirming: bool,
    /// The digits were typed (Desktop's `customDiscriminator`).
    custom_digits: bool,
    /// What the last reservation asked for, to match its answer.
    asked: Option<(String, Option<String>)>,
    task: Option<Task<()>>,
}

pub struct ProfileEditor {
    state: Entity<AppState>,
    pub page: Page,
    given: Entity<InputState>,
    family: Entity<InputState>,
    about: Entity<InputState>,
    about_emoji: Option<String>,
    picker: Option<Entity<EmojiPicker>>,
    photo: StagedPhoto,
    nickname: Entity<InputState>,
    digits: Entity<InputState>,
    username: UsernameState,
    /// Waiting for a profile write's answer.
    saving: bool,
    /// The link's color choices are showing.
    showing_colors: bool,
    resetting_link: bool,
    /// Set while code fills an input, so its change isn't taken as typing.
    filling: bool,
    _subscriptions: Vec<Subscription>,
}

impl ProfileEditor {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let limited = |placeholder: &'static str, graphemes: usize, bytes: usize, window: &mut Window, cx: &mut Context<Self>| {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
            let subscription = cx.subscribe_in(&input, window, move |_, input, event: &InputEvent, window, cx| {
                if let InputEvent::Change = event {
                    let value = input.read(cx).value().to_string();
                    // Desktop's about drops line breaks; both stop at the limit.
                    let clean = value.replace(['\r', '\n'], "");
                    let cut = truncate_text(&clean, graphemes, bytes).to_owned();
                    if cut != value {
                        input.update(cx, |input, cx| input.set_value(cut, window, cx));
                    }
                    cx.notify();
                }
            });
            (input, subscription)
        };
        let (given, given_sub) = limited("First Name (Required)", NAME_GRAPHEMES, NAME_BYTES, window, cx);
        let (family, family_sub) = limited("Last Name (Optional)", NAME_GRAPHEMES, NAME_BYTES, window, cx);
        let (about, about_sub) = limited("Write something about yourself...", ABOUT_GRAPHEMES, ABOUT_BYTES, window, cx);
        let nickname = cx.new(|cx| InputState::new(window, cx).placeholder("Username"));
        let digits = cx.new(|cx| InputState::new(window, cx).placeholder("00"));
        let subscriptions = vec![
            given_sub,
            family_sub,
            about_sub,
            cx.subscribe_in(&nickname, window, |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::Change = event
                    && !this.filling
                {
                    this.nickname_changed(window, cx);
                }
            }),
            cx.subscribe_in(&digits, window, |this, input, event: &InputEvent, window, cx| {
                if let InputEvent::Change = event
                    && !this.filling
                {
                    // Desktop allows only digits, nine at most.
                    let value = input.read(cx).value().to_string();
                    let clean: String = value.chars().filter(char::is_ascii_digit).take(DISCRIMINATOR_MAX_LENGTH).collect();
                    if clean != value {
                        this.filling = true;
                        input.update(cx, |input, cx| input.set_value(clean, window, cx));
                        this.filling = false;
                    }
                    this.digits_changed(window, cx);
                }
            }),
            cx.subscribe_in(&state, window, |this, _, event: &AppEvent, window, cx| this.on_app_event(event, window, cx)),
        ];
        Self {
            state,
            page: Page::Main,
            given,
            family,
            about,
            about_emoji: None,
            picker: None,
            photo: StagedPhoto::Current,
            nickname,
            digits,
            username: UsernameState::default(),
            saving: false,
            showing_colors: false,
            resetting_link: false,
            filling: false,
            _subscriptions: subscriptions,
        }
    }

    fn profile(&self, cx: &App) -> Option<OwnProfile> {
        self.state.read(cx).own_profile.clone()
    }

    pub fn title(&self) -> &'static str {
        self.page.title()
    }

    /// Opens a page, filled with what we have.
    pub fn go(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        let profile = self.profile(cx).unwrap_or_default();
        self.filling = true;
        match page {
            Page::Name => {
                self.given.update(cx, |i, cx| i.set_value(profile.given_name.clone(), window, cx));
                self.family.update(cx, |i, cx| i.set_value(profile.family_name.clone(), window, cx));
                self.given.update(cx, |i, cx| i.focus(window, cx));
            }
            Page::About => {
                self.about.update(cx, |i, cx| i.set_value(profile.about.clone(), window, cx));
                self.about_emoji = Some(profile.about_emoji.clone()).filter(|e| !e.is_empty());
                self.about.update(cx, |i, cx| i.focus(window, cx));
            }
            Page::Avatar => self.photo = StagedPhoto::Current,
            Page::Username => {
                let current = profile.username.clone().unwrap_or_default();
                let nickname = usernames::nickname_of(&current).to_owned();
                let digits = usernames::discriminator_of(&current).unwrap_or_default().to_owned();
                self.nickname.update(cx, |i, cx| i.set_value(nickname, window, cx));
                self.digits.update(cx, |i, cx| i.set_value(digits, window, cx));
                self.username = UsernameState::default();
                self.nickname.update(cx, |i, cx| i.focus(window, cx));
            }
            Page::Main | Page::UsernameLink => {}
        }
        self.filling = false;
        self.showing_colors = false;
        self.picker = None;
        self.page = page;
        cx.notify();
    }

    fn back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.username.task = None;
        self.go(Page::Main, window, cx);
    }

    fn on_app_event(&mut self, event: &AppEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            AppEvent::OwnProfileChanged => cx.notify(),
            AppEvent::ProfileSaved(result) if self.saving => {
                self.saving = false;
                match result {
                    Ok(()) => self.back(window, cx),
                    Err(message) => window.push_notification(Notification::error(message.clone()), cx),
                }
                cx.notify();
            }
            AppEvent::UsernameReserved { nickname, discriminator, result } => {
                if self.username.asked.as_ref() != Some(&(nickname.clone(), discriminator.clone())) {
                    return;
                }
                self.username.reserving = false;
                match result {
                    Ok(reservation) => {
                        self.username.problem = None;
                        // The digits it got, unless typed (Desktop shows the
                        // reservation's then).
                        if !self.username.custom_digits
                            && let Some(digits) = usernames::discriminator_of(&reservation.username)
                        {
                            let digits = digits.to_owned();
                            self.filling = true;
                            self.digits.update(cx, |i, cx| i.set_value(digits, window, cx));
                            self.filling = false;
                        }
                        self.username.reservation = Some(reservation.clone());
                    }
                    Err(problem) => {
                        self.username.reservation = None;
                        self.username.problem = Some(*problem);
                    }
                }
                cx.notify();
            }
            AppEvent::UsernameSaved(result) if self.username.confirming || self.page == Page::Main => {
                self.username.confirming = false;
                match result {
                    Ok(_) => {
                        if self.page == Page::Username {
                            self.back(window, cx);
                        }
                    }
                    Err(problem) if problem.inline() => self.username.problem = Some(*problem),
                    Err(problem) => {
                        let username = self.username.reservation.as_ref().map(|r| r.username.clone()).unwrap_or_default();
                        let message = problem.message(&username);
                        if *problem == UsernameProblem::ConflictOrGone {
                            // Desktop asks for new digits after this.
                            self.username.reservation = None;
                        }
                        window.open_alert_dialog(cx, move |alert, _, _| alert.description(SharedString::from(message.clone())));
                    }
                }
                cx.notify();
            }
            AppEvent::UsernameLinkReset(result) => {
                self.resetting_link = false;
                if let Err(message) = result {
                    window.push_notification(Notification::error(message.clone()), cx);
                }
                cx.notify();
            }
            _ => {}
        }
    }

    /// What the name and about pages would save, with the photo kept.
    fn edit(&self, cx: &App) -> Option<ProfileEdit> {
        let profile = self.profile(cx)?;
        Some(ProfileEdit {
            given_name: profile.given_name,
            family_name: profile.family_name,
            about: profile.about,
            about_emoji: profile.about_emoji,
            avatar: AvatarChange::Keep,
        })
    }

    fn save(&mut self, edit: ProfileEdit, cx: &mut Context<Self>) {
        self.saving = true;
        self.state.read(cx).send(Command::SetProfile(edit));
        cx.notify();
    }

    fn name_changed(&self, cx: &App) -> Option<ProfileEdit> {
        let mut edit = self.edit(cx)?;
        let given = self.given.read(cx).value().trim().to_owned();
        let family = self.family.read(cx).value().trim().to_owned();
        // Desktop's `shouldDisableSave`: a first name, and something new.
        if given.is_empty() || (given == edit.given_name && family == edit.family_name) {
            return None;
        }
        edit.given_name = given;
        edit.family_name = family;
        Some(edit)
    }

    fn about_changed(&self, cx: &App) -> Option<ProfileEdit> {
        let mut edit = self.edit(cx)?;
        let about = self.about.read(cx).value().to_string();
        // An empty about has no emoji (Desktop clears it with the text).
        let emoji = self.about_emoji.clone().filter(|_| !about.is_empty()).unwrap_or_default();
        if about == edit.about && emoji == edit.about_emoji {
            return None;
        }
        edit.about = about;
        edit.about_emoji = emoji;
        Some(edit)
    }

    fn photo_changed(&self, cx: &App) -> Option<ProfileEdit> {
        let mut edit = self.edit(cx)?;
        edit.avatar = match &self.photo {
            StagedPhoto::Current => return None,
            StagedPhoto::Remove => AvatarChange::Remove,
            StagedPhoto::New(bytes, _) => AvatarChange::New(bytes.clone()),
        };
        Some(edit)
    }

    /// A picked or preset image, cropped and saved as Desktop uploads it.
    pub fn stage_photo(&mut self, bytes: &[u8], window: &mut Window, cx: &mut Context<Self>) {
        match signal_backend::profiles::avatar_image(bytes) {
            Ok(jpeg) => {
                let image = Arc::new(Image::from_bytes(ImageFormat::Jpeg, jpeg.clone()));
                self.photo = StagedPhoto::New(Arc::new(jpeg), image);
            }
            Err(error) => window.push_notification(Notification::error(format!("That image can't be used: {error}")), cx),
        }
        cx.notify();
    }

    fn pick_photo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(gpui_kit::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose photo".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let Ok(bytes) = std::fs::read(&path) else { return };
            let _ = this.update_in(cx, |this, window, cx| this.stage_photo(&bytes, window, cx));
        })
        .detach();
    }

    fn nickname_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.username.custom_digits = false;
        self.username.problem = None;
        let nickname = self.nickname.read(cx).value().trim().to_owned();
        if nickname.is_empty() {
            // Desktop clears the reservation with the nickname.
            self.username.reservation = None;
            self.username.task = None;
            self.username.reserving = false;
            cx.notify();
            return;
        }
        self.reserve_later(nickname, None, window, cx);
    }

    fn digits_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.username.custom_digits = true;
        self.username.problem = None;
        self.username.reservation = None;
        let nickname = self.nickname.read(cx).value().trim().to_owned();
        let digits = self.digits.read(cx).value().to_string();
        // Desktop doesn't reserve while the digits are erased.
        if nickname.is_empty() || digits.is_empty() {
            self.username.task = None;
            self.username.reserving = false;
            cx.notify();
            return;
        }
        self.reserve_later(nickname, Some(digits), window, cx);
    }

    /// Reserves after Desktop's pause; typing again starts it over.
    fn reserve_later(&mut self, nickname: String, digits: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.username.reserving = true;
        self.username.task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(RESERVE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                this.username.asked = Some((nickname.clone(), digits.clone()));
                this.state.read(cx).send(Command::ReserveUsername { nickname, discriminator: digits });
            });
        }));
        cx.notify();
    }

    fn save_username(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(reservation) = self.username.reservation.clone() else { return };
        if self.username.reserving || self.username.confirming {
            return;
        }
        let current = self.profile(cx).and_then(|p| p.username);
        let confirm = {
            let this = cx.entity();
            move |window: &mut Window, cx: &mut App| {
                this.update(cx, |this, cx| {
                    this.username.confirming = true;
                    this.state.read(cx).send(Command::ConfirmUsername(reservation.clone()));
                    cx.notify();
                });
                let _ = window;
            }
        };
        // Desktop asks first when it replaces a username (not for a case change).
        if current.is_some() && !self.username.reservation.as_ref().is_some_and(Reservation::is_case_change) {
            window.open_alert_dialog(cx, move |alert, _, _| {
                let confirm = confirm.clone();
                alert
                    .description("Changing your username will reset your existing QR code and link. Are you sure?")
                    .confirm()
                    .ok_text("Continue")
                    .on_ok(move |_, window, cx| {
                        confirm(window, cx);
                        true
                    })
            });
        } else {
            confirm(window, cx);
        }
    }

    fn delete_username(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(username) = self.profile(cx).and_then(|p| p.username) else { return };
        let state = self.state.clone();
        let message = format!(
            "This will remove your username and disable your QR code and link. “{username}” will be available for others to claim. Are you sure?"
        );
        window.open_alert_dialog(cx, move |alert, _, _| {
            let state = state.clone();
            alert
                .description(SharedString::from(message.clone()))
                .confirm()
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    state.read(cx).send(Command::DeleteUsername);
                    true
                })
        });
    }

    fn reset_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            alert
                .description("If you reset your QR code and link, your existing QR code and link will no longer work.")
                .confirm()
                .ok_text("Reset")
                .on_ok(move |_, _, cx| {
                    this.update(cx, |this, cx| {
                        this.resetting_link = true;
                        this.state.read(cx).send(Command::ResetUsernameLink);
                        cx.notify();
                    });
                    true
                })
        });
    }

    fn toggle_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.take().is_some() {
            cx.notify();
            return;
        }
        let picker = cx.new(|cx| EmojiPicker::new(self.state.clone(), window, cx));
        let subscription = cx.subscribe_in(&picker, window, |this, _, event: &PickerEvent, _, cx| {
            if let PickerEvent::Picked(emoji) = event {
                this.about_emoji = Some(emoji.clone());
            }
            this.picker = None;
            cx.notify();
        });
        self._subscriptions.push(subscription);
        self.picker = Some(picker);
        cx.notify();
    }
}

/// A field's counter, as Desktop's `AxoTextField.Count` shows it.
fn counter(text: &str, graphemes: usize, bytes: usize, palette: Palette) -> Option<AnyElement> {
    remaining_count(text, graphemes, bytes).map(|(left, warn)| {
        div()
            .text_xs()
            .text_color(if warn { palette.danger } else { palette.text_secondary })
            .child(left.to_string())
            .into_any_element()
    })
}

/// A row of the main page: an icon, its text, and a click.
fn row(id: &'static str, icon: impl IntoElement, label: impl IntoElement, palette: Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .debug_selector(move || id.into())
        .flex()
        .items_center()
        .gap_3()
        .px_2()
        .py_2()
        .rounded_md()
        .cursor_pointer()
        .hover(|s| s.bg(palette.row_hover))
        .child(div().w(px(20.)).flex().justify_center().child(icon))
        .child(div().flex_1().min_w_0().truncate().child(label))
}

fn footer(cancel: Button, save: Button) -> impl IntoElement {
    div().flex().justify_end().gap_2().pt_2().child(cancel).child(save)
}

/// The photo we have, or one staged.
fn photo_element(photo: &StagedPhoto, profile: &OwnProfile, size: f32) -> AnyElement {
    let name = profile.name().unwrap_or_default();
    let color = 0;
    match photo {
        StagedPhoto::New(_, image) => img(image.clone()).size(px(size)).rounded_full().object_fit(ObjectFit::Cover).into_any_element(),
        StagedPhoto::Remove => crate::views::common::avatar(&name, color, px(size)).into_any_element(),
        StagedPhoto::Current => crate::views::common::person_avatar(profile.photo.as_deref(), &name, color, px(size)),
    }
}

impl Render for ProfileEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let profile = self.profile(cx).unwrap_or_default();
        let body = match self.page {
            Page::Main => self.render_main(&profile, palette, cx).into_any_element(),
            Page::Name => self.render_name(palette, cx).into_any_element(),
            Page::About => self.render_about(palette, cx).into_any_element(),
            Page::Avatar => self.render_avatar(&profile, palette, cx).into_any_element(),
            Page::Username => self.render_username(palette, cx).into_any_element(),
            Page::UsernameLink => self.render_link(&profile, palette, window, cx).into_any_element(),
        };
        div().debug_selector(|| "profile-editor".into()).flex().flex_col().gap_3().child(body)
    }
}

impl ProfileEditor {
    fn render_main(&mut self, profile: &OwnProfile, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let name = profile.name().unwrap_or_else(|| "Your name".into());
        let about = (!profile.about.is_empty()).then(|| format!("{} {}", profile.about_emoji, profile.about).trim().to_owned());
        let menu_this = cx.entity();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .pb_2()
                    .child(photo_element(&StagedPhoto::Current, profile, 80.))
                    .child(
                        Button::new("profile-edit-photo")
                            .label("Edit photo")
                            .small()
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| this.go(Page::Avatar, window, cx))),
                    ),
            )
            .child(
                row("profile-name-row", Icon::new(IconName::User).size_4(), SharedString::from(name), palette)
                    .on_click(cx.listener(|this, _, window, cx| this.go(Page::Name, window, cx))),
            )
            .child(
                row(
                    "profile-about-row",
                    Icon::new(IconName::Pencil).size_4(),
                    div()
                        .when(about.is_none(), |d| d.text_color(palette.text_secondary))
                        .child(SharedString::from(about.unwrap_or_else(|| "About".into()))),
                    palette,
                )
                .on_click(cx.listener(|this, _, window, cx| this.go(Page::About, window, cx))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(
                        div().flex_1().min_w_0().child(
                            row(
                                "profile-username-row",
                                div().text_sm().font_weight(FontWeight::SEMIBOLD).child("@"),
                                div()
                                    .when(profile.username.is_none(), |d| d.text_color(palette.text_secondary))
                                    .child(SharedString::from(profile.username.clone().unwrap_or_else(|| "Username".into()))),
                                palette,
                            )
                            .on_click(cx.listener(|this, _, window, cx| this.go(Page::Username, window, cx))),
                        ),
                    )
                    .when_some(profile.username.clone(), |d, username| {
                        d.child(
                            Button::new("profile-username-menu")
                                .icon(IconName::Ellipsis)
                                .ghost()
                                .small()
                                .tooltip("Copy or delete username")
                                .dropdown_menu(move |menu, _, _| {
                                    let (username, this) = (username.clone(), menu_this.clone());
                                    menu.item(PopupMenuItem::new("Copy username").on_click(move |_, window, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(username.clone()));
                                        window.push_notification(Notification::new().message("Copied username"), cx);
                                    }))
                                    .item(PopupMenuItem::new("Delete").on_click(move |_, window, cx| {
                                        this.update(cx, |this, cx| this.delete_username(window, cx));
                                    }))
                                }),
                        )
                    }),
            )
            .when(profile.username.is_some(), |d| {
                d.child(
                    row("profile-link-row", Icon::new(IconName::Link).size_4(), "QR code or link", palette)
                        .on_click(cx.listener(|this, _, window, cx| this.go(Page::UsernameLink, window, cx))),
                )
            })
            .child(div().pt_2().text_xs().text_color(palette.text_secondary).child(if profile.username.is_some() {
                "Your username, QR code & link aren’t visible on your profile. Only share them with people you trust."
            } else {
                "People can now message you using your optional username so you don’t have to give out your phone number."
            }))
            .child(
                div()
                    .pt_2()
                    .text_xs()
                    .text_color(palette.text_secondary)
                    .child("Your profile and changes to it will be visible to people you message, contacts and groups."),
            )
    }

    fn render_name(&mut self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let save = self.name_changed(cx);
        let field = |input: &Entity<InputState>, selector: &'static str, cx: &App| {
            let left = counter(&input.read(cx).value(), NAME_GRAPHEMES, NAME_BYTES, palette);
            div().debug_selector(move || selector.into()).child(Input::new(input).when_some(left, |i, left| i.suffix(left)))
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(field(&self.given, "profile-given", cx))
            .child(field(&self.family, "profile-family", cx))
            .child(footer(
                Button::new("profile-cancel").label("Cancel").outline().on_click(cx.listener(|this, _, window, cx| this.back(window, cx))),
                Button::new("profile-save")
                    .label("Save")
                    .primary()
                    .disabled(save.is_none() || self.saving)
                    .loading(self.saving)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(edit) = this.name_changed(cx) {
                            this.save(edit, cx);
                        }
                    })),
            ))
    }

    fn render_about(&mut self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let save = self.about_changed(cx);
        let text = self.about.read(cx).value().to_string();
        let left = counter(&text, ABOUT_GRAPHEMES, ABOUT_BYTES, palette);
        let emoji_button = Button::new("profile-about-emoji")
            .ghost()
            .small()
            .tooltip("Emoji")
            .map(|b| match &self.about_emoji {
                Some(emoji) => b.label(SharedString::from(emoji.clone())),
                None => b.icon(IconName::FaceSlightlySmiling),
            })
            .on_click(cx.listener(|this, _, window, cx| this.toggle_picker(window, cx)));
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .relative()
                    .debug_selector(|| "profile-about".into())
                    .child(Input::new(&self.about).prefix(emoji_button).when_some(left, |i, left| i.suffix(left)))
                    .when_some(self.picker.clone(), |d, picker| d.child(deferred(div().absolute().top(px(40.)).left_0().child(picker)))),
            )
            .child(div().flex().flex_col().children(DEFAULT_BIOS.iter().enumerate().map(|(ix, (emoji, text))| {
                let (emoji, text) = (*emoji, *text);
                div()
                    .id(("profile-bio", ix))
                    .debug_selector(move || format!("profile-bio-{ix}"))
                    .flex()
                    .items_center()
                    .gap_3()
                    .px_2()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(palette.row_hover))
                    .child(emoji)
                    .child(text)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.about_emoji = Some(emoji.to_owned());
                        this.about.update(cx, |i, cx| i.set_value(text, window, cx));
                        cx.notify();
                    }))
            })))
            .child(footer(
                Button::new("profile-cancel").label("Cancel").outline().on_click(cx.listener(|this, _, window, cx| this.back(window, cx))),
                Button::new("profile-save")
                    .label("Save")
                    .primary()
                    .disabled(save.is_none() || self.saving)
                    .loading(self.saving)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(edit) = this.about_changed(cx) {
                            this.save(edit, cx);
                        }
                    })),
            ))
    }

    fn render_avatar(&mut self, profile: &OwnProfile, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let save = self.photo_changed(cx);
        let has_photo = match &self.photo {
            StagedPhoto::Current => profile.photo.is_some(),
            StagedPhoto::Remove => false,
            StagedPhoto::New(..) => true,
        };
        let presets = crate::avatar_presets::PRESETS.iter().enumerate().map(|(ix, preset)| {
            let (bg, _) = crate::theme::avatar_colors(preset.color);
            let image = Arc::new(Image::from_bytes(ImageFormat::Svg, preset.svg.to_vec()));
            div()
                .id(("profile-preset", ix))
                .debug_selector(move || format!("profile-preset-{ix}"))
                .size(px(56.))
                .rounded_full()
                .overflow_hidden()
                .bg(bg)
                .cursor_pointer()
                .child(img(image).size(px(56.)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    match crate::avatar_presets::render(&crate::avatar_presets::PRESETS[ix]) {
                        Ok(png) => this.stage_photo(&png, window, cx),
                        Err(error) => tracing::warn!(%error, "couldn't draw a preset photo"),
                    }
                }))
        });
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().flex().justify_center().child(photo_element(&self.photo, profile, 96.)))
            .child(
                div()
                    .flex()
                    .justify_center()
                    .gap_2()
                    .child(
                        Button::new("profile-upload-photo")
                            .label("Upload photo")
                            .outline()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| this.pick_photo(window, cx))),
                    )
                    .when(has_photo, |d| {
                        d.child(
                            Button::new("profile-remove-photo")
                                .label("Remove")
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.photo = StagedPhoto::Remove;
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(div().text_xs().text_color(palette.text_secondary).child("Select an avatar"))
            .child(div().flex().flex_wrap().gap_2().children(presets))
            .child(footer(
                Button::new("profile-cancel").label("Cancel").outline().on_click(cx.listener(|this, _, window, cx| this.back(window, cx))),
                Button::new("profile-save")
                    .label("Save")
                    .primary()
                    .disabled(save.is_none() || self.saving)
                    .loading(self.saving)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(edit) = this.photo_changed(cx) {
                            this.save(edit, cx);
                        }
                    })),
            ))
    }

    fn render_username(&mut self, palette: Palette, cx: &mut Context<Self>) -> impl IntoElement {
        let nickname = self.nickname.read(cx).value().to_string();
        let digits = self.digits.read(cx).value().to_string();
        let show_digits = !nickname.trim().is_empty() || !digits.is_empty();
        let can_save = self.username.reservation.is_some() && !self.username.reserving && !self.username.confirming;
        let problem = self.username.problem.filter(|p| p.inline()).map(|p| p.message(""));
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Choose your username"))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().debug_selector(|| "username-nickname".into()).child(Input::new(&self.nickname)))
                    .when(show_digits, |d| {
                        d.child(div().text_color(palette.text_secondary).child("."))
                            .child(div().w(px(96.)).debug_selector(|| "username-digits".into()).child(Input::new(&self.digits)))
                    }),
            )
            .when_some(problem, |d, problem| {
                d.child(div().debug_selector(|| "username-problem".into()).text_sm().text_color(palette.danger).child(problem))
            })
            .child(
                div()
                    .text_xs()
                    .text_color(palette.text_secondary)
                    .child("Usernames are always paired with a set of numbers.")
                    .child(
                        div()
                            .id("username-learn-more")
                            .text_color(palette.accent)
                            .cursor_pointer()
                            .child("Learn More")
                            .on_click(|_, window, cx| {
                                window.open_alert_dialog(cx, |alert, _, _| {
                                    alert.title("What is this number?").description(
                                        "These digits help keep your username private so you can avoid unwanted messages. Share your username with only the people and groups you’d like to chat with. If you change usernames you’ll get a new set of digits.",
                                    )
                                });
                            }),
                    ),
            )
            .child(footer(
                Button::new("username-cancel").label("Cancel").outline().on_click(cx.listener(|this, _, window, cx| this.back(window, cx))),
                Button::new("username-save")
                    .label("Save")
                    .primary()
                    .disabled(!can_save)
                    .loading(self.username.reserving || self.username.confirming)
                    .on_click(cx.listener(|this, _, window, cx| this.save_username(window, cx))),
            ))
    }

    fn render_link(&mut self, profile: &OwnProfile, palette: Palette, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _ = window;
        let username = profile.username.clone().unwrap_or_default();
        let link = profile.username_link.as_ref().and_then(|l| l.url());
        let color = profile.username_link.as_ref().map_or(0, |l| l.color);
        let (fg, bg) = link_colors(color);
        let white_card = bg == 0xffffff;
        let state = self.state.clone();
        let card = div()
            .debug_selector(|| "username-link-card".into())
            .flex()
            .flex_col()
            .items_center()
            .gap_3()
            .p_5()
            .rounded_xl()
            .bg(gpui_kit::rgb(bg))
            .when(white_card, |d| d.border_1().border_color(gpui_kit::rgb(0xe9e9e9)))
            .child(div().p_3().rounded_lg().bg(gpui_kit::rgb(0xffffff)).map(|d| match &link {
                Some(link) => d.child(crate::views::link::qr_code_colored(link.as_bytes(), 200., gpui_kit::rgb(fg).into())),
                // Desktop's "Something went wrong with your QR code and link".
                None => d.size(px(200.)).flex().items_center().justify_center().child(
                    div().text_sm().text_center().text_color(gpui_kit::rgb(0x5c5c5c)).child("Your QR code and link need a reset."),
                ),
            }))
            .child(
                div()
                    .text_color(if white_card { gpui_kit::rgb(0x000000) } else { gpui_kit::rgb(0xffffff) })
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(username)),
            );
        let colors = LINK_COLORS.iter().map(|(id, fg, bg, _)| {
            let (id, fg, bg) = (*id, *fg, *bg);
            let state = state.clone();
            div()
                .id(("username-link-color", id as usize))
                .debug_selector(move || format!("username-link-color-{id}"))
                .size(px(36.))
                .rounded_full()
                .bg(gpui_kit::rgb(bg))
                .border_2()
                .border_color(if id == color.max(1) { palette.accent } else { gpui_kit::rgb(fg).into() })
                .cursor_pointer()
                .on_click(move |_, _, cx| state.read(cx).send(Command::SetUsernameLinkColor(id)))
        });
        let copy_link = link.clone();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .child(card)
            .when_some(link, |d, link| {
                d.child(div().debug_selector(|| "username-link-url".into()).text_xs().text_color(palette.text_secondary).child(link))
            })
            .when(self.showing_colors, |d| d.child(div().flex().flex_wrap().gap_2().justify_center().children(colors)))
            .child(
                div()
                    .flex()
                    .justify_center()
                    .gap_2()
                    .child(Button::new("username-link-copy").label("Copy to Clipboard").outline().small().disabled(copy_link.is_none()).on_click(
                        move |_, window, cx| {
                            if let Some(link) = &copy_link {
                                cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
                                window.push_notification(Notification::new().message("Copied link"), cx);
                            }
                        },
                    ))
                    .child(Button::new("username-link-colors").label("Color").outline().small().on_click(cx.listener(|this, _, _, cx| {
                        this.showing_colors = !this.showing_colors;
                        cx.notify();
                    })))
                    .child(
                        Button::new("username-link-reset")
                            .label(if self.resetting_link { "Resetting link..." } else { "Reset" })
                            .outline()
                            .small()
                            .disabled(self.resetting_link)
                            .on_click(cx.listener(|this, _, window, cx| this.reset_link(window, cx))),
                    ),
            )
            .child(div().text_xs().text_color(palette.text_secondary).child(
                "Only share your QR code and link with people you trust. When shared others will be able to view your username and start a chat with you.",
            ))
            .child(div().flex().justify_end().child(
                Button::new("username-link-done").label("Done").primary().on_click(cx.listener(|this, _, window, cx| this.back(window, cx))),
            ))
    }
}

/// Opens Desktop's profile editor.
pub fn open(state: Entity<AppState>, window: &mut Window, cx: &mut App) -> Entity<ProfileEditor> {
    open_at(state, Page::Main, window, cx)
}

/// Opens the profile editor at `page` (the QR code screen from About,
/// say).
pub fn open_at(state: Entity<AppState>, page: Page, window: &mut Window, cx: &mut App) -> Entity<ProfileEditor> {
    let editor = cx.new(|cx| ProfileEditor::new(state, window, cx));
    editor.update(cx, |editor, cx| editor.go(page, window, cx));
    let shown = editor.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let title = editor.read(cx).title();
        dialog.title(title).w(px(440.)).child(editor.clone())
    });
    shown
}

#[cfg(test)]
mod tests {
    use gpui_kit::{TestAppContext, VisualTestContext};

    use super::*;
    use crate::views::test_support::{click_at, demo, press, shows, type_into, wait};

    fn own(state: &Entity<AppState>, cx: &mut VisualTestContext) -> OwnProfile {
        state.read_with(cx, |s, _| s.own_profile.clone().unwrap_or_default())
    }

    #[gpui_kit::test]
    fn the_profile_is_edited_as_desktop_edits_it(cx: &mut TestAppContext) {
        let (backend, state, mut cx, _dir) = demo(cx);
        let cx = &mut cx;
        wait(cx, "our profile", &mut |cx| own(&state, cx).given_name == "Riley");
        let editor = cx.update(|window, cx| open(state.clone(), window, cx));
        assert!(shows(cx, "profile-editor"));

        // The name: Save waits for a change, and needs a first name.
        cx.update(|window, cx| editor.update(cx, |e, cx| e.go(Page::Name, window, cx)));
        let can_save = |cx: &mut VisualTestContext| editor.read_with(cx, |e, cx| e.name_changed(cx).is_some());
        assert!(!can_save(cx));
        let given = editor.read_with(cx, |e, _| e.given.clone());
        cx.update(|window, cx| given.update(cx, |i, cx| i.set_value("   ", window, cx)));
        assert!(!can_save(cx), "no first name");
        cx.update(|window, cx| given.update(cx, |i, cx| i.set_value(" Rae ", window, cx)));
        assert!(can_save(cx));
        press(cx, "profile-save");
        wait(cx, "the new name", &mut |cx| own(&state, cx).given_name == "Rae");
        assert_eq!(own(&state, cx).family_name, "Park");
        wait(cx, "the main page", &mut |cx| editor.read_with(cx, |e, _| e.page == Page::Main));

        // About: one of Desktop's presets, emoji and all.
        cx.update(|window, cx| editor.update(cx, |e, cx| e.go(Page::About, window, cx)));
        click_at(cx, "profile-bio-3");
        press(cx, "profile-save");
        wait(cx, "the new about", &mut |cx| own(&state, cx).about == "Coffee lover");
        assert_eq!(own(&state, cx).about_emoji, "☕");

        // A preset photo, cropped and saved as a JPEG.
        cx.update(|window, cx| editor.update(cx, |e, cx| e.go(Page::Avatar, window, cx)));
        click_at(cx, "profile-preset-3");
        press(cx, "profile-save");
        wait(cx, "our photo", &mut |cx| own(&state, cx).photo.is_some());
        let photo = std::fs::read(own(&state, cx).photo.unwrap()).unwrap();
        assert!(photo.starts_with(&[0xff, 0xd8]));
        // And removed.
        cx.update(|window, cx| editor.update(cx, |e, cx| e.go(Page::Avatar, window, cx)));
        press(cx, "profile-remove-photo");
        press(cx, "profile-save");
        wait(cx, "no photo", &mut |cx| own(&state, cx).photo.is_none());
        backend.shutdown();
    }

    #[gpui_kit::test]
    fn usernames_are_reserved_confirmed_linked_and_deleted(cx: &mut TestAppContext) {
        let (backend, state, mut cx, _dir) = demo(cx);
        let cx = &mut cx;
        wait(cx, "our username", &mut |cx| own(&state, cx).username.as_deref() == Some("riley.07"));
        let editor = cx.update(|window, cx| open(state.clone(), window, cx));
        cx.run_until_parked();
        cx.update(|window, cx| editor.update(cx, |e, cx| e.go(Page::Username, window, cx)));
        let nickname = editor.read_with(cx, |e, _| e.nickname.clone());
        let digits = editor.read_with(cx, |e, _| e.digits.clone());
        assert_eq!(digits.read_with(cx, |i, _| i.value().to_string()), "07");

        // Desktop's checks, after its pause.
        type_into(cx, &nickname, "9lives");
        wait(cx, "the problem", &mut |cx| cx.debug_bounds("username-problem").is_some());
        assert_eq!(editor.read_with(cx, |e, _| e.username.problem), Some(UsernameProblem::CheckStartingCharacter));
        type_into(cx, &nickname, "taken");
        wait(cx, "taken", &mut |cx| editor.read_with(cx, |e, _| e.username.problem == Some(UsernameProblem::UsernameNotAvailable)));

        // A free one gets digits, and replacing ours asks first.
        type_into(cx, &nickname, "trail_mix");
        wait(cx, "a reservation", &mut |cx| editor.read_with(cx, |e, _| e.username.reservation.is_some()));
        assert_eq!(digits.read_with(cx, |i, _| i.value().to_string()), "42");
        press(cx, "username-save");
        press(cx, "ok");
        wait(cx, "the new username", &mut |cx| own(&state, cx).username.as_deref() == Some("trail_mix.42"));
        wait(cx, "the main page", &mut |cx| editor.read_with(cx, |e, _| e.page == Page::Main));

        // Typed digits are tried as they are.
        cx.update(|window, cx| editor.update(cx, |e, cx| e.go(Page::Username, window, cx)));
        type_into(cx, &digits, "1a23");
        cx.run_until_parked();
        assert_eq!(digits.read_with(cx, |i, _| i.value().to_string()), "123", "digits only");
        wait(cx, "the typed digits", &mut |cx| {
            editor.read_with(cx, |e, _| e.username.reservation.as_ref().is_some_and(|r| r.username == "trail_mix.123"))
        });
        cx.update(|window, cx| editor.update(cx, |e, cx| e.go(Page::Main, window, cx)));

        // The link: its QR code, recolored, and reset.
        cx.update(|window, cx| editor.update(cx, |e, cx| e.go(Page::UsernameLink, window, cx)));
        wait(cx, "the link", &mut |cx| cx.debug_bounds("username-link-url").is_some());
        let first = own(&state, cx).username_link.unwrap().url().unwrap();
        assert!(first.starts_with("https://signal.me/#eu/"));
        press(cx, "username-link-colors");
        click_at(cx, "username-link-color-4");
        wait(cx, "the color", &mut |cx| own(&state, cx).username_link.is_some_and(|l| l.color == 4));
        press(cx, "username-link-reset");
        press(cx, "ok");
        wait(cx, "a new link", &mut |cx| own(&state, cx).username_link.and_then(|l| l.url()).is_some_and(|u| u != first));
        assert_eq!(own(&state, cx).username_link.unwrap().color, 4, "the color stays");

        // Deleted, after Desktop's question.
        cx.update(|window, cx| editor.update(cx, |e, cx| e.delete_username(window, cx)));
        press(cx, "ok");
        wait(cx, "no username", &mut |cx| own(&state, cx).username.is_none());
        assert!(own(&state, cx).username_link.is_none());
        backend.shutdown();
    }

    #[test]
    fn link_colors_are_desktops() {
        assert_eq!(link_colors(0), (0x2449c0, 0x506ecd), "unknown is blue");
        assert_eq!(link_colors(2), (0x000000, 0xffffff));
        assert_eq!(link_colors(8), (0x7651c5, 0xa183d4));
    }
}
