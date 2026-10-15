//! The chat list's width, as Signal Desktop sizes its left pane
//! (`ts/util/leftPaneWidth.std.ts`): dragged between 97 and 380 pixels;
//! under 200 it snaps to the narrow 97 (avatars only), otherwise it's at
//! least 280. Searching, the archive and the like need the full width.

/// Desktop's `MIN_WIDTH`: avatars only.
pub const MIN_WIDTH: f32 = 97.;
/// Below this, a drag snaps to [`MIN_WIDTH`].
const SNAP_WIDTH: f32 = 200.;
/// Desktop's `MIN_FULL_WIDTH`: names and previews fit from here.
pub const MIN_FULL_WIDTH: f32 = 280.;
/// Desktop's `MAX_WIDTH`.
pub const MAX_WIDTH: f32 = 380.;
/// Before any drag (Desktop's default preferred width is its maximum; this
/// app has used 340).
pub const DEFAULT_WIDTH: f32 = 340.;

/// The width to draw for a preferred (dragged-to) width
/// (Desktop's `getWidthFromPreferredWidth`).
pub fn width(preferred: f32, requires_full: bool) -> f32 {
    let clamped = if preferred.is_finite() { preferred.clamp(MIN_WIDTH, MAX_WIDTH) } else { DEFAULT_WIDTH };
    if requires_full || clamped >= SNAP_WIDTH { clamped.max(MIN_FULL_WIDTH) } else { MIN_WIDTH }
}

/// Avatars only.
pub fn is_narrow(width: f32) -> bool {
    width < MIN_FULL_WIDTH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_snap_as_desktops_do() {
        assert_eq!(width(340., false), 340.);
        assert_eq!(width(900., false), MAX_WIDTH);
        assert_eq!(width(250., false), MIN_FULL_WIDTH, "between the snap and full: full");
        assert_eq!(width(199., false), MIN_WIDTH, "under the snap: narrow");
        assert_eq!(width(10., false), MIN_WIDTH);
        assert_eq!(width(10., true), MIN_FULL_WIDTH, "searching needs the names");
        assert_eq!(width(f32::NAN, false), DEFAULT_WIDTH);
        assert!(is_narrow(width(150., false)) && !is_narrow(width(300., false)));
    }
}
