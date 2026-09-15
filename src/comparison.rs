//! Reproducible local export experiment. Uploads and returned files are supplied
//! by the photographer; the metrics never claim to model Instagram's encoder.

use crate::{
    Opts,
    encode::{self, Chroma},
    geometry::{self, Fit},
    image::{self, Rgb},
};
use serde_json::{Value, json};
use std::{
    fmt::Write as _,
    path::{Component, Path, PathBuf},
};

pub fn generate(files: &[PathBuf], opts: &Opts) -> Result<PathBuf, String> {
    if opts.dry {
        return Err(
            "--variants writes an experiment; use ordinary --dry-run to inspect framing".into(),
        );
    }
    // Exclusive creation prevents a new run overwriting an earlier experiment.
    std::fs::create_dir(&opts.out_dir).map_err(|e| {
        format!(
            "create a new comparison directory {}: {e}",
            opts.out_dir.display()
        )
    })?;
    let uploads = opts.out_dir.join("uploads");
    let returned = opts.out_dir.join("returned");
    let refs = opts.out_dir.join("references");
    for dir in [&uploads, &returned, &refs] {
        std::fs::create_dir(dir).map_err(|e| e.to_string())?;
    }
    let mut entries = Vec::new();
    for (index, path) in files.iter().enumerate() {
        let img = crate::load_oriented(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let reference_plan = geometry::plan(img.w, img.h, opts.fit, opts.gravity, 1080);
        if opts.fit == Fit::Full && reference_plan.outside_band {
            return Err(format!(
                "{} needs a fixed crop for comparison: rerun with --crop or --pad in a new output directory",
                path.display()
            ));
        }
        let reference_name = format!("s{:04}.png", index + 1);
        let reference = crate::render(&img, &reference_plan, opts.pad)?;
        write_reference(&refs.join(&reference_name), &reference)?;
        for width in [1080, 1440] {
            let plan = geometry::plan(img.w, img.h, opts.fit, opts.gravity, width);
            let rendered = crate::render(&img, &plan, opts.pad)?;
            for quality in [95, encode::DEFAULT_QUALITY] {
                for chroma in [Chroma::Full, Chroma::Quartered] {
                    let name = format!(
                        "s{:04}-w{width}-q{quality}-{}.jpg",
                        index + 1,
                        if chroma == Chroma::Full { "444" } else { "420" }
                    );
                    let bytes = encode::jpeg(&rendered, quality, chroma, opts.dither)?;
                    std::fs::write(uploads.join(&name), &bytes).map_err(|e| e.to_string())?;
                    entries.push(json!({"file":name,"source":path.file_name().unwrap_or_default().to_string_lossy(),"reference":reference_name,
                        "requested_width":width,"dimensions":[rendered.w,rendered.h],"quality":quality,"chroma":chroma.label(),"dither":opts.dither,"bytes":bytes.len()}));
                }
            }
        }
    }
    let manifest = json!({"version":1,"encoder_profile":encode::PROFILE,"fit":format!("{:?}",opts.fit).to_lowercase(),"gravity":format!("{:?}",opts.gravity).to_lowercase(),"pad_color":opts.pad,"variants":entries});
    let dest = opts.out_dir.join("manifest.json");
    std::fs::write(
        &dest,
        serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(opts.out_dir.join("README.txt"),
        "Upload each file in uploads/ with identical framing and upload settings. Record app/version, upload quality setting, date, and post type in your notes. Download the actual served image (not a screenshot) into returned/ using its matching upload filename. Do not convert it first. Run: ig-prep score <this-directory>. Missing returns are reported. Metrics use the lossless 1080-wide reference, normalizing each source group to the smallest returned width (at most 1080), and report actual served dimensions. Compare results within a source group. Inspect full-size returns too: scores at a common size cannot capture a higher-resolution rendition's extra detail. Repeat uploads to distinguish settings from server variability. HDR/gain-map and WebP returns are not supported. Keep filenames even if a JPEG or AVIF response has a different extension.\n").map_err(|e| e.to_string())?;
    Ok(dest)
}

fn write_reference(path: &Path, img: &Rgb) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut info = png::Info::with_size(img.w, img.h);
    info.color_type = png::ColorType::Rgb;
    info.bit_depth = png::BitDepth::Sixteen;
    info.icc_profile = Some(std::borrow::Cow::Owned(crate::color::SRGB_ICC.to_vec()));
    let mut writer = png::Encoder::with_info(std::io::BufWriter::new(file), info)
        .map_err(|e| e.to_string())?
        .write_header()
        .map_err(|e| e.to_string())?;
    let pixels: Vec<u8> = img
        .px
        .iter()
        .flat_map(|&v| ((crate::color::linear_to_srgb(v) * 65535.0).round() as u16).to_be_bytes())
        .collect();
    writer.write_image_data(&pixels).map_err(|e| e.to_string())
}

// Manifest entries are filenames, never arbitrary paths or symlink escapes.
fn local_file(root: &Path, name: &str) -> Result<PathBuf, String> {
    let p = Path::new(name);
    if p.components().count() != 1 || !matches!(p.components().next(), Some(Component::Normal(_))) {
        return Err("comparison filenames must be simple basenames".into());
    }
    let path = root
        .join(p)
        .canonicalize()
        .map_err(|e| format!("{name}: {e}"))?;
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if !path.starts_with(root) {
        return Err("comparison file escapes its directory".into());
    }
    Ok(path)
}

pub fn score(dir: &Path) -> Result<String, String> {
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(dir.join("manifest.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    if manifest["version"] != 1 {
        return Err("unsupported comparison manifest version".into());
    }
    let entries = manifest["variants"]
        .as_array()
        .ok_or("manifest has no variants")?;
    let mut groups = std::collections::BTreeMap::<String, Vec<&Value>>::new();
    for e in entries {
        groups
            .entry(
                e["reference"]
                    .as_str()
                    .ok_or("missing reference filename")?
                    .to_owned(),
            )
            .or_default()
            .push(e);
    }
    let mut report = String::from(
        "file\tserved\tcompared\tRGB RMSE (0-255; lower better)\t8x8 luma SSIM (higher better)\n",
    );
    let mut count = 0;
    for (reference_name, group) in groups {
        let reference =
            crate::decode::open(&local_file(&dir.join("references"), &reference_name)?)?.img;
        let mut returns = Vec::new();
        let mut width = reference.w.min(1080);
        for e in group {
            let name = e["file"].as_str().ok_or("missing return filename")?;
            // Validate before existence testing, including for missing entries.
            if Path::new(name).components().count() != 1
                || !matches!(
                    Path::new(name).components().next(),
                    Some(Component::Normal(_))
                )
            {
                return Err("invalid return filename".into());
            }
            if !dir.join("returned").join(name).exists() {
                writeln!(report, "{name}\tMISSING").unwrap();
                continue;
            }
            let image = crate::load_oriented(&local_file(&dir.join("returned"), name)?)?;
            let expected_h = image.w as f64 * reference.h as f64 / reference.w as f64;
            if (image.h as f64 - expected_h).abs() > 1.5 {
                return Err(format!(
                    "{name}: returned framing/aspect ratio differs from the reference; do not score unmatched crops"
                ));
            }
            width = width.min(image.w);
            returns.push((name.to_owned(), image));
        }
        let height = (width as f64 * reference.h as f64 / reference.w as f64)
            .round()
            .max(1.0) as u32;
        let reference = image::resize(&reference, width, height)?;
        for (name, returned) in returns {
            let normalized = image::resize(&returned, width, height)?;
            let (rmse, ssim) = metrics(&reference, &normalized);
            writeln!(
                report,
                "{name}\t{}x{}\t{width}x{height}\t{rmse:.4}\t{ssim:.6}",
                returned.w, returned.h
            )
            .unwrap();
            count += 1;
        }
    }
    writeln!(report,"Scored {count}/{} returns. Compare scores within each source group; inspect images and repeat uploads before choosing settings.",entries.len()).unwrap();
    Ok(report)
}

/// Error in display-encoded sRGB and local luminance structure. These are
/// complementary diagnostics, not a perceptual verdict or upload simulation.
fn metrics(reference: &Rgb, returned: &Rgb) -> (f64, f64) {
    let encode = |img: &Rgb| {
        img.px
            .iter()
            .map(|&v| crate::color::linear_to_srgb(v) as f64)
            .collect::<Vec<_>>()
    };
    let a = encode(reference);
    let b = encode(returned);
    let rmse = (a.iter().zip(&b).map(|(a, b)| (a - b).powi(2)).sum::<f64>() / a.len() as f64)
        .sqrt()
        * 255.0;
    let luma = |p: &[f64]| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
    let mut sum = 0.0;
    let mut blocks = 0;
    for y in (0..reference.h).step_by(8) {
        for x in (0..reference.w).step_by(8) {
            let mut pairs = Vec::with_capacity(64);
            for j in y..(y + 8).min(reference.h) {
                for i in x..(x + 8).min(reference.w) {
                    let offset = (j as usize * reference.w as usize + i as usize) * 3;
                    pairs.push((luma(&a[offset..offset + 3]), luma(&b[offset..offset + 3])));
                }
            }
            let n = pairs.len() as f64;
            let ma = pairs.iter().map(|p| p.0).sum::<f64>() / n;
            let mb = pairs.iter().map(|p| p.1).sum::<f64>() / n;
            let va = pairs.iter().map(|p| (p.0 - ma).powi(2)).sum::<f64>() / n;
            let vb = pairs.iter().map(|p| (p.1 - mb).powi(2)).sum::<f64>() / n;
            let cov = pairs.iter().map(|p| (p.0 - ma) * (p.1 - mb)).sum::<f64>() / n;
            sum += ((2.0 * ma * mb + 0.0001) * (2.0 * cov + 0.0009))
                / ((ma * ma + mb * mb + 0.0001) * (va + vb + 0.0009));
            blocks += 1;
        }
    }
    (rmse, sum / blocks as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metrics_distinguish_identical_and_changed_images() {
        let a = Rgb::new(
            16,
            16,
            (0..16 * 16 * 3).map(|i| (i % 256) as f32 / 255.0).collect(),
        );
        let b = Rgb::new(16, 16, a.px.iter().map(|v| 1.0 - v).collect());
        let same = metrics(&a, &a);
        let changed = metrics(&a, &b);
        assert_eq!(same.0, 0.0);
        assert!((same.1 - 1.0).abs() < 1e-10);
        assert!(changed.0 > 20.0);
        assert!(changed.1 < 0.8);
    }
    #[test]
    fn experiment_has_eight_unique_variants_and_scores_returned_files() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.png");
        let img = Rgb::new(
            16,
            16,
            (0..16 * 16 * 3).map(|i| (i % 256) as f32 / 255.0).collect(),
        );
        write_reference(&source, &img).unwrap();
        let mut opts = Opts::defaults();
        opts.out_dir = temp.path().join("experiment");
        let path = generate(std::slice::from_ref(&source), &opts).unwrap();
        let manifest: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(manifest["encoder_profile"], encode::PROFILE);
        let entries = manifest["variants"].as_array().unwrap();
        assert_eq!(entries.len(), 8);
        assert!(
            entries
                .iter()
                .any(|e| e["quality"] == encode::DEFAULT_QUALITY)
        );
        assert_eq!(
            std::fs::read_dir(opts.out_dir.join("uploads"))
                .unwrap()
                .count(),
            8
        );
        for e in entries {
            let name = e["file"].as_str().unwrap();
            std::fs::copy(
                opts.out_dir.join("uploads").join(name),
                opts.out_dir.join("returned").join(name),
            )
            .unwrap();
        }
        assert!(score(&opts.out_dir).unwrap().contains("Scored 8/8"));
        assert!(
            generate(&[source], &opts).is_err(),
            "never overwrite an experiment"
        );
        assert!(local_file(&opts.out_dir, "../source.png").is_err());
    }
}
