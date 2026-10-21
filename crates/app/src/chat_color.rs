//! Chat colors as Signal Desktop draws them: its presets
//! (`AxoTokens.ChatBubble` in `ts/axo/AxoTokens.std.ts`), custom colors
//! through `getHSL` (`ts/util/getHSL.std.ts`), and gradients as
//! `getChatBubbleColorData` turns them into CSS.

use gpui_kit::{
    A11ySubtreeBuilder, App, Background, Bounds, Element, ElementId, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId,
    Pixels, Rgba, Size, Styled, Window, linear_color_stop, linear_gradient, rgb,
};
use signal_store::{ChatColor, CustomColor, HueSaturation};

/// A bubble's fill: one color, or a CSS gradient.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BubbleFill {
    Solid(Hsla),
    /// CSS angle (0: to the top, clockwise), start and end colors.
    Gradient(f32, Hsla, Hsla),
}

impl BubbleFill {
    pub fn background(self) -> Background {
        match self {
            Self::Solid(color) => color.into(),
            Self::Gradient(deg, start, end) => linear_gradient(deg, linear_color_stop(start, 0.), linear_color_stop(end, 1.)),
        }
    }
}

/// The part of a gradient laid across the whole window that `bounds`
/// covers: Desktop's outgoing bubbles take `background-attachment: fixed`
/// (`.module-message__container--outgoing` in `stylesheets/_modules.scss`),
/// so the chat's bubbles together show one gradient. gpui draws a
/// gradient within its quad alone, and not along CSS's gradient line
/// (`fill_color` in its shaders), so this picks the angle and stops that
/// make gpui's gradient in `bounds` match CSS's across `window`.
fn window_slice(deg: f32, start: Hsla, end: Hsla, bounds: Bounds<Pixels>, window: Size<Pixels>) -> Slice {
    let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    let (window_w, window_h) = (f32::from(window.width), f32::from(window.height));
    if w <= 0. || h <= 0. {
        return Slice::Solid(start);
    }
    // CSS: 0deg points up, turning clockwise; the gradient line runs
    // through the window's center, as long as the window's corners reach.
    let theta = deg.to_radians();
    let (ux, uy) = (theta.sin(), -theta.cos());
    let line = (window_w * ux).abs() + (window_h * uy).abs();
    let center = bounds.center();
    let offset = (f32::from(center.x) - window_w / 2.) * ux + (f32::from(center.y) - window_h / 2.) * uy;
    // Where along the window's gradient (0 to 1) a point lies, from its
    // distance along the direction from the bubble's center.
    let along = |distance: f32| (offset + distance) / line + 0.5;
    let reach = ((w * ux).abs() + (h * uy).abs()) / 2.;
    let (from, to) = (along(-reach).clamp(0., 1.), along(reach).clamp(0., 1.));
    let color = |at: f32| {
        let (a, b) = (Rgba::from(start), Rgba::from(end));
        let mix = |x: f32, y: f32| x + (y - x) * at;
        Hsla::from(Rgba { r: mix(a.r, b.r), g: mix(a.g, b.g), b: mix(a.b, b.b), a: mix(a.a, b.a) })
    };
    if to - from < 1e-6 {
        return Slice::Solid(color(from));
    }
    // gpui squeezes its direction by the quad's aspect, so aim it where
    // the squeeze leaves it along CSS's.
    let radians = if w > h { (uy * w / h).atan2(ux) } else { uy.atan2(ux * h / w) };
    let (dx, dy) = if w > h { (radians.cos(), radians.sin() * h / w) } else { (radians.cos() * w / h, radians.sin()) };
    // gpui's 0 to 1 runs across the quad's width or height, whichever
    // its direction leans to.
    let span = if dx.abs() > dy.abs() { w } else { h };
    let stop = |at: f32| ((at - 0.5) * line - offset) / span + 0.5;
    Slice::Gradient((radians.to_degrees() + 90.).rem_euclid(360.), [(color(from), stop(from)), (color(to), stop(to))])
}

/// A gradient as gpui draws it: its angle, and its two colors with where
/// they stop.
#[derive(Debug, Clone, Copy)]
enum Slice {
    Solid(Hsla),
    Gradient(f32, [(Hsla, f32); 2]),
}

impl From<Slice> for Background {
    fn from(slice: Slice) -> Self {
        match slice {
            Slice::Solid(color) => color.into(),
            Slice::Gradient(angle, [from, to]) => linear_gradient(angle, linear_color_stop(from.0, from.1), linear_color_stop(to.0, to.1)),
        }
    }
}

/// Fills a bubble with its color, a gradient as a slice of one across the
/// window (`window_slice`), worked out from where it's painted.
pub trait WindowFillExt: Element + Styled + Sized {
    fn window_fill(self, fill: BubbleFill) -> WindowFill<Self> {
        WindowFill { element: self, fill }
    }
}

impl<E: Element + Styled> WindowFillExt for E {}

pub struct WindowFill<E> {
    element: E,
    fill: BubbleFill,
}

impl<E: Element + Styled> IntoElement for WindowFill<E> {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl<E: Element + Styled> Element for WindowFill<E> {
    type RequestLayoutState = E::RequestLayoutState;
    type PrepaintState = E::PrepaintState;

    fn id(&self) -> Option<ElementId> {
        self.element.id()
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        self.element.source_location()
    }

    fn a11y_role(&self) -> Option<gpui_kit::accesskit::Role> {
        self.element.a11y_role()
    }

    fn write_a11y_info(&self, node: &mut gpui_kit::accesskit::Node) {
        self.element.write_a11y_info(node);
    }

    fn a11y_synthetic_children(&mut self, prepaint: &mut Self::PrepaintState, builder: &mut A11ySubtreeBuilder) {
        self.element.a11y_synthetic_children(prepaint, builder);
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.element.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.element.prepaint(id, inspector_id, bounds, request_layout, window, cx)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let background: Background = match self.fill {
            BubbleFill::Solid(color) => color.into(),
            BubbleFill::Gradient(deg, start, end) => window_slice(deg, start, end, bounds, window.viewport_size()).into(),
        };
        self.element.style().background = Some(background.into());
        self.element.paint(id, inspector_id, bounds, request_layout, prepaint, window, cx);
    }
}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

/// Desktop's preset `name`, or none for a name it doesn't have.
pub fn preset(name: &str) -> Option<BubbleFill> {
    let solid = |v| Some(BubbleFill::Solid(hex(v)));
    let gradient = |deg, start, end| Some(BubbleFill::Gradient(deg, hex(start), hex(end)));
    match name {
        "crimson" => solid(0xcf163e),
        "vermilion" => solid(0xc73f0a),
        "burlap" => solid(0x6f6a58),
        "forest" => solid(0x3b7845),
        "wintergreen" => solid(0x1d8663),
        "teal" => solid(0x077d92),
        "blue" => solid(0x336ba3),
        "indigo" => solid(0x6058ca),
        "violet" => solid(0x9932c8),
        "plum" => solid(0xaa377a),
        "taupe" => solid(0x8f616a),
        "steel" => solid(0x71717f),
        "ultramarine" => gradient(180., 0x0552f0, 0x2c6bed),
        "basil" => gradient(180., 0x2f9373, 0x077343),
        "ember" => gradient(168., 0xe57c00, 0x5e0000),
        "fluorescent" => gradient(192., 0xec13dd, 0x1b36c6),
        "infrared" => gradient(192., 0xf65560, 0x442ced),
        "lagoon" => gradient(180., 0x004066, 0x32867d),
        "midnight" => gradient(180., 0x2c2c3a, 0x787891),
        "sea" => gradient(180., 0x498fd4, 0x2c66a0),
        "sublime" => gradient(180., 0x6281d5, 0x974460),
        "tangerine" => gradient(192., 0xdb7133, 0x911231),
        _ => None,
    }
}

/// Desktop's `calculateLightness`: how light a hue is drawn, in percent.
pub fn lightness_for(hue: f32) -> f32 {
    let between = |hue: f32, (min, from): (f32, f32), (max, to): (f32, f32)| from + (hue - min) / (max - min) * (to - from);
    match hue {
        h if h < 60. => between(h, (0., 45.), (60., 30.)),
        h if h < 180. => 30.,
        h if h < 240. => between(h, (180., 30.), (240., 50.)),
        h if h < 300. => between(h, (240., 50.), (300., 40.)),
        h => between(h, (300., 40.), (360., 45.)),
    }
}

/// Desktop's `getHSL`: a custom color's end as a color, its lightness from
/// the hue unless it came with one.
pub fn hsl(color: HueSaturation) -> Hsla {
    let lightness = color.lightness.map_or_else(|| lightness_for(color.hue), |l| l * 100.);
    Hsla { h: color.hue.rem_euclid(360.) / 360., s: (color.saturation / 100.).clamp(0., 1.), l: (lightness / 100.).clamp(0., 1.), a: 1. }
}

/// Desktop's `getChatBubbleColorData` for a custom color: its gradient at
/// `270 - deg` in CSS, or its one color.
pub fn custom(color: &CustomColor) -> BubbleFill {
    match color.end {
        Some(end) => BubbleFill::Gradient((270. - color.deg.unwrap_or(0.)).rem_euclid(360.), hsl(color.start), hsl(end)),
        None => BubbleFill::Solid(hsl(color.start)),
    }
}

/// A quote's fill and border in a chat of color `fill`, as Desktop's
/// `Quote` draws one in either direction (`.module-quote--color-solid` and
/// `--color-gradient` in `stylesheets/components/Quote.scss`): each color
/// 60% of the way to white in a light theme, 40% of the way to black in a
/// dark one. A gradient is fixed to the window, as bubbles are. The border
/// is the color (a gradient's start) beside incoming text, white beside
/// outgoing.
pub fn quote(fill: BubbleFill, outgoing: bool, dark: bool) -> (BubbleFill, Hsla) {
    let tone = |color: Hsla| Hsla { l: if dark { color.l * 0.6 } else { color.l + (1. - color.l) * 0.6 }, ..color };
    let (quote, border) = match fill {
        BubbleFill::Solid(color) => (BubbleFill::Solid(tone(color)), color),
        BubbleFill::Gradient(deg, start, end) => (BubbleFill::Gradient(deg, tone(start), tone(end)), start),
    };
    (quote, if outgoing { Hsla { h: 0., s: 0., l: 1., a: 1. } } else { border })
}

/// What a chat color fills our bubbles with; a preset Desktop doesn't
/// have falls back to ultramarine, its default.
pub fn fill(color: &ChatColor) -> BubbleFill {
    match color {
        ChatColor::Preset { name } => preset(name).or_else(|| preset("ultramarine")).expect("ultramarine is a preset"),
        ChatColor::Custom { value, .. } => custom(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_has_desktops_colors() {
        for name in signal_store::CHAT_COLOR_PRESETS {
            assert!(preset(name).is_some(), "{name}");
        }
        assert_eq!(preset("crimson"), Some(BubbleFill::Solid(hex(0xcf163e))));
        assert_eq!(fill(&ChatColor::preset("nonsense")), preset("ultramarine").unwrap());
    }

    #[test]
    fn custom_colors_follow_get_hsl() {
        // Desktop's lightness table: 45% at red, 30% from yellow to cyan,
        // 50% at blue.
        assert_eq!(lightness_for(0.), 45.);
        assert_eq!(lightness_for(120.), 30.);
        assert_eq!(lightness_for(240.), 50.);
        assert_eq!(lightness_for(210.), 40.);
        let solid = CustomColor { start: HueSaturation { hue: 240., saturation: 50., lightness: None }, end: None, deg: None };
        assert_eq!(custom(&solid), BubbleFill::Solid(Hsla { h: 240. / 360., s: 0.5, l: 0.5, a: 1. }));
        // A color from a backup keeps its lightness.
        let kept = HueSaturation { hue: 0., saturation: 100., lightness: Some(0.2) };
        assert!((hsl(kept).l - 0.2).abs() < 1e-6);
        let gradient = CustomColor { end: Some(kept), deg: Some(90.), ..solid };
        assert!(matches!(custom(&gradient), BubbleFill::Gradient(deg, _, _) if deg == 180.));
    }

    #[test]
    fn quotes_take_a_tone_of_the_chat_color() {
        let color = Hsla { h: 0.1, s: 0.8, l: 0.5, a: 1. };
        let white = Hsla { h: 0., s: 0., l: 1., a: 1. };
        // Quote.scss: css-darken($solid, 0.4) and css-lighten($solid, 0.6).
        assert_eq!(quote(BubbleFill::Solid(color), false, true), (BubbleFill::Solid(Hsla { l: 0.3, ..color }), color));
        assert_eq!(quote(BubbleFill::Solid(color), false, false), (BubbleFill::Solid(Hsla { l: 0.8, ..color }), color));
        assert_eq!(quote(BubbleFill::Solid(color), true, true).1, white);
        let end = Hsla { l: 0.2, ..color };
        let (fill, border) = quote(BubbleFill::Gradient(168., color, end), false, true);
        assert_eq!(border, color);
        let BubbleFill::Gradient(deg, start, stop) = fill else { panic!("a gradient stays one") };
        assert_eq!(deg, 168.);
        assert!((start.l - 0.3).abs() < 1e-6 && (stop.l - 0.12).abs() < 1e-6);
    }

    /// gpui's `fill_color` for a two-stop linear gradient at a point, as
    /// its Metal shader works it out.
    fn gpui_color((angle, [a, b]): (f32, [(Hsla, f32); 2]), bounds: Bounds<Pixels>, x: f32, y: f32) -> Rgba {
        let (w, h) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let radians = (angle.rem_euclid(360.) - 90.).to_radians();
        let (mut dx, mut dy) = (radians.cos(), radians.sin());
        if w > h { dy *= h / w } else { dx *= w / h }
        let center = bounds.center();
        let (cx, cy) = (x - f32::from(center.x), y - f32::from(center.y));
        let mut t = (cx * dx + cy * dy) / (dx * dx + dy * dy).sqrt();
        t = if dx.abs() > dy.abs() { (t + w / 2.) / w } else { (t + h / 2.) / h };
        let t = ((t - a.1) / (b.1 - a.1)).clamp(0., 1.);
        let (a, b) = (Rgba::from(a.0), Rgba::from(b.0));
        Rgba { r: a.r + (b.r - a.r) * t, g: a.g + (b.g - a.g) * t, b: a.b + (b.b - a.b) * t, a: 1. }
    }

    /// CSS's color at a point of a `deg` gradient across the window.
    fn css_color(deg: f32, start: Hsla, end: Hsla, window: Size<Pixels>, x: f32, y: f32) -> Rgba {
        let (w, h) = (f32::from(window.width), f32::from(window.height));
        let theta = deg.to_radians();
        let (ux, uy) = (theta.sin(), -theta.cos());
        let line = (w * ux).abs() + (h * uy).abs();
        let t = (((x - w / 2.) * ux + (y - h / 2.) * uy) / line + 0.5).clamp(0., 1.);
        let (a, b) = (Rgba::from(start), Rgba::from(end));
        Rgba { r: a.r + (b.r - a.r) * t, g: a.g + (b.g - a.g) * t, b: a.b + (b.b - a.b) * t, a: 1. }
    }

    #[test]
    fn bubbles_show_their_slice_of_the_window_gradient() {
        use gpui_kit::{point, px, size};
        let (start, end) = (hex(0xe57c00), hex(0x5e0000));
        let window = size(px(1200.), px(900.));
        // Wide and tall bubbles, one partly scrolled off the top, at
        // ember's angle, straight down, across and a steep diagonal.
        let bubbles = [
            Bounds::new(point(px(700.), px(80.)), size(px(420.), px(36.))),
            Bounds::new(point(px(300.), px(400.)), size(px(120.), px(300.))),
            Bounds::new(point(px(800.), px(-30.)), size(px(300.), px(90.))),
            Bounds::new(point(px(500.), px(500.)), size(px(80.), px(80.))),
        ];
        let mut gradients = 0;
        for deg in [168., 180., 90., 192., 30., 300.] {
            for bounds in bubbles {
                let slice = window_slice(deg, start, end, bounds, window);
                gradients += usize::from(matches!(slice, Slice::Gradient(..)));
                for (fx, fy) in [(0., 0.), (1., 0.), (0., 1.), (1., 1.), (0.5, 0.5), (0.2, 0.7)] {
                    let x = f32::from(bounds.origin.x) + fx * f32::from(bounds.size.width);
                    let y = f32::from(bounds.origin.y) + fy * f32::from(bounds.size.height);
                    if !(0. ..=f32::from(window.height)).contains(&y) {
                        continue;
                    }
                    let ours = match slice {
                        Slice::Gradient(angle, stops) => gpui_color((angle, stops), bounds, x, y),
                        Slice::Solid(color) => Rgba::from(color),
                    };
                    let css = css_color(deg, start, end, window, x, y);
                    for (a, b) in [(ours.r, css.r), (ours.g, css.g), (ours.b, css.b)] {
                        assert!((a - b).abs() < 2e-3, "{deg} {bounds:?} ({x}, {y}): {ours:?} vs {css:?}");
                    }
                }
            }
        }
        assert_eq!(gradients, 24);
    }
}
