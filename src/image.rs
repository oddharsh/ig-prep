//! An 8-bit RGB image, and the two things that must happen to it in the right
//! order: EXIF rotation, then resizing in linear light.

pub struct Rgb {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

impl Rgb {
    pub fn new(w: u32, h: u32, px: Vec<u8>) -> Self {
        debug_assert_eq!(px.len(), (w as usize) * (h as usize) * 3);
        Self { w, h, px }
    }

    fn at(&self, x: u32, y: u32) -> [u8; 3] {
        let i = ((y as usize) * (self.w as usize) + x as usize) * 3;
        [self.px[i], self.px[i + 1], self.px[i + 2]]
    }

    /// Apply an EXIF Orientation so that every later step works on pixels in
    /// the order a person sees them.
    ///
    /// Doing this first is what lets the geometry reason about width and height
    /// without caring which way the camera was held. Skipping it is how a
    /// portrait frame gets planned as a landscape one.
    pub fn oriented(self, orientation: u16) -> Rgb {
        if orientation <= 1 || orientation > 8 {
            return self;
        }
        let (w, h) = (self.w, self.h);
        let swap = matches!(orientation, 5..=8);
        let (nw, nh) = if swap { (h, w) } else { (w, h) };
        let mut px = vec![0u8; (nw as usize) * (nh as usize) * 3];
        for y in 0..h {
            for x in 0..w {
                let (nx, ny) = match orientation {
                    2 => (w - 1 - x, y),
                    3 => (w - 1 - x, h - 1 - y),
                    4 => (x, h - 1 - y),
                    5 => (y, x),
                    6 => (h - 1 - y, x),
                    7 => (h - 1 - y, w - 1 - x),
                    8 => (y, w - 1 - x),
                    _ => (x, y),
                };
                let p = self.at(x, y);
                let i = ((ny as usize) * (nw as usize) + nx as usize) * 3;
                px[i..i + 3].copy_from_slice(&p);
            }
        }
        Rgb::new(nw, nh, px)
    }

    pub fn crop(&self, x: u32, y: u32, w: u32, h: u32) -> Rgb {
        let mut px = Vec::with_capacity((w as usize) * (h as usize) * 3);
        for row in y..y + h {
            let start = ((row as usize) * (self.w as usize) + x as usize) * 3;
            px.extend_from_slice(&self.px[start..start + (w as usize) * 3]);
        }
        Rgb::new(w, h, px)
    }

    /// Place this image onto a larger canvas of a solid colour.
    pub fn pad_onto(&self, cw: u32, ch: u32, ox: u32, oy: u32, fill: [u8; 3]) -> Rgb {
        let mut px = Vec::with_capacity((cw as usize) * (ch as usize) * 3);
        for _ in 0..(cw as usize) * (ch as usize) {
            px.extend_from_slice(&fill);
        }
        let mut out = Rgb::new(cw, ch, px);
        for y in 0..self.h.min(ch - oy) {
            let src = ((y as usize) * (self.w as usize)) * 3;
            let dst = (((y + oy) as usize) * (cw as usize) + ox as usize) * 3;
            let n = (self.w as usize) * 3;
            out.px[dst..dst + n].copy_from_slice(&self.px[src..src + n]);
        }
        out
    }
}

/// sRGB transfer function, both ways.
///
/// Resizing averages pixels, and averaging sRGB values averages the wrong
/// numbers: sRGB is a perceptual encoding, so the mean of two encoded values is
/// not the encoding of the mean light. The visible cost is darkened edges and
/// muddy highlights, worst exactly where a photograph has fine detail against a
/// bright sky. So the image goes to linear light, gets resized, and comes back.
fn srgb_to_linear_table() -> [f32; 256] {
    let mut t = [0f32; 256];
    for (i, v) in t.iter_mut().enumerate() {
        let c = i as f32 / 255.0;
        *v = if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        };
    }
    t
}

fn linear_to_srgb(v: f32) -> u8 {
    let c = v.clamp(0.0, 1.0);
    let s = if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

/// Resize with Lanczos3, in linear light.
pub fn resize(src: &Rgb, w: u32, h: u32) -> Result<Rgb, String> {
    use fast_image_resize::images::Image;
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

    if (src.w, src.h) == (w, h) {
        return Ok(Rgb::new(src.w, src.h, src.px.clone()));
    }

    // Build the f32 buffer in ONE pass, straight into the bytes the resizer
    // wants. A 40 megapixel frame is 478 MB as f32x3, so materialising it twice
    // (once as f32, once as bytes) costs nearly a gigabyte of traffic to say
    // the same thing. Measured on a 5152x7728 HIF, removing the second copy
    // took a file from 2.4s to under a second.
    let table = srgb_to_linear_table();
    let mut bytes = vec![0u8; src.px.len() * 4];
    for (chunk, &i) in bytes.chunks_exact_mut(4).zip(src.px.iter()) {
        chunk.copy_from_slice(&table[i as usize].to_ne_bytes());
    }
    let src_img = Image::from_vec_u8(src.w, src.h, bytes, PixelType::F32x3)
        .map_err(|e| format!("source image: {e}"))?;
    let mut dst_img = Image::new(w, h, PixelType::F32x3);
    Resizer::new()
        .resize(
            &src_img,
            &mut dst_img,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3)),
        )
        .map_err(|e| format!("resize: {e}"))?;

    let out: Vec<u8> = dst_img
        .buffer()
        .chunks_exact(4)
        .map(|c| linear_to_srgb(f32::from_ne_bytes([c[0], c[1], c[2], c[3]])))
        .collect();
    Ok(Rgb::new(w, h, out))
}
