//! The rich input's text laid out: its lines shaped with a run per style,
//! wrapped to the input's width, and the visual rows they wrap into, for
//! finding where an offset is drawn and which offset a point is over.

use std::ops::Range;

use gpui_kit::{
    Font, FontStyle, FontWeight, Hsla, Pixels, Point, SharedString, StrikethroughStyle, TextRun, UnderlineStyle,
    WrappedLine, Window, point, px,
};
use signal_store::TextStyle;

use super::styles::Styles;

/// The font monospace text is drawn in, as in a message bubble.
pub const MONOSPACE_FAMILY: &str = "Menlo";

/// How the text is drawn, from the input's style.
#[derive(Clone)]
pub struct Look {
    pub font: Font,
    pub font_size: Pixels,
    pub line_height: Pixels,
    pub color: Hsla,
    pub placeholder_color: Hsla,
    /// Behind spoiler text: Desktop's composer shows a spoiler as a shaded
    /// box over still-readable text (`.quill--spoiler`).
    pub spoiler_background: Hsla,
}

/// The runs that draw `text` with `styles`, `marked` (IME composition)
/// underlined.
pub fn runs(text: &str, styles: &Styles, marked: Option<&Range<usize>>, look: &Look) -> Vec<TextRun> {
    let mut runs: Vec<TextRun> = Vec::new();
    let mut start = 0;
    let marked_at = |at: usize| marked.is_some_and(|m| m.start <= at && at < m.end);
    while start < text.len() {
        let (set, in_marked) = (styles.at(start), marked_at(start));
        let mut end = start + 1;
        while end < text.len() && styles.at(end) == set && marked_at(end) == in_marked {
            end += 1;
        }
        let mut font = look.font.clone();
        if set.has(TextStyle::Monospace) {
            font.family = MONOSPACE_FAMILY.into();
        }
        if set.has(TextStyle::Bold) {
            font.weight = FontWeight::BOLD;
        }
        if set.has(TextStyle::Italic) {
            font.style = FontStyle::Italic;
        }
        runs.push(TextRun {
            len: end - start,
            font,
            color: look.color,
            background_color: set.has(TextStyle::Spoiler).then_some(look.spoiler_background),
            underline: in_marked.then_some(UnderlineStyle { thickness: px(1.), color: Some(look.color), wavy: false }),
            strikethrough: set
                .has(TextStyle::Strikethrough)
                .then_some(StrikethroughStyle { thickness: px(1.), color: Some(look.color) }),
        });
        start = end;
    }
    runs
}

/// Shapes `text` (split at its newlines) wrapped to `width`.
pub fn shape(text: &str, runs: &[TextRun], width: Option<Pixels>, look: &Look, window: &Window) -> Vec<WrappedLine> {
    let runs: Vec<TextRun> = if text.is_empty() { Vec::new() } else { runs.to_vec() };
    window
        .text_system()
        .shape_text(SharedString::from(text.to_owned()), look.font_size, &runs, width, None)
        .map(|lines| lines.into_vec())
        .unwrap_or_default()
}

/// One visual row: `start..end` of the text (the end is where the next row
/// starts, or the line's end before its newline).
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub line: usize,
    pub start: usize,
    pub end: usize,
    /// Whether it's its line's last row (its end is before a newline or
    /// the text's end, not a wrap).
    pub last: bool,
}

/// Shaped lines and where each starts in the text.
pub struct Laid {
    pub lines: Vec<WrappedLine>,
    pub starts: Vec<usize>,
    pub rows: Vec<Row>,
    pub line_height: Pixels,
}

impl Laid {
    pub fn new(lines: Vec<WrappedLine>, line_height: Pixels) -> Laid {
        let mut starts = Vec::with_capacity(lines.len());
        let mut rows = Vec::new();
        let mut at = 0;
        for (ix, line) in lines.iter().enumerate() {
            starts.push(at);
            let mut row_start = at;
            for boundary in line.wrap_boundaries() {
                let run = &line.unwrapped_layout.runs[boundary.run_ix];
                let wrap_at = at + run.glyphs[boundary.glyph_ix].index;
                rows.push(Row { line: ix, start: row_start, end: wrap_at, last: false });
                row_start = wrap_at;
            }
            rows.push(Row { line: ix, start: row_start, end: at + line.len(), last: true });
            at += line.len() + 1;
        }
        if rows.is_empty() {
            starts.push(0);
            rows.push(Row { line: 0, start: 0, end: 0, last: true });
        }
        Laid { lines, starts, rows, line_height }
    }

    pub fn height(&self) -> Pixels {
        self.line_height * self.rows.len() as f32
    }

    /// The row an offset is drawn in: a caret where a line wraps is at the
    /// start of the next row (`upstream` false), a selection's end at the
    /// end of the row before (`upstream` true).
    pub fn row_for(&self, offset: usize, upstream: bool) -> usize {
        let fits = |row: &Row| {
            if upstream {
                row.start < offset && offset <= row.end || (row.start == offset && (row.last || row.start == row.end))
            } else {
                row.start <= offset && (offset < row.end || (row.last && offset == row.end))
            }
        };
        self.rows.iter().position(fits).unwrap_or(self.rows.len().saturating_sub(1))
    }

    /// x of `offset` in its line's unwrapped layout.
    fn line_x(&self, line: usize, offset: usize) -> Pixels {
        match self.lines.get(line) {
            Some(l) => l.unwrapped_layout.x_for_index(offset.saturating_sub(self.starts[line]).min(l.len())),
            None => px(0.),
        }
    }

    /// Where `offset` is drawn in row `row`, from the text's top left.
    pub fn position_in_row(&self, row: usize, offset: usize) -> Point<Pixels> {
        let r = &self.rows[row];
        let x = self.line_x(r.line, offset) - self.line_x(r.line, r.start);
        point(x, self.line_height * row as f32)
    }

    /// Where a caret at `offset` is drawn, from the text's top left.
    pub fn caret_position(&self, offset: usize) -> Point<Pixels> {
        self.position_in_row(self.row_for(offset, false), offset)
    }

    /// The offset nearest `x` in row `row` (a row that wraps stops before
    /// its last character, so a caret put there stays on that row).
    pub fn offset_in_row(&self, text: &str, row: usize, x: Pixels) -> usize {
        let r = &self.rows[row];
        let Some(line) = self.lines.get(r.line) else { return 0 };
        let line_start = self.starts[r.line];
        let start_x = self.line_x(r.line, r.start);
        let local = line.unwrapped_layout.closest_index_for_x(start_x + x.max(px(0.)));
        let mut offset = (line_start + local).clamp(r.start, r.end);
        if !r.last && offset == r.end && r.end > r.start {
            offset = previous_char(text, r.end);
        }
        offset
    }

    /// The offset nearest `position` (from the text's top left).
    pub fn offset_for_position(&self, text: &str, position: Point<Pixels>) -> usize {
        if position.y < px(0.) {
            return 0;
        }
        let row = (position.y / self.line_height).floor() as usize;
        if row >= self.rows.len() {
            return text.len();
        }
        self.offset_in_row(text, row, position.x)
    }

    /// The rectangles (from the text's top left) that cover `range`, one
    /// per row; a selected newline shows as a little space at its row's end.
    pub fn range_rects(&self, text: &str, range: &Range<usize>) -> Vec<(Point<Pixels>, Point<Pixels>)> {
        let mut rects = Vec::new();
        if range.is_empty() {
            return rects;
        }
        let first = self.row_for(range.start, false);
        let last = self.row_for(range.end, true);
        for row in first..=last.max(first) {
            let r = &self.rows[row];
            let start = range.start.max(r.start);
            let end = range.end.min(r.end);
            let top_left = self.position_in_row(row, start);
            let mut right = self.position_in_row(row, end).x;
            if r.last && range.end > r.end && text.as_bytes().get(r.end) == Some(&b'\n') {
                right += self.line_height / 4.;
            }
            if right > top_left.x {
                rects.push((top_left, point(right, top_left.y + self.line_height)));
            }
        }
        rects
    }
}

/// The start of the character before `at`.
pub fn previous_char(text: &str, at: usize) -> usize {
    text[..at.min(text.len())].char_indices().next_back().map_or(0, |(i, _)| i)
}
