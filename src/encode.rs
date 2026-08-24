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
//! 4:2:2 is the middle, and it is the honest one for a camera that shoots
//! 4:2:2. It is worth being precise about why the source ratio does NOT settle
//! the question on its own. A Fujifilm HIF halves chroma horizontally in its
//! own landscape orientation, so a 7728x5152 frame carries a 3864x5152 chroma
//! plane; held vertically that becomes 5152x3864, and the halved axis lands on
//! the LONG edge of the portrait. Against a 1440x2160 output that is still
//! 3.58x horizontally and 1.79x vertically of real chroma to average down.
//! Full-resolution output chroma is therefore measured rather than invented,
//! and 4:2:0 here would discard detail the source genuinely resolves.
//!
//! That reasoning holds only while the downscale stays large. Somebody
//! delivering at or near native size has a 4:2:2 source that cannot fill a
//! 4:4:4 output, and should say `--422` and mean it.
//!
//! UNVERIFIED, like the rest of the Instagram-facing assumptions here: which of
//! the three survives a round trip best is exactly what the upload harness is
//! supposed to settle.

use crate::image::Rgb;
use jpeg_encoder::{ColorType, Encoder, SamplingFactor};

/// What the chroma plane is worth on the way out, named for the JPEG ratio.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Chroma {
    /// 4:4:4, chroma at luma resolution.
    Full,
    /// 4:2:2, chroma halved horizontally. What the camera shot.
    Halved,
    /// 4:2:0, chroma halved on both axes. What Instagram will store anyway.
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

pub fn jpeg(img: &Rgb, quality: u8, chroma: Chroma) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut enc = Encoder::new(&mut out, quality);
    enc.set_sampling_factor(chroma.sampling());
    enc.set_progressive(false);
    enc.encode(&img.px, img.w as u16, img.h as u16, ColorType::Rgb)
        .map_err(|e| format!("encode: {e}"))?;
    Ok(out)
}
