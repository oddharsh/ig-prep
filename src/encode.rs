//! Standard sRGB JPEGs, using the site's ZenJPEG optimization settings.
//! These settings do not claim to bypass Instagram's processing.

use crate::image::Rgb;
use std::borrow::Cow;
use zenjpeg::encoder::{
    ChromaSubsampling, EncoderConfig, PixelLayout, ProgressiveScanMode, Unstoppable,
};

/// Identifies the encoder, precision and optimization recipe in experiments.
pub const PROFILE: &str = "zenjpeg-0.8.4-ycbcr-hybrid-progressive-search-f32-v1";
pub const DEFAULT_QUALITY: u8 = 99;

/// What the chroma plane is worth on the way out, named for the JPEG ratio.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Chroma {
    /// 4:4:4, chroma at luma resolution.
    Full,
    /// 4:2:2, chroma halved horizontally.
    Halved,
    /// 4:2:0, chroma halved on both axes.
    Quartered,
}

impl Chroma {
    fn sampling(self) -> ChromaSubsampling {
        match self {
            Chroma::Full => ChromaSubsampling::None,
            Chroma::Halved => ChromaSubsampling::HalfHorizontal,
            Chroma::Quartered => ChromaSubsampling::Quarter,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Chroma::Full => "4:4:4",
            Chroma::Halved => "4:2:2",
            Chroma::Quartered => "4:2:0",
        }
    }
}

pub fn jpeg(img: &Rgb, quality: u8, chroma: Chroma, dither: bool) -> Result<Vec<u8>, String> {
    if img.w == 0 || img.h == 0 || img.w > 65535 || img.h > 65535 {
        return Err("JPEG dimensions must be 1..65535".into());
    }
    let samples = (img.w as usize)
        .checked_mul(img.h as usize)
        .and_then(|n| n.checked_mul(3));
    if samples != Some(img.px.len()) || img.px.iter().any(|c| !c.is_finite()) {
        return Err("JPEG requires a complete, finite RGB buffer".into());
    }
    if !(1..=100).contains(&quality) {
        return Err("JPEG quality must be 1..100".into());
    }
    let pixels = export_pixels(img, dither);
    let config = EncoderConfig::ycbcr(quality, chroma.sampling())
        // auto_optimize resets scan mode, so scan search must follow it.
        .auto_optimize(true)
        .scan_mode(ProgressiveScanMode::ProgressiveSearch)
        // In 0.8.4 the SharpYUV path interprets samples as u8, even with an
        // f32 layout. Use the standard float conversion/downsampling path.
        .sharp_yuv(false);
    let mut enc = config
        .request()
        .icc_profile(crate::color::SRGB_ICC)
        .encode_from_bytes(img.w, img.h, PixelLayout::RgbF32Linear)
        .map_err(|e| format!("encode: {e}"))?;
    enc.push_packed(bytemuck::cast_slice(pixels.as_ref()), Unstoppable)
        .map_err(|e| format!("encode: {e}"))?;
    enc.finish().map_err(|e| format!("encode: {e}"))
}

/// Clip at export, retaining fractional samples through ZenJPEG's transform.
/// Optional neutral noise is still +/- half an 8-bit sRGB step, but no longer
/// followed by an explicit 8-bit rounding pass. Off by default.
fn export_pixels(img: &Rgb, dither: bool) -> Cow<'_, [f32]> {
    if !dither && img.px.iter().all(|c| (0.0..=1.0).contains(c)) {
        return Cow::Borrowed(&img.px);
    }
    let mut out = Vec::with_capacity(img.px.len());
    for (i, p) in img.px.as_chunks::<3>().0.iter().enumerate() {
        let noise = if dither {
            let mut x = (i as u32).wrapping_add(0x9e3779b9);
            x = (x ^ (x >> 16)).wrapping_mul(0x85ebca6b);
            x = (x ^ (x >> 13)).wrapping_mul(0xc2b2ae35);
            x ^= x >> 16;
            ((x >> 8) as f32 / 16777216.0 - 0.5) / 255.0
        } else {
            0.0
        };
        out.extend(p.iter().map(|&c| {
            if dither {
                crate::color::srgb_to_linear(
                    (crate::color::linear_to_srgb(c) + noise).clamp(0.0, 1.0),
                )
            } else {
                c.clamp(0.0, 1.0)
            }
        }));
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progressive_jpeg_preserves_colour_profile_sampling_and_odd_dimensions() {
        // Smooth, non-neutral colours catch a mistaken linear/sRGB input layout.
        let mut px = Vec::new();
        for y in 0..19 {
            for x in 0..33 {
                px.extend([0.15 + x as f32 / 100.0, 0.4, 0.5 + y as f32 / 100.0]);
            }
        }
        let img = Rgb::new(33, 19, px);
        for chroma in [Chroma::Full, Chroma::Halved, Chroma::Quartered] {
            let bytes = jpeg(&img, 98, chroma, false).unwrap();
            let mut decoder = zune_jpeg::JpegDecoder::new(&bytes);
            let decoded = decoder.decode().unwrap();
            assert_eq!(decoder.dimensions(), Some((33, 19)));
            assert_eq!(decoder.icc_profile().unwrap(), crate::color::SRGB_ICC);
            let sof = bytes.windows(2).position(|p| p == [0xff, 0xc2]).unwrap();
            assert_eq!(bytes[sof + 4], 8, "standard 8-bit JPEG precision");
            assert_eq!(
                bytes[sof + 11],
                match chroma {
                    Chroma::Full => 0x11,
                    Chroma::Halved => 0x21,
                    Chroma::Quartered => 0x22,
                }
            );
            assert!(bytes.windows(2).filter(|p| *p == [0xff, 0xda]).count() > 1);
            assert_eq!(decoded.len(), img.px.len());
            for (i, (&actual, &linear)) in decoded.iter().zip(&img.px).enumerate() {
                let expected = crate::color::linear_to_srgb(linear) * 255.0;
                assert!(
                    (actual as f32 - expected).abs() < 6.0,
                    "{} sample {i}: decoded {actual}, expected {expected}",
                    chroma.label()
                );
            }
        }
    }

    #[test]
    fn export_retains_fractional_precision_and_clips_gamut() {
        let c = crate::color::srgb_to_linear(127.25 / 255.0);
        let img = Rgb::new(1, 1, vec![c; 3]);
        assert_eq!(export_pixels(&img, false).as_ref(), img.px);
        let clipped = Rgb::new(1, 1, vec![-0.1, c, 1.1]);
        assert_eq!(export_pixels(&clipped, false).as_ref(), &[0.0, c, 1.0]);
        // Both inputs round to 127 in 8-bit RGB, but the encoder must retain
        // their distinct fractional DC values through quantization.
        let encode_gray = |value: f32| {
            let c = crate::color::srgb_to_linear(value / 255.0);
            jpeg(
                &Rgb::new(8, 8, vec![c; 8 * 8 * 3]),
                100,
                Chroma::Full,
                false,
            )
            .unwrap()
        };
        assert_ne!(encode_gray(127.05), encode_gray(127.45));
    }

    #[test]
    fn dither_is_reproducible_neutral_and_unbiased() {
        let c = crate::color::srgb_to_linear(127.25 / 255.0);
        let img = Rgb::new(100, 100, vec![c; 100 * 100 * 3]);
        let out = export_pixels(&img, true);
        assert_eq!(out, export_pixels(&img, true));
        for p in out.as_chunks::<3>().0 {
            assert_eq!(p[0], p[1]);
            assert_eq!(p[1], p[2]);
        }
        let srgb: Vec<f32> = out
            .iter()
            .map(|&v| crate::color::linear_to_srgb(v) * 255.0)
            .collect();
        assert!(srgb.iter().all(|v| (v - 127.25).abs() <= 0.5001));
        let mean = srgb.iter().map(|&v| v as f64).sum::<f64>() / srgb.len() as f64;
        assert!((mean - 127.25).abs() < 0.02, "{mean}");
    }

    #[test]
    fn invalid_input_is_rejected() {
        for img in [
            Rgb {
                w: 0,
                h: 1,
                px: vec![],
            },
            Rgb {
                w: 65536,
                h: 1,
                px: vec![],
            },
            Rgb {
                w: 1,
                h: 1,
                px: vec![0.0],
            },
            Rgb::new(1, 1, vec![f32::NAN; 3]),
            Rgb::new(1, 1, vec![f32::INFINITY; 3]),
        ] {
            assert!(jpeg(&img, 95, Chroma::Full, false).is_err());
        }
        let img = Rgb::new(1, 1, vec![0.5; 3]);
        for q in [0, 101] {
            assert!(jpeg(&img, q, Chroma::Full, false).is_err());
        }
        for q in [1, 100] {
            let bytes = jpeg(&img, q, Chroma::Full, false).unwrap();
            assert_eq!(
                zune_jpeg::JpegDecoder::new(&bytes).decode().unwrap().len(),
                3
            );
        }
    }
}
