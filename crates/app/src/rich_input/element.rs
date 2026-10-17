//! Draws a [`RichInput`]: lays its text out at the width it gets, grows
//! with it between its row limits, and paints the selection, the styled
//! text and the caret, clipped to what it shows.

use std::rc::Rc;

use gpui_kit::{
    App, AvailableSpace, Bounds, ContentMask, Element, ElementId, ElementInputHandler, Entity, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, PaintQuad, Pixels, Style, TextAlign, TextRun, Window, WrappedLine,
    fill, point, px, relative, size,
};

use super::RichInput;
use super::layout::{self, Laid, Look};

pub struct RichTextElement {
    input: Entity<RichInput>,
}

impl RichTextElement {
    pub fn new(input: Entity<RichInput>) -> Self {
        RichTextElement { input }
    }
}

/// What a frame draws, built from the input once per layout.
#[derive(Clone)]
pub struct Content {
    text: String,
    runs: Vec<TextRun>,
    /// The placeholder and its runs while the text is empty.
    placeholder: Option<(String, Vec<TextRun>)>,
    look: Look,
    rows: (usize, usize),
}

impl Content {
    fn new(input: &RichInput, window: &Window, cx: &App) -> Content {
        let style = window.text_style();
        let palette = crate::theme::Palette::get(cx);
        let look = Look {
            font: style.font(),
            font_size: input.text_size,
            line_height: input.line_height,
            color: style.color,
            placeholder_color: palette.text_secondary,
            spoiler_background: gpui_kit::hsla(0., 0., 0.5, 0.3),
        };
        let runs = layout::runs(&input.text, &input.styles, input.marked.as_ref(), &look);
        let placeholder = (input.text.is_empty() && !input.placeholder.is_empty()).then(|| {
            let text = input.placeholder.to_string();
            let run = look.font.clone();
            let runs = vec![TextRun {
                len: text.len(),
                font: run,
                color: look.placeholder_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            (text, runs)
        });
        Content { text: input.text.clone(), runs, placeholder, look, rows: input.rows }
    }

    fn lay_out(&self, width: Option<Pixels>, window: &Window) -> Laid {
        let lines = if self.text.is_empty() { Vec::new() } else { layout::shape(&self.text, &self.runs, width, &self.look, window) };
        Laid::new(lines, self.look.line_height)
    }

    /// The height for `laid`: its rows, kept between the input's limits.
    fn height(&self, laid: &Laid) -> Pixels {
        let rows = laid.rows.len().clamp(self.rows.0, self.rows.1);
        self.look.line_height * rows as f32
    }
}

pub struct Prepainted {
    laid: Rc<Laid>,
    placeholder: Vec<WrappedLine>,
    selection: Vec<PaintQuad>,
    caret: Option<PaintQuad>,
    origin: gpui_kit::Point<Pixels>,
}

impl IntoElement for RichTextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RichTextElement {
    type RequestLayoutState = Content;
    type PrepaintState = Prepainted;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let content = Content::new(self.input.read(cx), window, cx);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let measured = content.clone();
        let id = window.request_measured_layout(style, move |known, available, window, _| {
            let width = known.width.or(match available.width {
                AvailableSpace::Definite(width) => Some(width),
                _ => None,
            });
            let laid = measured.lay_out(width, window);
            size(width.unwrap_or(px(0.)), measured.height(&laid))
        });
        (id, content)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        content: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let laid = Rc::new(content.lay_out(Some(bounds.size.width), window));
        let placeholder = match &content.placeholder {
            Some((text, runs)) => layout::shape(text, runs, Some(bounds.size.width), &content.look, window),
            None => Vec::new(),
        };
        let (scroll_y, selected, caret_at, focused, caret_on) = self.input.update(cx, |input, _| {
            input.laid_out(laid.clone(), bounds);
            (input.scroll_y, input.selected.clone(), input.cursor(), input.focus_handle.is_focused(window), input.caret_on)
        });
        let origin = bounds.origin - point(px(0.), scroll_y);
        let palette = crate::theme::Palette::get(cx);
        let selection = laid
            .range_rects(&content.text, &selected)
            .into_iter()
            .map(|(a, b)| fill(Bounds::from_corners(origin + a, origin + b), gpui_kit::Hsla { a: 0.3, ..palette.accent }))
            .collect();
        let caret = (focused && caret_on && selected.is_empty()).then(|| {
            let at = laid.caret_position(caret_at);
            fill(Bounds::new(origin + at, size(px(1.5), laid.line_height)), palette.text)
        });
        Prepainted { laid, placeholder, selection, caret, origin }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepainted: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus_handle.clone();
        window.handle_input(&focus, ElementInputHandler::new(bounds, self.input.clone()), cx);
        let line_height = prepainted.laid.line_height;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for quad in prepainted.selection.drain(..) {
                window.paint_quad(quad);
            }
            let mut y = prepainted.origin.y;
            for line in &prepainted.placeholder {
                let _ = line.paint(point(bounds.origin.x, y), line_height, TextAlign::Left, None, window, cx);
                y += line.size(line_height).height;
            }
            let mut y = prepainted.origin.y;
            for line in &prepainted.laid.lines {
                let origin = point(prepainted.origin.x, y);
                let _ = line.paint_background(origin, line_height, TextAlign::Left, None, window, cx);
                let _ = line.paint(origin, line_height, TextAlign::Left, None, window, cx);
                y += line.size(line_height).height;
            }
            if let Some(caret) = prepainted.caret.take() {
                window.paint_quad(caret);
            }
        });
    }
}
