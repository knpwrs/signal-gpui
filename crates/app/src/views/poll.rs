//! Polls as Signal Desktop shows them: the poll in its bubble
//! (`ts/components/conversation/poll-message/PollMessageContents.dom.tsx`),
//! "Poll details" (`PollVotesModal.dom.tsx`) and "New poll"
//! (`ts/components/PollCreateModal.dom.tsx`).

use std::collections::BTreeSet;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{Disableable as _, Icon, Sizable as _, WindowExt as _};
use gpui_kit::prelude::*;
use gpui_kit::{AnyElement, App, Context, Entity, FontWeight, Hsla, SharedString, Subscription, WeakEntity, Window, div, px};
use signal_backend::Command;
use signal_store::{
    ConversationId, Message, POLL_OPTION_MAX, POLL_OPTIONS_MAX, POLL_OPTIONS_MIN, POLL_QUESTION_MAX_SEND, Poll, PollTally,
};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::state::AppState;
use crate::theme::{Palette, color_for};
use crate::views::common::person_avatar;
use crate::views::conversation::ConversationView;

/// Desktop's poll is 275px wide.
pub const POLL_WIDTH: f32 = 275.;
const CHECKBOX: f32 = 24.;
const CHECKBOX_GAP: f32 = 12.;

/// Desktop's status line under the question.
pub fn status_text(poll: &Poll) -> &'static str {
    if poll.ended {
        "Poll · Final results"
    } else if poll.allow_multiple {
        "Poll · Select one or more"
    } else {
        "Poll · Select one"
    }
}

/// The options our vote names after clicking option `index` to `checked`
/// (Desktop's `handlePollOptionClicked`): starting from what shows as
/// chosen (the vote still sending, else the one sent), a single-choice
/// poll drops the rest.
pub fn vote_after_click(tally: &PollTally, allow_multiple: bool, index: u32, checked: bool) -> Vec<u32> {
    let mut chosen: BTreeSet<u32> = tally.chosen().clone();
    if checked {
        if !allow_multiple {
            chosen.clear();
        }
        chosen.insert(index);
    } else {
        chosen.remove(&index);
    }
    chosen.into_iter().collect()
}

/// The poll inside its bubble. `fg` is the bubble's text color and
/// `secondary` its quieter one; `can_vote` is false where we can't send
/// (a group we left, the Signal chat).
#[allow(clippy::too_many_arguments)]
pub fn render_poll(
    message: &Message,
    poll: &Poll,
    outgoing: bool,
    fg: Hsla,
    secondary: Hsla,
    can_vote: bool,
    palette: Palette,
    view: WeakEntity<ConversationView>,
    state: Entity<AppState>,
) -> AnyElement {
    let id = message.id;
    let tally = poll.tally();
    let voting = can_vote && !poll.ended;
    let bar_width = POLL_WIDTH - if voting { CHECKBOX + CHECKBOX_GAP } else { 0. };
    // Incoming bubbles use the accent; ours, which are the accent, use white.
    let (fill, track) = if outgoing { (fg, Hsla { a: 0.3, ..fg }) } else { (palette.accent, Hsla { a: 0.12, ..fg }) };
    let chosen = tally.chosen().clone();
    let pending = tally.pending.is_some();
    let options = poll.options.iter().enumerate().map(|(ix, option)| {
        let count = tally.count(ix);
        let checked = chosen.contains(&(ix as u32));
        let next = vote_after_click(&tally, poll.allow_multiple, ix as u32, !checked);
        let state = state.clone();
        div()
            .flex()
            .gap(px(CHECKBOX_GAP))
            .when(voting, |d| {
                d.child(
                    div()
                        .id(SharedString::from(format!("poll-{id}-option-{ix}")))
                        .debug_selector(move || format!("poll-{id}-option-{ix}"))
                        .flex_none()
                        .size(px(CHECKBOX))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .border_2()
                        .cursor_pointer()
                        .border_color(if checked { fill } else { secondary })
                        .when(checked, |d| d.bg(fill))
                        // A vote still sending shows, fainter.
                        .when(pending, |d| d.opacity(0.6))
                        .on_click(move |_, _, cx| {
                            state.read(cx).send(Command::VotePoll { message_id: id, options: next.clone() });
                        })
                        .when(checked, |d| {
                            d.child(Icon::new(IconName::Check).size_4().text_color(if outgoing { palette.accent } else { palette.on_accent }))
                        }),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .justify_between()
                            .gap_3()
                            .child(div().min_w_0().child(option.clone()))
                            .when(tally.total_votes > 0, |d| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        // Ended: a check where we voted (Desktop's "You voted").
                                        .when(poll.ended && tally.ours.contains(&(ix as u32)), |d| {
                                            d.child(
                                                div()
                                                    .size(px(16.))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded_full()
                                                    .bg(fill)
                                                    .child(Icon::new(IconName::Check).size_3().text_color(if outgoing {
                                                        palette.accent
                                                    } else {
                                                        palette.on_accent
                                                    })),
                                            )
                                        })
                                        .child(
                                            div()
                                                .debug_selector(move || format!("poll-{id}-count-{ix}"))
                                                .text_sm()
                                                .text_color(secondary)
                                                .child(count.to_string()),
                                        ),
                                )
                            }),
                    )
                    .child(
                        div()
                            .w(px(bar_width))
                            .h(px(8.))
                            .rounded_full()
                            .bg(track)
                            .child(div().h_full().w(px(bar_width * tally.share(ix))).rounded_full().bg(fill)),
                    ),
            )
    });
    div()
        .debug_selector(move || format!("poll-{id}"))
        .w(px(POLL_WIDTH))
        .max_w_full()
        .mt_1()
        .flex()
        .flex_col()
        .child(div().mb_1().font_weight(FontWeight::SEMIBOLD).child(poll.question.clone()))
        .child(div().mb_4().text_sm().font_weight(FontWeight::MEDIUM).text_color(secondary).child(status_text(poll)))
        .child(div().flex().flex_col().gap_3().children(options))
        .child(div().mt_4().h(px(40.)).flex().items_center().justify_center().map(|d| {
            if tally.total_votes > 0 {
                d.child(
                    Button::new(SharedString::from(format!("poll-{id}-votes")))
                        .label("View votes")
                        .small()
                        .on_click(move |_, window, cx| {
                            let state = state_for_votes(&view, cx);
                            open_votes(view.clone(), state, id, window, cx)
                        }),
                )
            } else {
                d.text_sm().child("No votes")
            }
        }))
        .into_any_element()
}

fn state_for_votes(view: &WeakEntity<ConversationView>, cx: &App) -> Option<Entity<AppState>> {
    view.upgrade().map(|v| v.read(cx).state.clone())
}

/// Desktop's "Poll details": each option, its votes and who cast them,
/// the winning ones starred, and "End poll" for our own poll. It follows
/// the message as votes arrive.
pub fn open_votes(view: WeakEntity<ConversationView>, state: Option<Entity<AppState>>, message_id: i64, window: &mut Window, cx: &mut App) {
    let Some(state) = state else { return };
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let found = view.upgrade().and_then(|v| {
            let v = v.read(cx);
            let message = v.messages.iter().find(|m| m.id == message_id)?.clone();
            let names: Vec<(Option<String>, String)> = message
                .poll
                .iter()
                .flat_map(|p| p.votes.iter().map(|vote| vote.author.clone()))
                .map(|author| {
                    let name = v.reactor_name(author.as_deref(), cx);
                    (author, name)
                })
                .collect();
            Some((message, names))
        });
        let dialog = dialog.title("Poll details").w(px(440.));
        let Some((message, names)) = found else { return dialog };
        let Some(poll) = message.poll.clone() else { return dialog };
        let tally = poll.tally();
        let most = tally.most();
        let name_of = |author: &Option<String>| {
            names.iter().find(|(a, _)| a == author).map(|(_, n)| n.clone()).unwrap_or_else(|| "Unknown".into())
        };
        let options = poll.options.iter().enumerate().map(|(ix, option)| {
            let voters = tally.voters.get(ix).cloned().unwrap_or_default();
            let winning = !voters.is_empty() && voters.len() == most;
            div()
                .flex()
                .flex_col()
                .gap_3()
                .pb_4()
                .when(ix + 1 < poll.options.len(), |d| d.border_b_1().border_color(palette.panel_border))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap_3()
                        .child(div().flex_1().min_w_0().font_weight(FontWeight::SEMIBOLD).child(option.clone()))
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_sm()
                                .when(winning, |d| d.child(div().debug_selector(move || format!("poll-winning-{ix}")).child("★")))
                                .when(!voters.is_empty(), |d| {
                                    let n = voters.len();
                                    d.child(if n == 1 { "1 vote".to_owned() } else { format!("{n} votes") })
                                }),
                        ),
                )
                .map(|d| {
                    if voters.is_empty() {
                        d.child(div().text_sm().text_color(palette.text_secondary).child("No votes"))
                    } else {
                        d.child(div().flex().flex_col().gap_4().children(voters.iter().map(|author| {
                            let name = name_of(author);
                            let photo = author.as_deref().and_then(|a| state.read(cx).photo_of(a).map(|p| p.to_path_buf()));
                            div()
                                .flex()
                                .items_center()
                                .gap_3()
                                .child(person_avatar(photo.as_deref(), &name, color_for(author.as_deref().unwrap_or("")), px(36.)))
                                .child(div().flex_1().min_w_0().truncate().child(name))
                        })))
                    }
                })
        });
        let can_end = message.direction == signal_store::Direction::Outgoing && !poll.ended;
        let end_state = state.clone();
        dialog
            .child(
                div()
                    .id("poll-details")
                    .max_h(px(480.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(div().font_weight(FontWeight::SEMIBOLD).child("Question"))
                            .child(div().rounded_md().px_3().py_1().bg(palette.input_bg).child(poll.question.clone())),
                    )
                    .children(options)
                    .when(tally.total_votes == 0, |d| {
                        d.child(div().flex().justify_center().text_color(palette.text_secondary).child("No votes"))
                    }),
            )
            .when(can_end, |dialog| {
                dialog.footer(DialogFooter::new().child(Button::new("poll-end").label("End poll").outline().on_click(
                    move |_, window, cx| {
                        end_state.read(cx).send(Command::EndPoll { message_id });
                        window.close_all_dialogs(cx);
                    },
                )))
            })
    });
}

fn graphemes(text: &str) -> usize {
    text.graphemes(true).count()
}

fn truncated(text: &str, max: usize) -> Option<String> {
    (graphemes(text) > max).then(|| text.graphemes(true).take(max).collect())
}

/// Desktop's "New poll" form: the question, options that grow and shrink
/// as you type, and "Allow multiple votes".
pub struct PollForm {
    pub question: Entity<InputState>,
    pub options: Vec<Entity<InputState>>,
    pub allow_multiple: bool,
    /// The first reason it can't be sent, after Send was pressed.
    pub error: Option<&'static str>,
    _subscriptions: Vec<Subscription>,
}

impl PollForm {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let question = cx.new(|cx| InputState::new(window, cx).placeholder("Ask a question"));
        let subscription = cx.subscribe_in(&question, window, |this, input, event: &InputEvent, window, cx| {
            if let InputEvent::Change = event {
                Self::limit(input, POLL_QUESTION_MAX_SEND, window, cx);
                this.error = None;
                cx.notify();
            }
        });
        let mut form = Self { question, options: Vec::new(), allow_multiple: true, error: None, _subscriptions: vec![subscription] };
        // Desktop starts with two empty options, and multiple votes allowed.
        for _ in 0..POLL_OPTIONS_MIN {
            form.add_option(window, cx);
        }
        form
    }

    /// Desktop's `maxLengthCount`: no more than `max` characters.
    fn limit(input: &Entity<InputState>, max: usize, window: &mut Window, cx: &mut App) {
        let value = input.read(cx).value().to_string();
        if let Some(cut) = truncated(&value, max) {
            input.update(cx, |input, cx| input.set_value(cut, window, cx));
        }
    }

    fn add_option(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let number = self.options.len() + 1;
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(format!("Option {number}")));
        self._subscriptions.push(cx.subscribe_in(&input, window, |this, input, event: &InputEvent, window, cx| match event {
            InputEvent::Change => {
                Self::limit(input, POLL_OPTION_MAX, window, cx);
                this.on_option_changed(input, window, cx);
            }
            // Enter moves to the next option.
            InputEvent::PressEnter { .. } => {
                let next = this.options.iter().position(|o| o == input).and_then(|ix| this.options.get(ix + 1)).cloned();
                if let Some(next) = next {
                    next.update(cx, |input, cx| input.focus(window, cx));
                }
            }
            _ => {}
        }));
        self.options.push(input);
    }

    /// Desktop's `computeOptionsAfterChange`: typing in the last option adds
    /// an empty one (up to 10); emptying one in the middle removes it, and
    /// emptying the one before an empty last removes the last (down to 2).
    fn on_option_changed(&mut self, input: &Entity<InputState>, window: &mut Window, cx: &mut Context<Self>) {
        self.error = None;
        let Some(ix) = self.options.iter().position(|o| o == input) else { return };
        let filled = |o: &Entity<InputState>, cx: &App| !o.read(cx).value().trim().is_empty();
        let has_text = filled(input, cx);
        let len = self.options.len();
        let last = ix + 1 == len;
        if last && has_text && len < POLL_OPTIONS_MAX {
            self.add_option(window, cx);
        }
        let len = self.options.len();
        let mut removed = None;
        if ix + 2 == len && !has_text && len > POLL_OPTIONS_MIN && !filled(&self.options[len - 1], cx) {
            self.options.pop();
            removed = Some(self.options.len());
        }
        if !last && !has_text && self.options.len() > POLL_OPTIONS_MIN && removed.is_none() {
            self.options.remove(ix);
            removed = Some(ix);
            if self.options.last().is_some_and(|o| filled(o, cx)) && self.options.len() < POLL_OPTIONS_MAX {
                self.add_option(window, cx);
            }
        }
        if let Some(removed) = removed {
            self.renumber(window, cx);
            let focus = removed.min(self.options.len() - 1);
            self.options[focus].update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    /// Placeholders follow the options' places ("Option 1", "Option 2"…).
    fn renumber(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for (ix, input) in self.options.iter().enumerate() {
            input.update(cx, |input, cx| input.set_placeholder(format!("Option {}", ix + 1), window, cx));
        }
    }

    pub fn values(&self, cx: &App) -> (String, Vec<String>) {
        let question = self.question.read(cx).value().to_string();
        let options = self.options.iter().map(|o| o.read(cx).value().to_string()).collect();
        (question, options)
    }

    /// Desktop's Send is enabled once anything is typed.
    pub fn can_send(&self, cx: &App) -> bool {
        let (question, options) = self.values(cx);
        !question.trim().is_empty() || options.iter().any(|o| !o.trim().is_empty())
    }
}

/// Characters left, shown once within 30 of the limit (Desktop's
/// `whenToShowRemainingCount`).
fn remaining(input: &Entity<InputState>, max: usize, cx: &App) -> Option<String> {
    let left = max.saturating_sub(graphemes(&input.read(cx).value()));
    (left <= 30).then(|| left.to_string())
}

impl Render for PollForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = Palette::get(cx);
        let field = |input: &Entity<InputState>, max: usize, cx: &App| {
            let left = remaining(input, max, cx);
            Input::new(input).when_some(left, |i, left| i.suffix(div().text_xs().text_color(palette.text_secondary).child(left)))
        };
        let allow = self.allow_multiple;
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Question"))
            .child(div().debug_selector(|| "poll-question".into()).child(field(&self.question, POLL_QUESTION_MAX_SEND, cx)))
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Options"))
            .child(div().flex().flex_col().gap_3().children(self.options.iter().enumerate().map(|(ix, input)| {
                div().debug_selector(move || format!("poll-new-option-{ix}")).child(field(input, POLL_OPTION_MAX, cx))
            })))
            .child(div().border_t_1().border_color(palette.panel_border))
            .child(
                div().flex().items_center().gap_3().child(div().flex_1().child("Allow multiple votes")).child(
                    Switch::new("poll-allow-multiple").checked(allow).on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.allow_multiple = *checked;
                        cx.notify();
                    })),
                ),
            )
            .when_some(self.error, |d, error| {
                d.child(div().flex().justify_center().gap_1().text_sm().child(div().text_color(palette.danger).child("⚠")).child(error))
            })
    }
}

/// Desktop's "New poll" dialog.
pub fn open_create(state: Entity<AppState>, conversation: ConversationId, window: &mut Window, cx: &mut App) {
    let form = cx.new(|cx| PollForm::new(window, cx));
    form.read(cx).question.clone().update(cx, |input, cx| input.focus(window, cx));
    window.open_dialog(cx, move |dialog, _, cx| {
        let ready = form.read(cx).can_send(cx);
        let (send_form, send_state, chat) = (form.clone(), state.clone(), conversation.clone());
        dialog
            .title("New poll")
            .w(px(440.))
            .child(form.clone())
            .footer(
                DialogFooter::new()
                    .child(Button::new("poll-cancel").label("Cancel").outline().on_click(|_, window, cx| window.close_all_dialogs(cx)))
                    .child(Button::new("poll-send").label("Send").primary().disabled(!ready).on_click(move |_, window, cx| {
                        if let Some(command) = submit(&send_form, &chat, cx) {
                            send_state.read(cx).send(command);
                            window.close_all_dialogs(cx);
                        }
                    })),
            )
    });
}

/// The poll to send, or (with the form showing why) nothing yet.
pub fn submit(form: &Entity<PollForm>, conversation: &ConversationId, cx: &mut App) -> Option<Command> {
    let (question, options) = form.read(cx).values(cx);
    if let Some(error) = signal_store::poll_send_error(&question, &options) {
        form.update(cx, |form, cx| {
            form.error = Some(error);
            cx.notify();
        });
        return None;
    }
    let allow_multiple = form.read(cx).allow_multiple;
    Some(Command::SendPoll { conversation: conversation.clone(), question, options, allow_multiple })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{TestAppContext, VisualTestContext};

    #[test]
    fn clicking_an_option_follows_desktop() {
        let mut poll = Poll::new("Q".into(), vec!["A".into(), "B".into(), "C".into()], false);
        let vote = |options: Vec<u32>, vote_count| signal_store::PollVote { author: None, options, vote_count, sent_at: 1, pending: false };
        poll.apply_vote(vote(vec![0], 1), signal_store::VoteSource::Sync);
        let tally = poll.tally();
        // Single choice: another option replaces ours; unchecking clears it.
        assert_eq!(vote_after_click(&tally, false, 1, true), [1]);
        assert_eq!(vote_after_click(&tally, false, 0, false), Vec::<u32>::new());
        // Multiple: they add up.
        assert_eq!(vote_after_click(&tally, true, 2, true), [0, 2]);
        // A vote still sending is what we build on.
        poll.allow_multiple = true;
        poll.apply_vote(vote(vec![2], 2), signal_store::VoteSource::ThisDevice);
        assert_eq!(vote_after_click(&poll.tally(), true, 1, true), [1, 2]);
    }

    #[gpui_kit::test]
    fn the_new_poll_form_grows_and_checks_like_desktops(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let window = cx.add_window(PollForm::new);
        let form = window.root(cx).unwrap();
        let mut cx = VisualTestContext::from_window(*window, cx);
        // Typed, so the inputs report their changes as they do in use.
        let set = |cx: &mut VisualTestContext, input: &Entity<InputState>, text: &str| {
            let input = input.clone();
            cx.update(|window, cx| input.update(cx, |input, cx| input.focus(window, cx)));
            cx.run_until_parked();
            cx.simulate_keystrokes("cmd-a backspace");
            if !text.is_empty() {
                cx.simulate_input(text);
            }
            cx.run_until_parked();
        };
        let count = |cx: &mut VisualTestContext| form.read_with(cx, |f, _| f.options.len());
        assert_eq!(count(&mut cx), 2);
        assert!(form.read_with(&cx, |f, cx| f.allow_multiple && !f.can_send(cx)));
        // Typing in the last option adds one.
        let second = form.read_with(&cx, |f, _| f.options[1].clone());
        set(&mut cx, &second, "Pho");
        assert_eq!(count(&mut cx), 3);
        // Emptying it again takes the empty last one away.
        set(&mut cx, &second, "");
        assert_eq!(count(&mut cx), 2);
        // Too long: cut to 100 characters.
        let question = form.read_with(&cx, |f, _| f.question.clone());
        set(&mut cx, &question, &"q".repeat(150));
        assert_eq!(question.read_with(&cx, |q, _| q.value().len()), 100);
        // Sending with one option says why it can't.
        set(&mut cx, &second, "Pho");
        let chat = ConversationId("g:00".into());
        assert!(cx.update(|_, cx| submit(&form, &chat, cx)).is_none());
        assert_eq!(form.read_with(&cx, |f, _| f.error), Some("Poll requires at least 2 options"));
        let first = form.read_with(&cx, |f, _| f.options[0].clone());
        set(&mut cx, &first, "Tacos");
        let Some(Command::SendPoll { question: q, options, allow_multiple, .. }) = cx.update(|_, cx| submit(&form, &chat, cx)) else {
            panic!("not sent")
        };
        assert_eq!(q.len(), 100);
        assert_eq!((options, allow_multiple), (vec!["Tacos".to_owned(), "Pho".to_owned(), String::new()], true));
    }
}
