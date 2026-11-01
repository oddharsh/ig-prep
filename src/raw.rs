//! A camera RAW beside a photograph, used for the detail the photograph no
//! longer holds at the delivery size.
//!
//! The photograph, whether a camera JPEG or HIF or an edit exported from one,
//! stays the authority on how the picture looks. The RAW only fills the
//! frequencies above what the photograph itself samples across the output.
//! A full-size camera file is therefore delivered exactly as before, and its
//! RAW is never decoded.
//!
//! Measured 2026-11-01 on five X-T50 RAFs, each HIF exported at 2048 and
//! rebuilt at 3072, scored against the HIF's own 3072 output. Scores are
//! SSIMULACRA2 / Butteraugli max norm, from fast-ssim2 0.9.0 and imazen's
//! butteraugli 0.9.3 (the site's photo-metrics), Lanczos enlargement first:
//!
//! | Film simulation    | Enlarged    | With the RAW |
//! |--------------------|-------------|--------------|
//! | Nostalgic Neg      | 74.9 / 4.33 | 77.6 / 3.15  |
//! | Nostalgic Neg      | 76.1 / 2.64 | 76.4 / 2.55  |
//! | Classic Chrome     | 83.5 / 3.17 | 83.5 / 2.36  |
//! | Provia             | 66.7 / 4.34 | 75.9 / 4.82  |
//! | Eterna             | 80.6 / 3.47 | 79.5 / 4.34  |
//!
//! Eterna's loss is glitter the RAW engine demosaics into different sparkles.
//! The same work measured that Apple's RAW render carries a fifth of the HIF's
//! fine-detail energy, which is why the RAW never replaces detail the
//! photograph has.

use crate::color::{linear_to_srgb, srgb_to_linear};
use crate::image::{Rgb, resize, resize_window};
use std::path::{Path, PathBuf};

/// RAW formats the macOS RAW engine behind `sips` decodes.
pub const EXTS: [&str; 14] = [
    "raf", "dng", "arw", "cr2", "cr3", "nef", "nrw", "orf", "rw2", "pef", "srw", "3fr", "iiq",
    "rwl",
];

/// Below this, the photograph and the RAW are not the same exposure. A true
/// pair measured 0.970; mirrored, shifted by 3% and rotated 180 degrees it
/// measured 0.016 or less.
const MIN_STRUCTURE: f32 = 0.5;

/// Width of the thumbnails the colour map and the pairing check are fitted on.
const THUMB: u32 = 480;

/// How far the two frames' aspect ratios may differ, as a fraction. An export
/// rounds each edge to a whole pixel; a crop in the edit moves it much further.
const ASPECT_SLACK: f64 = 0.003;

/// Side of the output tiles the RAW's detail is trusted over. 32 and 128
/// scored within 0.1 SSIMULACRA2 of 64 on every frame above.
const TILE: u32 = 64;

pub fn is_raw(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

/// The first RAW sharing the photograph's stem, in its own folder and then in
/// each of `dirs`. Stems compare without case because cameras write upper case
/// and exporters often do not. Only named folders are searched: camera
/// counters wrap at 9999, so a drive-wide search finds somebody's DSCF0001.
/// A symlink is never a match, so the MCP server's --root, which vouches for
/// the folders, also bounds every RAW read out of them.
pub fn find(photo: &Path, dirs: &[PathBuf]) -> Option<PathBuf> {
    let stem = photo.file_stem()?.to_str()?;
    let own = photo.parent().map(|p| {
        if p.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            p.to_path_buf()
        }
    });
    own.iter().chain(dirs).find_map(|dir| {
        let mut hits: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .filter(|p| {
                is_raw(p)
                    && p.file_stem()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| s.eq_ignore_ascii_case(stem))
            })
            .collect();
        hits.sort();
        hits.into_iter().next()
    })
}

/// When the shutter fired, to the hundredth where the camera records it.
fn captured(path: &Path) -> Option<String> {
    let p = exif_sooc::read(path).ok()?;
    let at = p.get("DateTimeOriginal")?.value.as_str()?.to_owned();
    let sub = p
        .get("SubSecTimeOriginal")
        .and_then(|t| t.value.as_str())
        .unwrap_or_default();
    Some(format!("{at}.{sub}"))
}

/// Why a RAW found beside the photograph cannot lend it detail. Each check
/// runs before the RAW is trusted with a single pixel.
pub fn mismatch(photo_path: &Path, raw_path: &Path) -> Option<String> {
    match (captured(photo_path), captured(raw_path)) {
        (Some(a), Some(b)) if a != b => Some(format!("captured at {b}, the photograph at {a}")),
        _ => None,
    }
}

pub fn shape_mismatch(photo: &Rgb, raw: &Rgb) -> Option<String> {
    let a = photo.w as f64 / photo.h as f64;
    let b = raw.w as f64 / raw.h as f64;
    ((a / b - 1.0).abs() > ASPECT_SLACK).then(|| {
        format!(
            "its frame is {}x{} and the photograph's {}x{}, so the edit cropped it",
            raw.w, raw.h, photo.w, photo.h
        )
    })
}

/// Gamma-encoded copy, so fitting and band splitting happen on perceptual
/// steps rather than on light, where shadows would count for nothing.
fn encoded(img: &Rgb) -> Rgb {
    Rgb::new(
        img.w,
        img.h,
        img.px.iter().map(|&c| linear_to_srgb(c)).collect(),
    )
}

fn thumbs(photo: &Rgb, raw: &Rgb) -> Result<(Rgb, Rgb), String> {
    let h = ((THUMB as f64) * photo.h as f64 / photo.w as f64)
        .round()
        .max(1.0) as u32;
    Ok((
        encoded(&resize(photo, THUMB, h)?),
        encoded(&resize(raw, THUMB, h)?),
    ))
}

/// Correlation of the two frames' fine luma structure. Tone, colour and film
/// simulation drop out under the normalisation, so an edit scores like the
/// camera file and only a different exposure or framing scores low.
fn structure(a: &Rgb, b: &Rgb) -> Result<f32, String> {
    let high = |img: &Rgb| -> Result<Vec<f32>, String> {
        let blur = resize(
            &resize(img, (img.w / 4).max(1), (img.h / 4).max(1))?,
            img.w,
            img.h,
        )?;
        let luma = |p: &[f32]| 0.299 * p[0] + 0.587 * p[1] + 0.114 * p[2];
        let d: Vec<f32> = img
            .px
            .as_chunks::<3>()
            .0
            .iter()
            .zip(blur.px.as_chunks::<3>().0.iter())
            .map(|(p, q)| luma(p) - luma(q))
            .collect();
        let n = d.len() as f32;
        let mean = d.iter().sum::<f32>() / n;
        let sd = (d.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / n).sqrt();
        Ok(d.iter().map(|v| (v - mean) / sd.max(1e-9)).collect())
    };
    let (x, y) = (high(a)?, high(b)?);
    Ok(x.iter().zip(&y).map(|(p, q)| p * q).sum::<f32>() / x.len() as f32)
}

/// A cubic in gamma-encoded RGB from the RAW's rendering to the photograph's
/// look. Fitted 3.2 levels RMS on the Nostalgic Neg pair, against 11.5 with
/// no map; what it misses at low frequencies the merge takes from the
/// photograph anyway, so it only has to be right about fine detail.
pub struct Look([[f64; 20]; 3]);

fn terms([r, g, b]: [f64; 3]) -> [f64; 20] {
    [
        1.0,
        r,
        g,
        b,
        r * r,
        g * g,
        b * b,
        r * g,
        r * b,
        g * b,
        r * r * r,
        g * g * g,
        b * b * b,
        r * r * g,
        r * r * b,
        g * g * r,
        g * g * b,
        b * b * r,
        b * b * g,
        r * g * b,
    ]
}

impl Look {
    /// Least squares through the normal equations, with a ridge small enough
    /// to leave a real fit alone and large enough that a RAW with no colour in
    /// some region cannot make the system singular.
    fn fit(raw: &Rgb, photo: &Rgb) -> Look {
        let mut ata = [[0.0f64; 20]; 20];
        let mut atb = [[0.0f64; 3]; 20];
        for (p, q) in raw
            .px
            .as_chunks::<3>()
            .0
            .iter()
            .zip(photo.px.as_chunks::<3>().0.iter())
        {
            let t = terms([p[0], p[1], p[2]].map(|c| c.clamp(0.0, 1.0) as f64));
            for i in 0..20 {
                for j in 0..20 {
                    ata[i][j] += t[i] * t[j];
                }
                for c in 0..3 {
                    atb[i][c] += t[i] * q[c] as f64;
                }
            }
        }
        let ridge = 1e-9 * (0..20).map(|i| ata[i][i]).sum::<f64>();
        for (i, row) in ata.iter_mut().enumerate() {
            row[i] += ridge;
        }
        let mut coef = [[0.0; 20]; 3];
        for c in 0..3 {
            let x = solve(ata, std::array::from_fn(|i| atb[i][c]));
            coef[c] = x;
        }
        Look(coef)
    }

    fn apply(&self, img: &Rgb) -> Rgb {
        let mut px = Vec::with_capacity(img.px.len());
        for p in img.px.as_chunks::<3>().0.iter() {
            let t = terms([p[0], p[1], p[2]].map(|c| c.clamp(0.0, 1.0) as f64));
            for c in 0..3 {
                px.push(self.0[c].iter().zip(&t).map(|(k, v)| k * v).sum::<f64>() as f32);
            }
        }
        Rgb::new(img.w, img.h, px)
    }
}

/// Gaussian elimination with partial pivoting; the ridge keeps it regular.
fn solve(mut a: [[f64; 20]; 20], mut b: [f64; 20]) -> [f64; 20] {
    for col in 0..20 {
        let pivot = (col..20)
            .max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))
            .unwrap_or(col);
        a.swap(col, pivot);
        b.swap(col, pivot);
        let p = a[col];
        for row in col + 1..20 {
            let f = a[row][col] / p[col];
            for (x, y) in a[row][col..].iter_mut().zip(&p[col..]) {
                *x -= f * y;
            }
            b[row] -= f * b[col];
        }
    }
    let mut x = [0.0; 20];
    for row in (0..20).rev() {
        let s: f64 = (row + 1..20).map(|k| a[row][k] * x[k]).sum();
        x[row] = (b[row] - s) / a[row][row];
    }
    x
}

/// Fit the RAW to the photograph, or say why it is not the same picture.
pub fn pair(photo: &Rgb, raw: &Rgb) -> Result<Look, String> {
    if let Some(why) = shape_mismatch(photo, raw) {
        return Err(why);
    }
    let (p, r) = thumbs(photo, raw)?;
    let s = structure(&p, &r)?;
    if s < MIN_STRUCTURE {
        return Err(format!(
            "its structure correlates {s:.2} with the photograph's, under {MIN_STRUCTURE}"
        ));
    }
    Ok(Look::fit(&r, &p))
}

/// The photograph wherever it has samples, and the RAW's detail above them.
///
/// `crop` is a window in RAW pixels and `out` the size it is delivered at. The
/// photograph is enlarged whole, and the mapped RAW adds only what survives
/// subtracting its own copy resampled through the photograph's sample count,
/// scaled by [`trust`]. Each sum is held within the photograph's range over
/// one of its samples, so a RAW highlight the map extrapolates cannot become
/// a white ring: replacing the photograph's band with the RAW's and leaving
/// the result unbounded scored Butteraugli 13.6 on the Eterna frame above.
pub fn merge(
    photo: &Rgb,
    raw: &Rgb,
    look: &Look,
    crop: (u32, u32, u32, u32),
    out: (u32, u32),
) -> Result<Rgb, String> {
    let sx = photo.w as f64 / raw.w as f64;
    let sy = photo.h as f64 / raw.h as f64;
    let window = [
        crop.0 as f64 * sx,
        crop.1 as f64 * sy,
        crop.2 as f64 * sx,
        crop.3 as f64 * sy,
    ];
    let held = (
        (window[2].round() as u32).clamp(1, out.0),
        (window[3].round() as u32).clamp(1, out.1),
    );
    let mapped = look.apply(&encoded(&resize(
        &raw.crop(crop.0, crop.1, crop.2, crop.3),
        out.0,
        out.1,
    )?));
    let shown = encoded(&resize_window(photo, window, out.0, out.1)?);
    let low = resize(&resize(&mapped, held.0, held.1)?, out.0, out.1)?;
    let gain = trust(&shown, &mapped, held)?;
    let reach = (out.0 as f64 / held.0 as f64).ceil() as usize;
    let (w, h) = (out.0 as usize, out.1 as usize);
    let floor = spread(&shown.px, w, h, reach, f32::min);
    let ceiling = spread(&shown.px, w, h, reach, f32::max);
    let px = mapped
        .px
        .iter()
        .zip(&low.px)
        .zip(floor.iter().zip(&ceiling))
        .zip(shown.px.iter().zip(&gain.px))
        .map(|(((m, l), (lo, hi)), (s, g))| {
            srgb_to_linear((s + g * (m - l)).clamp(*lo, *hi).clamp(0.0, 1.0))
        })
        .collect();
    Ok(Rgb::new(out.0, out.1, px))
}

/// How far to trust the RAW's detail around each pixel: per tile and channel,
/// the least-squares gain from the RAW's octave just below the photograph's
/// Nyquist to the photograph's own, kept within [0, 1]. Where the RAW's fine
/// texture is noise the photograph does not share, it lends nothing.
fn trust(shown: &Rgb, mapped: &Rgb, held: (u32, u32)) -> Result<Rgb, String> {
    let half = ((held.0 / 2).max(1), (held.1 / 2).max(1));
    let octave = |img: &Rgb| -> Result<Vec<f32>, String> {
        let a = resize(&resize(img, held.0, held.1)?, img.w, img.h)?;
        let b = resize(&resize(img, half.0, half.1)?, img.w, img.h)?;
        Ok(a.px.iter().zip(&b.px).map(|(x, y)| x - y).collect())
    };
    let (s, m) = (octave(shown)?, octave(mapped)?);
    let (w, tw, th) = (
        shown.w as usize,
        shown.w.div_ceil(TILE) as usize,
        shown.h.div_ceil(TILE) as usize,
    );
    let mut cross = vec![0.0f64; tw * th * 3];
    let mut power = vec![0.0f64; tw * th * 3];
    for (i, (a, b)) in s.iter().zip(&m).enumerate() {
        let (p, c) = (i / 3, i % 3);
        let t = ((p / w) / TILE as usize * tw + (p % w) / TILE as usize) * 3 + c;
        cross[t] += (a * b) as f64;
        power[t] += (b * b) as f64;
    }
    let tiles = cross
        .iter()
        .zip(&power)
        .map(|(x, p)| {
            if *p > 0.0 {
                (x / p).clamp(0.0, 1.0) as f32
            } else {
                0.0
            }
        })
        .collect();
    let mut gain = resize(&Rgb::new(tw as u32, th as u32, tiles), shown.w, shown.h)?;
    gain.px.iter_mut().for_each(|g| *g = g.clamp(0.0, 1.0));
    Ok(gain)
}

/// Each channel's `pick` (least or greatest) within `r` pixels of each pixel.
fn spread(px: &[f32], w: usize, h: usize, r: usize, pick: fn(f32, f32) -> f32) -> Vec<f32> {
    let at = |x: usize, y: usize, c: usize| (y * w + x) * 3 + c;
    let mut across = vec![0.0; px.len()];
    let mut out = vec![0.0; px.len()];
    for y in 0..h {
        for x in 0..w {
            for c in 0..3 {
                across[at(x, y, c)] = (x.saturating_sub(r)..=(x + r).min(w - 1))
                    .map(|i| px[at(i, y, c)])
                    .fold(px[at(x, y, c)], pick);
            }
        }
    }
    for y in 0..h {
        for x in 0..w {
            for c in 0..3 {
                out[at(x, y, c)] = (y.saturating_sub(r)..=(y + r).min(h - 1))
                    .map(|j| across[at(x, j, c)])
                    .fold(across[at(x, y, c)], pick);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame with structure at every scale, so the pairing check has
    /// something to correlate and a band split has something to split.
    fn scene(w: u32, h: u32) -> Rgb {
        let px = (0..w * h)
            .flat_map(|i| {
                let (x, y) = ((i % w) as f32, (i / w) as f32);
                // The last term, about 4 pixels a period, is finer than a
                // third-size export can hold.
                let v = 0.5
                    + 0.2 * (x * 0.31).sin() * (y * 0.17).cos()
                    + 0.1 * (x * 0.07).sin()
                    + 0.06 * (x * 1.6).sin() * (y * 1.4).cos();
                [v * 0.9, v, v * 0.8].map(srgb_to_linear)
            })
            .collect();
        Rgb::new(w, h, px)
    }

    #[test]
    fn finds_a_raw_by_stem_without_case_and_skips_other_stems() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["DSCF0001.RAF", "dscf0002.dng", "DSCF0001.xmp"] {
            std::fs::write(dir.path().join(f), b"").unwrap();
        }
        let photo = dir.path().join("dscf0001.jpg");
        assert_eq!(find(&photo, &[]), Some(dir.path().join("DSCF0001.RAF")));
        assert_eq!(find(&dir.path().join("DSCF0003.jpg"), &[]), None);
    }

    #[test]
    fn a_named_folder_is_searched_after_the_photographs_own() {
        let (here, card) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(card.path().join("XT500078.RAF"), b"").unwrap();
        let photo = here.path().join("XT500078.jpg");
        assert_eq!(find(&photo, &[]), None);
        assert_eq!(
            find(&photo, &[card.path().to_path_buf()]),
            Some(card.path().join("XT500078.RAF"))
        );
    }

    #[test]
    fn a_cropped_edit_is_refused_and_a_rounded_export_is_not() {
        let raw = Rgb::new(7728, 5152, vec![0.0; 7728 * 5152 * 3]);
        let rounded = Rgb::new(2048, 1365, vec![0.0; 2048 * 1365 * 3]);
        let cropped = Rgb::new(2048, 1536, vec![0.0; 2048 * 1536 * 3]);
        assert_eq!(shape_mismatch(&rounded, &raw), None);
        assert_eq!(
            shape_mismatch(&cropped, &raw).as_deref(),
            Some("its frame is 7728x5152 and the photograph's 2048x1536, so the edit cropped it")
        );
    }

    #[test]
    fn a_mirrored_raw_fails_the_pairing_check_and_the_true_one_passes() {
        let raw = scene(960, 640);
        let mirrored = Rgb::new(
            960,
            640,
            raw.px
                .as_chunks::<{ 960 * 3 }>()
                .0
                .iter()
                .flat_map(|row| {
                    row.as_chunks::<3>()
                        .0
                        .iter()
                        .rev()
                        .flatten()
                        .copied()
                        .collect::<Vec<_>>()
                })
                .collect(),
        );
        assert!(pair(&raw, &raw).is_ok());
        assert_eq!(
            pair(&raw, &mirrored)
                .err()
                .map(|e| e.ends_with("under 0.5")),
            Some(true)
        );
    }

    #[test]
    fn the_look_reproduces_a_global_tone_and_colour_change() {
        // A grade inside the cubic's span, crossing channels the way a film
        // simulation does, so any error left is the solver's.
        let raw = encoded(&scene(64, 64));
        let graded = Rgb::new(
            64,
            64,
            raw.px
                .as_chunks::<3>()
                .0
                .iter()
                .flat_map(|p| {
                    let [r, g, b] = [p[0], p[1], p[2]];
                    [
                        r * r,
                        0.1 + 0.8 * g - 0.2 * g * g * g,
                        0.9 * b + 0.1 * r * g,
                    ]
                })
                .collect(),
        );
        let got = Look::fit(&raw, &graded).apply(&raw);
        let worst = got
            .px
            .iter()
            .zip(&graded.px)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(worst < 1.0 / 255.0, "worst error {worst}");
    }

    #[test]
    fn a_photograph_at_full_size_comes_through_the_merge_unchanged() {
        let photo = scene(120, 80);
        let look = Look::fit(&encoded(&photo), &encoded(&photo));
        let out = merge(&photo, &scene(120, 80), &look, (0, 0, 120, 80), (120, 80)).unwrap();
        let worst = out
            .px
            .iter()
            .zip(&photo.px)
            .map(|(a, b)| (linear_to_srgb(*a) - linear_to_srgb(*b)).abs())
            .fold(0.0, f32::max);
        assert!(worst < 0.5 / 255.0, "worst error {worst}");
    }

    #[test]
    fn a_small_export_gains_the_raws_fine_detail() {
        // The export is the RAW at a third of the size: its fine detail is gone,
        // and the merge should bring it back nearer than enlarging does.
        let raw = scene(360, 240);
        let export = resize(&raw, 120, 80).unwrap();
        let look = Look::fit(&encoded(&resize(&raw, 120, 80).unwrap()), &encoded(&export));
        let merged = merge(&export, &raw, &look, (0, 0, 360, 240), (360, 240)).unwrap();
        let enlarged = resize(&export, 360, 240).unwrap();
        let err = |img: &Rgb| -> f32 {
            let e: f32 = img
                .px
                .iter()
                .zip(&raw.px)
                .map(|(a, b)| (linear_to_srgb(*a) - linear_to_srgb(*b)).powi(2))
                .sum();
            (e / img.px.len() as f32).sqrt() * 255.0
        };
        let (m, e) = (err(&merged), err(&enlarged));
        assert!(m < e / 4.0, "merged {m:.2} levels, enlarged {e:.2}");
    }

    #[test]
    fn a_raws_noise_the_photograph_does_not_share_is_held_back() {
        // Same scene as above, but the RAW also carries grain at every scale
        // that the export never had: its octave below the export's Nyquist
        // disagrees with the export's, so the merge should stay near the
        // enlargement. Ungated, it would sit where the RAW does.
        let clean = scene(360, 240);
        let raw = Rgb::new(
            360,
            240,
            clean
                .px
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    // murmur3's finalizer, so the grain is white.
                    let mut n = i as u32;
                    n = (n ^ (n >> 16)).wrapping_mul(0x85eb_ca6b);
                    n = (n ^ (n >> 13)).wrapping_mul(0xc2b2_ae35);
                    n ^= n >> 16;
                    v + 0.2 * (n as f32 / u32::MAX as f32 - 0.5)
                })
                .collect(),
        );
        let export = resize(&clean, 120, 80).unwrap();
        let look = Look::fit(&encoded(&export), &encoded(&export));
        let merged = merge(&export, &raw, &look, (0, 0, 360, 240), (360, 240)).unwrap();
        let enlarged = resize(&export, 360, 240).unwrap();
        let apart = |img: &Rgb| -> f32 {
            let e: f32 = img
                .px
                .iter()
                .zip(&enlarged.px)
                .map(|(a, b)| (linear_to_srgb(*a) - linear_to_srgb(*b)).powi(2))
                .sum();
            (e / img.px.len() as f32).sqrt() * 255.0
        };
        let (m, r) = (apart(&merged), apart(&raw));
        assert!(
            m < r / 5.0,
            "merge moved {m:.2} levels from the enlargement, the RAW sits {r:.2}"
        );
    }
}
