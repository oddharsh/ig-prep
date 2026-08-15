//! ig-prep: prepare photographs for Instagram's compression.
//!
//! The whole idea is to do the destructive work ourselves, carefully, so that
//! Instagram has as little left to do as possible. It resizes and re-encodes
//! whatever it receives; what it does NOT do is resample an image that already
//! arrives at the size it wants.

mod decode;
mod encode;
mod geometry;
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
    -h, --help
";

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let (mut paths, mut fit, mut gravity) = (Vec::new(), Fit::Full, Gravity::Center);
    let mut width = geometry::TARGET_WIDTH;
    let (mut quality, mut subsample, mut dry) = (95u8, false, false);
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
    if !dry {
        if let Err(e) = std::fs::create_dir_all(&out_dir) {
            eprintln!("ig-prep: {}: {e}", out_dir.display());
            std::process::exit(1);
        }
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

    let img = match decode::open(path) {
        Ok(i) => i.oriented(orientation),
        Err(e) => return format!("{name}: {e}"),
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
        scaled.pad_onto(plan.canvas.0, plan.canvas.1, plan.offset.0, plan.offset.1, o.pad)
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
