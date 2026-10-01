//! Local encoder diagnostics with source-coordinate companion crops.
use crate::{comparison, encode, geometry, image::Rgb};
use serde_json::json;
use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl Rect {
    fn parse(value: &str) -> Result<Self, String> {
        let parts: Result<Vec<u32>, _> = value.split(',').map(str::parse).collect();
        match parts.as_deref() {
            Ok(&[x, y, w, h]) if w > 0 && h > 0 => Ok(Self { x, y, w, h }),
            _ => Err("--detail needs x,y,width,height in oriented source pixels; width and height must be positive".into()),
        }
    }
    fn validate(self, img: &Rgb) -> Result<Self, String> {
        if self.x.checked_add(self.w).is_none_or(|v| v > img.w)
            || self.y.checked_add(self.h).is_none_or(|v| v > img.h)
        {
            return Err(format!(
                "detail rectangle exceeds oriented source {}x{}",
                img.w, img.h
            ));
        }
        Ok(self)
    }
    fn scaled(self, source: &Rgb, rendered: &Rgb) -> Self {
        let x = (self.x as u64 * rendered.w as u64 / source.w as u64) as u32;
        let y = (self.y as u64 * rendered.h as u64 / source.h as u64) as u32;
        let right = ((self.x as u64 + self.w as u64) * rendered.w as u64 / source.w as u64) as u32;
        let bottom = ((self.y as u64 + self.h as u64) * rendered.h as u64 / source.h as u64) as u32;
        Self {
            x,
            y,
            w: (right - x).max(1),
            h: (bottom - y).max(1),
        }
    }
    fn window(self) -> Self {
        let w = self.w.min(256);
        let h = self.h.min(256);
        Self {
            x: self.x + (self.w - w) / 2,
            y: self.y + (self.h - h) / 2,
            w,
            h,
        }
    }
}

pub fn run(args: &[String]) -> Result<PathBuf, String> {
    let mut width = geometry::TARGET_WIDTH;
    let mut detail = None;
    let mut output = None;
    let mut source = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-w" | "--width" => {
                width = args
                    .next()
                    .ok_or("--width needs a value")?
                    .parse()
                    .map_err(|_| "width must be a whole number")?;
                if !(1..=16384).contains(&width) {
                    return Err("width must be 1..16384".into());
                }
            }
            "--detail" => {
                detail = Some(Rect::parse(
                    args.next().ok_or("--detail needs a rectangle")?,
                )?)
            }
            "-o" | "--out" => {
                output = Some(PathBuf::from(args.next().ok_or("--out needs a directory")?))
            }
            a if a.starts_with('-') => return Err(format!("unknown diagnose option {a}")),
            _ if source.is_none() => source = Some(PathBuf::from(arg)),
            _ => return Err("diagnose takes one source image per experiment".into()),
        }
    }
    let output = output.ok_or("diagnose requires -o <NEW-directory>")?;
    let source = source.ok_or("diagnose requires a source image")?;
    generate(&source, &output, width, detail)?;
    Ok(output.join("index.html"))
}

fn generate(source: &Path, output: &Path, width: u32, detail: Option<Rect>) -> Result<(), String> {
    let img = crate::load_oriented(source)?;
    let detail = detail.map(|r| r.validate(&img)).transpose()?;
    // Creating a new directory is the only write boundary: existing exports are never replaced.
    std::fs::create_dir(output)
        .map_err(|e| format!("create NEW diagnostic directory {}: {e}", output.display()))?;
    let mut report = String::from(
        "image\tquality\tdimensions\tbytes\tSSIMULACRA2 (higher better)\tButteraugli (lower better)\n",
    );
    let mut html = String::from(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>Photo detail comparison</title><style>body{font:16px system-ui;margin:32px;background:#171717;color:#eee}a{color:#9cd3ff}.panels{display:flex;gap:16px;overflow:auto}figure{margin:0}img{image-rendering:pixelated;max-width:none}figcaption{margin:8px 0}pre{overflow:auto;line-height:1.6}p{max-width:75ch}</style><h1>Photo detail comparison</h1><p>Each preview shows the same region at 2× nearest-neighbour enlargement. Compare text and edges with the lossless resized reference. Scores measure JPEG encoding loss at each export's own dimensions; compare Q99 and Q100 within a row, not across different crops.</p>",
    );
    let mut entries = Vec::new();
    for (label, region) in [("full", None), ("detail", detail)] {
        if label == "detail" && region.is_none() {
            continue;
        }
        let crop;
        let input = if let Some(r) = region {
            crop = img.crop(r.x, r.y, r.w, r.h);
            &crop
        } else {
            &img
        };
        let plan = geometry::plan(
            input.w,
            input.h,
            geometry::Fit::Full,
            geometry::Gravity::Center,
            width,
        );
        let rendered = crate::render(input, &plan, [255; 3])?;
        let reference = output.join(format!("{label}-reference.png"));
        comparison::write_reference(&reference, &rendered)?;
        let window = if label == "full" {
            detail
                .map(|r| r.scaled(&img, &rendered))
                .unwrap_or(Rect {
                    x: 0,
                    y: 0,
                    w: rendered.w,
                    h: rendered.h,
                })
                .window()
        } else {
            Rect {
                x: 0,
                y: 0,
                w: rendered.w,
                h: rendered.h,
            }
            .window()
        };
        comparison::write_reference(
            &output.join(format!("{label}-reference-preview.png")),
            &rendered.crop(window.x, window.y, window.w, window.h),
        )?;
        write!(html,"<h2>{label}: {}×{}</h2><p><a href=\"{label}-reference.png\">Lossless reference</a> · <a href=\"{label}-q99.jpg\">Q99 JPEG</a> · <a href=\"{label}-q100.jpg\">Q100 JPEG</a></p><div class=\"panels\">",rendered.w,rendered.h).unwrap();
        panel(&mut html, label, "reference", window);
        for quality in [encode::DEFAULT_QUALITY, 100] {
            let name = format!("{label}-q{quality}.jpg");
            let path = output.join(&name);
            let bytes = encode::jpeg(&rendered, quality, encode::Chroma::Full, false)?;
            std::fs::write(&path, &bytes).map_err(|e| e.to_string())?;
            let (ssim, butter) = comparison::perceptual_metrics(&reference, &path)?;
            let decoded = crate::decode::open(&path)?.img;
            comparison::write_reference(
                &output.join(format!("{label}-q{quality}-preview.png")),
                &decoded.crop(window.x, window.y, window.w, window.h),
            )?;
            panel(&mut html, label, &format!("q{quality}"), window);
            writeln!(
                report,
                "{name}\t{quality}\t{}x{}\t{}\t{ssim:.6}\t{butter:.6}",
                rendered.w,
                rendered.h,
                bytes.len()
            )
            .unwrap();
            entries.push(json!({"file":name,"reference":format!("{label}-reference.png"),"quality":quality,"dimensions":[rendered.w,rendered.h],"bytes":bytes.len(),"ssimulacra2":ssim,"butteraugli":butter,"preview_rectangle":[window.x,window.y,window.w,window.h]}));
        }
        html.push_str("</div>");
    }
    html.push_str("<h2>Scores</h2><pre>");
    html.push_str(&report); // Fixed filenames and numeric results only; no source-controlled HTML.
    html.push_str("</pre><p><a href=\"scores.tsv\">Download scores</a> · <a href=\"manifest.json\">Experiment settings</a></p><p>Full-frame and detail JPEGs are separate exports. Q100 remains lossy. These measurements do not predict Instagram recompression. A detail rectangle outside 3:4–1.91:1 will still need framing in the app.</p></html>");
    std::fs::write(output.join("scores.tsv"), report).map_err(|e| e.to_string())?;
    std::fs::write(output.join("manifest.json"),serde_json::to_vec_pretty(&json!({"version":1,"encoder_profile":encode::PROFILE,"source":source.file_name().unwrap_or_default().to_string_lossy(),"source_dimensions":[img.w,img.h],"detail_rectangle":detail.map(|r| [r.x,r.y,r.w,r.h]),"requested_width":width,"chroma":"4:4:4","dither":false,"metrics":"libjxl CLI defaults; reference first; same-size SDR sRGB","variants":entries})).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::write(output.join("index.html"), html).map_err(|e| e.to_string())
}

fn panel(html: &mut String, label: &str, kind: &str, r: Rect) {
    write!(html,"<figure><img alt=\"{label} {kind} detail\" src=\"{label}-{kind}-preview.png\" width=\"{}\" height=\"{}\"><figcaption>{kind} · {}×{} pixels at 2×</figcaption></figure>",r.w*2,r.h*2,r.w,r.h).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rectangles_are_strict_and_cannot_overflow() {
        let img = Rgb::new(100, 200, vec![0.0; 100 * 200 * 3]);
        for value in ["0,0,0,1", "-1,0,1,1", "1,2,3", "1,2,3,4,5"] {
            assert!(Rect::parse(value).is_err());
        }
        assert!(
            Rect::parse("4294967295,0,2,1")
                .unwrap()
                .validate(&img)
                .is_err()
        );
        assert!(Rect::parse("99,199,2,1").unwrap().validate(&img).is_err());
        assert!(Rect::parse("99,199,1,1").unwrap().validate(&img).is_ok());
    }
    #[test]
    fn tiny_and_edge_preview_regions_stay_inside_render() {
        let source = Rgb::new(100, 200, vec![0.0; 100 * 200 * 3]);
        let rendered = Rgb::new(7, 14, vec![0.0; 7 * 14 * 3]);
        for r in [
            Rect {
                x: 99,
                y: 199,
                w: 1,
                h: 1,
            },
            Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 200,
            },
        ] {
            let r = r.scaled(&source, &rendered).window();
            assert!(r.w > 0 && r.h > 0 && r.x + r.w <= rendered.w && r.y + r.h <= rendered.h);
        }
    }
}
