//! Desktop's safety number view: the 60 digits and a QR code to compare
//! with the other person's device, and "Mark as verified".

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::prelude::*;
use gpui_kit::{App, Entity, FontWeight, Window, div, px, white};
use signal_backend::Command;
use signal_store::ConversationId;

use crate::state::AppState;
use crate::theme::Palette;

const QR_SIZE: f32 = 180.;

/// `conversation` is their 1:1 chat's id (whether or not the chat exists);
/// `name` is theirs.
pub fn open(state: Entity<AppState>, conversation: ConversationId, name: String, window: &mut Window, cx: &mut App) {
    state.update(cx, |state, _| state.safety_number = None);
    state.read(cx).send(Command::LoadSafetyNumber { conversation: conversation.clone() });
    window.open_dialog(cx, move |dialog, _, cx| {
        let palette = Palette::get(cx);
        let loaded = state
            .read(cx)
            .safety_number
            .as_ref()
            .filter(|(id, _)| *id == conversation)
            .map(|(_, result)| result.clone());
        let body = match loaded {
            None => div().flex().justify_center().py_6().child(Spinner::new()).into_any_element(),
            Some(Err(error)) => div().text_sm().text_color(palette.text_secondary).child(error).into_any_element(),
            Some(Ok(number)) => {
                let (state, id, verified) = (state.clone(), conversation.clone(), number.verified);
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_4()
                    .child(
                        // Black on white whatever the theme: phones scan this.
                        div().p_3().rounded_lg().bg(white()).child(crate::views::link::qr_code(&number.qr, QR_SIZE)),
                    )
                    .child(
                        div()
                            .font_family("Menlo")
                            .text_lg()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .children(blocks(&number.digits).chunks(4).map(|row| {
                                div().flex().gap_4().children(row.iter().map(|block| div().child(block.clone())))
                            })),
                    )
                    .child(
                        div().text_sm().text_color(palette.text_secondary).text_center().child(format!(
                            "To verify end-to-end encryption with {}, compare the numbers above with their device. \
                             They can also scan your code with their device.",
                            name
                        )),
                    )
                    .when(verified, |d| {
                        d.child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(format!("✓ {} is verified", name)))
                    })
                    .child(
                        Button::new("toggle-verified")
                            .label(if verified { "Clear verification" } else { "Mark as verified" })
                            .when(!verified, |b| b.primary())
                            .when(verified, |b| b.outline())
                            .on_click(move |_, _, cx| {
                                state.read(cx).send(Command::SetVerified { conversation: id.clone(), verified: !verified });
                            }),
                    )
                    .into_any_element()
            }
        };
        dialog.title("Safety number").w(px(420.)).child(body)
    });
}

/// Groups of five, as Signal shows them.
fn blocks(digits: &str) -> Vec<String> {
    digits.as_bytes().chunks(5).map(|chunk| String::from_utf8_lossy(chunk).into_owned()).collect()
}

#[cfg(test)]
mod tests {
    use gpui_kit::component::WindowExt as _;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, TestAppContext, WindowBounds, WindowOptions, px, size};
    use signal_backend::{Backend, Config};
    use signal_store::ConversationId;

    #[gpui_kit::test]
    fn shows_the_number_and_marks_verified(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        // The demo backend answers from its own Tokio threads.
        cx.executor().allow_parking();
        let dir = tempfile::tempdir().unwrap();
        let config = Config { data_dir: dir.path().to_owned(), db_key: [7; 32], device_name: "test".into(), demo: true };
        let (backend, mut events) = Backend::start(config).unwrap();
        let state = cx.update(|cx| crate::state::new_state(backend.clone(), cx));
        // As the app does: backend events into the model.
        cx.update(|cx| {
            use futures::StreamExt as _;
            let state = state.clone();
            cx.spawn(async move |cx| {
                while let Some(event) = events.next().await {
                    state.update(cx, |state, cx| state.handle(event, cx));
                }
            })
            .detach();
        });
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(800.), px(900.)) })),
            ..Default::default()
        };
        let (window, _sidebar) = cx
            .update(|cx| {
                let state = state.clone();
                gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| crate::views::sidebar::Sidebar::new(state, window, cx)))
            })
            .unwrap();
        let maya = ConversationId("d:00000000-0000-4000-8000-00000000a11c".into());
        let conversation = (0..300)
            .find_map(|_| {
                let (db, id) = (backend.db().clone(), maya.clone());
                let found = futures::executor::block_on(backend.spawn(async move { db.conversation(&id).await })).ok().flatten();
                if found.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                found
            })
            .expect("the demo has Maya");
        cx.update_window(window, |_, window, cx| super::open(state.clone(), conversation.id, conversation.title, window, cx)).unwrap();
        let loaded = |cx: &mut TestAppContext| {
            for _ in 0..300 {
                cx.run_until_parked();
                if let Some((_, Ok(number))) = state.read_with(cx, |s, _| s.safety_number.clone()) {
                    return number;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("no safety number");
        };
        let number = loaded(cx);
        assert_eq!((number.digits.len(), number.verified), (60, false));
        assert!(cx.update_window(window, |_, window, cx| window.has_active_dialog(cx)).unwrap());

        cx.update_window(window, |_, window, cx| window.click("toggle-verified", cx)).unwrap();
        let verified = (0..300).any(|_| {
            cx.run_until_parked();
            let done = state.read_with(cx, |s, _| matches!(&s.safety_number, Some((_, Ok(n))) if n.verified));
            if !done {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            done
        });
        assert!(verified, "Mark as verified didn't take");
        backend.shutdown();
    }

    #[test]
    fn twelve_blocks_of_five() {
        let digits: String = (0..60).map(|i| char::from(b'0' + (i % 10) as u8)).collect();
        let blocks = super::blocks(&digits);
        assert_eq!(blocks.len(), 12);
        assert_eq!(blocks[0], "01234");
        assert_eq!(blocks[11], "56789");
    }
}
