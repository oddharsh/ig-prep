//! What shape to hand Instagram, and why.
//!
//! Instagram accepts a band of aspect ratios and crops anything outside it.
//! The band, and the pixel width it serves, are the two numbers this whole
//! tool turns on, and both are ASSUMPTIONS until a round trip measures them.
//! They live here as named constants rather than scattered through the code so
//! that calibrating means editing one block.

/// Widest frame Instagram shows without cropping (1.91:1).
pub const MAX_LANDSCAPE: f64 = 1.91;
/// Tallest frame Instagram shows without cropping (4:5).
pub const MIN_PORTRAIT: f64 = 0.8;

/// Pixel width to deliver.
///
/// UNVERIFIED, and the single most valuable thing a calibration run would
/// settle. 1440 is chosen over 1080 because the failure modes are not
/// symmetric: if Instagram wants 1080 it downscales cleanly from 1440, and if
/// it wants 1440 and gets 1080 it UPSCALES, which invents detail that was never
/// there. Guessing high costs a resample; guessing low costs the picture.
pub const TARGET_WIDTH: u32 = 1440;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Deliver the whole frame at the target width, and crop in the app.
    ///
    /// This is the default because it keeps the framing decision with the
    /// photographer AND costs nothing: cropping a portrait to 4:5 removes
    /// HEIGHT only, so every vertical crop of a target-width frame is already
    /// exactly the target size and Instagram has nothing left to resample.
    /// The one thing that breaks it is pinch-zooming in the crop UI, which
    /// changes the scale and puts the resample back.
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

    // Never enlarge. Upscaling to reach a target invents detail, and a frame
    // smaller than the target is better delivered as it is.
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
                let new_h = (w as f64 / want).round() as u32;
                let y = match gravity {
                    Gravity::Top => 0,
                    Gravity::Bottom => h - new_h,
                    Gravity::Center => (h - new_h) / 2,
                };
                ((0, y, w, new_h), want)
            } else {
                // Too wide: take a narrower slice, full height.
                let new_w = (h as f64 * want).round() as u32;
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

    let (canvas, offset) = if fit == Fit::Pad && outside_band {
        let want = if ratio < MIN_PORTRAIT {
            MIN_PORTRAIT
        } else {
            MAX_LANDSCAPE
        };
        if ratio < want {
            // Too tall to show: widen the canvas rather than cut the picture.
            let cw2 = ((scale_h as f64) * want).round() as u32;
            (
                (cw2.max(scale_w), scale_h),
                ((cw2.saturating_sub(scale_w)) / 2, 0),
            )
        } else {
            let ch2 = ((scale_w as f64) / want).round() as u32;
            (
                (scale_w, ch2.max(scale_h)),
                (0, (ch2.saturating_sub(scale_h)) / 2),
            )
        }
    } else {
        ((scale_w, scale_h), (0, 0))
    };

    Plan {
        crop,
        scale: (scale_w, scale_h),
        canvas,
        offset,
        ratio: ratio_used,
        outside_band,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The claim the default mode rests on: a 4:5 crop of a target-width
    /// portrait is already exactly the target size, so Instagram has no reason
    /// to resample. If this stops holding, the default is no longer free.
    #[test]
    fn a_four_five_crop_of_a_full_frame_needs_no_resample() {
        let p = plan(5152, 7728, Fit::Full, Gravity::Center, 1440);
        assert_eq!(p.scale.0, 1440, "delivered at the target width");
        // The user drags a 4:5 window over it in the app.
        let crop_h = (p.scale.0 as f64 / MIN_PORTRAIT).round() as u32;
        assert_eq!((p.scale.0, crop_h), (1440, 1800));
        assert!(crop_h <= p.scale.1, "the 4:5 window fits inside the frame");
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
        assert_eq!(p.crop.3, 6440, "height is cut to reach 4:5");
        assert_eq!(p.scale, (1440, 1800));
    }

    #[test]
    fn padding_widens_rather_than_cutting() {
        let p = plan(5152, 7728, Fit::Pad, Gravity::Center, 1440);
        assert_eq!(p.crop, (0, 0, 5152, 7728), "nothing is cut");
        assert_eq!(p.canvas.0 as f64 / p.canvas.1 as f64, MIN_PORTRAIT);
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
        assert_eq!(bottom.crop.1, 7728 - 6440);
    }
}
