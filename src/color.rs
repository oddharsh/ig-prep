//! Decode source colour into linear sRGB; encode its transfer function only at export.

use crate::image::Rgb;
use moxcms::{ColorProfile, DataColorSpace, Layout, RenderingIntent, TransformOptions};

/// Versioned upstream profile bytes are also the output profile. See profiles/README.md.
pub const SRGB_ICC: &[u8] = include_bytes!("../profiles/sRGB-v4.icc");
#[cfg(test)]
pub const P3_ICC: &[u8] = include_bytes!("../profiles/DisplayP3-v4.icc");

pub fn srgb_profile() -> ColorProfile {
    static PROFILE: std::sync::OnceLock<ColorProfile> = std::sync::OnceLock::new();
    PROFILE
        .get_or_init(|| ColorProfile::new_from_slice(SRGB_ICC).expect("bundled sRGB profile"))
        .clone()
}

pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub fn linear_to_srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

pub fn profile(icc: Option<&[u8]>) -> Result<ColorProfile, String> {
    match icc {
        Some(bytes) => ColorProfile::new_from_slice(bytes).map_err(|e| format!("ICC profile: {e}")),
        None => Ok(srgb_profile()),
    }
}

/// Untagged images are assumed sRGB. Explicit but unsupported profiles fail.
/// Quantization is deferred until export, including when no resize is needed.
pub fn convert<T: Copy + Into<f32>>(
    w: u32,
    h: u32,
    samples: &[T],
    channels: usize,
    max: f32,
    source: &ColorProfile,
) -> Result<Rgb, String> {
    let pixels = (w as usize)
        .checked_mul(h as usize)
        .ok_or("image dimensions overflow")?;
    if w == 0
        || h == 0
        || samples.len() != pixels.checked_mul(channels).ok_or("image size overflow")?
    {
        return Err("decoded dimensions do not match the sample buffer".into());
    }
    if let Some(cicp) = source.cicp
        && matches!(
            cicp.transfer_characteristics,
            moxcms::TransferCharacteristics::Smpte2084 | moxcms::TransferCharacteristics::Hlg
        )
    {
        return Err(
            "HDR (PQ/HLG) requires a reviewed SDR tone-mapped export before conversion".into(),
        );
    }
    let gray = channels <= 2;
    let layout = match source.color_space {
        DataColorSpace::Rgb => Layout::Rgb,
        DataColorSpace::Gray if gray => Layout::Gray,
        _ => return Err("source ICC colour space does not match RGB/gray pixels".into()),
    };
    let mut linear = srgb_profile();
    linear.cicp = None;
    linear.red_trc = Some(moxcms::curve_from_gamma(1.0));
    linear.green_trc = linear.red_trc.clone();
    linear.blue_trc = linear.red_trc.clone();
    let options = TransformOptions {
        rendering_intent: RenderingIntent::RelativeColorimetric,
        prefer_fixed_point: false,
        allow_extended_range_rgb_xyz: true,
        ..Default::default()
    };
    let transform = source
        .create_transform_f32(layout, &linear, Layout::Rgb, options)
        .map_err(|e| format!("colour transform: {e}"))?;
    let mut out = vec![0.0; pixels.checked_mul(3).ok_or("image size overflow")?];
    let mut row = Vec::with_capacity(w as usize * 3);
    for (src, dst) in samples
        .chunks_exact(w as usize * channels)
        .zip(out.chunks_exact_mut(w as usize * 3))
    {
        row.clear();
        for p in src.chunks_exact(channels) {
            if layout == Layout::Gray {
                row.push(p[0].into() / max);
            } else if gray {
                row.extend_from_slice(&[p[0].into() / max; 3]);
            } else {
                row.extend(p[..3].iter().map(|&v| v.into() / max));
            }
        }
        transform
            .transform(&row, dst)
            .map_err(|e| format!("colour transform: {e}"))?;
    }
    if out.iter().any(|v| !v.is_finite()) {
        return Err("colour transform produced non-finite pixels".into());
    }
    Ok(Rgb::new(w, h, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p3_primary_converts_to_linear_srgb_without_early_gamut_clipping() {
        let img = convert(
            1,
            1,
            &[1.0f32, 0.0, 0.0],
            3,
            1.0,
            &ColorProfile::new_display_p3(),
        )
        .unwrap();
        // Independently known Display-P3 -> sRGB matrix, red column.
        for (&got, expected) in img.px.iter().zip([1.2249, -0.0421, -0.0196]) {
            assert!((got - expected).abs() < 0.002, "{got} != {expected}");
        }
    }

    #[test]
    fn srgb_midgray_is_linearized_and_roundtrips() {
        let img = convert(1, 1, &[128u8; 3], 3, 255.0, &srgb_profile()).unwrap();
        assert!((img.px[0] - 0.21586).abs() < 0.0001);
        assert!((linear_to_srgb(img.px[0]) * 255.0 - 128.0).abs() < 0.01);
    }

    #[test]
    fn adjacent_sixteen_bit_samples_remain_distinct() {
        let img = convert(
            2,
            1,
            &[32768u16, 32768, 32768, 32769, 32769, 32769],
            3,
            65535.0,
            &srgb_profile(),
        )
        .unwrap();
        assert!(img.px[3] > img.px[0]);
    }

    #[test]
    fn malformed_icc_is_not_silently_assumed_srgb() {
        assert!(profile(Some(b"bad profile")).is_err());
    }
}
