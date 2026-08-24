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
mod mcp;

use encode::Chroma;
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
    --444              chroma at full resolution (default). The 3.58x/1.79x
                       downscale means a 4:2:2 source still fills it.
    --422              chroma halved horizontally, matching what the camera
                       shot. The honest choice near native size.
    --420              chroma halved on both axes, matching what Instagram
                       stores anyway. Smallest upload.
    --pad-color <hex>  fill for --pad (default ffffff)
    -o, --out <dir>    output directory (default ./ig)
    -n, --dry-run      report the plan for each file, write nothing
    --check            report files whose two rotations disagree, convert
                       nothing. Exits non-zero if any do.
    -h, --help

MCP SERVER
    ig-prep mcp [--root <dir>]
                  Speak MCP on stdin and stdout, so a model with file access
                  can convert photographs where they already are. Tools:
                  ig_plan, ig_convert, ig_check_rotation. --root confines every
                  path argument to one directory; without it any readable path
                  may be converted.
";

fn main() {
    // `mcp` is a MODE rather than an option, and it is matched before the
    // option loop because everything below parses arguments for a conversion
    // this process is not going to perform.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("mcp") {
        let mut root = None;
        let mut rest = argv[1..].iter();
        while let Some(a) = rest.next() {
            match a.as_str() {
                "--root" => match rest.next() {
                    Some(d) => root = Some(PathBuf::from(d)),
                    None => {
                        eprintln!("ig-prep: --root needs a directory");
                        std::process::exit(2);
                    }
                },
                other => {
                    eprintln!("ig-prep mcp: unknown option {other}");
                    std::process::exit(2);
                }
            }
        }
        std::process::exit(mcp::serve(root));
    }

    let mut args = std::env::args().skip(1).peekable();
    let (mut paths, mut fit, mut gravity) = (Vec::new(), Fit::Full, Gravity::Center);
    let mut width = geometry::TARGET_WIDTH;
    let (mut quality, mut chroma, mut dry) = (95u8, Chroma::Full, false);
    let mut check = false;
    let mut out_dir = PathBuf::from("ig");
    let mut pad = [255u8, 255, 255];

    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => return print!("{USAGE}"),
            "--full" => fit = Fit::Full,
            "--crop" => fit = Fit::Crop,
            "--pad" => fit = Fit::Pad,
            "--444" => chroma = Chroma::Full,
            "--422" => chroma = Chroma::Halved,
            "--420" => chroma = Chroma::Quartered,
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

    let opts = Opts {
        fit,
        gravity,
        width,
        quality,
        chroma,
        dry,
        pad,
        out_dir: out_dir.clone(),
    };

    for r in run_all(&files, &opts) {
        println!("{}", r.line());
    }
}

/// Convert a batch across the available cores, in input order.
///
/// Shared by the CLI and the MCP server so that a conversion cannot depend on
/// which door it came through. Order is preserved because a caller matching
/// results against the paths it sent has no other key to match on: the report
/// carries the source path, but a model reading the text lines reads them
/// positionally.
pub fn run_all(files: &[PathBuf], opts: &Opts) -> Vec<Report> {
    if files.is_empty() {
        return Vec::new();
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(files.len());
    let chunk = files.len().div_ceil(threads);
    let parts: Vec<Vec<Report>> = std::thread::scope(|s| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|part| s.spawn(move || part.iter().map(|p| one(p, opts)).collect::<Vec<_>>()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    parts.into_iter().flatten().collect()
}

/// Report files whose container transform and EXIF Orientation disagree.
///
/// Carrying BOTH is normal and is what every Fujifilm HIF does; a viewer that
/// applies one of them is right. What cannot be recovered is the two saying
/// DIFFERENT things, because then there is no orientation the file agrees on
/// and every viewer is wrong in some way. Only that case is a failure here.
/// How a file's two statements about its own rotation line up.
pub enum Agreement {
    /// Container transform and EXIF Orientation, saying the same thing. What
    /// every Fujifilm HIF does, and not a problem.
    Both,
    /// The two say DIFFERENT things, so no viewer can be right.
    Disagree,
    /// Only one of the two is present, which is unambiguous.
    One,
    /// Neither is present, so the pixels are already upright.
    Neither,
    Unreadable,
}

impl Agreement {
    pub fn label(&self) -> &'static str {
        match self {
            Agreement::Both => "both",
            Agreement::Disagree => "disagree",
            Agreement::One => "one",
            Agreement::Neither => "neither",
            Agreement::Unreadable => "unreadable",
        }
    }
}

pub struct Check {
    pub name: String,
    pub source: PathBuf,
    pub agreement: Agreement,
    /// What each of the two says, as prose, present only when that one is.
    pub container: Option<String>,
    pub exif: Option<String>,
    pub error: Option<String>,
}

pub fn check_one(path: &Path) -> Check {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            return Check {
                name,
                source: path.to_path_buf(),
                agreement: Agreement::Unreadable,
                container: None,
                exif: None,
                error: Some(e.to_string()),
            };
        }
    };
    let exif = exif_sooc::read(path)
        .ok()
        .and_then(|p| p.get("Orientation").and_then(|t| t.value.as_i64()))
        .and_then(|v| heif::Transform::from_exif(v as u16));
    let container = heif::container_transform(&bytes);
    let agreement = match (exif, container) {
        (Some(e), Some(c)) if e == c => Agreement::Both,
        (Some(_), Some(_)) => Agreement::Disagree,
        (Some(_), None) | (None, Some(_)) => Agreement::One,
        (None, None) => Agreement::Neither,
    };
    Check {
        name,
        source: path.to_path_buf(),
        agreement,
        container: container.map(|c| c.describe()),
        exif: exif.map(|e| e.describe()),
        error: None,
    }
}

/// The note a disagreement earns. Shared so the CLI and the MCP server explain
/// the same finding the same way.
pub const DISAGREE_NOTE: &str = "A file whose two rotations disagree has no orientation every viewer can \
agree on. Converting it here bakes ONE of them into the pixels and drops the tags, which at least \
makes the result unambiguous.";

/// Report files whose container transform and EXIF Orientation disagree.
///
/// Carrying BOTH is normal and is what every Fujifilm HIF does; a viewer that
/// applies one of them is right. What cannot be recovered is the two saying
/// DIFFERENT things, because then there is no orientation the file agrees on
/// and every viewer is wrong in some way. Only that case is a failure here.
fn run_check(files: &[PathBuf]) -> i32 {
    let (mut both, mut disagree, mut one, mut neither) = (0, 0, 0, 0);
    for path in files {
        let c = check_one(path);
        match c.agreement {
            Agreement::Unreadable => println!("{}: {}", c.name, c.error.unwrap_or_default()),
            Agreement::Both => both += 1,
            Agreement::Disagree => {
                disagree += 1;
                println!(
                    "{}: DISAGREE  container says {}, EXIF says {}",
                    c.name,
                    c.container.unwrap_or_default(),
                    c.exif.unwrap_or_default()
                );
            }
            Agreement::One => one += 1,
            Agreement::Neither => neither += 1,
        }
    }
    println!(
        "\n{} files: {disagree} disagree, {both} say the same thing twice, {one} say it once, {neither} say nothing",
        files.len()
    );
    if disagree > 0 {
        println!("\n{DISAGREE_NOTE}");
        1
    } else {
        0
    }
}

pub struct Opts {
    pub fit: Fit,
    pub gravity: Gravity,
    pub width: u32,
    pub quality: u8,
    pub chroma: Chroma,
    pub dry: bool,
    pad: [u8; 3],
    pub out_dir: PathBuf,
}

impl Opts {
    /// The defaults the CLI applies, so a caller that sets nothing gets the
    /// same conversion the bare `ig-prep <dir>` command performs.
    pub fn defaults() -> Opts {
        Opts {
            fit: Fit::Full,
            gravity: Gravity::Center,
            width: geometry::TARGET_WIDTH,
            quality: 95,
            chroma: Chroma::Full,
            dry: false,
            pad: [255, 255, 255],
            out_dir: PathBuf::from("ig"),
        }
    }

    /// `--pad-color`, parsed from `ffffff` or `#ffffff`. Anything else leaves
    /// the fill alone rather than substituting a colour nobody asked for.
    pub fn set_pad_color(&mut self, hex: &str) {
        let h = hex.trim_start_matches('#');
        if h.len() == 6 {
            for i in 0..3 {
                self.pad[i] = u8::from_str_radix(&h[i * 2..i * 2 + 2], 16).unwrap_or(255);
            }
        }
    }
}

/// What became of one file.
///
/// The CLI renders this as a line and the MCP server hands the same fields
/// back as JSON, so the two surfaces cannot end up describing different
/// conversions. Every field is optional in the way the failure actually is:
/// a file that would not decode has no dimensions, and a dry run has no
/// output path, rather than either being reported as a zero.
pub struct Report {
    pub name: String,
    pub source: PathBuf,
    /// Display dimensions, after rotation. Every later number derives from
    /// these rather than from what the decoder happened to hand back.
    pub from: Option<(u32, u32)>,
    pub to: Option<(u32, u32)>,
    /// True when the source ratio is outside what Instagram shows uncropped.
    pub outside_band: bool,
    pub output: Option<PathBuf>,
    pub bytes: Option<usize>,
    pub chroma: &'static str,
    pub error: Option<String>,
    note: &'static str,
}

impl Report {
    fn failed(path: &Path, name: String, e: String) -> Report {
        Report {
            name,
            source: path.to_path_buf(),
            from: None,
            to: None,
            outside_band: false,
            output: None,
            bytes: None,
            chroma: "",
            error: Some(e),
            note: "",
        }
    }

    /// The one line the CLI prints. Kept here so that changing what a run says
    /// changes it in one place for both surfaces.
    pub fn line(&self) -> String {
        if let Some(e) = &self.error {
            return format!("{}: {e}", self.name);
        }
        let (fw, fh) = self.from.unwrap_or((0, 0));
        let (tw, th) = self.to.unwrap_or((0, 0));
        let summary = format!("{}: {fw}x{fh} -> {tw}x{th}{}", self.name, self.note);
        match self.bytes {
            Some(b) => format!("{summary}  {} {} KB", self.chroma, b / 1024),
            None => summary,
        }
    }
}

pub fn one(path: &Path, o: &Opts) -> Report {
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    // Orientation comes from the metadata reader rather than from the decoder,
    // because a HEIF's rotation lives in its EXIF and sips does not apply it.
    let orientation = exif_sooc::read(path)
        .ok()
        .and_then(|p| p.get("Orientation").and_then(|t| t.value.as_i64()))
        .unwrap_or(1) as u16;

    let decoded = match decode::open(path) {
        Ok(d) => d,
        Err(e) => return Report::failed(path, name, e),
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
    let mut report = Report {
        name,
        source: path.to_path_buf(),
        from: Some((img.w, img.h)),
        to: Some(plan.canvas),
        outside_band: plan.outside_band,
        output: None,
        bytes: None,
        chroma: o.chroma.label(),
        error: None,
        note,
    };
    if o.dry {
        return report;
    }

    let cropped = if plan.crop == (0, 0, img.w, img.h) {
        img
    } else {
        img.crop(plan.crop.0, plan.crop.1, plan.crop.2, plan.crop.3)
    };
    let scaled = match image::resize(&cropped, plan.scale.0, plan.scale.1) {
        Ok(s) => s,
        Err(e) => return Report::failed(path, report.name, e),
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
    let bytes = match encode::jpeg(&final_img, o.quality, o.chroma) {
        Ok(b) => b,
        Err(e) => return Report::failed(path, report.name, e),
    };
    let dest = o.out_dir.join(format!(
        "{}.jpg",
        path.file_stem().unwrap_or_default().to_string_lossy()
    ));
    match std::fs::write(&dest, &bytes) {
        Ok(()) => {
            report.bytes = Some(bytes.len());
            report.output = Some(dest);
            report
        }
        Err(e) => Report::failed(path, report.name, format!("{}: {e}", dest.display())),
    }
}

const EXTS: [&str; 8] = ["jpg", "jpeg", "heif", "heic", "hif", "jxl", "png", "avif"];

pub fn expand(paths: &[PathBuf]) -> Vec<PathBuf> {
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
