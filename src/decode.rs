//! Getting pixels out of whatever the camera or the export produced.
//!
//! JPEG and PNG are decoded in-process. HEIF and JPEG XL are handed to a
//! converter, because a HEIF holds an HEVC frame and there is no pure-Rust
//! decoder worth trusting for it. On macOS `sips` is already present and is
//! what the rest of this photo pipeline uses; `djxl` ships with libjxl.

use crate::image::Rgb;
use std::path::Path;
use std::process::Command;

/// A decoded image, and whether the decoder already applied EXIF orientation.
///
/// Decoders disagree about this and both answers are defensible, so the only
/// safe thing is to know which one you got. Measured on a 7728x5152 frame with
/// Orientation 8:
///
///   sips    leaves pixels stored-order and copies the tag forward, through
///           every output format and through a resize
///   djxl    returns 5152x7728 and writes "Horizontal (normal)", having
///           applied the rotation itself
///
/// Applying orientation on top of a decoder that already did it turns a
/// portrait into a sideways portrait, and the geometry planner then sizes it as
/// a landscape. That is a wrong picture rather than an error, which is why this
/// flag exists instead of an assumption.
pub struct Decoded {
    pub img: Rgb,
    pub orientation_applied: bool,
}

pub fn open(path: &Path) -> Result<Decoded, String> {
    let head = read_head(path)?;
    if head.starts_with(&[0xFF, 0xD8]) {
        // zune-jpeg decodes pixels and does not look at EXIF.
        return jpeg(&std::fs::read(path).map_err(|e| e.to_string())?).map(not_applied);
    }
    if head.starts_with(&[0x89, b'P', b'N', b'G']) {
        return png(&std::fs::read(path).map_err(|e| e.to_string())?).map(not_applied);
    }
    // HEIF/HEIC/HIF, AVIF, and JPEG XL both of its signatures.
    let is_bmff = head.get(4..8) == Some(b"ftyp");
    let is_jxl = head.starts_with(&[0xFF, 0x0A])
        || head.starts_with(&[0, 0, 0, 0x0C, b'J', b'X', b'L', b' ']);
    if is_bmff || is_jxl {
        return via_converter(path, is_jxl).map(|img| Decoded {
            img,
            // djxl normalises; sips does not.
            orientation_applied: is_jxl,
        });
    }
    Err("not a JPEG, PNG, HEIF or JPEG XL".into())
}

fn not_applied(img: Rgb) -> Decoded {
    Decoded {
        img,
        orientation_applied: false,
    }
}

fn read_head(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut b = vec![0u8; 16];
    let n = f.read(&mut b).map_err(|e| e.to_string())?;
    b.truncate(n);
    Ok(b)
}

/// Decode a JPEG to three channels.
///
/// `jpeg_set_out_colorspace` is a REQUEST rather than an instruction, and for a
/// single-channel source zune-jpeg declines it and returns Luma anyway. That
/// declining is silent, which is what makes it worth a comment: the buffer
/// comes back at one byte per pixel, `Rgb::new` labels it as three, and the
/// mismatch surfaces two layers later inside the resizer as "Size of buffer is
/// smaller than required" — an error that names neither the file nor its
/// colour. Every black-and-white JPEG failed that way.
///
/// So the colourspace the decoder actually USED is what gets expanded here,
/// never the one it was asked for. The length check below is the backstop for
/// the next colourspace this misses: a decoder disagreeing with its own
/// dimensions should be caught at the decode boundary, where the file name is
/// still in hand.
fn jpeg(bytes: &[u8]) -> Result<Rgb, String> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;

    let mut d = zune_jpeg::JpegDecoder::new(bytes);
    d.set_options(
        zune_jpeg::zune_core::options::DecoderOptions::default()
            .jpeg_set_out_colorspace(ColorSpace::RGB),
    );
    let px = d.decode().map_err(|e| format!("jpeg: {e:?}"))?;
    let (w, h) = d.dimensions().ok_or("jpeg: no dimensions")?;
    let space = d.get_output_colorspace().ok_or("jpeg: no colourspace")?;

    let px = match space {
        ColorSpace::RGB => px,
        ColorSpace::Luma => px.iter().flat_map(|&v| [v, v, v]).collect(),
        ColorSpace::LumaA => px
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|c| [c[0], c[0], c[0]])
            .collect(),
        ColorSpace::RGBA => px
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|c| [c[0], c[1], c[2]])
            .collect(),
        other => return Err(format!("jpeg: unsupported colourspace {other:?}")),
    };

    let want = w * h * 3;
    if px.len() != want {
        return Err(format!(
            "jpeg: decoded {} bytes for a {w}x{h} {space:?} image, expected {want}",
            px.len()
        ));
    }
    Ok(Rgb::new(w as u32, h as u32, px))
}

fn png(bytes: &[u8]) -> Result<Rgb, String> {
    let dec = png::Decoder::new(bytes);
    let mut reader = dec.read_info().map_err(|e| format!("png: {e}"))?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("png: {e}"))?;
    let px = match info.color_type {
        png::ColorType::Rgb => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgba => buf[..info.buffer_size()]
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|c| [c[0], c[1], c[2]])
            .collect(),
        png::ColorType::Grayscale => buf[..info.buffer_size()]
            .iter()
            .flat_map(|&v| [v, v, v])
            .collect(),
        other => return Err(format!("png: unsupported colour type {other:?}")),
    };
    Ok(Rgb::new(info.width, info.height, px))
}

/// Convert to an uncompressed intermediate in a temporary file, then decode it.
///
/// Lossless on purpose: this sits in front of the resize, and a lossy step
/// there would throw away detail before the one operation that needs it.
///
/// TIFF rather than PNG, which is worth 13x on a 40 megapixel frame. Measured
/// on a 5152x7728 HIF: `sips -s format png` takes 5.96s because it deflates
/// 160 MB, and `sips -s format tiff` takes 0.45s because it does not. The TIFF
/// is 326 MB on disk and is deleted immediately; the trade is disk bandwidth
/// for compute, and compute was losing badly.
fn via_converter(path: &Path, is_jxl: bool) -> Result<Rgb, String> {
    let tmp = std::env::temp_dir().join(format!(
        "ig-prep-{}-{}.{}",
        std::process::id(),
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("in"),
        if is_jxl { "png" } else { "tiff" }
    ));
    let ok = if is_jxl {
        Command::new("djxl")
            .args([path.as_os_str(), tmp.as_os_str()])
            .output()
    } else {
        Command::new("sips")
            .args([
                "-s".as_ref(),
                "format".as_ref(),
                "tiff".as_ref(),
                path.as_os_str(),
                "--out".as_ref(),
                tmp.as_os_str(),
            ])
            .output()
    };
    let out = ok.map_err(|e| {
        format!(
            "{} not found ({e}); it is needed to decode this format",
            if is_jxl { "djxl" } else { "sips" }
        )
    })?;
    if !out.status.success() {
        return Err(format!(
            "{} failed: {}",
            if is_jxl { "djxl" } else { "sips" },
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let bytes = std::fs::read(&tmp).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&tmp);
    if is_jxl { png(&bytes) } else { tiff(&bytes) }
}

/// Just enough TIFF to read what `sips` writes: uncompressed, chunky, 8 or 16
/// bits per sample, RGB or RGBA. Anything else is refused rather than guessed
/// at, because a wrong stride reads as a sheared image and looks like a bug in
/// the resize.
///
/// 16-bit is the normal case out of a HEIF, since the source is 10-bit and
/// sips widens rather than truncates. It is narrowed to 8 here with rounding.
/// Carrying the extra bits through the resize would only matter for an output
/// deeper than 8-bit JPEG, which Instagram does not accept.
fn tiff(d: &[u8]) -> Result<Rgb, String> {
    let le = match d.get(0..4) {
        Some([0x49, 0x49, 0x2a, 0x00]) => true,
        Some([0x4d, 0x4d, 0x00, 0x2a]) => false,
        _ => return Err("tiff: bad header".into()),
    };
    let u16at = |o: usize| -> Option<u16> {
        let b = d.get(o..o + 2)?;
        Some(if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    };
    let u32at = |o: usize| -> Option<u32> {
        let b = d.get(o..o + 4)?;
        Some(if le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    };

    let ifd = u32at(4).ok_or("tiff: no ifd")? as usize;
    let n = u16at(ifd).ok_or("tiff: no entries")? as usize;
    let mut f = std::collections::HashMap::new();
    let mut strip_offsets = Vec::new();
    let mut strip_counts = Vec::new();
    for i in 0..n {
        let e = ifd + 2 + i * 12;
        let (tag, fmt, count, val) = (
            u16at(e).ok_or("tiff: entry")?,
            u16at(e + 2).ok_or("tiff: entry")?,
            u32at(e + 4).ok_or("tiff: entry")? as usize,
            u32at(e + 8).ok_or("tiff: entry")?,
        );
        let read_n = |i: usize| -> Option<u32> {
            let width = if fmt == 3 { 2 } else { 4 };
            let inline = count * width <= 4;
            let base = if inline { e + 8 } else { val as usize };
            if fmt == 3 {
                u16at(base + i * 2).map(u32::from)
            } else {
                u32at(base + i * 4)
            }
        };
        match tag {
            // Strips and tiles are alternatives, and sips writes TILES: a
            // 7728x5152 frame comes back as four 3936x2592 tiles, padded out
            // to tile boundaries. Reading it as one run of rows produces a
            // sheared image rather than an error, so both are handled.
            0x0111 | 0x0144 => strip_offsets = (0..count).filter_map(read_n).collect(),
            0x0117 | 0x0145 => strip_counts = (0..count).filter_map(read_n).collect(),
            _ => {
                f.insert(tag, read_n(0).unwrap_or(val));
            }
        }
    }

    let w = *f.get(&0x0100).ok_or("tiff: no width")?;
    let h = *f.get(&0x0101).ok_or("tiff: no height")?;
    let bits = f.get(&0x0102).copied().unwrap_or(8);
    let compression = f.get(&0x0103).copied().unwrap_or(1);
    let samples = f.get(&0x0115).copied().unwrap_or(3) as usize;
    let planar = f.get(&0x011c).copied().unwrap_or(1);
    if compression != 1 || !(bits == 8 || bits == 16) || planar != 1 || !(3..=4).contains(&samples)
    {
        return Err(format!(
            "tiff: only uncompressed 8/16-bit chunky RGB(A) is read (compression {compression}, bits {bits}, samples {samples}, planar {planar})"
        ));
    }

    let bytes_per = if bits == 16 { 2 } else { 1 };
    let want = (w as usize) * (h as usize) * samples * bytes_per;
    let mut raw = Vec::with_capacity(want);
    for (o, c) in strip_offsets.iter().zip(strip_counts.iter()) {
        let (o, c) = (*o as usize, *c as usize);
        raw.extend_from_slice(d.get(o..o + c).ok_or("tiff: strip past end")?);
    }
    let tile_w = f.get(&0x0142).copied().unwrap_or(0) as usize;
    let tile_h = f.get(&0x0143).copied().unwrap_or(0) as usize;

    // Tiled: reassemble into row order, dropping each tile's padding.
    if tile_w > 0 && tile_h > 0 {
        let (iw, ih) = (w as usize, h as usize);
        let stride = samples * bytes_per;
        let across = iw.div_ceil(tile_w);
        let mut out = vec![0u8; iw * ih * stride];
        let mut pos = 0usize;
        for (t, count) in strip_counts.iter().enumerate() {
            let start = *strip_offsets.get(t).ok_or("tiff: tile offset")? as usize;
            let tile = d
                .get(start..start + *count as usize)
                .ok_or("tiff: tile past end")?;
            let (tx, ty) = ((t % across) * tile_w, (t / across) * tile_h);
            for row in 0..tile_h {
                let y = ty + row;
                if y >= ih {
                    break;
                }
                let cols = tile_w.min(iw.saturating_sub(tx));
                let src = row * tile_w * stride;
                let dst = (y * iw + tx) * stride;
                let n = cols * stride;
                if src + n <= tile.len() && dst + n <= out.len() {
                    out[dst..dst + n].copy_from_slice(&tile[src..src + n]);
                }
            }
            pos += 1;
        }
        if pos == 0 {
            return Err("tiff: no tiles".into());
        }
        raw = out;
    } else if raw.len() < want {
        return Err("tiff: short pixel data".into());
    }

    raw.truncate(want);
    let px: Vec<u8> = if bits == 8 {
        if samples == 3 {
            raw
        } else {
            raw.as_chunks::<4>()
                .0
                .iter()
                .flat_map(|c| [c[0], c[1], c[2]])
                .collect()
        }
    } else {
        let rd = |c: &[u8]| -> u8 {
            let v = if le {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            };
            // Round rather than truncate: dropping the low byte outright
            // darkens every value by half a step, which is visible as a shift
            // across a smooth sky.
            // The +32895 rounds to nearest; the result cannot exceed 255, so
            // there is nothing left to clamp.
            ((v as u32 * 255 + 32895) >> 16) as u8
        };
        raw.chunks_exact(samples * 2)
            .flat_map(|p| [rd(&p[0..2]), rd(&p[2..4]), rd(&p[4..6])])
            .collect()
    };
    Ok(Rgb::new(w, h, px))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grayscale JPEG must come back as three channels.
    ///
    /// This is a regression rather than a nicety: `jpeg_set_out_colorspace` is
    /// a request the decoder declines for a single-channel source, so every
    /// black-and-white JPEG used to reach the resizer at a third of the length
    /// its dimensions claimed and die there with an error naming neither.
    /// Built here rather than committed as a fixture so the test carries its
    /// own input.
    #[test]
    fn grayscale_jpeg_expands_to_rgb() {
        let (w, h) = (32usize, 24usize);
        let luma: Vec<u8> = (0..w * h).map(|i| (i % 256) as u8).collect();
        let mut encoded = Vec::new();
        jpeg_encoder::Encoder::new(&mut encoded, 90)
            .encode(&luma, w as u16, h as u16, jpeg_encoder::ColorType::Luma)
            .expect("encode a luma jpeg");

        let img = jpeg(&encoded).expect("decode the luma jpeg");
        assert_eq!((img.w, img.h), (w as u32, h as u32));
        assert_eq!(img.px.len(), w * h * 3, "must be three channels");
        // Expanded rather than merely padded: a pixel is grey, so its three
        // channels agree.
        for px in img.px.as_chunks::<3>().0.iter() {
            assert_eq!(px[0], px[1]);
            assert_eq!(px[1], px[2]);
        }
    }

    /// The colour path is untouched by that expansion.
    #[test]
    fn colour_jpeg_still_decodes_to_rgb() {
        let (w, h) = (32usize, 24usize);
        let rgb: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                [
                    (i % 256) as u8,
                    ((i * 3) % 256) as u8,
                    ((i * 7) % 256) as u8,
                ]
            })
            .collect();
        let mut encoded = Vec::new();
        jpeg_encoder::Encoder::new(&mut encoded, 90)
            .encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
            .expect("encode an rgb jpeg");

        let img = jpeg(&encoded).expect("decode the rgb jpeg");
        assert_eq!((img.w, img.h), (w as u32, h as u32));
        assert_eq!(img.px.len(), w * h * 3);
    }
}
