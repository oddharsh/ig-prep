//! Final sRGB quantization and JPEG encoding. Chroma and quality are calibration
//! variables: no setting here claims to bypass Instagram's processing.

use crate::image::Rgb;
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

/// What the chroma plane is worth on the way out, named for the JPEG ratio.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Chroma {
    /// 4:4:4, chroma at luma resolution.
    Full,
    /// 4:2:2, chroma halved horizontally. Half horizontal chroma resolution.
    Halved,
    /// 4:2:0, chroma halved on both axes. Quarter chroma resolution.
    Quartered,
}

impl Chroma {
    fn sampling(self) -> SamplingFactor {
        match self {
            Chroma::Full => SamplingFactor::R_4_4_4,
            Chroma::Halved => SamplingFactor::R_4_2_2,
            Chroma::Quartered => SamplingFactor::R_4_2_0,
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
    let width = u16::try_from(img.w).map_err(|_| "JPEG width exceeds 65535")?;
    let height = u16::try_from(img.h).map_err(|_| "JPEG height exceeds 65535")?;
    if width == 0 || height == 0 {
        return Err("JPEG dimensions must be nonzero".into());
    }
    let pixels = quantize(img, dither);
    let mut out = Vec::new();
    let mut enc = Encoder::new(&mut out, quality);
    enc.set_sampling_factor(chroma.sampling());
    enc.set_progressive(false);
    enc.add_icc_profile(crate::color::SRGB_ICC)
        .map_err(|e| format!("embed sRGB: {e}"))?;
    enc.encode(&pixels, width, height, ColorType::Rgb)
        .map_err(|e| format!("encode: {e}"))?;
    Ok(out)
}

/// Optional deterministic, uniform +/- half-LSB dither, shared by RGB channels
/// to avoid introducing coloured speckle into neutral gradients. Off by default.
pub fn quantize(img: &Rgb, dither: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(img.px.len());
    for (i, p) in img.px.as_chunks::<3>().0.iter().enumerate() {
        let noise = if dither {
            let mut x = (i as u32).wrapping_add(0x9e3779b9);
            x = (x ^ (x >> 16)).wrapping_mul(0x85ebca6b);
            x = (x ^ (x >> 13)).wrapping_mul(0xc2b2ae35);
            x ^= x >> 16;
            (x >> 8) as f32 / 16777216.0 - 0.5
        } else {
            0.0
        };
        out.extend(p.iter().map(|&c| {
            (crate::color::linear_to_srgb(c) * 255.0 + noise)
                .round()
                .clamp(0.0, 255.0) as u8
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jpeg_carries_srgb_and_requested_sampling() {
        let img = Rgb::new(8, 8, vec![0.21586; 8 * 8 * 3]);
        for chroma in [Chroma::Full, Chroma::Halved, Chroma::Quartered] {
            let bytes = jpeg(&img, 95, chroma, false).unwrap();
            let mut decoder = zune_jpeg::JpegDecoder::new(&bytes);
            decoder.decode_headers().unwrap();
            let profile =
                moxcms::ColorProfile::new_from_slice(&decoder.icc_profile().unwrap()).unwrap();
            assert_eq!(profile.color_space, moxcms::DataColorSpace::Rgb);
            let sof = bytes.windows(2).position(|p| p == [0xff, 0xc0]).unwrap();
            assert_eq!(
                bytes[sof + 11],
                match chroma {
                    Chroma::Full => 0x11,
                    Chroma::Halved => 0x21,
                    Chroma::Quartered => 0x22,
                }
            );
        }
    }
    #[test]
    fn dither_is_reproducible_neutral_and_unbiased() {
        let c = crate::color::srgb_to_linear(127.25 / 255.0);
        let img = Rgb::new(100, 100, vec![c; 100 * 100 * 3]);
        let out = quantize(&img, true);
        assert_eq!(out, quantize(&img, true));
        for p in out.as_chunks::<3>().0 {
            assert_eq!(p[0], p[1]);
            assert_eq!(p[1], p[2]);
        }
        let mean = out.iter().map(|&v| v as f64).sum::<f64>() / out.len() as f64;
        assert!((mean - 127.25).abs() < 0.02, "{mean}");
        assert!(quantize(&img, false).iter().all(|&v| v == 127));
    }
}
