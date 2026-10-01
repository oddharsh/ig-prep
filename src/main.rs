//! ig-prep: prepare photographs for Instagram's compression.
//!
//! Controls colour conversion, precision and downscaling before upload.
//! Actual Instagram processing is measured separately through a round trip.

mod color;
mod comparison;
mod decode;
mod diagnostics;
mod encode;
mod geometry;
mod heif;
mod image;
mod mcp;
mod pick;
mod simulate;

use encode::Chroma;
use geometry::{Fit, Gravity};
use std::path::{Path, PathBuf};

const USAGE: &str = "\
ig-prep — prepare photographs for Instagram's compression

USAGE:
    ig-prep [OPTIONS] <PATH>...   (a file or a directory)

    Reads JPEG, HEIF/HEIC/HIF, JPEG XL and PNG. Writes sRGB JPEG.

FIT
    --full        deliver the whole frame at the target width (default);
                  choose the final crop in the app
    --crop        crop to Instagram's nearest allowed ratio here instead
    --pad         pad to it, keeping the whole frame
    --gravity <center|top|bottom>   where --crop takes its window
    --pick        choose the window here, in your browser, one photograph
                  at a time. The export is a whole-pixel, in-band window at
                  the target width, so the app has nothing to resample. The
                  fit flags are ignored; -o, -w, -q and chroma apply.

OPTIONS
    -w, --width <px>   target width (default 3072)
    -q <1-100>         ZenJPEG quality (default 99; differs from older exports)
    --444              full chroma resolution (default)
    --422              chroma halved horizontally
    --420              chroma halved on both axes
    --dither           experimental neutral noise before JPEG encoding
    --match            encode on the servers' own quantisation tables, as
                       measured in September 2026, at 4:4:4 with no trellis:
                       about half the bytes, faster, and slightly ahead of q99
                       after the servers in local models. Cannot combine with
                       -q, the chroma flags or --dither.
    --variants         write eight variants (1080/1440, q95/99, 444/420),
                       references and a manifest to a NEW --out directory;
                       use --crop or --pad for out-of-band sources
    --pad-color <hex>  fill for --pad (default ffffff)
    -o, --out <dir>    output directory (default ./ig)
    -n, --dry-run      report the plan for each file, write nothing
    --check            report files whose two rotations disagree, convert
                       nothing. Exits non-zero if any do.
    -h, --help

DETAIL DIAGNOSTICS
    ig-prep diagnose [-w <px>] [--detail x,y,w,h] -o <NEW-directory> <file>
                  Compare Q99/Q100 with same-size lossless references and metrics.
                  Detail rectangle uses oriented source pixels; writes companion crops.
                  Requires ssimulacra2 and butteraugli_main on PATH.

COMPARISON
    ig-prep --variants --crop -o comparison <PATH>...
    ig-prep score [--local] [--perceptual] comparison
                  Download served images into comparison/returned/ with their
                  upload filenames, then score against the lossless references.
                  Scoring supports the same image formats as conversion.

SIMULATE
    ig-prep simulate [-o <dir>] <PATH>...
                  Re-encode JPEG uploads the way Instagram's servers did when
                  measured in September 2026: its quantisation tables, 4:2:0,
                  progressive. Writes <name>.ig.jpg to <dir> (default ./ig-sim)
                  and reports the PSNR against each upload. Refuses uploads
                  wider than the 3072 tier, whose downscale is not modelled,
                  and does not model the app's crop.

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
    if argv.first().map(String::as_str) == Some("diagnose") {
        match diagnostics::run(&argv[1..]) {
            Ok(path) => println!("Diagnostics written to {}", path.display()),
            Err(e) => {
                eprintln!("ig-prep: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if argv.first().map(String::as_str) == Some("score") {
        let local = argv.iter().any(|a| a == "--local");
        let perceptual = argv.iter().any(|a| a == "--perceptual");
        let paths: Vec<_> = argv[1..]
            .iter()
            .filter(|a| !matches!(a.as_str(), "--local" | "--perceptual"))
            .collect();
        if paths.len() != 1 || paths[0].starts_with('-') {
            eprintln!("Usage: ig-prep score [--local] [--perceptual] <comparison-directory>");
            std::process::exit(2);
        }
        match comparison::score_with(Path::new(paths[0]), local, perceptual) {
            Ok(report) => println!("{report}"),
            Err(e) => {
                eprintln!("ig-prep: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if argv.first().map(String::as_str) == Some("simulate") {
        std::process::exit(run_simulate(&argv[1..]));
    }
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
    let (mut quality, mut chroma, mut dry) = (encode::DEFAULT_QUALITY, Chroma::Full, false);
    let mut check = false;
    let mut dither = false;
    let mut variants = false;
    let mut pick = false;
    let mut match_tables = false;
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
            "--dither" => dither = true,
            "--variants" => variants = true,
            "--pick" => pick = true,
            "--match" => match_tables = true,
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
                    if h.len() == 6 && h.is_ascii() && h.bytes().all(|b| b.is_ascii_hexdigit()) {
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

    if !(1..=16384).contains(&width) || !(1..=100).contains(&quality) {
        eprintln!("ig-prep: width must be 1..16384 and quality must be 1..100");
        std::process::exit(2);
    }
    if variants
        && (check
            || dry
            || argv.iter().any(|s| {
                matches!(
                    s.as_str(),
                    "-w" | "--width" | "-q" | "--444" | "--422" | "--420" | "--match"
                )
            }))
    {
        eprintln!(
            "ig-prep: --variants uses fixed width, quality and chroma values; it cannot combine with --check or --dry-run"
        );
        std::process::exit(2);
    }
    if match_tables
        && argv
            .iter()
            .any(|s| matches!(s.as_str(), "-q" | "--444" | "--422" | "--420" | "--dither"))
    {
        eprintln!("ig-prep: {MATCH_CONFLICT}");
        std::process::exit(2);
    }
    if pick && (check || dry || variants) {
        eprintln!(
            "ig-prep: --pick chooses the framing interactively; it cannot combine with --check, --dry-run or --variants"
        );
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
    if !dry
        && !variants
        && let Err(e) = std::fs::create_dir_all(&out_dir)
    {
        eprintln!("ig-prep: {}: {e}", out_dir.display());
        std::process::exit(1);
    }

    let opts = Opts {
        fit,
        gravity,
        width,
        quality,
        chroma,
        dither,
        match_tables,
        dry,
        pad,
        out_dir: out_dir.clone(),
    };

    if pick {
        std::process::exit(pick::run(&files, &opts));
    }
    if variants {
        match comparison::generate(&files, &opts) {
            Ok(path) => println!("Comparison written to {}", path.display()),
            Err(e) => {
                eprintln!("ig-prep: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let reports = run_all(&files, &opts);
    for r in &reports {
        println!("{}", r.line());
    }
    if reports.iter().any(|r| r.error.is_some()) {
        std::process::exit(1);
    }
}

/// `ig-prep simulate`: the servers' encode, applied locally to uploads.
///
/// Directories expand the same way as for conversion, but only JPEGs are
/// simulated: the tool models what the servers do to an upload, and an
/// upload is a JPEG this tool wrote. Anything else is named and skipped
/// rather than silently dropped, so a directory of sources does not read as
/// a finished run.
fn run_simulate(args: &[String]) -> i32 {
    let mut out_dir = PathBuf::from("ig-sim");
    let mut paths = Vec::new();
    let mut rest = args.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return 0;
            }
            "-o" | "--out" => match rest.next() {
                Some(d) => out_dir = PathBuf::from(d),
                None => {
                    eprintln!("ig-prep simulate: --out needs a directory");
                    return 2;
                }
            },
            s if s.starts_with('-') => {
                eprintln!("ig-prep simulate: unknown option {s}");
                return 2;
            }
            s => paths.push(PathBuf::from(s)),
        }
    }
    if paths.is_empty() {
        eprintln!("Usage: ig-prep simulate [-o <dir>] <PATH>...");
        return 2;
    }
    let is_jpeg = |p: &Path| {
        p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "jpg" | "jpeg"))
    };
    let (files, skipped): (Vec<PathBuf>, Vec<PathBuf>) =
        expand(&paths).into_iter().partition(|p| is_jpeg(p));
    for s in &skipped {
        eprintln!(
            "{}: skipped, simulate reads JPEG uploads; convert first",
            s.display()
        );
    }
    if files.is_empty() {
        eprintln!("ig-prep simulate: no JPEG uploads");
        return 1;
    }
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!("ig-prep: {}: {e}", out_dir.display());
        return 1;
    }
    let mut failed = false;
    for f in &files {
        match simulate::one(f, &out_dir) {
            Ok(s) => println!("{}", s.line()),
            Err(e) => {
                failed = true;
                eprintln!("{}: {e}", f.display());
            }
        }
    }
    if failed { 1 } else { 0 }
}

/// Both doors refuse these rather than drop them: a matched encode has no
/// quality setting, no chroma choice and no noise.
pub const MATCH_CONFLICT: &str = "--match encodes on the servers' tables at 4:4:4; it cannot combine with -q, a chroma flag or --dither";

/// The one encode both doors use, so a matched upload is a matched upload
/// whichever way it was asked for.
pub fn encode_with(o: &Opts, img: &image::Rgb) -> Result<Vec<u8>, String> {
    if o.match_tables {
        encode::jpeg_match(img)
    } else {
        encode::jpeg(img, o.quality, o.chroma, o.dither)
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
        .min(2) // Bound peak memory while retaining full-precision camera frames.
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

#[derive(Clone)]
pub struct Opts {
    pub fit: Fit,
    pub gravity: Gravity,
    pub width: u32,
    pub quality: u8,
    pub chroma: Chroma,
    pub dither: bool,
    /// Encode on the servers' measured tables instead of ZenJPEG's.
    pub match_tables: bool,
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
            quality: encode::DEFAULT_QUALITY,
            chroma: Chroma::Full,
            dither: false,
            match_tables: false,
            dry: false,
            pad: [255, 255, 255],
            out_dir: PathBuf::from("ig"),
        }
    }

    /// What the report says the encode was: the chroma mode, or the servers'
    /// tables when those replace it.
    pub fn encode_label(&self) -> &'static str {
        if self.match_tables {
            "servers' tables"
        } else {
            self.chroma.label()
        }
    }

    /// `--pad-color`, parsed from `ffffff` or `#ffffff`. Anything else leaves
    /// the fill alone rather than substituting a colour nobody asked for.
    pub fn set_pad_color(&mut self, hex: &str) {
        let h = hex.trim_start_matches('#');
        if h.len() == 6 && h.is_ascii() && h.bytes().all(|b| b.is_ascii_hexdigit()) {
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

    let img = match load_oriented(path) {
        Ok(img) => img,
        Err(e) => return Report::failed(path, name, e),
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
        chroma: o.encode_label(),
        error: None,
        note,
    };
    if o.dry {
        return report;
    }

    let final_img = match render(&img, &plan, o.pad) {
        Ok(img) => img,
        Err(e) => return Report::failed(path, report.name, e),
    };
    let bytes = match encode_with(o, &final_img) {
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

fn load_oriented(path: &Path) -> Result<image::Rgb, String> {
    // Orientation comes from the metadata reader rather than from the decoder,
    // because a HEIF's rotation lives in its EXIF and sips does not apply it.
    let orientation = exif_sooc::read(path)
        .ok()
        .and_then(|p| p.get("Orientation").and_then(|t| t.value.as_i64()))
        .unwrap_or(1) as u16;

    let decoded = decode::open(path)?;

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

    Ok(img)
}

fn render(img: &image::Rgb, plan: &geometry::Plan, pad: [u8; 3]) -> Result<image::Rgb, String> {
    let cropped;
    let src = if plan.crop == (0, 0, img.w, img.h) {
        img
    } else {
        cropped = img.crop(plan.crop.0, plan.crop.1, plan.crop.2, plan.crop.3);
        &cropped
    };
    let scaled = image::resize(src, plan.scale.0, plan.scale.1)?;
    Ok(if plan.canvas != plan.scale {
        scaled.pad_onto(
            plan.canvas.0,
            plan.canvas.1,
            plan.offset.0,
            plan.offset.1,
            pad,
        )
    } else {
        scaled
    })
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
