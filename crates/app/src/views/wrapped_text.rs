//! Message text wrapped where Unicode's line breaking rules (UAX #14)
//! allow, as Signal Desktop's browser wraps it. GPUI's own wrapper breaks
//! before any `?` (its `LineWrapper::is_word_char` leaves `?` out so URLs
//! can wrap), which left a lone `?` at the start of a row. UAX #14 never
//! breaks before closing punctuation, and breaks after `/` and `?` in URLs.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::{
    App, AvailableSpace, Bounds, CursorStyle, DispatchPhase, Element, ElementId, GlobalElementId, Hitbox,
    HitboxBehavior, InspectorElementId, IntoElement, LayoutId, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ShapedLine, SharedString, Size, Style, TextRun, WhiteSpace, Window, point,
};
use unicode_linebreak::{BreakOpportunity, linebreaks};
use unicode_segmentation::UnicodeSegmentation as _;

type ClickListener = Rc<dyn Fn(usize, &mut Window, &mut App)>;

/// Text drawn in `runs`, wrapped to its width at UAX #14 break
/// opportunities, with ranges that can be clicked.
pub struct WrappedText {
    id: ElementId,
    text: SharedString,
    runs: Vec<TextRun>,
    clicks: Vec<Range<usize>>,
    on_click: Option<ClickListener>,
    laid: Rc<RefCell<Option<Laid>>>,
}

impl WrappedText {
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>, runs: Vec<TextRun>) -> Self {
        WrappedText { id: id.into(), text: text.into(), runs, clicks: Vec::new(), on_click: None, laid: Rc::default() }
    }

    /// `listener` gets the index in `ranges` of a range pressed and
    /// released in, as GPUI's `InteractiveText::on_click`.
    pub fn on_click(mut self, ranges: Vec<Range<usize>>, listener: impl Fn(usize, &mut Window, &mut App) + 'static) -> Self {
        self.clicks = ranges;
        self.on_click = Some(Rc::new(listener));
        self
    }
}

/// The text laid out at one width: each row with the offset in the text
/// where it starts.
struct Laid {
    width: Option<Pixels>,
    rows: Vec<(usize, ShapedLine)>,
    line_height: Pixels,
    size: Size<Pixels>,
}

impl Laid {
    fn new(text: &str, runs: &[TextRun], width: Option<Pixels>, font_size: Pixels, line_height: Pixels, window: &Window) -> Laid {
        let mut rows = Vec::new();
        let mut size = Size::<Pixels>::default();
        let mut start = 0;
        for paragraph in text.split('\n') {
            let range = start..start + paragraph.len();
            let line = window.text_system().shape_line(paragraph.to_owned().into(), font_size, &runs_in(runs, range), None);
            // As GPUI's wrapped lines measure: a row that wraps is as wide
            // as the space it wraps in.
            size.width = size.width.max(width.map_or(line.width(), |width| width.min(line.width())));
            let breaks = width.map(|width| wrap_points(paragraph, &offsets(&line), width)).unwrap_or_default();
            let (mut rest, mut at) = (line, 0);
            for cut in breaks {
                let (row, tail) = rest.split_at(cut - at);
                rows.push((start + at, row));
                (rest, at) = (tail, cut);
            }
            rows.push((start + at, rest));
            start += paragraph.len() + 1;
        }
        size.width = size.width.ceil();
        size.height = line_height * rows.len() as f32;
        Laid { width, rows, line_height, size }
    }

    /// The offset of the character under `position`, if it's over one.
    fn index_at(&self, bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<usize> {
        if !bounds.contains(&position) {
            return None;
        }
        let row = ((position.y - bounds.top()) / self.line_height).floor() as usize;
        let (start, line) = self.rows.get(row)?;
        line.index_for_x(position.x - bounds.left()).map(|index| start + index)
    }
}

/// The parts of `runs` that cover `range` of their text.
fn runs_in(runs: &[TextRun], range: Range<usize>) -> Vec<TextRun> {
    let mut start = 0;
    let mut within = Vec::new();
    for run in runs {
        let (from, to) = (start.max(range.start), (start + run.len).min(range.end));
        if from < to {
            within.push(TextRun { len: to - from, ..run.clone() });
        }
        start += run.len;
    }
    within
}

/// Each byte offset's x position in `line` (the next glyph's, inside a
/// character), and the line's width at its end.
fn offsets(line: &ShapedLine) -> Vec<Pixels> {
    let mut glyph_x = vec![None; line.len() + 1];
    for glyph in line.runs.iter().flat_map(|run| &run.glyphs) {
        if let Some(slot) = glyph_x.get_mut(glyph.index) {
            slot.get_or_insert(glyph.position.x);
        }
    }
    let mut next = line.width();
    let mut xs: Vec<Pixels> = glyph_x
        .into_iter()
        .rev()
        .map(|x| {
            next = x.unwrap_or(next);
            next
        })
        .collect();
    xs.reverse();
    xs
}

/// Whitespace that may hang past a row's end. No-break spaces are part of
/// a word.
fn hangs(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\u{a0}' | '\u{2007}' | '\u{202f}')
}

/// Where rows after the first start when `text` (one line, without
/// newlines) wraps at `width`; `xs[i]` is offset `i`'s x position. Rows
/// end at the last UAX #14 break opportunity that fits, with trailing
/// spaces hanging, and a word wider than a row is cut between graphemes.
fn wrap_points(text: &str, xs: &[Pixels], width: Pixels) -> Vec<usize> {
    let mut breaks = Vec::new();
    let mut start = 0;
    // The last break opportunity the current row fits up to.
    let mut fits = None;
    for (at, kind) in linebreaks(text) {
        loop {
            let end = start + text[start..at].trim_end_matches(hangs).len();
            if end == start || xs[end] - xs[start] <= width {
                if at > start {
                    fits = Some(at);
                }
                break;
            }
            let cut = fits.take().unwrap_or_else(|| squeeze(&text[..end], xs, start, width));
            breaks.push(cut);
            start = cut;
        }
        if kind == BreakOpportunity::Mandatory && start < at && at < text.len() {
            breaks.push(at);
            start = at;
            fits = None;
        }
    }
    breaks
}

/// Where to cut `text[start..]`, too wide for a row of its own: after as
/// many whole graphemes as fit, and at least one.
fn squeeze(text: &str, xs: &[Pixels], start: usize, width: Pixels) -> usize {
    let mut cut = None;
    for (i, grapheme) in text[start..].grapheme_indices(true) {
        let after = start + i + grapheme.len();
        if cut.is_some() && xs[after] - xs[start] > width {
            break;
        }
        cut = Some(after);
    }
    cut.unwrap_or(text.len())
}

impl IntoElement for WrappedText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WrappedText {
    type RequestLayoutState = ();
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn a11y_role(&self) -> Option<gpui_kit::accesskit::Role> {
        Some(gpui_kit::accesskit::Role::Label)
    }

    fn write_a11y_info(&self, node: &mut gpui_kit::accesskit::Node) {
        node.set_value(self.text.to_string());
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        _: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Sized from the text style, as GPUI's `StyledText` is.
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.pixel_snap(style.line_height.to_pixels(font_size.into(), window.rem_size()));
        let wraps = style.white_space == WhiteSpace::Normal;
        let text = self.text.clone();
        let runs = match std::mem::take(&mut self.runs) {
            runs if runs.is_empty() => vec![style.to_run(text.len())],
            runs => runs,
        };
        let laid = self.laid.clone();
        let id = window.request_measured_layout(Style::default(), move |known, available, window, _| {
            let width = known.width.or(match available.width {
                AvailableSpace::Definite(width) => Some(width),
                _ => None,
            });
            let width = width.filter(|_| wraps);
            if let Some(laid) = laid.borrow().as_ref()
                && laid.width == width
            {
                return laid.size;
            }
            let fresh = Laid::new(&text, &runs, width, font_size, line_height, window);
            let size = fresh.size;
            *laid.borrow_mut() = Some(fresh);
            size
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
        self.on_click.is_some().then(|| window.insert_hitbox(bounds, HitboxBehavior::Normal))
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(laid) = &*self.laid.borrow() else {
            return;
        };
        let align = window.text_style().text_align;
        for (i, (_, row)) in laid.rows.iter().enumerate() {
            let origin = point(bounds.left(), bounds.top() + laid.line_height * i as f32);
            let _ = row.paint_background(origin, laid.line_height, align, Some(bounds.size.width), window, cx);
            let _ = row.paint(origin, laid.line_height, align, Some(bounds.size.width), window, cx);
        }

        let (Some(hitbox), Some(listener), Some(global_id)) = (hitbox.clone(), self.on_click.clone(), global_id) else {
            return;
        };
        let clicks = Rc::new(std::mem::take(&mut self.clicks));
        let over_click = laid
            .index_at(bounds, window.mouse_position())
            .is_some_and(|index| clicks.iter().any(|range| range.contains(&index)));
        if over_click {
            window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
        }
        // Draw again when the pointer crosses a clickable range's edge, so
        // the cursor follows it, as `InteractiveText` does.
        window.on_mouse_event({
            let (laid, clicks) = (self.laid.clone(), clicks.clone());
            move |event: &MouseMoveEvent, phase, window, _| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let now = laid
                    .borrow()
                    .as_ref()
                    .and_then(|laid| laid.index_at(bounds, event.position))
                    .is_some_and(|index| clicks.iter().any(|range| range.contains(&index)));
                if now != over_click {
                    window.refresh();
                }
            }
        });
        // Where the press began, kept between frames.
        window.with_element_state::<Rc<Cell<Option<usize>>>, _>(global_id, |pressed, window| {
            let pressed = pressed.unwrap_or_default();
            window.on_mouse_event({
                let (pressed, hitbox, laid) = (pressed.clone(), hitbox.clone(), self.laid.clone());
                move |event: &MouseDownEvent, phase, window, _| {
                    if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                        pressed.set(laid.borrow().as_ref().and_then(|laid| laid.index_at(bounds, event.position)));
                    }
                }
            });
            window.on_mouse_event({
                let (pressed, laid) = (pressed.clone(), self.laid.clone());
                move |event: &MouseUpEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    let Some(down) = pressed.take() else {
                        return;
                    };
                    let up = laid.borrow().as_ref().and_then(|laid| laid.index_at(bounds, event.position));
                    if let Some(up) = up.filter(|_| hitbox.is_hovered(window)) {
                        for (i, range) in clicks.iter().enumerate() {
                            if range.contains(&down) && range.contains(&up) {
                                listener(i, window, cx);
                            }
                        }
                    }
                }
            });
            ((), pressed)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::AppContext as _;
    use gpui_kit::px;

    /// Wrap points for text whose every byte is 1px wide.
    fn wrap(text: &str, width: f32) -> Vec<usize> {
        let xs: Vec<Pixels> = (0..=text.len()).map(|i| px(i as f32)).collect();
        wrap_points(text, &xs, px(width))
    }

    fn rows(text: &str, width: f32) -> Vec<&str> {
        let mut starts = vec![0];
        starts.extend(wrap(text, width));
        starts.push(text.len());
        starts.windows(2).map(|pair| &text[pair[0]..pair[1]]).collect()
    }

    #[test]
    fn question_mark_stays_with_its_word() {
        // GPUI's wrapper put the `?` on the next row.
        assert_eq!(rows("go toward? Is", 9.), ["go ", "toward? ", "Is"]);
        assert_eq!(rows("money go toward? Is the", 15.), ["money go ", "toward? Is the"]);
    }

    #[test]
    fn closing_punctuation_stays_with_its_word() {
        assert_eq!(rows("so (see) now!", 7.), ["so ", "(see) ", "now!"]);
    }

    #[test]
    fn trailing_spaces_hang() {
        assert_eq!(rows("ab   cd", 2.), ["ab   ", "cd"]);
    }

    #[test]
    fn urls_wrap_after_slashes() {
        let url = "https://example.org/some/path?q=1";
        for row in rows(url, 12.) {
            assert!(row.len() <= 12, "{row:?} is wider than the row");
            assert!(!row.starts_with('/') && !row.starts_with('?'), "{row:?} starts with punctuation");
        }
    }

    #[test]
    fn a_long_word_is_cut_between_graphemes() {
        assert_eq!(rows("aaaaaaaaaa", 4.), ["aaaa", "aaaa", "aa"]);
        // "e" and a combining acute accent are one grapheme.
        assert_eq!(rows("e\u{301}e\u{301}e\u{301}", 4.), ["e\u{301}", "e\u{301}", "e\u{301}"]);
    }

    #[test]
    fn the_time_spacer_moves_as_one_word() {
        assert_eq!(rows("hello 0000", 8.), ["hello ", "0000"]);
        assert_eq!(rows("hello 0000", 10.), ["hello 0000"]);
    }

    #[test]
    fn text_that_fits_does_not_wrap() {
        assert!(wrap("short", 50.).is_empty());
        assert!(wrap("", 5.).is_empty());
    }

    struct Host {
        text: &'static str,
        width: f32,
        laid: Rc<RefCell<Option<Laid>>>,
        clicked: Rc<Cell<Option<usize>>>,
    }

    impl gpui_kit::Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut gpui_kit::Context<Self>) -> impl IntoElement {
            use gpui_kit::{ParentElement as _, Styled as _};
            let run = TextRun {
                len: self.text.len(),
                font: gpui_kit::font(".SystemUIFont"),
                color: gpui_kit::black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let clicked = self.clicked.clone();
            let mut text = WrappedText::new("text", self.text, vec![run]).on_click(std::iter::once(0..5).collect(), move |i, _, _| clicked.set(Some(i)));
            text.laid = self.laid.clone();
            gpui_kit::div().w(px(self.width)).text_size(px(14.5)).line_height(px(20.)).child(text)
        }
    }

    /// The window, the text's layout and the range last clicked.
    type Mounted = (gpui_kit::VisualTestContext, Rc<RefCell<Option<Laid>>>, Rc<Cell<Option<usize>>>);

    fn mount(cx: &mut gpui_kit::TestAppContext, text: &'static str, width: f32) -> Mounted {
        cx.update(gpui_kit::init);
        let (laid, clicked) = (Rc::<RefCell<Option<Laid>>>::default(), Rc::<Cell<Option<usize>>>::default());
        let window = cx.update(|cx| {
            let host = Host { text, width, laid: laid.clone(), clicked: clicked.clone() };
            gpui_kit::open_window(Default::default(), cx, |_, cx| cx.new(|_| host)).unwrap()
        });
        let mut vcx = gpui_kit::VisualTestContext::from_window(window.0, cx);
        vcx.update(|window, _| window.refresh());
        vcx.run_until_parked();
        (vcx, laid, clicked)
    }

    fn row_texts(text: &str, laid: &Laid) -> Vec<String> {
        let mut starts: Vec<usize> = laid.rows.iter().map(|(start, _)| *start).collect();
        starts.push(text.len());
        starts.windows(2).map(|pair| text[pair[0]..pair[1]].to_owned()).collect()
    }

    /// On macOS's text system, at many widths: no row starts with `?`
    /// (GPUI's own wrapping does at some of them), and every row fits.
    #[cfg(target_os = "macos")]
    #[test]
    fn question_marks_stay_on_their_row_as_laid_out_on_macos() {
        const TEXT: &str = "Can you explain how this works? What does this go toward? Is it paid direct to the venue? How does that help us later? Who decides? Why not?";
        let mut gpui_orphans = 0;
        for width in (90..400).step_by(3) {
            let mut cx = gpui_kit::TestAppContext::build_with_text_system(
                gpui_kit::TestDispatcher::new(0),
                None,
                std::sync::Arc::new(gpui_apple::AppleTextSystem::new()),
            );
            let (mut vcx, laid, _) = mount(&mut cx, TEXT, width as f32);
            let laid = laid.borrow();
            let laid = laid.as_ref().unwrap();
            for (row, (_, line)) in row_texts(TEXT, laid).iter().zip(&laid.rows) {
                assert!(!row.starts_with('?'), "at {width}px a row starts with ?: {row:?}");
                let visible = row.trim_end().len();
                assert!(line.x_for_index(visible) <= px(width as f32), "at {width}px {row:?} overflows");
            }
            gpui_orphans += vcx.update(|window, _| {
                let mut wrapper = window.text_system().line_wrapper(gpui_kit::font(".SystemUIFont"), px(14.5));
                let fragments = [gpui_kit::LineFragment::text(TEXT)];
                wrapper
                    .wrap_line(&fragments, px(width as f32), Default::default())
                    .filter(|boundary| TEXT[boundary.ix..].starts_with('?'))
                    .count()
            });
        }
        assert!(gpui_orphans > 0, "GPUI's wrapper no longer orphans ?, so this test proves nothing");
    }

    #[gpui_kit::test]
    fn clicks_land_on_their_range(cx: &mut gpui_kit::TestAppContext) {
        let (mut vcx, laid, clicked) = mount(cx, "link and more text", 400.);
        let x = laid.borrow().as_ref().unwrap().rows[0].1.x_for_index(2);
        vcx.simulate_click(point(x, px(10.)), Default::default());
        assert_eq!(clicked.get(), Some(0));
        clicked.set(None);
        let x = laid.borrow().as_ref().unwrap().rows[0].1.x_for_index(10);
        vcx.simulate_click(point(x, px(10.)), Default::default());
        assert_eq!(clicked.get(), None, "outside the range");
    }
}
