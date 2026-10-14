//! The screens the app waits on, as Desktop does: the history transfer after
//! linking, and catching up on the messages queued while it was closed.
//! Nothing is read or sent from a half-loaded database.

use chrono::{Local, TimeZone as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, FontWeight, IntoElement, div, px};
use signal_backend::{Command, HistoryProgress};

use crate::state::{AppState, CatchUp};
use crate::theme::Palette;
use crate::views::common::format_size;

const BAR_WIDTH: f32 = 360.;

/// Desktop's "Syncing messages" screen.
pub fn render_history(state: &Entity<AppState>, progress: &HistoryProgress, cx: &mut App) -> impl IntoElement {
    let palette = Palette::get(cx);
    // `None`: no total yet, so the bar slides.
    let (fraction, hint) = match progress {
        HistoryProgress::Downloading { done, total: Some(total) } if *total > 0 => {
            let fraction = (*done as f64 / *total as f64).clamp(0., 1.);
            let hint = format!(
                "Downloading {} of {} ({:.0}%)…",
                format_size(*done),
                format_size(*total),
                fraction * 100.
            );
            (Some(fraction), hint)
        }
        HistoryProgress::Downloading { done, .. } => (None, format!("Downloading {}…", format_size(*done))),
        HistoryProgress::Importing { done, total } => {
            let fraction = if *total == 0 { 1. } else { (*done as f64 / *total as f64).clamp(0., 1.) };
            (Some(fraction), "Finalizing message transfer…".into())
        }
        _ => (None, "Waiting for your phone to send your messages…".into()),
    };
    // Skipping mid-import would leave a partial history.
    let can_skip = matches!(progress, HistoryProgress::Waiting | HistoryProgress::Downloading { .. });

    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .bg(palette.background)
        .text_color(palette.text)
        .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child("Syncing messages"))
        .child(bar("history-progress", fraction, palette))
        .child(div().text_sm().text_color(palette.text_secondary).child(hint))
        .child(
            div()
                .mt_2()
                .max_w(px(BAR_WIDTH))
                .text_center()
                .text_color(palette.text_secondary)
                .child("This may take a few minutes. Keep Signal open on your phone and this app open until it finishes."),
        )
        .when(can_skip, |d| {
            let state = state.clone();
            d.child(
                div().mt_4().child(
                    Button::new("skip-history")
                        .ghost()
                        .label("Continue without your messages")
                        .on_click(move |_, _, cx| state.read(cx).send(Command::SkipHistory)),
                ),
            )
        })
}

/// Desktop's "Loading messages from 3 days ago…" screen.
pub fn render_catch_up(catch_up: &CatchUp, cx: &mut App) -> impl IntoElement {
    let palette = Palette::get(cx);
    let page = div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_3()
        .bg(palette.background)
        .text_color(palette.text_secondary);
    match (catch_up.fraction(), catch_up.latest) {
        (Some(fraction), Some(latest)) => page
            .child(bar("catch-up-progress", Some(fraction as f64), palette))
            .child(loading_message(latest, Local::now())),
        // Until the first message: the same spinner as before the account loads.
        _ => page.child(Spinner::new()),
    }
}

fn bar(id: &'static str, fraction: Option<f64>, palette: Palette) -> impl IntoElement {
    let progress = Progress::new(id).w(px(BAR_WIDTH)).color(palette.accent);
    match fraction {
        Some(fraction) => progress.value((fraction * 100.) as f32),
        None => progress.loading(true),
    }
}

/// Desktop's wording, by whole days between `latest` and midnight `now`.
fn loading_message(latest: i64, now: chrono::DateTime<Local>) -> String {
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|m| Local.from_local_datetime(&m).earliest())
        .map_or(now.timestamp_millis(), |m| m.timestamp_millis());
    const DAY: i64 = 24 * 60 * 60 * 1000;
    // Whole days, rounded up (`i64::div_ceil` isn't stable).
    let before_midnight = (midnight - latest).max(0);
    match (before_midnight + DAY - 1) / DAY {
        ..=0 => "Loading messages from today…".into(),
        1 => "Loading messages from yesterday…".into(),
        days => format!("Loading messages from {days} days ago…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_how_far_back_it_is() {
        let now = Local.with_ymd_and_hms(2026, 10, 14, 15, 0, 0).unwrap();
        let at = |d: u32, h: u32| Local.with_ymd_and_hms(2026, 10, d, h, 0, 0).unwrap().timestamp_millis();
        assert_eq!(loading_message(at(14, 9), now), "Loading messages from today…");
        assert_eq!(loading_message(at(13, 23), now), "Loading messages from yesterday…");
        assert_eq!(loading_message(at(13, 1), now), "Loading messages from yesterday…");
        assert_eq!(loading_message(at(12, 20), now), "Loading messages from 2 days ago…");
        assert_eq!(loading_message(at(4, 12), now), "Loading messages from 10 days ago…");
    }

    #[test]
    fn catch_up_runs_from_the_first_message_to_when_it_started() {
        let mut catch_up = CatchUp { since: 1_000, first: None, latest: None };
        assert_eq!(catch_up.fraction(), None);
        catch_up.first = Some(200);
        catch_up.latest = Some(600);
        assert_eq!(catch_up.fraction(), Some(0.5));
        // Messages sent after it started don't overshoot.
        catch_up.latest = Some(5_000);
        assert_eq!(catch_up.fraction(), Some(1.));
    }
}
