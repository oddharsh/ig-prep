//! Decode at source precision, then colour-manage every format into linear sRGB.
//! HEIF uses macOS sips; JPEG XL uses djxl. Their intermediates retain profiles.
//!
//! JPEG goes through jpeg-decoder rather than zune-jpeg. zune-jpeg 0.4 rejects
//! a progressive scan whose length is a multiple of the restart interval, and
//! ZenJPEG's exports at Instagram sizes are exactly that shape, so the tool
//! could not read its own output. jpeg-decoder matches libjpeg-turbo to within
//! one level on those files. Upstream fixed zune-jpeg in 0.5.13.

use crate::{color, image::Rgb};
use moxcms::ColorProfile;
use std::{io::Cursor, path::Path, process::Command};

pub struct Decoded {
    pub img: Rgb,
    pub orientation_applied: bool,
}

pub fn open(path: &Path) -> Result<Decoded, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let (img, orientation_applied) = if bytes.starts_with(&[0xff, 0xd8]) {
        (jpeg(&bytes)?, false)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        (png(&bytes)?, false)
    } else if bytes.starts_with(&[0xff, 0x0a]) || bytes.starts_with(b"\0\0\0\x0cJXL ") {
        (via_converter(path, true)?, true)
    } else if bytes.get(4..8) == Some(b"ftyp") {
        crate::heif::check_sdr(&bytes)?;
        (via_converter(path, false)?, false)
    } else {
        return Err("not a JPEG, PNG, HEIF or JPEG XL".into());
    };
    Ok(Decoded {
        img,
        orientation_applied,
    })
}

fn jpeg(bytes: &[u8]) -> Result<Rgb, String> {
    use jpeg_decoder::{Decoder, PixelFormat};
    let mut d = Decoder::new(bytes);
    let px = d.decode().map_err(|e| format!("jpeg: {e}"))?;
    let info = d.info().ok_or("jpeg: no image info")?;
    // YCbCr and RGB-coded frames both arrive as RGB; only CMYK and 16-bit
    // lossless keep their own layout, and neither is a camera format.
    let channels = match info.pixel_format {
        PixelFormat::RGB24 => 3,
        PixelFormat::L8 => 1,
        other => return Err(format!("jpeg: unsupported sample format {other:?}")),
    };
    let icc = d.icc_profile();
    let source = color::profile(icc.as_deref())?;
    color::convert(
        info.width.into(),
        info.height.into(),
        &px,
        channels,
        255.0,
        &source,
    )
}

fn png_profile(info: &png::Info<'_>) -> Result<ColorProfile, String> {
    // PNG's cICP describes the image itself and takes precedence over ICC.
    if let Some(c) = info.coding_independent_code_points {
        if c.matrix_coefficients != 0 || !c.is_video_full_range_image {
            return Err("png: only full-range RGB cICP is supported".into());
        }
        let cicp = moxcms::CicpProfile {
            color_primaries: c
                .color_primaries
                .try_into()
                .map_err(|e| format!("png primaries: {e}"))?,
            transfer_characteristics: c
                .transfer_function
                .try_into()
                .map_err(|e| format!("png transfer: {e}"))?,
            matrix_coefficients: moxcms::MatrixCoefficients::Identity,
            full_range: true,
        };
        let _: moxcms::ColorPrimaries = cicp
            .color_primaries
            .try_into()
            .map_err(|e| format!("png primaries: {e}"))?;
        let _: moxcms::ToneReprCurve = cicp
            .transfer_characteristics
            .try_into()
            .map_err(|e| format!("png transfer: {e}"))?;
        return Ok(ColorProfile::new_from_cicp(cicp));
    }
    if let Some(icc) = &info.icc_profile {
        return color::profile(Some(icc));
    }
    let mut profile = color::srgb_profile();
    if info.srgb.is_some() {
        return Ok(profile);
    }
    if let Some(chrm) = info.source_chromaticities {
        let xy = |p: (png::ScaledFloat, png::ScaledFloat)| moxcms::Chromaticity {
            x: p.0.into_value(),
            y: p.1.into_value(),
        };
        let white = xy(chrm.white);
        profile.update_rgb_colorimetry(
            moxcms::XyY {
                x: white.x as f64,
                y: white.y as f64,
                yb: 1.0,
            },
            moxcms::ColorPrimaries {
                red: xy(chrm.red),
                green: xy(chrm.green),
                blue: xy(chrm.blue),
            },
        );
    }
    if let Some(gamma) = info.source_gamma {
        let gamma = gamma.into_value();
        if gamma <= 0.0 {
            return Err("png: invalid gamma".into());
        }
        profile.cicp = None;
        profile.red_trc = Some(moxcms::curve_from_gamma(1.0 / gamma));
        profile.green_trc = profile.red_trc.clone();
        profile.blue_trc = profile.red_trc.clone();
    }
    Ok(profile)
}

fn png(bytes: &[u8]) -> Result<Rgb, String> {
    let mut decoder = png::Decoder::new(bytes);
    // Expand palettes and sub-byte gray, but retain 16-bit samples.
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().map_err(|e| format!("png: {e}"))?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let frame = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("png: {e}"))?;
    let source = png_profile(reader.info())?;
    let channels = frame.color_type.samples();
    let bytes = &buf[..frame.buffer_size()];
    match frame.bit_depth {
        png::BitDepth::Eight => {
            color::convert(frame.width, frame.height, bytes, channels, 255.0, &source)
        }
        png::BitDepth::Sixteen => {
            let samples: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|p| u16::from_be_bytes(*p))
                .collect();
            color::convert(
                frame.width,
                frame.height,
                &samples,
                channels,
                65535.0,
                &source,
            )
        }
        depth => Err(format!("png: unsupported bit depth {depth:?}")),
    }
}

fn via_converter(path: &Path, is_jxl: bool) -> Result<Rgb, String> {
    // A private directory avoids collisions between equal stems and cleans up on failure.
    let temp = tempfile::Builder::new()
        .prefix("ig-prep-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let target = temp.path().join(if is_jxl {
        "decoded.png"
    } else {
        "decoded.tiff"
    });
    let mut cmd = Command::new(if is_jxl { "djxl" } else { "sips" });
    if is_jxl {
        cmd.arg(path).arg(&target);
    } else {
        cmd.args(["-s", "format", "tiff"])
            .arg(path)
            .arg("--out")
            .arg(&target);
    }
    let result = cmd.output().map_err(|e| {
        format!(
            "{} is needed to decode this format: {e}",
            if is_jxl { "djxl" } else { "sips" }
        )
    })?;
    if !result.status.success() {
        return Err(format!(
            "converter failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    let bytes = std::fs::read(target).map_err(|e| e.to_string())?;
    if is_jxl { png(&bytes) } else { tiff(&bytes) }
}

fn tiff(bytes: &[u8]) -> Result<Rgb, String> {
    use tiff::{
        ColorType,
        decoder::{Decoder, DecodingResult, Limits},
        tags::Tag,
    };
    let read = || -> Result<Rgb, String> {
        let mut limits = Limits::default();
        // 40MP RGBA16 exceeds the decoder's 256MiB default; remain bounded.
        limits.decoding_buffer_size = 1024 * 1024 * 1024;
        let mut d = Decoder::new(Cursor::new(bytes))
            .map_err(|e| e.to_string())?
            .with_limits(limits);
        let (w, h) = d.dimensions().map_err(|e| e.to_string())?;
        let channels = match d.colortype().map_err(|e| e.to_string())? {
            ColorType::RGB(_) => 3,
            ColorType::RGBA(_) => 4,
            ColorType::Gray(_) => 1,
            ColorType::GrayA(_) => 2,
            other => return Err(format!("unsupported colour type {other:?}")),
        };
        let icc = d
            .find_tag(Tag::IccProfile)
            .map_err(|e| e.to_string())?
            .map(|v| v.into_u8_vec())
            .transpose()
            .map_err(|e| e.to_string())?;
        let profile = color::profile(icc.as_deref())?;
        match d.read_image().map_err(|e| e.to_string())? {
            DecodingResult::U8(px) => color::convert(w, h, &px, channels, 255.0, &profile),
            DecodingResult::U16(px) => color::convert(w, h, &px, channels, 65535.0, &profile),
            _ => Err("only 8/16-bit integer TIFF samples are supported".into()),
        }
    };
    read().map_err(|e| format!("tiff: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    fn png_bytes(depth: png::BitDepth, profile: Option<Vec<u8>>, pixels: &[u8], w: u32) -> Vec<u8> {
        let mut info = png::Info::with_size(w, 1);
        info.color_type = png::ColorType::Rgb;
        info.bit_depth = depth;
        info.icc_profile = profile.map(Cow::Owned);
        let mut bytes = Vec::new();
        png::Encoder::with_info(&mut bytes, info)
            .unwrap()
            .write_header()
            .unwrap()
            .write_image_data(pixels)
            .unwrap();
        bytes
    }

    #[test]
    fn grayscale_jpeg_expands_to_linear_rgb() {
        let mut bytes = Vec::new();
        jpeg_encoder::Encoder::new(&mut bytes, 95)
            .encode(&[128; 16], 4, 4, jpeg_encoder::ColorType::Luma)
            .unwrap();
        let img = jpeg(&bytes).unwrap();
        assert_eq!(img.px.len(), 48);
        for p in img.px.as_chunks::<3>().0 {
            assert_eq!(p[0], p[1]);
            assert_eq!(p[1], p[2]);
        }
        assert!((img.px[0] - 0.21586).abs() < 0.001);
    }

    #[test]
    fn png_and_jpeg_honor_embedded_p3() {
        let icc = color::P3_ICC.to_vec();
        let tagged = png(&png_bytes(
            png::BitDepth::Eight,
            Some(icc.clone()),
            &[128, 90, 60],
            1,
        ))
        .unwrap();
        let untagged = png(&png_bytes(png::BitDepth::Eight, None, &[128, 90, 60], 1)).unwrap();
        assert!((tagged.px[0] - untagged.px[0]).abs() > 0.01);
        let mut bytes = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut bytes, 100);
        enc.add_icc_profile(&icc).unwrap();
        enc.encode(&[128, 90, 60], 1, 1, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        let jpg = jpeg(&bytes).unwrap();
        for (&got, expected) in jpg.px.iter().zip(tagged.px) {
            assert!((got - expected).abs() < 0.01);
        }
    }

    /// A scan whose length is a multiple of the restart interval ends flush
    /// with its final interval, and T.81 figure B.2 puts no RST marker after
    /// that one. zune-jpeg 0.4 rejected the layout as "Marker SOS found in
    /// bitstream", so the two fixtures below are the regression guard.
    #[test]
    fn progressive_scan_ending_on_a_restart_interval_decodes() {
        // 16x8 at 4:4:4 is two data units per scan; an interval of one MCU
        // makes every scan end on an interval boundary.
        let px: Vec<u8> = (0..16 * 8)
            .flat_map(|i| [(i % 16) as u8 * 12 + 30, (i / 16) as u8 * 20 + 40, 160])
            .collect();
        let mut bytes = Vec::new();
        let mut enc = jpeg_encoder::Encoder::new(&mut bytes, 95);
        enc.set_progressive(true);
        enc.set_restart_interval(1);
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
        enc.encode(&px, 16, 8, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        assert!(
            bytes.windows(2).any(|p| p == [0xff, 0xdd]),
            "no DRI segment"
        );
        assert!(
            bytes
                .windows(2)
                .any(|p| p[0] == 0xff && (0xd0..=0xd7).contains(&p[1])),
            "no RST markers"
        );
        let img = jpeg(&bytes).unwrap();
        assert_eq!((img.w, img.h), (16, 8));
        for (i, (&linear, &expected)) in img.px.iter().zip(&px).enumerate() {
            let got = color::linear_to_srgb(linear) * 255.0;
            assert!(
                (got - f32::from(expected)).abs() < 8.0,
                "sample {i}: decoded {got}, expected {expected}"
            );
        }
    }

    #[test]
    fn own_export_at_a_realistic_size_reads_back() {
        // 1440x960 at 4:4:4 is 180x120 data units. ZenJPEG restarts every four
        // rows of them, and 720 divides 21600, which is the shape that failed.
        let (w, h) = (1440u32, 960u32);
        let mut px = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let mut n = (y * w + x).wrapping_mul(0x9e3779b9);
                n ^= n >> 15;
                let noise = (n & 0xff) as f32 / 255.0 * 0.01 - 0.005;
                px.extend([
                    0.05 + 0.7 * x as f32 / w as f32 + noise,
                    0.05 + 0.7 * y as f32 / h as f32 + noise,
                    0.3 + noise,
                ]);
            }
        }
        let img = Rgb::new(w, h, px);
        let bytes = crate::encode::jpeg(&img, 95, crate::encode::Chroma::Full, false).unwrap();
        let dri = bytes
            .windows(2)
            .position(|p| p == [0xff, 0xdd])
            .map(|i| u16::from_be_bytes([bytes[i + 4], bytes[i + 5]]))
            .expect("the export defines a restart interval");
        assert_eq!(
            (w / 8 * (h / 8)) % u32::from(dri),
            0,
            "the fixture only guards the regression while every scan is a whole number of intervals"
        );
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("export.jpg");
        std::fs::write(&path, &bytes).unwrap();
        let decoded = open(&path).unwrap();
        assert!(!decoded.orientation_applied);
        assert_eq!((decoded.img.w, decoded.img.h), (w, h));
        let (mut worst, mut total) = (0.0f32, 0.0f64);
        for (&got, &expected) in decoded.img.px.iter().zip(&img.px) {
            let d = (color::linear_to_srgb(got) - color::linear_to_srgb(expected)).abs() * 255.0;
            worst = worst.max(d);
            total += f64::from(d);
        }
        let mean = total / img.px.len() as f64;
        // Measured 5.8 and 0.76 at the time of writing; a dropped or shifted
        // row would push the worst sample past 40.
        assert!(worst < 12.0 && mean < 1.5, "worst {worst}, mean {mean}");
    }

    #[test]
    fn png_retains_low_sixteen_bit_values() {
        let samples = [32768u16, 32768, 32768, 32769, 32769, 32769];
        let raw: Vec<u8> = samples.iter().flat_map(|v| v.to_be_bytes()).collect();
        let p = png(&png_bytes(png::BitDepth::Sixteen, None, &raw, 2)).unwrap();
        assert!(p.px[3] > p.px[0]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn sips_intermediate_preserves_profile_and_high_precision() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.png");
        let raw: Vec<u8> = [32768u16, 20000, 10000]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect();
        let bytes = png_bytes(
            png::BitDepth::Sixteen,
            Some(color::P3_ICC.to_vec()),
            &raw,
            1,
        );
        std::fs::write(&path, &bytes).unwrap();
        let expected = png(&bytes).unwrap();
        let got = via_converter(&path, false).unwrap();
        for (&a, b) in got.px.iter().zip(expected.px) {
            assert!((a - b).abs() < 0.0001, "{a} != {b}");
        }
    }

    #[test]
    fn png_cicp_and_gamma_are_honoured() {
        let mut info = png::Info::with_size(1, 1);
        info.source_gamma = Some(png::ScaledFloat::new(1.0));
        let profile = png_profile(&info).unwrap();
        let img = color::convert(1, 1, &[128u8; 3], 3, 255.0, &profile).unwrap();
        assert!((img.px[0] - 128.0 / 255.0).abs() < 0.0001);
        info.coding_independent_code_points = Some(png::CodingIndependentCodePoints {
            color_primaries: 12,
            transfer_function: 13,
            matrix_coefficients: 0,
            is_video_full_range_image: true,
        });
        let profile = png_profile(&info).unwrap();
        let img = color::convert(1, 1, &[255u8, 0, 0], 3, 255.0, &profile).unwrap();
        assert!((img.px[0] - 1.2249).abs() < 0.002);
        info.coding_independent_code_points
            .as_mut()
            .unwrap()
            .transfer_function = 16;
        let hdr = png_profile(&info).unwrap();
        assert!(color::convert(1, 1, &[128u8; 3], 3, 255.0, &hdr).is_err());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn exported_jpeg_has_a_profile_apple_recognizes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("output.jpg");
        let img = Rgb::new(8, 8, vec![0.25; 8 * 8 * 3]);
        std::fs::write(
            &path,
            crate::encode::jpeg(&img, 95, crate::encode::Chroma::Full, false).unwrap(),
        )
        .unwrap();
        let output = Command::new("sips")
            .args(["-g", "profile"])
            .arg(path)
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("profile: sRGB"));
    }

    #[test]
    fn tiff_preserves_low_bits_and_rejects_incomplete_data() {
        let mut bytes = Cursor::new(Vec::new());
        let samples = [32768u16, 32768, 32768, 32769, 32769, 32769];
        tiff::encoder::TiffEncoder::new(&mut bytes)
            .unwrap()
            .write_image::<tiff::encoder::colortype::RGB16>(2, 1, &samples)
            .unwrap();
        let image = tiff(bytes.get_ref()).unwrap();
        assert!(image.px[3] > image.px[0]);
        assert!(tiff(&bytes.get_ref()[..bytes.get_ref().len() / 2]).is_err());
    }
}
