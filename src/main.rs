//! ig-prep: prepare photographs for Instagram's compression.
//!
//! The whole idea is to do the destructive work ourselves, carefully, so that
//! Instagram has as little left to do as possible. It resizes and re-encodes
//! whatever it receives; what it does NOT do is resample an image that already
//! arrives at the size it wants.

mod decode;
mod encode;
mod geometry;
mod heif;
mod image;

use geometry::{Fit, Gravity};
use std::path::{Path, PathBuf};

const USAGE: &str = "\
ig-prep — prepare photographs for Instagram's compression

USAGE:
    ig-prep [OPTIONS] <PATH>...   (a file or a directory)

    Reads JPEG, HEIF/HEIC/HIF, JPEG XL and PNG. Writes sRGB JPEG.

FIT
    --full        deliver the whole frame at the target width (default), so you
                  can still choose the framing in the app and Instagram has
                  nothing left to resample. Drag, do not pinch-zoom.
    --crop        crop to Instagram's nearest allowed ratio here instead
    --pad         pad to it, keeping the whole frame
    --gravity <center|top|bottom>   where --crop takes its window

OPTIONS
    -w, --width <px>   target width (default 1440)
    -q <1-100>         JPEG quality (default 95)
    --420              subsample chroma (default is 4:4:4)
    --pad-color <hex>  fill for --pad (default ffffff)
    -o, --out <dir>    output directory (default ./ig)
    -n, --dry-run      report the plan for each file, write nothing
    --check            report files whose two rotations disagree, convert
                       nothing. Exits non-zero if any do.
    -h, --help
";

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let (mut paths, mut fit, mut gravity) = (Vec::new(), Fit::Full, Gravity::Center);
    let mut width = geometry::TARGET_WIDTH;
    let (mut quality, mut subsample, mut dry) = (95u8, false, false);
    let mut check = false;
    let mut out_dir = PathBuf::from("ig");
    let mut pad = [255u8, 255, 255];

    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => return print!("{USAGE}"),
            "--full" => fit = Fit::Full,
            "--crop" => fit = Fit::Crop,
            "--pad" => fit = Fit::Pad,
            "--420" => subsample = true,
            "-n" | "--dry-run" => dry = true,
            "--check" => check = true,
            "--gravity" => {
                gravity = match args.next().as_deref() {
                    Some("top") => Gravity::Top,
                    Some("bottom") => Gravity::Bottom,
                    _ => Gravity::Center,
                }
            }
            "-w" | "--width" => width = args.next().and_then(|v| v.parse().ok()).unwrap_or(width),
            "-q" => quality = args.next().and_then(|v| v.parse().ok()).unwrap_or(quality),
            "-o" | "--out" => out_dir = args.next().map(PathBuf::from).unwrap_or(out_dir),
            "--pad-color" => {
                if let Some(h) = args.next() {
                    let h = h.trim_start_matches('#');
                    if h.len() == 6 {
                        for i in 0..3 {
                            pad[i] = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap_or(255);
                        }
                    }
                }
            }
            s if s.starts_with('-') => {
                eprintln!("ig-prep: unknown option {s}");
                std::process::exit(2);
            }
            s => paths.push(PathBuf::from(s)),
        }
    }

    if paths.is_empty() {
        print!("{USAGE}");
        std::process::exit(2);
    }

    let files = expand(&paths);
    if files.is_empty() {
        eprintln!("ig-prep: no readable images");
        std::process::exit(1);
    }
    if check {
        std::process::exit(run_check(&files));
    }
    if !dry && let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("ig-prep: {}: {e}", out_dir.display());
        std::process::exit(1);
    }

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(files.len());
    let chunk = files.len().div_ceil(threads);
    let opts = Opts {
        fit,
        gravity,
        width,
        quality,
        subsample,
        dry,
        pad,
        out_dir: out_dir.clone(),
    };

    let lines: Vec<Vec<String>> = std::thread::scope(|s| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|part| {
                let opts = &opts;
                s.spawn(move || part.iter().map(|p| one(p, opts)).collect::<Vec<_>>())
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for l in lines.into_iter().flatten() {
        println!("{l}");
    }
}

/// Report files whose container transform and EXIF Orientation disagree.
///
/// Carrying BOTH is normal and is what every Fujifilm HIF does; a viewer that
/// applies one of them is right. What cannot be recovered is the two saying
/// DIFFERENT things, because then there is no orientation the file agrees on
/// and every viewer is wrong in some way. Only that case is a failure here.
fn run_check(files: &[PathBuf]) -> i32 {
    let (mut both, mut disagree, mut one, mut neither) = (0, 0, 0, 0);
    for path in files {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                println!("{name}: {e}");
                continue;
            }
        };
        let exif = exif_sooc::read(path)
            .ok()
            .and_then(|p| p.get("Orientation").and_then(|t| t.value.as_i64()))
            .and_then(|v| heif::Transform::from_exif(v as u16));
        let container = heif::container_transform(&bytes);

        match (exif, container) {
            (Some(e), Some(c)) if e == c => both += 1,
            (Some(e), Some(c)) => {
                disagree += 1;
                println!(
                    "{name}: DISAGREE  container says {}, EXIF says {}",
                    c.describe(),
                    e.describe()
                );
            }
            (Some(_), None) | (None, Some(_)) => one += 1,
            (None, None) => neither += 1,
        }
    }
    println!(
        "\n{} files: {disagree} disagree, {both} say the same thing twice, {one} say it once, {neither} say nothing",
        files.len()
    );
    if disagree > 0 {
        println!(
            "\nA file whose two rotations disagree has no orientation every viewer can\n\
             agree on. Converting it here bakes ONE of them into the pixels and drops\n\
             the tags, which at least makes the result unambiguous."
        );
        1
    } else {
        0
    }
}

struct Opts {
    fit: Fit,
    gravity: Gravity,
    width: u32,
    quality: u8,
    subsample: bool,
    dry: bool,
    pad: [u8; 3],
    out_dir: PathBuf,
}

fn one(path: &Path, o: &Opts) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();

    // Orientation comes from the metadata reader rather than from the decoder,
    // because a HEIF's rotation lives in its EXIF and sips does not apply it.
    let orientation = exif_sooc::read(path)
        .ok()
        .and_then(|p| p.get("Orientation").and_then(|t| t.value.as_i64()))
        .unwrap_or(1) as u16;

    let decoded = match decode::open(path) {
        Ok(d) => d,
        Err(e) => return format!("{name}: {e}"),
    };

    // Trust the measurement over the flag where the measurement can tell.
    // A rotation that swaps the axes is visible in the dimensions: if the
    // decoder handed back the DISPLAY shape rather than the stored one, it
    // rotated, whatever this build believes about that decoder. Orientations
    // 2, 3 and 4 are mirrors and a 180 turn, which leave the dimensions alone,
    // so those fall back to the per-decoder flag.
    let swaps = matches!(orientation, 5..=8);
    let already = if swaps {
        exif_sooc::read(path)
            .ok()
            .and_then(|p| p.dimensions())
            .map(|(dw, dh)| (decoded.img.w, decoded.img.h) == (dw, dh))
            .unwrap_or(decoded.orientation_applied)
    } else {
        decoded.orientation_applied
    };

    let img = if already {
        decoded.img
    } else {
        decoded.img.oriented(orientation)
    };

    let plan = geometry::plan(img.w, img.h, o.fit, o.gravity, o.width);
    let note = if plan.outside_band {
        match o.fit {
            Fit::Full => " (outside Instagram's band, crop it in the app)",
            Fit::Crop => " (cropped to fit)",
            Fit::Pad => " (padded to fit)",
        }
    } else {
        ""
    };
    let summary = format!(
        "{name}: {}x{} -> {}x{}{note}",
        img.w, img.h, plan.canvas.0, plan.canvas.1
    );
    if o.dry {
        return summary;
    }

    let cropped = if plan.crop == (0, 0, img.w, img.h) {
        img
    } else {
        img.crop(plan.crop.0, plan.crop.1, plan.crop.2, plan.crop.3)
    };
    let scaled = match image::resize(&cropped, plan.scale.0, plan.scale.1) {
        Ok(s) => s,
        Err(e) => return format!("{name}: {e}"),
    };
    let final_img = if plan.canvas != plan.scale {
        scaled.pad_onto(
            plan.canvas.0,
            plan.canvas.1,
            plan.offset.0,
            plan.offset.1,
            o.pad,
        )
    } else {
        scaled
    };
    let bytes = match encode::jpeg(&final_img, o.quality, o.subsample) {
        Ok(b) => b,
        Err(e) => return format!("{name}: {e}"),
    };
    let dest = o.out_dir.join(format!(
        "{}.jpg",
        path.file_stem().unwrap_or_default().to_string_lossy()
    ));
    match std::fs::write(&dest, &bytes) {
        Ok(()) => format!("{summary}  {} KB", bytes.len() / 1024),
        Err(e) => format!("{name}: {e}"),
    }
}

const EXTS: [&str; 8] = ["jpg", "jpeg", "heif", "heic", "hif", "jxl", "png", "avif"];

fn expand(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            if let Ok(rd) = std::fs::read_dir(p) {
                let mut here: Vec<PathBuf> = rd
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| {
                        p.extension()
                            .and_then(|e| e.to_str())
                            .map(|e| EXTS.contains(&e.to_ascii_lowercase().as_str()))
                            .unwrap_or(false)
                    })
                    .collect();
                here.sort();
                out.extend(here);
            }
        } else {
            out.push(p.clone());
        }
    }
    out
}
