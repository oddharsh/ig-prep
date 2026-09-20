//! What shape to hand Instagram, and why.
//!
//! Instagram accepts a band of aspect ratios and crops anything outside it.
//! The band, and the pixel width it serves, are the two numbers this whole
//! tool turns on, and both are ASSUMPTIONS until a round trip measures them.
//! They live here as named constants rather than scattered through the code so
//! that calibrating means editing one block.

/// Widest frame Instagram shows without cropping (1.91:1).
pub const MAX_LANDSCAPE: f64 = 1.91;
/// Tallest frame Instagram shows without cropping (3:4).
pub const MIN_PORTRAIT: f64 = 0.75;

/// Delivery width. Measured in September 2026 on real posts: Instagram served
/// uploads at their own width, unchanged, up to 3072 for single posts and
/// carousel items alike, and reduced full-resolution uploads to 3072x4096.
/// Whether that cap is 3072 wide or 4096 tall is untested on frames other
/// than 3:4.
pub const TARGET_WIDTH: u32 = 3072;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Keep the full frame and leave the crop decision in the app.
    /// Matching width alone does not prove that the app avoids resampling.
    Full,
    /// Crop to the nearest allowed ratio here rather than in the app.
    Crop,
    /// Pad to the nearest allowed ratio, keeping the whole frame.
    Pad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gravity {
    Center,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plan {
    /// Source rectangle to take, after EXIF rotation.
    pub crop: (u32, u32, u32, u32),
    /// Size to resize that rectangle to.
    pub scale: (u32, u32),
    /// Final canvas, which is larger than `scale` only when padding.
    pub canvas: (u32, u32),
    /// Where `scale` sits inside `canvas`.
    pub offset: (u32, u32),
    pub ratio: f64,
    /// True when the source ratio is outside what Instagram shows uncropped.
    pub outside_band: bool,
}

/// Decide everything geometric in one place, from display dimensions.
pub fn plan(w: u32, h: u32, fit: Fit, gravity: Gravity, target_width: u32) -> Plan {
    let ratio = w as f64 / h as f64;
    let outside_band = !(MIN_PORTRAIT..=MAX_LANDSCAPE).contains(&ratio);

    // Choose the padded canvas first, then fit the source into it. Width is
    // the FINAL canvas limit, including borders; source pixels never enlarge.
    if fit == Fit::Pad && outside_band {
        let want = ratio.clamp(MIN_PORTRAIT, MAX_LANDSCAPE);
        let native_width = if ratio < want {
            (h as f64 * want).ceil() as u32
        } else {
            w
        };
        let canvas_w = target_width.min(native_width);
        let canvas_h = if ratio < want {
            (canvas_w as f64 / want).floor().max(1.0) as u32
        } else {
            (canvas_w as f64 / want).ceil().max(1.0) as u32
        };
        let factor = (canvas_w as f64 / w as f64)
            .min(canvas_h as f64 / h as f64)
            .min(1.0);
        let scale_w = (w as f64 * factor).round().max(1.0) as u32;
        let scale_h = (h as f64 * factor).round().max(1.0) as u32;
        return Plan {
            crop: (0, 0, w, h),
            scale: (scale_w, scale_h),
            canvas: (canvas_w, canvas_h),
            offset: ((canvas_w - scale_w) / 2, (canvas_h - scale_h) / 2),
            ratio: canvas_w as f64 / canvas_h as f64,
            outside_band,
        };
    }
    let width = target_width.min(w);

    let (crop, ratio_used) = match (fit, outside_band) {
        (Fit::Crop, true) => {
            let want = if ratio < MIN_PORTRAIT {
                MIN_PORTRAIT
            } else {
                MAX_LANDSCAPE
            };
            if ratio < want {
                // Too tall: take a shorter slice, full width.
                let new_h = (w as f64 / want).floor() as u32;
                let y = match gravity {
                    Gravity::Top => 0,
                    Gravity::Bottom => h - new_h,
                    Gravity::Center => (h - new_h) / 2,
                };
                ((0, y, w, new_h), want)
            } else {
                // Too wide: take a narrower slice, full height.
                let new_w = (h as f64 * want).floor() as u32;
                ((((w - new_w) / 2), 0, new_w, h), want)
            }
        }
        _ => ((0, 0, w, h), ratio),
    };

    let (cw, ch) = (crop.2, crop.3);
    let scale_w = width.min(cw);
    let scale_h = ((scale_w as f64) / (cw as f64 / ch as f64))
        .round()
        .max(1.0) as u32;

    let canvas = (scale_w, scale_h);
    let offset = (0, 0);

    Plan {
        crop,
        scale: (scale_w, scale_h),
        canvas,
        offset,
        ratio: ratio_used,
        outside_band,
    }
}

/// Tolerance on a hand-chosen window's ratio, so integer rounding at the
/// band's edges (a 3:4 window of 5152 wide is 6869 rows, 0.75004) passes.
const RATIO_TOLERANCE: f64 = 0.002;

/// A window chosen by hand, in display pixels, delivered at the target width.
///
/// The window has to be inside the band because the whole point is that the
/// app then has nothing to crop, and inside the frame because there is
/// nothing to pad it with. Source pixels never enlarge, as everywhere else.
pub fn plan_window(
    w: u32,
    h: u32,
    window: (u32, u32, u32, u32),
    target_width: u32,
) -> Result<Plan, String> {
    let (x, y, cw, ch) = window;
    if cw == 0 || ch == 0 {
        return Err("window must have a width and a height".into());
    }
    if x.checked_add(cw).is_none_or(|r| r > w) || y.checked_add(ch).is_none_or(|b| b > h) {
        return Err(format!(
            "window {cw}x{ch} at {x},{y} falls outside the {w}x{h} frame"
        ));
    }
    let ratio = cw as f64 / ch as f64;
    if !(MIN_PORTRAIT - RATIO_TOLERANCE..=MAX_LANDSCAPE + RATIO_TOLERANCE).contains(&ratio) {
        return Err(format!(
            "window ratio {ratio:.4} is outside Instagram's band ({MIN_PORTRAIT} to {MAX_LANDSCAPE})"
        ));
    }
    let scale_w = target_width.min(cw);
    let scale_h = ((scale_w as f64) / ratio).round().max(1.0) as u32;
    Ok(Plan {
        crop: window,
        scale: (scale_w, scale_h),
        canvas: (scale_w, scale_h),
        offset: (0, 0),
        ratio,
        outside_band: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hand_chosen_window_lands_on_exact_tier_dimensions() {
        // The full width of a 2:3 portrait at 3:4, 4:5 and 1:1.
        for (ch, expect) in [(6869, 4096), (6440, 3840), (5152, 3072)] {
            let p = plan_window(5152, 7728, (0, 400, 5152, ch), 3072).unwrap();
            assert_eq!(p.scale, (3072, expect));
            assert_eq!(p.canvas, p.scale, "never padded");
            assert!(!p.outside_band);
        }
        // Zoomed in to exactly the tier width: delivered as is, never enlarged.
        let p = plan_window(5152, 7728, (1000, 1000, 3072, 4096), 3072).unwrap();
        assert_eq!(p.scale, (3072, 4096));
        let p = plan_window(5152, 7728, (0, 0, 1536, 2048), 3072).unwrap();
        assert_eq!(p.scale, (1536, 2048));
        // A landscape frame that already fits is its own window.
        let p = plan_window(7728, 5152, (0, 0, 7728, 5152), 3072).unwrap();
        assert_eq!(p.scale, (3072, 2048));
    }

    #[test]
    fn a_window_outside_the_frame_or_the_band_is_refused() {
        assert!(plan_window(5152, 7728, (1, 0, 5152, 6869), 3072).is_err());
        assert!(plan_window(5152, 7728, (0, 1000, 5152, 6869), 3072).is_err());
        assert!(plan_window(5152, 7728, (0, 0, 0, 10), 3072).is_err());
        assert!(
            plan_window(5152, 7728, (0, 0, 5152, 7728), 3072).is_err(),
            "2:3 is too tall"
        );
        assert!(
            plan_window(7728, 5152, (0, 0, 7728, 3000), 3072).is_err(),
            "wider than 1.91:1"
        );
        assert!(plan_window(5152, 7728, (0, 0, u32::MAX, 10), 3072).is_err());
    }

    /// This verifies geometry, not Instagram's upload implementation.
    #[test]
    fn a_three_four_crop_of_a_full_frame_fits_at_the_requested_width() {
        let p = plan(5152, 7728, Fit::Full, Gravity::Center, 1440);
        assert_eq!(p.scale.0, 1440, "delivered at the target width");
        // The user drags a 3:4 window over it in the app.
        let crop_h = (p.scale.0 as f64 / MIN_PORTRAIT).round() as u32;
        assert_eq!((p.scale.0, crop_h), (1440, 1920));
        assert!(crop_h <= p.scale.1, "the 3:4 window fits inside the frame");
    }

    #[test]
    fn three_two_landscape_is_inside_the_band_and_only_scales() {
        let p = plan(7728, 5152, Fit::Full, Gravity::Center, 1440);
        assert!(!p.outside_band);
        assert_eq!(p.scale, (1440, 960));
        assert_eq!(p.canvas, p.scale, "no padding for a frame that fits");
    }

    #[test]
    fn cropping_a_two_three_portrait_takes_height_only() {
        let p = plan(5152, 7728, Fit::Crop, Gravity::Center, 1440);
        assert_eq!(p.crop.2, 5152, "full width is kept");
        assert_eq!(p.crop.3, 6869, "height is cut to reach 3:4");
        assert_eq!(p.scale, (1440, 1920));
    }

    #[test]
    fn padding_widens_rather_than_cutting() {
        let p = plan(5152, 7728, Fit::Pad, Gravity::Center, 1440);
        assert_eq!(p.crop, (0, 0, 5152, 7728), "nothing is cut");
        assert_eq!(p.canvas.0 as f64 / p.canvas.1 as f64, MIN_PORTRAIT);
        assert_eq!(p.canvas, (1440, 1920));
        assert_eq!(p.scale, (1280, 1920));
        assert!(p.offset.0 > 0, "the frame is centred in a wider canvas");
    }

    #[test]
    fn a_small_frame_is_never_enlarged() {
        let p = plan(900, 1200, Fit::Full, Gravity::Center, 1440);
        assert_eq!(p.scale, (900, 1200));
    }

    #[test]
    fn gravity_moves_the_crop_window() {
        let top = plan(5152, 7728, Fit::Crop, Gravity::Top, 1440);
        let bottom = plan(5152, 7728, Fit::Crop, Gravity::Bottom, 1440);
        assert_eq!(top.crop.1, 0);
        assert_eq!(bottom.crop.1, 7728 - 6869);
    }

    #[test]
    fn three_four_is_in_band_for_every_fit() {
        for fit in [Fit::Full, Fit::Crop, Fit::Pad] {
            let p = plan(3000, 4000, fit, Gravity::Center, 1440);
            assert!(!p.outside_band);
            assert_eq!(p.canvas, (1440, 1920));
            assert_eq!(p.crop, (0, 0, 3000, 4000));
        }
    }
    #[test]
    fn padded_canvas_obeys_width_and_never_enlarges_content() {
        for (w, h) in [
            (5152, 7728),
            (1440, 2160),
            (90, 180),
            (10000, 1000),
            (1, 100),
            (100, 1),
        ] {
            for width in [1, 1080, 1440] {
                let p = plan(w, h, Fit::Pad, Gravity::Center, width);
                assert!(p.canvas.0 <= width);
                assert!(p.scale.0 <= w && p.scale.1 <= h);
                assert!(p.offset.0 + p.scale.0 <= p.canvas.0);
                assert!(p.offset.1 + p.scale.1 <= p.canvas.1);
                assert!((MIN_PORTRAIT..=MAX_LANDSCAPE).contains(&p.ratio));
            }
        }
        let p = plan(90, 180, Fit::Pad, Gravity::Center, 1440);
        assert_eq!(p.scale, (90, 180));
        assert_eq!(p.canvas, (135, 180));
    }
}
