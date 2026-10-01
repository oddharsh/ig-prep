//! What Instagram would serve for an upload, built from measured facts rather
//! than a model of its code.
//!
//! Sixteen served renditions fingerprinted on 2026-09-19 (two accounts, HQ
//! upload on) shared one recipe: the upload's own width up to the tier,
//! 4:2:0 chroma, progressive scans, optimised Huffman tables, no ICC profile,
//! and ONE pair of quantisation tables. The pixels a viewer receives depend on
//! the tables and the chroma chain, not on the scan script, so reproducing
//! those two is enough to preview the damage locally. The chroma chain here is
//! the common libjpeg one: triangle upsampling on decode, box averaging on
//! encode. Instagram's exact filters are not known; a 2x2 box is the usual
//! choice and the one every result in the README was scored with.
//!
//! What this deliberately does NOT model: the app's crop, which measured as a
//! 1:1 cut at a fractional vertical offset, and the downscale applied to
//! uploads wider than the tier, which measured soft. Uploads wider than
//! `TARGET_WIDTH` are refused rather than guessed at.

use crate::geometry::{MAX_LANDSCAPE, MIN_PORTRAIT, TARGET_WIDTH};
use jpeg_encoder::{ColorType, Encoder, QuantizationTableType, SamplingFactor};
use std::path::{Path, PathBuf};

/// Luma table in natural order, copied from served files. It matches no
/// scaling of the IJG table: the high frequencies cap at 13.
#[rustfmt::skip]
pub const LUMA: [u16; 64] = [
     2,  2,  2,  3,  4,  5,  7,  8,
     2,  2,  2,  3,  4,  5,  7,  8,
     2,  2,  3,  4,  5,  7,  8, 10,
     3,  3,  4,  5,  7,  8, 10, 11,
     4,  4,  5,  7,  8, 10, 11, 13,
     5,  5,  7,  8, 10, 11, 13, 13,
     7,  7,  8, 10, 11, 13, 13, 13,
     8,  8, 10, 11, 13, 13, 13, 13,
];

/// Chroma table in natural order: the IJG standard chroma table at close to
/// quality 94.
#[rustfmt::skip]
pub const CHROMA: [u16; 64] = [
     2,  2,  3,  6, 13, 13, 13, 13,
     2,  3,  3,  9, 13, 13, 13, 13,
     3,  3,  7, 13, 13, 13, 13, 13,
     6,  9, 13, 13, 13, 13, 13, 13,
    13, 13, 13, 13, 13, 13, 13, 13,
    13, 13, 13, 13, 13, 13, 13, 13,
    13, 13, 13, 13, 13, 13, 13, 13,
    13, 13, 13, 13, 13, 13, 13, 13,
];

/// For tests elsewhere: the natural-order index of each zigzag position.
#[cfg(test)]
#[rustfmt::skip]
pub const ZIGZAG_TEST: [usize; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10, 17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

#[derive(Debug)]
pub struct Simulated {
    pub path: PathBuf,
    pub out: PathBuf,
    pub w: u32,
    pub h: u32,
    pub bytes_in: usize,
    pub bytes_out: usize,
    /// RGB PSNR of the simulation against the upload: how much the encode
    /// alone changes this file. Infinite when nothing changed.
    pub psnr: f64,
    pub outside_band: bool,
}

impl Simulated {
    pub fn line(&self) -> String {
        let psnr = if self.psnr.is_finite() {
            format!("{:.1} dB", self.psnr)
        } else {
            "identical".into()
        };
        let note = if self.outside_band {
            "; outside the band, the app crops before this encode"
        } else {
            ""
        };
        format!(
            "{}: {}x{} {} KB -> {} {} KB, {psnr} against the upload{note}",
            self.path.display(),
            self.w,
            self.h,
            self.bytes_in / 1024,
            self.out.display(),
            self.bytes_out / 1024,
        )
    }
}

/// Re-encode one upload the way the servers did, writing `<stem>.ig.jpg`.
pub fn one(path: &Path, out_dir: &Path) -> Result<Simulated, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let (w, h, rgb) = decode(&bytes)?;
    if w > TARGET_WIDTH {
        return Err(format!(
            "{w} px wide exceeds the {TARGET_WIDTH} px tier; Instagram would downscale it with a filter this simulation does not model"
        ));
    }
    let encoded = encode(w, h, &rgb)?;
    let (_, _, back) = decode(&encoded)?;
    let psnr = psnr(&rgb, &back);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("unnameable file")?;
    let out = out_dir.join(format!("{stem}.ig.jpg"));
    std::fs::write(&out, &encoded).map_err(|e| format!("{}: {e}", out.display()))?;
    let ratio = w as f64 / h as f64;
    Ok(Simulated {
        path: path.to_path_buf(),
        out,
        w,
        h,
        bytes_in: bytes.len(),
        bytes_out: encoded.len(),
        psnr,
        outside_band: !(MIN_PORTRAIT..=MAX_LANDSCAPE).contains(&ratio),
    })
}

/// The servers' recipe on 8-bit RGB: measured tables, 4:2:0, progressive,
/// optimised Huffman. No profile is embedded because none is served.
pub fn encode(w: u32, h: u32, rgb: &[u8]) -> Result<Vec<u8>, String> {
    if w == 0 || h == 0 || w > 65535 || h > 65535 {
        return Err("JPEG dimensions must be 1..65535".into());
    }
    if rgb.len() != (w as usize) * (h as usize) * 3 {
        return Err("simulate requires a complete RGB buffer".into());
    }
    let mut buf = Vec::new();
    // Quality only scales the built-in tables; custom tables are used as given.
    let mut enc = Encoder::new(&mut buf, 100);
    enc.set_quantization_tables(
        QuantizationTableType::Custom(Box::new(LUMA)),
        QuantizationTableType::Custom(Box::new(CHROMA)),
    );
    enc.set_sampling_factor(SamplingFactor::F_2_2);
    enc.set_progressive(true);
    enc.set_optimized_huffman_tables(true);
    // The crate's own sampler decimates, keeping one pixel of every 2x2 block.
    // libjpeg averages the block, and every served file was scored against
    // that chain, so the averaging happens here and the sampler picks it up.
    let ycc = ycbcr_box420(w, h, rgb);
    enc.encode(&ycc, w as u16, h as u16, ColorType::Ycbcr)
        .map_err(|e| format!("simulate: {e}"))?;
    Ok(buf)
}

/// BT.601 YCbCr as libjpeg computes it, with each 2x2 chroma block replaced
/// by its average in all four pixels. Edge pixels average with themselves,
/// which is what libjpeg's edge replication amounts to.
fn ycbcr_box420(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
    let (w, h) = (w as usize, h as usize);
    let mut ycc = vec![0u8; w * h * 3];
    for (p, o) in rgb.chunks_exact(3).zip(ycc.chunks_exact_mut(3)) {
        let (r, g, b) = (p[0] as f32, p[1] as f32, p[2] as f32);
        let to_u8 = |v: f32| v.round().clamp(0.0, 255.0) as u8;
        o[0] = to_u8(0.299 * r + 0.587 * g + 0.114 * b);
        o[1] = to_u8(-0.168_736 * r - 0.331_264 * g + 0.5 * b + 128.0);
        o[2] = to_u8(0.5 * r - 0.418_688 * g - 0.081_312 * b + 128.0);
    }
    for by in (0..h).step_by(2) {
        for bx in (0..w).step_by(2) {
            let ys = by..(by + 2).min(h);
            let xs = bx..(bx + 2).min(w);
            for c in 1..3 {
                let (mut sum, mut n) = (0u32, 0u32);
                for y in ys.clone() {
                    for x in xs.clone() {
                        sum += ycc[(y * w + x) * 3 + c] as u32;
                        n += 1;
                    }
                }
                let avg = ((sum + n / 2) / n) as u8;
                for y in ys.clone() {
                    for x in xs.clone() {
                        ycc[(y * w + x) * 3 + c] = avg;
                    }
                }
            }
        }
    }
    ycc
}

/// Decode as the servers do: 8-bit, profile ignored, greyscale widened to RGB.
///
/// This calls jpeg-decoder directly rather than going through the converter's
/// decode path, which applies the embedded profile and widens to linear float.
/// The servers do neither, so neither does this.
fn decode(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    use jpeg_decoder::PixelFormat;
    let mut d = jpeg_decoder::Decoder::new(std::io::Cursor::new(bytes));
    let px = d.decode().map_err(|e| format!("jpeg: {e}"))?;
    let info = d.info().ok_or("jpeg: no dimensions")?;
    let rgb = match info.pixel_format {
        PixelFormat::RGB24 => px,
        PixelFormat::L8 => px.iter().flat_map(|&v| [v, v, v]).collect(),
        other => return Err(format!("jpeg: unsupported pixel format {other:?}")),
    };
    Ok((info.width as u32, info.height as u32, rgb))
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum::<f64>()
        / a.len().max(1) as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DQT segments store tables in zigzag order; this maps back to natural.
    #[rustfmt::skip]
    const ZIGZAG: [usize; 64] = [
         0,  1,  8, 16,  9,  2,  3, 10, 17, 24, 32, 25, 18, 11,  4,  5,
        12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13,  6,  7, 14, 21, 28,
        35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51,
        58, 59, 52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
    ];

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                px.extend([
                    (40 + x * 150 / w.max(1)) as u8,
                    (90 + y * 120 / h.max(1)) as u8,
                    (200 - x * 60 / w.max(1)) as u8,
                ]);
            }
        }
        px
    }

    fn tables(bytes: &[u8]) -> Vec<(u8, [u16; 64])> {
        let mut out = Vec::new();
        let mut i = 2;
        while i + 4 <= bytes.len() {
            if bytes[i] != 0xff {
                i += 1;
                continue;
            }
            let m = bytes[i + 1];
            if m == 0xda {
                break;
            }
            let len = ((bytes[i + 2] as usize) << 8) | bytes[i + 3] as usize;
            if m == 0xdb {
                let seg = &bytes[i + 4..i + 2 + len];
                let mut p = 0;
                while p < seg.len() {
                    let id = seg[p] & 15;
                    p += 1;
                    let mut natural = [0u16; 64];
                    for (k, &v) in seg[p..p + 64].iter().enumerate() {
                        natural[ZIGZAG[k]] = v as u16;
                    }
                    p += 64;
                    out.push((id, natural));
                }
            }
            i += 2 + len;
        }
        out
    }

    #[test]
    fn served_tables_are_written_verbatim_with_420_progressive_scans() {
        let bytes = encode(33, 19, &gradient(33, 19)).unwrap();
        let dqt = tables(&bytes);
        assert_eq!(dqt.len(), 2);
        assert_eq!(dqt[0], (0, LUMA));
        assert_eq!(dqt[1], (1, CHROMA));
        let sof = bytes.windows(2).position(|p| p == [0xff, 0xc2]).unwrap();
        assert_eq!(bytes[sof + 11], 0x22, "luma sampled 2x2 against chroma");
        assert!(bytes.windows(2).filter(|p| *p == [0xff, 0xda]).count() > 1);
        assert!(
            !bytes.windows(11).any(|w| w == b"ICC_PROFILE"),
            "no profile is served"
        );
    }

    #[test]
    fn simulation_round_trips_and_reports_its_own_damage() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("upload.jpg");
        let upload = crate::encode::jpeg(
            &crate::image::Rgb::new(
                64,
                48,
                gradient(64, 48)
                    .iter()
                    .map(|&v| crate::color::srgb_to_linear(v as f32 / 255.0))
                    .collect(),
            ),
            99,
            crate::encode::Chroma::Full,
            false,
        )
        .unwrap();
        std::fs::write(&src, &upload).unwrap();
        let s = one(&src, dir.path()).unwrap();
        assert_eq!((s.w, s.h), (64, 48));
        assert_eq!(s.out, dir.path().join("upload.ig.jpg"));
        assert_eq!(std::fs::read(&s.out).unwrap().len(), s.bytes_out);
        assert!(
            s.psnr > 35.0,
            "smooth content survives the tables: {}",
            s.psnr
        );
        assert!(!s.outside_band, "4:3 sits inside the band");
        let (w, h, _) = decode(&std::fs::read(&s.out).unwrap()).unwrap();
        assert_eq!((w, h), (64, 48), "the servers keep the upload's size");
    }

    #[test]
    fn greyscale_uploads_widen_to_colour_and_wide_uploads_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let grey = dir.path().join("grey.jpg");
        let mut buf = Vec::new();
        let enc = Encoder::new(&mut buf, 90);
        enc.encode(&[128u8; 16 * 8], 16, 8, ColorType::Luma)
            .unwrap();
        std::fs::write(&grey, &buf).unwrap();
        let s = one(&grey, dir.path()).unwrap();
        let (_, _, rgb) = decode(&std::fs::read(&s.out).unwrap()).unwrap();
        assert_eq!(rgb.len(), 16 * 8 * 3);

        let wide = dir.path().join("wide.jpg");
        let w = TARGET_WIDTH + 1;
        let mut buf = Vec::new();
        let enc = Encoder::new(&mut buf, 90);
        enc.encode(
            &vec![128u8; (w * 8 * 3) as usize],
            w as u16,
            8,
            ColorType::Rgb,
        )
        .unwrap();
        std::fs::write(&wide, &buf).unwrap();
        let err = one(&wide, dir.path()).unwrap_err();
        assert!(err.contains("does not model"), "{err}");
        assert!(!dir.path().join("wide.ig.jpg").exists());
    }

    #[test]
    fn chroma_is_averaged_over_each_block_rather_than_decimated() {
        // A pixel-frequency red/cyan checkerboard has neutral chroma once
        // averaged 2x2; decimation would keep one colour for the whole block.
        let mut px = Vec::new();
        for y in 0..16 {
            for x in 0..16 {
                px.extend(if (x + y) % 2 == 0 {
                    [255u8, 0, 0]
                } else {
                    [0, 255, 255]
                });
            }
        }
        let (_, _, back) = decode(&encode(16, 16, &px).unwrap()).unwrap();
        let mean = |c: usize| {
            back.iter()
                .skip(c)
                .step_by(3)
                .map(|&v| v as f64)
                .sum::<f64>()
                / 256.0
        };
        assert!(
            (mean(0) - mean(2)).abs() < 20.0,
            "red {} vs blue {}",
            mean(0),
            mean(2)
        );
    }

    #[test]
    fn a_two_three_portrait_is_flagged_as_cropped_by_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("tall.jpg");
        std::fs::write(&src, encode(32, 48, &gradient(32, 48)).unwrap()).unwrap();
        let s = one(&src, dir.path()).unwrap();
        assert!(s.outside_band);
        assert!(s.line().contains("the app crops"));
    }
}
