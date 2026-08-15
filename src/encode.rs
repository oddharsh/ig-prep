//! Writing the file Instagram receives.
//!
//! Chroma subsampling is the one encoder choice with a real argument behind it.
//! Instagram re-encodes everything it is given, almost certainly at 4:2:0, so
//! the question is not what survives on disk but what its encoder is handed.
//! Giving it 4:4:4 means its own subsampling step averages full-resolution
//! chroma rather than chroma that has already been halved once, and two
//! successive halvings visibly smear saturated edges. The cost is bytes on an
//! upload, which nobody sees.
//!
//! UNVERIFIED, like the rest of the Instagram-facing assumptions here: worth a
//! round trip before anyone treats it as fact.

use crate::image::Rgb;
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

pub fn jpeg(img: &Rgb, quality: u8, subsample: bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut enc = Encoder::new(&mut out, quality);
    enc.set_sampling_factor(if subsample {
        SamplingFactor::F_2_2
    } else {
        SamplingFactor::F_1_1
    });
    enc.set_progressive(false);
    enc.encode(&img.px, img.w as u16, img.h as u16, ColorType::Rgb)
        .map_err(|e| format!("encode: {e}"))?;
    Ok(out)
}
