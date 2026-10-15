//! Desktop's "Story privacy" (`StoriesSettingsModal`, and `SendStoryModal`'s
//! first-post "My Story privacy"): who My Story goes to ("All Signal
//! connections", "All except...", "Only share with..."), replies, custom
//! stories (new, edit, delete), and view receipts. Changes go to the
//! backend, which saves them to Storage Service
//! (`ts/state/ducks/storyDistributionLists`).

use std::collections::BTreeSet;
use std::rc::Rc;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::dialog::{Dialog, DialogFooter};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Disableable as _, Sizable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Entity, FontWeight, SharedString, Window, div, px};
use signal_backend::StoryList;
use signal_store::DirectoryEntry;

use crate::state::AppState;
use crate::theme::{Palette, color_for};
use crate::views::common::person_avatar;

/// Runs once the first post's My Story privacy is saved.
pub type OnSaved = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Page {
    /// The lists, view receipts, and turning stories off.
    Lists,
    /// My Story's viewers and replies.
    MyStory,
    /// A custom story's viewers and replies, by id.
    List(String),
    /// Choosing people.
    Pick(Pick),
    /// Naming a new custom story with the people chosen.
    Name,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Pick {
    /// My Story: "All except...".
    Exclude,
    /// My Story: "Only share with...".
    Only,
    /// A custom story's viewers, by id.
    Viewers(String),
    /// A new custom story's viewers.
    NewStory,
}

pub(crate) struct Privacy {
    state: Entity<AppState>,
    pub(crate) page: Page,
    people: Vec<DirectoryEntry>,
    pub(crate) chosen: BTreeSet<String>,
    search: Entity<InputState>,
    pub(crate) name: Entity<InputState>,
    /// The first post's My Story, saved together; `None` from the Stories
    /// tab, where each change is saved as it's made (as Desktop's are).
    pub(crate) staged: Option<StoryList>,
    on_saved: Option<OnSaved>,
}

impl Privacy {
    /// My Story as shown: staged, else saved.
    fn my_story(&self, cx: &App) -> StoryList {
        self.staged.clone().unwrap_or_else(|| self.state.read(cx).my_story())
    }

    fn set_my_story(&mut self, list: StoryList, cx: &mut App) {
        match &mut self.staged {
            Some(staged) => *staged = list,
            None => self.state.update(cx, |s, cx| s.save_story_list(list, cx)),
        }
    }

    fn list(&self, id: &str, cx: &App) -> Option<StoryList> {
        self.state.read(cx).story_lists.iter().find(|l| l.id == id).cloned()
    }

    fn pick(&mut self, pick: Pick, cx: &App) {
        self.chosen = match &pick {
            Pick::Exclude => {
                let my = self.my_story(cx);
                if my.block_list { my.members.into_iter().collect() } else { BTreeSet::new() }
            }
            Pick::Only => {
                let my = self.my_story(cx);
                if my.block_list { BTreeSet::new() } else { my.members.into_iter().collect() }
            }
            Pick::Viewers(id) => self.list(id, cx).map(|l| l.members.into_iter().collect()).unwrap_or_default(),
            Pick::NewStory => BTreeSet::new(),
        };
        self.page = Page::Pick(pick);
    }

    /// The people chosen are done: Desktop's `hideMyStoriesFrom`,
    /// `updateStoryViewers`, or on to naming a new story.
    pub(crate) fn done_picking(&mut self, cx: &mut App) {
        let Page::Pick(pick) = self.page.clone() else { return };
        if self.chosen.is_empty() {
            return;
        }
        let chosen: Vec<String> = self.chosen.iter().cloned().collect();
        self.page = match pick {
            Pick::Exclude => {
                let list = StoryList { members: chosen, block_list: true, ..self.my_story(cx) };
                self.set_my_story(list, cx);
                Page::MyStory
            }
            Pick::Only => {
                let list = StoryList { members: chosen, block_list: false, ..self.my_story(cx) };
                self.set_my_story(list, cx);
                Page::MyStory
            }
            Pick::Viewers(id) => {
                if let Some(list) = self.list(&id, cx) {
                    self.state.update(cx, |s, cx| s.save_story_list(StoryList { members: chosen, ..list }, cx));
                }
                Page::List(id)
            }
            Pick::NewStory => Page::Name,
        };
    }

    /// Desktop's `createDistributionList`, with the people chosen.
    pub(crate) fn create(&mut self, cx: &mut App) {
        let name = self.name.read(cx).value().trim().to_owned();
        if name.is_empty() || self.chosen.is_empty() {
            return;
        }
        let list = StoryList::new_custom(name, self.chosen.iter().cloned().collect());
        self.state.update(cx, |s, cx| s.save_story_list(list, cx));
        self.chosen.clear();
        self.page = Page::Lists;
    }

    fn back(&mut self) {
        self.page = match &self.page {
            Page::Pick(Pick::Exclude | Pick::Only) => Page::MyStory,
            Page::Pick(Pick::Viewers(id)) => Page::List(id.clone()),
            Page::Name => Page::Pick(Pick::NewStory),
            _ => Page::Lists,
        };
    }

    /// Signal connections, for "n viewers" (Desktop's `getListViewers`).
    fn viewers(&self, list: &StoryList) -> usize {
        if list.is_my_story() && list.block_list {
            self.people.len().saturating_sub(list.members.len())
        } else {
            list.members.len()
        }
    }

    fn name_of(&self, sid: &str) -> String {
        self.people.iter().find(|p| p.id.as_direct() == Some(sid)).map(|p| p.title.clone()).unwrap_or_else(|| "Unknown".into())
    }
}

/// Desktop's "Story privacy", from the Stories tab.
pub fn open(state: Entity<AppState>, window: &mut Window, cx: &mut App) {
    show(state, Page::Lists, None, None, window, cx);
}

/// Desktop's first post to My Story: "My Story privacy", saved together
/// (with `hasSetMyStoriesPrivacy`), then `on_saved`.
pub fn first_post(state: Entity<AppState>, on_saved: OnSaved, window: &mut Window, cx: &mut App) {
    let staged = state.read(cx).my_story();
    show(state, Page::MyStory, Some(staged), Some(on_saved), window, cx);
}

pub(crate) fn show(
    state: Entity<AppState>,
    page: Page,
    staged: Option<StoryList>,
    on_saved: Option<OnSaved>,
    window: &mut Window,
    cx: &mut App,
) -> Option<Entity<Privacy>> {
    if window.has_active_dialog(cx) {
        return None;
    }
    let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search name or number"));
    let name = cx.new(|cx| InputState::new(window, cx).placeholder("Story name (required)"));
    let privacy = cx.new(|_| Privacy { state: state.clone(), page, people: Vec::new(), chosen: BTreeSet::new(), search, name, staged, on_saved });
    let backend = state.read(cx).backend.clone();
    let db = backend.db().clone();
    let me = state.read(cx).account.as_ref().map(|a| a.aci.clone()).unwrap_or_default();
    let query = backend.spawn(async move { db.people().await });
    let loaded = privacy.clone();
    cx.spawn(async move |cx| {
        let people = query.await.unwrap_or_default();
        loaded.update(cx, |p, cx| {
            p.people = people.into_iter().filter(|p| p.id.as_direct().is_some_and(|sid| sid != me)).collect();
            cx.notify();
        });
    })
    .detach();
    let shown = privacy.clone();
    window.open_dialog(cx, move |dialog, window, cx| render(&privacy, dialog, window, cx));
    Some(shown)
}

/// A footer button, findable in tests.
fn tag(id: &'static str, button: Button) -> impl IntoElement {
    div().debug_selector(move || id.into()).child(button)
}

/// A round check, Desktop's checkbox and radio.
fn check(on: bool, palette: Palette) -> impl IntoElement {
    div()
        .size(px(18.))
        .flex_none()
        .rounded_full()
        .border_2()
        .border_color(if on { palette.accent } else { palette.text_secondary })
        .when(on, |d| d.bg(palette.accent))
}

fn row(id: impl Into<SharedString>, palette: Palette) -> gpui_kit::Stateful<gpui_kit::Div> {
    let id: SharedString = id.into();
    let selector = id.to_string();
    div()
        .id(id)
        .debug_selector(move || selector.clone())
        .flex()
        .items_center()
        .gap_3()
        .px_2()
        .py_1p5()
        .rounded_lg()
        .cursor_pointer()
        .hover(|d| d.bg(palette.row_hover))
}

fn heading(text: &'static str, palette: Palette) -> impl IntoElement {
    div().pt_2().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(palette.text_secondary).child(text)
}

fn render(privacy: &Entity<Privacy>, dialog: Dialog, window: &mut Window, cx: &mut App) -> Dialog {
    let palette = Palette::get(cx);
    let page = privacy.read(cx).page.clone();
    let first_post = privacy.read(cx).staged.is_some();
    let (title, body): (String, AnyElement) = match &page {
        Page::Lists => ("Story privacy".into(), render_lists(privacy, palette, cx)),
        Page::MyStory => {
            (if first_post { "My Story privacy" } else { "My Story" }.into(), render_my_story(privacy, palette, cx))
        }
        Page::List(id) => {
            let name = privacy.read(cx).list(id, cx).map(|l| l.name).unwrap_or_default();
            (name, render_list(privacy, id, palette, window, cx))
        }
        Page::Pick(pick) => (
            if *pick == Pick::Exclude { "Hide story from" } else { "Choose Viewers" }.into(),
            render_picker(privacy, palette, cx),
        ),
        Page::Name => ("Name story".into(), render_name(privacy, palette, cx)),
    };
    let footer = DialogFooter::new();
    let back = |id: &'static str, label: &'static str| {
        let privacy = privacy.clone();
        Button::new(id).label(label).outline().on_click(move |_, window, cx| {
            privacy.update(cx, |p, _| p.back());
            window.refresh();
        })
    };
    let footer = match &page {
        Page::Lists => footer.child(Button::new("privacy-done").label("Done").primary().on_click(|_, window, cx| window.close_all_dialogs(cx))),
        Page::MyStory if first_post => {
            let save = privacy.clone();
            footer
                .child(Button::new("privacy-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
                .child(tag("privacy-save", Button::new("privacy-save").label("Save").primary().on_click(move |_, window, cx| {
                    let (list, on_saved, state) = {
                        let p = save.read(cx);
                        (p.staged.clone(), p.on_saved.clone(), p.state.clone())
                    };
                    if let Some(list) = list {
                        state.update(cx, |s, cx| s.save_story_list(list, cx));
                    }
                    window.close_all_dialogs(cx);
                    if let Some(on_saved) = on_saved {
                        on_saved(window, cx);
                    }
                })))
        }
        Page::MyStory | Page::List(_) => footer.child(back("privacy-back", "Back")),
        Page::Pick(_) => {
            let done = privacy.clone();
            // Desktop's pickers need someone chosen (an empty "Only share
            // with" would send to no one).
            let any = !privacy.read(cx).chosen.is_empty();
            footer.child(back("privacy-back", "Back")).child(tag("privacy-picked", Button::new("privacy-picked").label("Done").primary().disabled(!any).on_click(
                move |_, window, cx| {
                    done.update(cx, |p, cx| p.done_picking(cx));
                    window.refresh();
                },
            )))
        }
        Page::Name => {
            let create = privacy.clone();
            let ready = !privacy.read(cx).name.read(cx).value().trim().is_empty();
            footer.child(back("privacy-back", "Back")).child(tag("privacy-create", Button::new("privacy-create").label("Create").primary().disabled(!ready).on_click(
                move |_, window, cx| {
                    create.update(cx, |p, cx| p.create(cx));
                    window.refresh();
                },
            )))
        }
    };
    dialog.title(title).w(px(440.)).child(div().id("story-privacy").debug_selector(|| "story-privacy".into()).child(body)).footer(footer)
}

fn render_lists(privacy: &Entity<Privacy>, palette: Palette, cx: &App) -> AnyElement {
    let p = privacy.read(cx);
    let state = p.state.read(cx);
    let my = state.my_story();
    let receipts = state.story_view_receipts_on();
    let my_viewers = p.viewers(&my);
    let my_label = if my.block_list && !my.members.is_empty() {
        format!("All except... · {} excluded", my.members.len())
    } else {
        format!("{} · {}", my_story_label(&my), viewers_text(my_viewers))
    };
    let lists = state.custom_story_lists();
    let (receipts_state, off_state) = (p.state.clone(), p.state.clone());
    let go = |page: Page| {
        let privacy = privacy.clone();
        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
            privacy.update(cx, |p, cx| match page.clone() {
                Page::Pick(pick) => p.pick(pick, cx),
                page => p.page = page,
            });
            window.refresh();
        }
    };
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_sm().text_color(palette.text_secondary).child(
            "Stories automatically disappear after 24 hours. Choose who can view your story or create new stories with specific viewers or groups.",
        ))
        .child(heading("My Stories", palette))
        .child(row("privacy-new-story", palette).on_click(go(Page::Pick(Pick::NewStory))).child(div().font_weight(FontWeight::SEMIBOLD).child("New Story")))
        .child(
            row("privacy-my-story", palette)
                .on_click(go(Page::MyStory))
                .child(div().flex_1().child("My Story").child(div().text_xs().text_color(palette.text_secondary).child(my_label))),
        )
        .children(lists.into_iter().map(|list| {
            let viewers = viewers_text(list.members.len());
            row(SharedString::from(format!("privacy-list-{}", list.id)), palette).on_click(go(Page::List(list.id.clone()))).child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().child(list.name.clone()))
                    .child(div().text_xs().text_color(palette.text_secondary).child(format!("Custom story · {viewers}"))),
            )
        }))
        .child(div().my_2().h(px(1.)).bg(palette.panel_border))
        .child(
            row("privacy-view-receipts", palette)
                .on_click(move |_, window, cx| {
                    receipts_state.update(cx, |s, cx| s.set_story_view_receipts(!receipts, cx));
                    window.refresh();
                })
                .child(check(receipts, palette))
                .child(div().flex_1().child("View Receipts").child(div().text_xs().text_color(palette.text_secondary).child(
                    "See and share when stories are viewed. If disabled, you won't see when others view your story.",
                ))),
        )
        .child(div().pt_2().text_sm().text_color(palette.text_secondary).child(
            "If you opt out of stories you will no longer be able to share or view stories.",
        ))
        .child(div().child(Button::new("privacy-stories-off").label("Turn off stories").small().outline().on_click(
            move |_, window, cx| crate::views::settings::confirm_stories_off(off_state.clone(), window, cx),
        )))
        .into_any_element()
}

/// Desktop's `getI18nForMyStory`.
fn my_story_label(list: &StoryList) -> &'static str {
    if list.members.is_empty() || list.block_list { "All Signal connections" } else { "Only share with" }
}

fn viewers_text(count: usize) -> String {
    if count == 1 { "1 viewer".into() } else { format!("{count} viewers") }
}

fn replies_row(id: &'static str, on: bool, palette: Palette, toggle: impl Fn(&mut Window, &mut App) + 'static) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .child(heading("Replies & reactions", palette))
        .child(
            row(id, palette)
                .on_click(move |_, window, cx| {
                    toggle(window, cx);
                    window.refresh();
                })
                .child(check(on, palette))
                .child(div().flex_1().child("Allow replies & reactions").child(
                    div().text_xs().text_color(palette.text_secondary).child("Let people who can view your story react and reply."),
                )),
        )
}

/// Desktop's `EditMyStoryPrivacy`.
fn render_my_story(privacy: &Entity<Privacy>, palette: Palette, cx: &App) -> AnyElement {
    let p = privacy.read(cx);
    let my = p.my_story(cx);
    let first_post = p.staged.is_some();
    let all = my.block_list && my.members.is_empty();
    let except = my.block_list && !my.members.is_empty();
    let only = !my.block_list && !my.members.is_empty();
    let all_privacy = privacy.clone();
    let choose = |pick: Pick| {
        let privacy = privacy.clone();
        move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
            privacy.update(cx, |p, cx| p.pick(pick.clone(), cx));
            window.refresh();
        }
    };
    let replies_privacy = privacy.clone();
    let replies = my.allows_replies;
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_sm().text_color(palette.text_secondary).child(if first_post {
            "Choose which Signal connections can view your story. You can always change this in privacy settings."
        } else {
            "Choose who can view your story. Changes won't affect stories you've already sent."
        }))
        .child(heading("Who can view this story", palette))
        .child(
            row("privacy-all", palette)
                .on_click(move |_, window, cx| {
                    all_privacy.update(cx, |p, cx| {
                        let list = StoryList { members: Vec::new(), block_list: true, ..p.my_story(cx) };
                        p.set_my_story(list, cx);
                    });
                    window.refresh();
                })
                .child(check(all, palette))
                .child(div().flex_1().child("All Signal connections"))
                .child(div().text_xs().text_color(palette.text_secondary).child(viewers_text(p.people.len()))),
        )
        .child(
            row("privacy-except", palette)
                .on_click(choose(Pick::Exclude))
                .child(check(except, palette))
                .child(div().flex_1().child("All except..."))
                .when(except, |d| d.child(div().text_xs().text_color(palette.text_secondary).child(format!("{} excluded", my.members.len())))),
        )
        .child(
            row("privacy-only", palette)
                .on_click(choose(Pick::Only))
                .child(check(only, palette))
                .child(div().flex_1().child("Only share with..."))
                .when(only, |d| d.child(div().text_xs().text_color(palette.text_secondary).child(viewers_text(my.members.len())))),
        )
        .child(replies_row("privacy-replies", replies, palette, move |_, cx| {
            replies_privacy.update(cx, |p, cx| {
                let list = StoryList { allows_replies: !replies, ..p.my_story(cx) };
                p.set_my_story(list, cx);
            });
        }))
        .into_any_element()
}

/// Desktop's `DistributionListSettingsModal` for a custom story.
fn render_list(privacy: &Entity<Privacy>, id: &str, palette: Palette, _window: &mut Window, cx: &App) -> AnyElement {
    let p = privacy.read(cx);
    let Some(list) = p.list(id, cx) else {
        return div().text_sm().text_color(palette.text_secondary).child("This story was deleted.").into_any_element();
    };
    let add = privacy.clone();
    let add_id = list.id.clone();
    let state = p.state.clone();
    let members: Vec<(String, String)> = list.members.iter().map(|m| (m.clone(), p.name_of(m))).collect();
    let people_rows = members.into_iter().map(|(sid, title)| {
        let (state, list, remove) = (state.clone(), list.clone(), sid.clone());
        let photo = state.read(cx).photo_of(&sid).map(std::path::Path::to_path_buf);
        div()
            .flex()
            .items_center()
            .gap_3()
            .px_2()
            .py_1()
            .child(person_avatar(photo.as_deref(), &title, color_for(&format!("d:{sid}")), px(28.)))
            .child(div().flex_1().truncate().child(title.clone()))
            .child(Button::new(SharedString::from(format!("privacy-remove-{sid}"))).label("Remove").small().ghost().on_click(
                move |_, window, cx| {
                    let (state, list, remove) = (state.clone(), list.clone(), remove.clone());
                    let heading = format!("Remove {title}");
                    window.open_alert_dialog(cx, move |alert, _, _| {
                        let (state, list, remove) = (state.clone(), list.clone(), remove.clone());
                        alert
                            .title(heading.clone())
                            .description("This person will no longer see your story.")
                            .confirm()
                            .ok_text("Remove")
                            .ok_variant(ButtonVariant::Danger)
                            .on_ok(move |_, window, cx| {
                                let members = list.members.iter().filter(|m| **m != remove).cloned().collect();
                                state.update(cx, |s, cx| s.save_story_list(StoryList { members, ..list.clone() }, cx));
                                window.refresh();
                                true
                            })
                    });
                },
            ))
    });
    let (replies_state, replies_list) = (state.clone(), list.clone());
    let (delete_state, delete_list, delete_privacy) = (state.clone(), list.clone(), privacy.clone());
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(heading("Who can view this story", palette))
        .child(row("privacy-add-viewer", palette).on_click(move |_, window, cx| {
            add.update(cx, |p, cx| p.pick(Pick::Viewers(add_id.clone()), cx));
            window.refresh();
        }).child(div().font_weight(FontWeight::SEMIBOLD).child("Add viewer")))
        .children(people_rows)
        .child(replies_row("privacy-list-replies", list.allows_replies, palette, move |_, cx| {
            let list = StoryList { allows_replies: !replies_list.allows_replies, ..replies_list.clone() };
            replies_state.update(cx, |s, cx| s.save_story_list(list, cx));
        }))
        .child(div().pt_2().child(Button::new("privacy-delete-list").label("Delete custom story").small().outline().on_click(
            move |_, window, cx| {
                let (state, list, privacy) = (delete_state.clone(), delete_list.clone(), delete_privacy.clone());
                window.open_alert_dialog(cx, move |alert, _, _| {
                    let (state, list, privacy) = (state.clone(), list.clone(), privacy.clone());
                    alert
                        .description(format!("Are you sure you want to delete \"{}\"? Updates shared to this story will also be deleted.", list.name))
                        .confirm()
                        .ok_text("Delete")
                        .ok_variant(ButtonVariant::Danger)
                        .on_ok(move |_, window, cx| {
                            state.update(cx, |s, cx| s.delete_story_list(list.id.clone(), cx));
                            privacy.update(cx, |p, _| p.page = Page::Lists);
                            window.refresh();
                            true
                        })
                });
            },
        )))
        .into_any_element()
}

/// Desktop's `EditDistributionListModal` people list.
fn render_picker(privacy: &Entity<Privacy>, palette: Palette, cx: &App) -> AnyElement {
    let p = privacy.read(cx);
    let needle = p.search.read(cx).value().trim().to_lowercase();
    let state = p.state.read(cx);
    let rows: Vec<AnyElement> = p
        .people
        .iter()
        .filter(|person| {
            needle.is_empty()
                || person.title.to_lowercase().contains(&needle)
                || person.phone.as_deref().is_some_and(|ph| ph.contains(&needle))
        })
        .map(|person| {
            let sid = person.id.as_direct().unwrap_or_default().to_owned();
            let on = p.chosen.contains(&sid);
            let toggle = privacy.clone();
            let flip = sid.clone();
            row(SharedString::from(format!("privacy-pick-{sid}")), palette)
                .on_click(move |_, window, cx| {
                    toggle.update(cx, |p, _| {
                        if !p.chosen.remove(&flip) {
                            p.chosen.insert(flip.clone());
                        }
                    });
                    window.refresh();
                })
                .child(person_avatar(state.photo_of(&sid), &person.title, color_for(&person.id.0), px(32.)))
                .child(div().flex_1().truncate().child(person.title.clone()))
                .child(check(on, palette))
                .into_any_element()
        })
        .collect();
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(Input::new(&p.search))
        .child(div().text_xs().text_color(palette.text_secondary).child(format!("{} chosen", p.chosen.len())))
        .child(div().id("privacy-people").h(px(320.)).overflow_y_scroll().children(rows))
        .into_any_element()
}

fn render_name(privacy: &Entity<Privacy>, palette: Palette, cx: &App) -> AnyElement {
    let p = privacy.read(cx);
    let names: Vec<String> = p.chosen.iter().map(|sid| p.name_of(sid)).collect();
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(Input::new(&p.name))
        .child(div().text_xs().text_color(palette.text_secondary).child("Only you can see the name of this story."))
        .child(heading("Viewers", palette))
        .child(div().text_sm().child(names.join(", ")))
        .into_any_element()
}
