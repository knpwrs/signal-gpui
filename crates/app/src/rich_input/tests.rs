use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    Entity, FontWeight, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, div, font, point, px, size,
};
use signal_store::{StyleSpan, TextStyle};

use super::*;

struct Host {
    input: Entity<RichInput>,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w(px(300.)).font_family(".SystemUIFont").child(self.input.clone())
    }
}

fn mount(cx: &mut TestAppContext) -> (VisualTestContext, Entity<RichInput>) {
    cx.update(gpui_kit::init);
    let (window, input) = cx.update(|cx| {
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(gpui_kit::Bounds { origin: Default::default(), size: size(px(600.), px(400.)) })),
            ..Default::default()
        };
        let mut input = None;
        let window = gpui_kit::open_window(options, cx, |window, cx| {
            let entity = cx.new(|cx| RichInput::new(window, cx).auto_grow(1, 4).text_size(px(14.5), px(20.)).placeholder("Message"));
            input = Some(entity.clone());
            cx.new(|_| Host { input: entity })
        })
        .unwrap();
        (window, input.unwrap())
    });
    let mut vcx = VisualTestContext::from_window(window.0, cx);
    let focus = vcx.update(|_, cx| input.read(cx).focus_handle.clone());
    vcx.update(|window, cx| window.focus(&focus, cx));
    draw(&mut vcx);
    (vcx, input)
}

fn draw(cx: &mut VisualTestContext) {
    cx.update(|window, _| window.refresh());
    cx.run_until_parked();
}

fn set(cx: &mut VisualTestContext, input: &Entity<RichInput>, text: &str, spans: &[StyleSpan]) {
    cx.update(|window, cx| input.update(cx, |i, cx| i.set_content(text, spans, window, cx)));
    draw(cx);
}

fn value(cx: &mut VisualTestContext, input: &Entity<RichInput>) -> (String, Vec<(String, TextStyle)>) {
    cx.update(|_, cx| {
        let input = input.read(cx);
        let text = input.value().to_owned();
        let spans = input.spans().iter().map(|s| (text[s.start..s.start + s.len].to_owned(), s.style)).collect();
        (text, spans)
    })
}

fn span(start: usize, len: usize, style: TextStyle) -> StyleSpan {
    StyleSpan { start, len, style }
}

/// macOS's own text system, so fonts measure differently (the default
/// test platform gives every glyph one width).
#[cfg(target_os = "macos")]
fn real_text() -> TestAppContext {
    TestAppContext::build_with_text_system(
        gpui_kit::TestDispatcher::new(0),
        None,
        std::sync::Arc::new(gpui_apple::AppleTextSystem::new()),
    )
}

/// The proof the input rests on: bold and monospace text is shaped in its
/// own font, and the caret, hit testing, `range_to_bounds` and the IME's
/// bounds follow the drawn glyphs, not one font's widths.
#[cfg(target_os = "macos")]
#[test]
fn mixed_fonts_are_drawn_and_measured_as_shaped() {
    let mut cx = real_text();
    let (mut vcx, input) = mount(&mut cx);
    let text = "plain bold mono";
    set(&mut vcx, &input, text, &[span(6, 4, TextStyle::Bold), span(11, 4, TextStyle::Monospace)]);

    let width_of = |vcx: &mut VisualTestContext, text: &str, weight: FontWeight, family: &str| {
        vcx.update(|window, _| {
            let mut f = font(family);
            f.weight = weight;
            let run = gpui_kit::TextRun { len: text.len(), font: f, color: gpui_kit::black(), background_color: None, underline: None, strikethrough: None };
            window.text_system().shape_line(text.to_owned().into(), px(14.5), &[run], None).width
        })
    };
    let bold = width_of(&mut vcx, "bold", FontWeight::BOLD, ".SystemUIFont");
    let regular = width_of(&mut vcx, "bold", FontWeight::NORMAL, ".SystemUIFont");
    let mono = width_of(&mut vcx, "mono", FontWeight::NORMAL, MONOSPACE_FAMILY);
    let mono_as_text = width_of(&mut vcx, "mono", FontWeight::NORMAL, ".SystemUIFont");
    assert!((bold - regular).abs() > px(0.5), "bold is wider: {bold:?} vs {regular:?}");
    assert!((mono - mono_as_text).abs() > px(0.5), "Menlo differs: {mono:?} vs {mono_as_text:?}");

    let (laid, origin) = vcx.update(|_, cx| {
        let input = input.read(cx);
        (input.laid.clone().unwrap(), input.text_origin())
    });
    let x = |at: usize| laid.caret_position(at).x;
    let close = |a: Pixels, b: Pixels| (a - b).abs() < px(0.01);
    assert!(close(x(10) - x(6), bold), "{:?} vs {bold:?}", x(10) - x(6));
    assert!(close(x(15) - x(11), mono), "{:?} vs {mono:?}", x(15) - x(11));

    // Every caret position hit-tests back to its own offset.
    for (at, _) in text.char_indices().chain([(text.len(), ' ')]) {
        let p = laid.caret_position(at) + point(px(0.), px(10.));
        assert_eq!(laid.offset_for_position(text, p), at, "offset {at}");
    }
    // range_to_bounds (the spell check's underlines and the formatting
    // menu's anchor) and the IME's bounds follow the same glyphs.
    let bounds = vcx.update(|_, cx| input.read(cx).range_to_bounds(&(11..15)).unwrap());
    assert!(close(bounds.size.width, mono) && close(bounds.left(), origin.x + x(11)), "{bounds:?}");
    let (ime, index) = vcx.update(|window, cx| {
        input.update(cx, |i, cx| {
            let ime = i.bounds_for_range(6..10, Bounds::default(), window, cx).unwrap();
            let index = i.character_index_for_point(origin + laid.caret_position(11) + point(px(1.), px(5.)), window, cx);
            (ime, index)
        })
    });
    assert!(close(ime.size.width, bold), "{ime:?}");
    assert_eq!(index, Some(11));
}

/// Wrapping follows the shaped widths too: monospace text wraps where its
/// wider glyphs run out of room, and the caret moves down with it.
#[cfg(target_os = "macos")]
#[test]
fn styled_text_wraps_by_its_own_widths() {
    let mut cx = real_text();
    let (mut vcx, input) = mount(&mut cx);
    let words = "iiii ".repeat(12);
    set(&mut vcx, &input, &words, &[]);
    let plain_rows = vcx.update(|_, cx| input.read(cx).laid.as_ref().unwrap().rows.len());
    set(&mut vcx, &input, &words, &[span(0, words.len(), TextStyle::Monospace)]);
    let (rows, laid) = vcx.update(|_, cx| {
        let laid = input.read(cx).laid.clone().unwrap();
        (laid.rows.len(), laid)
    });
    assert!(rows > plain_rows, "monospace takes more rows: {rows} vs {plain_rows}");
    let second = &laid.rows[1];
    assert_eq!(laid.caret_position(second.start).y, px(20.), "a wrapped row's start is drawn on that row");
    assert_eq!(laid.caret_position(second.start).x, px(0.));
}

#[gpui_kit::test]
fn typing_continues_the_style_before_the_caret(cx: &mut TestAppContext) {
    let (mut vcx, input) = mount(cx);
    set(&mut vcx, &input, "a bold", &[span(2, 4, TextStyle::Bold)]);
    vcx.update(|window, cx| window.input("er", cx));
    draw(&mut vcx);
    assert_eq!(value(&mut vcx, &input), ("a bolder".into(), vec![("bolder".into(), TextStyle::Bold)]));
}

#[gpui_kit::test]
fn toggling_a_style_over_a_selection_and_at_the_caret(cx: &mut TestAppContext) {
    let (mut vcx, input) = mount(cx);
    set(&mut vcx, &input, "say hi", &[]);
    vcx.update(|_, cx| input.update(cx, |i, cx| {
        i.set_selected_range(4..6, cx);
        i.toggle_style(TextStyle::Italic, cx);
    }));
    assert_eq!(value(&mut vcx, &input).1, [("hi".into(), TextStyle::Italic)]);
    assert!(vcx.update(|_, cx| input.read(cx).style_active(TextStyle::Italic)));
    // Partly italic: toggling makes all of it italic, then none.
    vcx.update(|_, cx| input.update(cx, |i, cx| {
        i.set_selected_range(0..6, cx);
        i.toggle_style(TextStyle::Italic, cx);
    }));
    assert_eq!(value(&mut vcx, &input).1, [("say hi".into(), TextStyle::Italic)]);
    vcx.update(|_, cx| input.update(cx, |i, cx| i.toggle_style(TextStyle::Italic, cx)));
    assert_eq!(value(&mut vcx, &input).1, []);
    // At the caret: what's typed next takes it, until the caret moves.
    vcx.update(|_, cx| input.update(cx, |i, cx| {
        i.set_selected_range(6..6, cx);
        i.toggle_style(TextStyle::Bold, cx);
    }));
    vcx.update(|window, cx| window.input("!!", cx));
    assert_eq!(value(&mut vcx, &input), ("say hi!!".into(), vec![("!!".into(), TextStyle::Bold)]));
    vcx.update(|_, cx| input.update(cx, |i, cx| {
        i.toggle_style(TextStyle::Bold, cx);
        i.set_selected_range(0..0, cx);
        i.set_selected_range(8..8, cx);
    }));
    vcx.update(|window, cx| window.input("?", cx));
    assert_eq!(value(&mut vcx, &input).1, [("!!?".into(), TextStyle::Bold)], "a moved caret forgets the toggle");
}

#[gpui_kit::test]
fn deleting_and_undo_keep_styles_in_step(cx: &mut TestAppContext) {
    let (mut vcx, input) = mount(cx);
    set(&mut vcx, &input, "one two three", &[span(4, 3, TextStyle::Strikethrough)]);
    vcx.update(|_, cx| input.update(cx, |i, cx| i.set_selected_range(2..6, cx)));
    vcx.update(|window, cx| window.press("backspace", cx));
    assert_eq!(value(&mut vcx, &input), ("ono three".into(), vec![("o".into(), TextStyle::Strikethrough)]));
    vcx.update(|window, cx| window.press("cmd-z", cx));
    assert_eq!(value(&mut vcx, &input), ("one two three".into(), vec![("two".into(), TextStyle::Strikethrough)]));
    vcx.update(|window, cx| window.press("cmd-shift-z", cx));
    assert_eq!(value(&mut vcx, &input).0, "ono three");
    // A style toggle is a step of its own.
    vcx.update(|_, cx| input.update(cx, |i, cx| {
        i.set_selected_range(0..3, cx);
        i.toggle_style(TextStyle::Bold, cx);
    }));
    vcx.update(|window, cx| window.press("cmd-z", cx));
    assert_eq!(value(&mut vcx, &input), ("ono three".into(), vec![("o".into(), TextStyle::Strikethrough)]));
}

#[gpui_kit::test]
fn copied_text_pastes_with_its_styles_and_other_text_plain(cx: &mut TestAppContext) {
    let (mut vcx, input) = mount(cx);
    set(&mut vcx, &input, "keep ||this|| ", &[]);
    set(&mut vcx, &input, "keep this ", &[span(5, 4, TextStyle::Spoiler)]);
    vcx.update(|_, cx| input.update(cx, |i, cx| i.set_selected_range(5..9, cx)));
    vcx.update(|window, cx| {
        window.press("cmd-c", cx);
        window.press("cmd-right", cx);
        window.press("cmd-v", cx);
    });
    assert_eq!(value(&mut vcx, &input), ("keep this this".into(), vec![
        ("this".into(), TextStyle::Spoiler),
        ("this".into(), TextStyle::Spoiler),
    ]));
    vcx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string(" *plain*".into())));
    vcx.update(|window, cx| window.press("cmd-v", cx));
    let (text, spans) = value(&mut vcx, &input);
    assert_eq!(text, "keep this this *plain*", "markers typed or pasted stay text");
    assert_eq!(spans.len(), 2);
}

#[gpui_kit::test]
fn keyboard_movement_and_selection(cx: &mut TestAppContext) {
    let (mut vcx, input) = mount(cx);
    set(&mut vcx, &input, "one two\nthree", &[]);
    let sel = |vcx: &mut VisualTestContext| vcx.update(|_, cx| (input.read(cx).selected_range(), input.read(cx).cursor()));
    vcx.update(|window, cx| window.press("alt-left", cx));
    assert_eq!(sel(&mut vcx), (8..8, 8));
    vcx.update(|window, cx| window.press("up", cx));
    assert_eq!(sel(&mut vcx).1, 0);
    vcx.update(|window, cx| window.press("alt-shift-right", cx));
    assert_eq!(sel(&mut vcx).0, 0..3);
    vcx.update(|window, cx| window.press("shift-down", cx));
    assert_eq!(sel(&mut vcx).0, 0..11, "down keeps the column");
    vcx.update(|window, cx| window.press("cmd-a", cx));
    assert_eq!(sel(&mut vcx).0, 0..13);
    vcx.update(|window, cx| window.press("right", cx));
    vcx.update(|window, cx| window.press("cmd-left", cx));
    assert_eq!(sel(&mut vcx).1, 8);
    vcx.update(|window, cx| window.press("alt-backspace", cx));
    assert_eq!(value(&mut vcx, &input).0, "one three", "the line break and the word before it, as macOS");
    vcx.update(|window, cx| window.press("shift-enter", cx));
    assert_eq!(value(&mut vcx, &input).0, "one \nthree");
}

#[gpui_kit::test]
fn clicks_place_the_caret_and_double_clicks_select_a_word(cx: &mut TestAppContext) {
    let (mut vcx, input) = mount(cx);
    set(&mut vcx, &input, "hello there", &[]);
    let (laid, origin) = vcx.update(|_, cx| (input.read(cx).laid.clone().unwrap(), input.read(cx).text_origin()));
    let at = origin + laid.caret_position(8) + point(px(0.), px(10.));
    vcx.simulate_click(at, gpui_kit::Modifiers::none());
    assert_eq!(vcx.update(|_, cx| input.read(cx).selected_range()), 8..8);
    vcx.update(|window, cx| {
        window.dispatch_event(
            gpui_kit::PlatformInput::MouseDown(MouseDownEvent { button: MouseButton::Left, position: at, modifiers: Default::default(), click_count: 2, first_mouse: false }),
            cx,
        );
        window.dispatch_event(
            gpui_kit::PlatformInput::MouseUp(MouseUpEvent { button: MouseButton::Left, position: at, modifiers: Default::default(), click_count: 2 }),
            cx,
        );
    });
    assert_eq!(vcx.update(|_, cx| input.read(cx).selected_range()), 6..11);
}

#[gpui_kit::test]
fn it_grows_with_its_text_then_scrolls(cx: &mut TestAppContext) {
    let (mut vcx, input) = mount(cx);
    let height = |vcx: &mut VisualTestContext| vcx.update(|_, cx| input.read(cx).input_bounds().size.height);
    assert_eq!(height(&mut vcx), px(20.), "one row, empty");
    set(&mut vcx, &input, "1\n2\n3", &[]);
    assert_eq!(height(&mut vcx), px(60.));
    set(&mut vcx, &input, "1\n2\n3\n4\n5\n6", &[]);
    assert_eq!(height(&mut vcx), px(80.), "four rows at most");
    // The caret (at the end) is scrolled into view.
    assert_eq!(vcx.update(|_, cx| input.read(cx).scroll_y), px(40.));
}

#[gpui_kit::test]
fn ime_composition_is_marked_then_committed(cx: &mut TestAppContext) {
    let (mut vcx, input) = mount(cx);
    set(&mut vcx, &input, "bold ", &[span(0, 4, TextStyle::Bold)]);
    vcx.update(|_, cx| input.update(cx, |i, cx| i.set_selected_range(4..4, cx)));
    vcx.update(|window, cx| input.update(cx, |i, cx| {
        i.replace_and_mark_text_in_range(None, "に", Some(1..1), window, cx);
        assert_eq!(i.marked_text_range(window, cx), Some(4..5));
        i.replace_and_mark_text_in_range(None, "日本", Some(2..2), window, cx);
        i.replace_text_in_range(None, "日本", window, cx);
        assert_eq!(i.marked_text_range(window, cx), None);
    }));
    assert_eq!(value(&mut vcx, &input), ("bold日本 ".into(), vec![("bold日本".into(), TextStyle::Bold)]));
}

/// The runs each style draws with: Desktop's composer shows a spoiler as
/// a shaded box over readable text, and monospace in a code font.
#[test]
fn each_style_has_its_run() {
    let text = "b i s m sp ime";
    let styles = Styles::from_spans(text, &[
        span(0, 1, TextStyle::Bold),
        span(2, 1, TextStyle::Italic),
        span(4, 1, TextStyle::Strikethrough),
        span(6, 1, TextStyle::Monospace),
        span(8, 2, TextStyle::Spoiler),
    ]);
    let look = layout::Look {
        font: font(".SystemUIFont"),
        font_size: px(14.),
        line_height: px(20.),
        color: gpui_kit::black(),
        placeholder_color: gpui_kit::black(),
        spoiler_background: gpui_kit::red(),
    };
    let runs = layout::runs(text, &styles, Some(&(11..14)), &look);
    let at = |offset: usize| {
        let mut start = 0;
        runs.iter().find(|r| {
            start += r.len;
            offset < start
        })
        .unwrap()
        .clone()
    };
    assert_eq!(runs.iter().map(|r| r.len).sum::<usize>(), text.len());
    assert_eq!(at(0).font.weight, FontWeight::BOLD);
    assert_eq!(at(2).font.style, gpui_kit::FontStyle::Italic);
    assert!(at(4).strikethrough.is_some());
    assert_eq!(at(6).font.family.as_ref(), MONOSPACE_FAMILY);
    assert_eq!(at(8).background_color, Some(gpui_kit::red()));
    assert!(at(12).underline.is_some(), "IME composition underlined");
    assert!(at(1).background_color.is_none() && at(1).font.weight == FontWeight::NORMAL && at(1).underline.is_none());
}

#[test]
fn trimmed_text_moves_its_spans() {
    let text = "  hi there \n";
    let styles = Styles::from_spans(text, &[span(2, 2, TextStyle::Bold), span(5, 6, TextStyle::Italic)]);
    let start = text.len() - text.trim_start().len();
    let end = text.trim_end().len();
    let trimmed = &text[start..end];
    assert_eq!(styles.slice(start..end).to_spans(trimmed), [span(0, 2, TextStyle::Bold), span(3, 5, TextStyle::Italic)]);
}
