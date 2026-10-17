//! A multi-line text input that shows Signal's formatting as styled text,
//! the way Signal Desktop's composer (Quill) does: bold, italic,
//! strikethrough, monospace and spoilers drawn in place, without markers.
//!
//! gpui-kit's `Textarea` draws all of its text in one style, so this is
//! built on GPUI's own pieces: an [`EntityInputHandler`] for typing and
//! IME, text shaped with a run per style and wrapped, and its own caret,
//! selection, mouse and keyboard handling, clipboard and undo. It answers
//! the same key bindings as gpui-kit's inputs (their `Input` key context
//! and actions), so the app's shortcuts and the composer's captured actions
//! (Enter, Escape, ↑ and ↓, Tab, Paste) reach it as they reached the
//! `Textarea`.
//!
//! The input owns the text and its [`Styles`]; every edit splices both.

mod element;
mod layout;
pub mod styles;

use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::input::{
    Backspace, Copy, Cut, Delete, DeleteToBeginningOfLine, DeleteToEndOfLine, DeleteToNextWordEnd,
    DeleteToPreviousWordStart, Enter, MoveDown, MoveEnd, MoveHome, MoveLeft, MovePageDown, MovePageUp, MoveRight,
    MoveToEnd, MoveToEndOfLine, MoveToNextWord, MoveToPreviousWord, MoveToStart, MoveToStartOfLine, MoveUp, Paste,
    Redo, SelectAll, SelectToEnd, SelectToEndOfLine, SelectToNextWordEnd,
    SelectToPreviousWordStart, SelectToStart, SelectToStartOfLine, ShowCharacterPalette, Undo,
};
use gpui_kit::base::actions::{SelectDown, SelectLeft, SelectRight, SelectUp};
use gpui_kit::component::native_menu::NativeMenu;
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Bounds, ClipboardEntry, ClipboardItem, Context, CursorStyle, EntityInputHandler, EventEmitter, FocusHandle,
    Focusable, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollWheelEvent,
    SharedString, Subscription, Task, TestSupportExt as _, UTF16Selection, Window, div, point, px,
};
use signal_store::{StyleSpan, TextStyle};
use unicode_segmentation::UnicodeSegmentation as _;

#[cfg(test)]
pub use layout::MONOSPACE_FAMILY;
use layout::Laid;
use styles::{StyleSet, Styles};

/// gpui-kit's inputs' key context, whose bindings this input answers.
const KEY_CONTEXT: &str = "Input";
/// Undo steps kept.
const UNDO_LIMIT: usize = 200;

/// How long the caret shows, then hides, as it blinks (macOS's 500 ms).
const CARET_BLINK: std::time::Duration = std::time::Duration::from_millis(500);

/// Builds the right-click menu from an empty one.
type MenuBuilder = Rc<dyn Fn(NativeMenu, &mut Window, &mut App) -> NativeMenu>;

#[derive(Clone, Debug, PartialEq)]
pub enum RichInputEvent {
    /// The text changed by an edit (typing, deleting, pasting, undo, or
    /// [`RichInput::replace`] and [`RichInput::insert`]); not by
    /// [`RichInput::set_value`] or [`RichInput::set_content`]. `restored`:
    /// by undo or redo, which shouldn't set off typing's substitutions again.
    Change { restored: bool },
    Blur,
}

/// The text, its styles and the selection, as undo restores them.
#[derive(Clone)]
struct Snapshot {
    text: String,
    styles: Styles,
    selected: Range<usize>,
}

/// What a mouse drag selects by: what the click that started it picked.
#[derive(Clone, Copy, PartialEq)]
enum Granularity {
    Char,
    Word,
    Line,
}

/// The rich input's state; render it as a child view.
pub struct RichInput {
    focus_handle: FocusHandle,
    text: String,
    styles: Styles,
    selected: Range<usize>,
    reversed: bool,
    marked: Option<Range<usize>>,
    /// A style toggled at a caret (Quill's cursor format): the styles text
    /// typed at that offset takes, until the caret moves.
    pending: Option<(usize, StyleSet)>,
    /// Where ↑ and ↓ keep the caret, across rows of different lengths.
    goal_x: Option<Pixels>,
    drag: Option<(Granularity, Range<usize>)>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The caret after the last character typed, while typing goes on
    /// there, so a run of typing is one undo step.
    typing_at: Option<usize>,
    placeholder: SharedString,
    rows: (usize, usize),
    text_size: Pixels,
    line_height: Pixels,
    /// The last drawn layout, its viewport and the text's origin in it.
    laid: Option<Rc<Laid>>,
    viewport: Bounds<Pixels>,
    scroll_y: Pixels,
    /// Scroll the caret into view at the next layout.
    reveal: bool,
    context_menu: Option<MenuBuilder>,
    /// The caret's blink: shown, and the task that blinks it while focused.
    caret_on: bool,
    _blink: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<RichInputEvent> for RichInput {}

impl Focusable for RichInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// What the clipboard carries besides the text, so formatted text copied
/// from the input pastes formatted (the spans are over the copied text).
#[derive(serde::Serialize, serde::Deserialize)]
struct ClipboardStyles {
    styles: Vec<StyleSpan>,
}

impl RichInput {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let subscriptions = vec![
            cx.on_focus(&focus_handle, window, |this, _, cx| this.show_caret(cx)),
            cx.on_blur(&focus_handle, window, |this, _, cx| {
                this.drag = None;
                this._blink = None;
                cx.emit(RichInputEvent::Blur);
                cx.notify();
            }),
        ];
        RichInput {
            focus_handle,
            text: String::new(),
            styles: Styles::default(),
            selected: 0..0,
            reversed: false,
            marked: None,
            pending: None,
            goal_x: None,
            drag: None,
            undo: Vec::new(),
            redo: Vec::new(),
            typing_at: None,
            placeholder: SharedString::default(),
            rows: (1, 1),
            text_size: px(14.),
            line_height: px(20.),
            laid: None,
            viewport: Bounds::default(),
            scroll_y: px(0.),
            reveal: false,
            context_menu: None,
            caret_on: true,
            _blink: None,
            _subscriptions: subscriptions,
        }
    }

    /// The caret moved or the text changed: while it blinks (focused), it
    /// shows at once and the blink starts over.
    fn caret_moved(&mut self, cx: &mut Context<Self>) {
        if self._blink.is_some() {
            self.show_caret(cx);
        }
    }

    /// Shows the caret, then blinks it while the input has focus (it shows
    /// again at every edit or move).
    fn show_caret(&mut self, cx: &mut Context<Self>) {
        self.caret_on = true;
        self._blink = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(CARET_BLINK).await;
                if this
                    .update(cx, |this, cx| {
                        this.caret_on = !this.caret_on;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Grows from `min` rows to `max`, then scrolls.
    pub fn auto_grow(mut self, min: usize, max: usize) -> Self {
        self.rows = (min.max(1), max.max(min.max(1)));
        self
    }

    pub fn text_size(mut self, size: Pixels, line_height: Pixels) -> Self {
        self.text_size = size;
        self.line_height = line_height;
        self
    }

    pub fn set_auto_grow(&mut self, min: usize, max: usize, cx: &mut Context<Self>) {
        self.rows = (min.max(1), max.max(min.max(1)));
        cx.notify();
    }

    /// Replaces the menu shown on a right click (by default Cut, Copy,
    /// Paste and Select All).
    pub fn set_context_menu(&mut self, menu: impl Fn(NativeMenu, &mut Window, &mut App) -> NativeMenu + 'static) {
        self.context_menu = Some(Rc::new(menu));
    }

    pub fn value(&self) -> &str {
        &self.text
    }

    /// Signal's spans for the text's formatting.
    pub fn spans(&self) -> Vec<StyleSpan> {
        self.styles.to_spans(&self.text)
    }

    /// The text and its formatting with the whitespace at either end cut
    /// off (spans moved to match), as a message sends.
    pub fn trimmed(&self) -> (String, Vec<StyleSpan>) {
        let start = self.text.len() - self.text.trim_start().len();
        let end = self.text.trim_end().len().max(start);
        let text = self.text[start..end].to_owned();
        let spans = self.styles.slice(start..end).to_spans(&text);
        (text, spans)
    }

    pub fn selected_range(&self) -> Range<usize> {
        self.selected.clone()
    }

    pub fn cursor(&self) -> usize {
        if self.reversed { self.selected.start } else { self.selected.end }
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus_handle, cx);
    }

    /// Replaces the text, unformatted, without an undo step or a change
    /// event; the caret goes to the end.
    pub fn set_value(&mut self, text: impl Into<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.set_content(text, &[], window, cx);
    }

    /// Replaces the text and its formatting, as [`Self::set_value`].
    pub fn set_content(&mut self, text: impl Into<String>, spans: &[StyleSpan], _: &mut Window, cx: &mut Context<Self>) {
        self.text = text.into();
        self.styles = Styles::from_spans(&self.text, spans);
        self.selected = self.text.len()..self.text.len();
        self.reversed = false;
        self.marked = None;
        self.pending = None;
        self.goal_x = None;
        self.typing_at = None;
        self.undo.clear();
        self.redo.clear();
        self.reveal = true;
        self.caret_moved(cx);
        cx.notify();
    }

    pub fn set_selected_range(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        let (start, end) = (self.clamp(range.start.min(range.end)), self.clamp(range.start.max(range.end)));
        self.select(start..end, false, cx);
    }

    /// Replaces the selection with `text`, as typing it would.
    pub fn replace(&mut self, text: impl Into<String>, _: &mut Window, cx: &mut Context<Self>) {
        let range = self.selected.clone();
        self.edit(range, &text.into(), Inserted::Typed, cx);
    }

    /// Inserts `text` at the caret (replacing a selection), as typing it.
    pub fn insert(&mut self, text: impl Into<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.replace(text, window, cx);
    }

    /// Where `range` is drawn, in window coordinates: from its start's top
    /// to its end's bottom (several rows tall when it wraps). None until
    /// the input has been drawn.
    pub fn range_to_bounds(&self, range: &Range<usize>) -> Option<Bounds<Pixels>> {
        let laid = self.laid.as_ref()?;
        let origin = self.text_origin();
        let start_row = laid.row_for(range.start, false);
        let end_row = if range.is_empty() { start_row } else { laid.row_for(range.end, true) };
        let start = laid.position_in_row(start_row, range.start);
        let end = laid.position_in_row(end_row, range.end);
        Some(Bounds::from_corners(origin + start, origin + end + point(px(0.), laid.line_height)))
    }

    /// The fonts the first line was last drawn in, run by run.
    #[cfg(test)]
    pub fn drawn_fonts(&self) -> Vec<gpui_kit::FontId> {
        let Some(line) = self.laid.as_ref().and_then(|l| l.lines.first()) else { return Vec::new() };
        line.unwrapped_layout.runs.iter().map(|r| r.font_id).collect()
    }

    /// The area the text is drawn in (what the input shows of it).
    pub fn input_bounds(&self) -> Bounds<Pixels> {
        self.viewport
    }

    /// Whether all of the selection has `style` (at a caret, whether text
    /// typed there would): what Desktop's formatting menu lights, Quill's
    /// `getFormat` (`ts/quill/formatting/menu.dom.tsx`'s
    /// `isStyleEnabledInSelection`).
    pub fn style_active(&self, style: TextStyle) -> bool {
        if self.selected.is_empty() {
            return self.typing_style(self.selected.clone()).has(style);
        }
        self.styles.all_have(&self.text, self.selected.clone(), style)
    }

    /// Desktop's formatting shortcuts and menu (`toggleForStyle`): `style`
    /// comes off the selection if all of it has it, else goes on all of
    /// it. At a caret it applies to what's typed next there, as Quill's
    /// cursor format does.
    pub fn toggle_style(&mut self, style: TextStyle, cx: &mut Context<Self>) {
        let on = !self.style_active(style);
        if self.selected.is_empty() {
            let at = self.selected.start;
            let set = self.typing_style(at..at).with(style, on);
            self.pending = Some((at, set));
        } else {
            self.push_undo();
            self.styles.set(&self.text, self.selected.clone(), style, on);
        }
        cx.notify();
    }

    fn text_origin(&self) -> Point<Pixels> {
        self.viewport.origin - point(px(0.), self.scroll_y)
    }

    fn clamp(&self, offset: usize) -> usize {
        let mut offset = offset.min(self.text.len());
        while !self.text.is_char_boundary(offset) {
            offset -= 1;
        }
        offset
    }

    /// The styles text typed over `range` takes.
    fn typing_style(&self, range: Range<usize>) -> StyleSet {
        match self.pending {
            Some((at, set)) if range == (at..at) => set,
            _ => self.styles.inherited(&self.text, range),
        }
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot { text: self.text.clone(), styles: self.styles.clone(), selected: self.selected.clone() }
    }

    fn push_undo(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.typing_at = None;
    }

    /// Replaces `range` with `new`: the one place the text changes by an
    /// edit, so its styles follow every edit.
    fn edit(&mut self, range: Range<usize>, new: &str, inserted: Inserted, cx: &mut Context<Self>) {
        let range = self.clamp(range.start.min(range.end))..self.clamp(range.start.max(range.end));
        if range.is_empty() && new.is_empty() {
            return;
        }
        // A run of characters typed one after another is one undo step; a
        // space or a line ends it, as in macOS's text views.
        let continues = matches!(inserted, Inserted::Typed)
            && range.is_empty()
            && self.typing_at == Some(range.start)
            && !new.starts_with(char::is_whitespace);
        // An IME composition (marked text) is one step, however many times
        // it's revised before it's committed.
        if !continues && self.marked.is_none() {
            self.push_undo();
        }
        match &inserted {
            Inserted::Typed => {
                let style = self.typing_style(range.clone());
                self.styles.splice(range.clone(), new, style);
            }
            Inserted::Plain => self.styles.splice(range.clone(), new, StyleSet::NONE),
            Inserted::Styled(spans) => self.styles.replace(range.clone(), Styles::from_spans(new, spans)),
        }
        self.text.replace_range(range.clone(), new);
        let caret = range.start + new.len();
        self.selected = caret..caret;
        self.reversed = false;
        self.marked = None;
        self.pending = None;
        self.goal_x = None;
        self.typing_at = matches!(inserted, Inserted::Typed).then_some(caret);
        self.reveal = true;
        self.caret_moved(cx);
        cx.emit(RichInputEvent::Change { restored: false });
        cx.notify();
    }

    /// Moves the selection (the caret is its end, or its start when
    /// `reversed`).
    fn select(&mut self, range: Range<usize>, reversed: bool, cx: &mut Context<Self>) {
        if self.selected != range || self.reversed != reversed {
            self.pending = None;
            self.typing_at = None;
        }
        // Moves up and down put their column back after this.
        self.goal_x = None;
        self.selected = range;
        self.reversed = reversed && !self.selected.is_empty();
        self.reveal = true;
        self.caret_moved(cx);
        cx.notify();
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clamp(offset);
        self.select(offset..offset, false, cx);
    }

    /// Moves the caret end of the selection to `offset`, keeping the other.
    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = self.clamp(offset);
        let anchor = if self.reversed { self.selected.end } else { self.selected.start };
        if offset < anchor {
            self.select(offset..anchor, true, cx);
        } else {
            self.select(anchor..offset, false, cx);
        }
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.text.grapheme_indices(true).rev().find_map(|(ix, _)| (ix < offset).then_some(ix)).unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.text.grapheme_indices(true).find_map(|(ix, _)| (ix > offset).then_some(ix)).unwrap_or(self.text.len())
    }

    /// The start of the word before `offset` (⌥←).
    fn previous_word_start(&self, offset: usize) -> usize {
        self.text
            .unicode_word_indices()
            .map(|(ix, _)| ix)
            .rev()
            .find(|ix| *ix < offset)
            .unwrap_or(0)
    }

    /// The end of the word after `offset` (⌥→).
    fn next_word_end(&self, offset: usize) -> usize {
        self.text
            .unicode_word_indices()
            .map(|(ix, w)| ix + w.len())
            .find(|end| *end > offset)
            .unwrap_or(self.text.len())
    }

    /// The word at `offset` (a double click), or the space or punctuation
    /// there.
    fn word_at(&self, offset: usize) -> Range<usize> {
        for (ix, word) in self.text.split_word_bound_indices() {
            if offset < ix + word.len() || ix + word.len() == self.text.len() {
                if word == "\n" && offset == ix && ix > 0 {
                    continue;
                }
                return ix..ix + word.len();
            }
        }
        offset..offset
    }

    /// The line (between newlines) at `offset` (a triple click).
    fn line_at(&self, offset: usize) -> Range<usize> {
        let start = self.text[..offset].rfind('\n').map_or(0, |i| i + 1);
        let end = self.text[offset..].find('\n').map_or(self.text.len(), |i| offset + i);
        start..end
    }

    /// The visual row the caret is on: its start and where End goes.
    fn row_bounds(&self, offset: usize) -> Range<usize> {
        match &self.laid {
            Some(laid) if laid.rows.last().is_some_and(|r| r.end == self.text.len()) => {
                let row = &laid.rows[laid.row_for(offset, false)];
                let end = if row.last || row.end == row.start { row.end } else { layout::previous_char(&self.text, row.end) };
                row.start..end
            }
            _ => self.line_at(offset),
        }
    }

    /// The offset a row up (`delta` -1) or down from the caret, keeping
    /// its x; past the first or last row, the start or end.
    fn vertical(&mut self, delta: isize) -> usize {
        let caret = self.cursor();
        let Some(laid) = self.laid.clone().filter(|l| l.rows.last().is_some_and(|r| r.end == self.text.len())) else {
            return if delta < 0 { 0 } else { self.text.len() };
        };
        let row = laid.row_for(caret, false) as isize;
        let x = *self.goal_x.get_or_insert(laid.caret_position(caret).x);
        let target = row + delta;
        if target < 0 {
            0
        } else if target as usize >= laid.rows.len() {
            self.text.len()
        } else {
            laid.offset_in_row(&self.text, target as usize, x)
        }
    }

    fn offset_for_point(&self, position: Point<Pixels>) -> usize {
        match &self.laid {
            Some(laid) => laid.offset_for_position(&self.text, position - self.text_origin()),
            None => self.text.len(),
        }
    }

    fn utf16_from(&self, offset: usize) -> usize {
        self.text[..self.clamp(offset)].encode_utf16().count()
    }

    fn offset_from_utf16(&self, utf16: usize) -> usize {
        let mut count = 0;
        for (ix, c) in self.text.char_indices() {
            if count >= utf16 {
                return ix;
            }
            count += c.len_utf16();
        }
        self.text.len()
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.utf16_from(range.start)..self.utf16_from(range.end)
    }

    // Actions.

    fn backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected.is_empty() { self.previous_boundary(self.cursor())..self.cursor() } else { self.selected.clone() };
        self.edit(range, "", Inserted::Plain, cx);
    }

    fn delete(&mut self, _: &Delete, _: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected.is_empty() { self.cursor()..self.next_boundary(self.cursor()) } else { self.selected.clone() };
        self.edit(range, "", Inserted::Plain, cx);
    }

    fn delete_to_line_start(&mut self, _: &DeleteToBeginningOfLine, _: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected.is_empty() {
            let caret = self.cursor();
            let start = self.row_bounds(caret).start;
            // At a row's start: the newline before it, as macOS does.
            if start == caret { self.previous_boundary(caret)..caret } else { start..caret }
        } else {
            self.selected.clone()
        };
        self.edit(range, "", Inserted::Plain, cx);
    }

    fn delete_to_line_end(&mut self, _: &DeleteToEndOfLine, _: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected.is_empty() {
            let caret = self.cursor();
            let end = self.line_at(caret).end;
            if end == caret { caret..self.next_boundary(caret) } else { caret..end }
        } else {
            self.selected.clone()
        };
        self.edit(range, "", Inserted::Plain, cx);
    }

    fn delete_word_before(&mut self, _: &DeleteToPreviousWordStart, _: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected.is_empty() { self.previous_word_start(self.cursor())..self.cursor() } else { self.selected.clone() };
        self.edit(range, "", Inserted::Plain, cx);
    }

    fn delete_word_after(&mut self, _: &DeleteToNextWordEnd, _: &mut Window, cx: &mut Context<Self>) {
        let range = if self.selected.is_empty() { self.cursor()..self.next_word_end(self.cursor()) } else { self.selected.clone() };
        self.edit(range, "", Inserted::Plain, cx);
    }

    /// Enter not taken by the composer (⇧Enter, or Enter in the large
    /// composer): a new line.
    fn enter(&mut self, _: &Enter, _: &mut Window, cx: &mut Context<Self>) {
        let range = self.selected.clone();
        self.edit(range, "\n", Inserted::Typed, cx);
    }

    fn left(&mut self, _: &MoveLeft, _: &mut Window, cx: &mut Context<Self>) {
        let to = if self.selected.is_empty() { self.previous_boundary(self.cursor()) } else { self.selected.start };
        self.move_to(to, cx);
    }

    fn right(&mut self, _: &MoveRight, _: &mut Window, cx: &mut Context<Self>) {
        let to = if self.selected.is_empty() { self.next_boundary(self.cursor()) } else { self.selected.end };
        self.move_to(to, cx);
    }

    fn up(&mut self, _: &MoveUp, _: &mut Window, cx: &mut Context<Self>) {
        let goal = self.goal_x;
        let to = self.vertical(-1);
        let keep = self.goal_x.or(goal);
        self.move_to(to, cx);
        self.goal_x = keep;
    }

    fn down(&mut self, _: &MoveDown, _: &mut Window, cx: &mut Context<Self>) {
        let goal = self.goal_x;
        let to = self.vertical(1);
        let keep = self.goal_x.or(goal);
        self.move_to(to, cx);
        self.goal_x = keep;
    }

    fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.vertical(-1);
        let keep = self.goal_x;
        self.select_to(to, cx);
        self.goal_x = keep;
    }

    fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        let to = self.vertical(1);
        let keep = self.goal_x;
        self.select_to(to, cx);
        self.goal_x = keep;
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor()), cx);
    }

    fn home(&mut self, _: &MoveHome, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.row_bounds(self.cursor()).start, cx);
    }

    fn end(&mut self, _: &MoveEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.row_bounds(self.cursor()).end, cx);
    }

    fn line_start(&mut self, _: &MoveToStartOfLine, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.line_at(self.cursor()).start, cx);
    }

    fn line_end(&mut self, _: &MoveToEndOfLine, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.line_at(self.cursor()).end, cx);
    }

    fn select_to_line_start(&mut self, _: &SelectToStartOfLine, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.row_bounds(self.cursor()).start, cx);
    }

    fn select_to_line_end(&mut self, _: &SelectToEndOfLine, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.row_bounds(self.cursor()).end, cx);
    }

    fn start(&mut self, _: &MoveToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn page_up(&mut self, _: &MovePageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }

    fn end_of_text(&mut self, _: &MoveToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.text.len(), cx);
    }

    fn page_down(&mut self, _: &MovePageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.text.len(), cx);
    }

    fn select_to_start(&mut self, _: &SelectToStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }

    fn select_to_end(&mut self, _: &SelectToEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.text.len(), cx);
    }

    fn word_left(&mut self, _: &MoveToPreviousWord, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.previous_word_start(self.cursor()), cx);
    }

    fn word_right(&mut self, _: &MoveToNextWord, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.next_word_end(self.cursor()), cx);
    }

    fn select_word_left(&mut self, _: &SelectToPreviousWordStart, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_word_start(self.cursor()), cx);
    }

    fn select_word_right(&mut self, _: &SelectToNextWordEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_word_end(self.cursor()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.select(0..self.text.len(), false, cx);
    }

    fn show_character_palette(&mut self, _: &ShowCharacterPalette, window: &mut Window, _: &mut Context<Self>) {
        window.show_character_palette();
    }

    /// The selection on the clipboard, with its formatting for pasting
    /// here.
    fn copy_selection(&self, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            return;
        }
        let range = self.selected.clone();
        let text = self.text[range.clone()].to_owned();
        let spans = self.styles.slice(range).to_spans(&text);
        let item = if spans.is_empty() {
            ClipboardItem::new_string(text)
        } else {
            ClipboardItem::new_string_with_json_metadata(text, ClipboardStyles { styles: spans })
        };
        cx.write_to_clipboard(item);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_selection(cx);
    }

    fn cut(&mut self, _: &Cut, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected.is_empty() {
            self.copy_selection(cx);
            let range = self.selected.clone();
            self.edit(range, "", Inserted::Plain, cx);
        }
    }

    /// Text copied here pastes with its formatting; anything else pastes
    /// as plain text, as Desktop's composer takes plain text.
    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else { return };
        let styled = match item.entries() {
            [ClipboardEntry::String(s)] => s.metadata_json::<ClipboardStyles>().map(|m| (s.text().clone(), m.styles)),
            _ => None,
        };
        let range = self.selected.clone();
        match styled {
            Some((text, spans)) => self.edit(range, &text, Inserted::Styled(spans), cx),
            None => {
                if let Some(text) = item.text() {
                    self.edit(range, &text.replace("\r\n", "\n"), Inserted::Plain, cx);
                }
            }
        }
    }

    fn restore(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        self.text = snapshot.text;
        self.styles = snapshot.styles;
        self.selected = snapshot.selected;
        self.reversed = false;
        self.marked = None;
        self.pending = None;
        self.typing_at = None;
        self.goal_x = None;
        self.reveal = true;
        self.caret_moved(cx);
        cx.emit(RichInputEvent::Change { restored: true });
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(snapshot) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore(snapshot, cx);
        }
    }

    fn redo(&mut self, _: &Redo, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(snapshot) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore(snapshot, cx);
        }
    }

    // The mouse.

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus(window, cx);
        let at = self.offset_for_point(event.position);
        let (granularity, range) = match event.click_count {
            2 => (Granularity::Word, self.word_at(at)),
            n if n >= 3 => (Granularity::Line, self.line_at(at)),
            _ => (Granularity::Char, at..at),
        };
        if event.modifiers.shift && granularity == Granularity::Char {
            self.select_to(at, cx);
            let anchor = if self.reversed { self.selected.end } else { self.selected.start };
            self.drag = Some((Granularity::Char, anchor..anchor));
            return;
        }
        self.drag = Some((granularity, range.clone()));
        self.select(range, false, cx);
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some((granularity, origin)) = self.drag.clone() else { return };
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag = None;
            return;
        }
        let at = self.offset_for_point(event.position);
        let target = match granularity {
            Granularity::Char => at..at,
            Granularity::Word => self.word_at(at),
            Granularity::Line => self.line_at(at),
        };
        if target.start < origin.start {
            self.select(target.start..origin.end, true, cx);
        } else {
            self.select(origin.start..target.end.max(origin.end), false, cx);
        }
    }

    fn mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.drag = None;
    }

    /// A right click: the caret goes there unless it's in the selection,
    /// then the menu.
    fn right_click(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus(window, cx);
        let at = self.offset_for_point(event.position);
        if !(self.selected.start <= at && at <= self.selected.end && !self.selected.is_empty()) {
            self.move_to(at, cx);
        }
        let selected = !self.selected.is_empty();
        let menu = match self.context_menu.clone() {
            Some(build) => build(NativeMenu::new(), window, cx),
            None => NativeMenu::new()
                .menu_with_disabled("Cut", !selected, Box::new(Cut))
                .menu_with_disabled("Copy", !selected, Box::new(Copy))
                .menu("Paste", Box::new(Paste))
                .separator()
                .menu("Select All", Box::new(SelectAll)),
        };
        let position = event.position;
        window.defer(cx, move |window, cx| menu.show(position, window, cx));
    }

    fn scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(laid) = &self.laid else { return };
        let max = (laid.height() - self.viewport.size.height).max(px(0.));
        if max <= px(0.) {
            return;
        }
        let delta = event.delta.pixel_delta(self.line_height).y;
        self.scroll_y = (self.scroll_y - delta).clamp(px(0.), max);
        cx.stop_propagation();
        cx.notify();
    }

    /// After a layout: keeps the scroll in range, and the caret in view
    /// when it moved.
    fn laid_out(&mut self, laid: Rc<Laid>, viewport: Bounds<Pixels>) {
        let max = (laid.height() - viewport.size.height).max(px(0.));
        if self.reveal {
            self.reveal = false;
            let caret = laid.caret_position(self.cursor()).y;
            if caret < self.scroll_y {
                self.scroll_y = caret;
            } else if caret + laid.line_height > self.scroll_y + viewport.size.height {
                self.scroll_y = caret + laid.line_height - viewport.size.height;
            }
        }
        self.scroll_y = self.scroll_y.clamp(px(0.), max);
        self.laid = Some(laid);
        self.viewport = viewport;
    }
}

/// What an edit inserts, for its styles.
enum Inserted {
    /// Typed (or put in as if typed): it takes the styles around it.
    Typed,
    /// Pasted from elsewhere, or nothing (a deletion): unformatted.
    Plain,
    /// Pasted from this input: with the formatting it was copied with.
    Styled(Vec<StyleSpan>),
}

impl EntityInputHandler for RichInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual.replace(self.range_to_utf16(&range));
        Some(self.text[range].to_owned())
    }

    fn selected_text_range(&mut self, _: bool, _: &mut Window, _: &mut Context<Self>) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: self.range_to_utf16(&self.selected), reversed: self.reversed })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|r| self.range_to_utf16(r))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }

    fn replace_text_in_range(&mut self, range_utf16: Option<Range<usize>>, text: &str, _: &mut Window, cx: &mut Context<Self>) {
        let range = range_utf16
            .map(|r| self.range_from_utf16(&r))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        self.edit(range, text, Inserted::Typed, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        selected_utf16: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .map(|r| self.range_from_utf16(&r))
            .or(self.marked.clone())
            .unwrap_or(self.selected.clone());
        let start = self.clamp(range.start.min(range.end));
        self.edit(range, text, Inserted::Typed, cx);
        self.marked = (!text.is_empty()).then(|| start..start + text.len());
        if let Some(selected) = selected_utf16 {
            // Within the marked text, in UTF-16.
            let marked = &self.text[start..start + text.len()];
            let to_bytes = |utf16: usize| {
                let mut count = 0;
                for (ix, c) in marked.char_indices() {
                    if count >= utf16 {
                        return ix;
                    }
                    count += c.len_utf16();
                }
                marked.len()
            };
            self.selected = start + to_bytes(selected.start)..start + to_bytes(selected.end);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        self.range_to_bounds(&range)
    }

    fn character_index_for_point(&mut self, point: Point<Pixels>, _: &mut Window, _: &mut Context<Self>) -> Option<usize> {
        let offset = self.offset_for_point(point);
        Some(self.utf16_from(offset))
    }
}

impl Render for RichInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(("rich-input", cx.entity_id()))
            .test_support()
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .w_full()
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::delete_to_line_start))
            .on_action(cx.listener(Self::delete_to_line_end))
            .on_action(cx.listener(Self::delete_word_before))
            .on_action(cx.listener(Self::delete_word_after))
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_up))
            .on_action(cx.listener(Self::select_down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::line_start))
            .on_action(cx.listener(Self::line_end))
            .on_action(cx.listener(Self::select_to_line_start))
            .on_action(cx.listener(Self::select_to_line_end))
            .on_action(cx.listener(Self::start))
            .on_action(cx.listener(Self::end_of_text))
            .on_action(cx.listener(Self::page_up))
            .on_action(cx.listener(Self::page_down))
            .on_action(cx.listener(Self::select_to_start))
            .on_action(cx.listener(Self::select_to_end))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::select_word_left))
            .on_action(cx.listener(Self::select_word_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::right_click))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::mouse_up))
            .on_mouse_move(cx.listener(Self::mouse_move))
            .on_scroll_wheel(cx.listener(Self::scroll))
            .child(element::RichTextElement::new(cx.entity()))
    }
}

#[cfg(test)]
mod tests;
