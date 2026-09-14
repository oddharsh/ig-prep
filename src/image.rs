//! Floating-point linear sRGB. Colour conversion precedes geometry; only export quantizes.

pub struct Rgb {
    pub w: u32,
    pub h: u32,
    pub px: Vec<f32>,
}

impl Rgb {
    pub fn new(w: u32, h: u32, px: Vec<f32>) -> Self {
        debug_assert_eq!(px.len(), (w as usize) * (h as usize) * 3);
        Self { w, h, px }
    }

    fn at(&self, x: u32, y: u32) -> [f32; 3] {
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
        let mut px = vec![0.0; (nw as usize) * (nh as usize) * 3];
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
        let fill = fill.map(|v| crate::color::srgb_to_linear(v as f32 / 255.0));
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

/// Lanczos3 operates directly on linear-light samples, without an intermediate copy.
pub fn resize(src: &Rgb, w: u32, h: u32) -> Result<Rgb, String> {
    use fast_image_resize::images::{Image, ImageRef};
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
    if (src.w, src.h) == (w, h) {
        return Ok(Rgb::new(w, h, src.px.clone()));
    }
    let source = ImageRef::new(
        src.w,
        src.h,
        bytemuck::cast_slice(&src.px),
        PixelType::F32x3,
    )
    .map_err(|e| format!("source image: {e}"))?;
    let mut pixels = vec![0.0f32; w as usize * h as usize * 3];
    let mut target = Image::from_slice_u8(
        w,
        h,
        bytemuck::cast_slice_mut(&mut pixels),
        PixelType::F32x3,
    )
    .map_err(|e| format!("target image: {e}"))?;
    Resizer::new()
        .resize(
            &source,
            &mut target,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3)),
        )
        .map_err(|e| format!("resize: {e}"))?;
    Ok(Rgb::new(w, h, pixels))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resizing_averages_light_without_quantizing() {
        let src = Rgb::new(2, 1, vec![0.1, 0.1, 0.1, 0.1001, 0.1001, 0.1001]);
        let out = resize(&src, 1, 1).unwrap();
        assert!((out.px[0] - 0.10005).abs() < 0.00001);
        assert_eq!(resize(&src, 2, 1).unwrap().px, src.px);
    }
    #[test]
    fn padding_colour_is_also_linear() {
        let src = Rgb::new(1, 1, vec![1.0; 3]);
        let out = src.pad_onto(3, 1, 1, 0, [128; 3]);
        assert!((out.px[0] - 0.21586).abs() < 0.0001);
        assert_eq!(&out.px[3..6], &[1.0; 3]);
    }
    #[test]
    fn all_orientations_preserve_float_samples() {
        let img = || Rgb::new(2, 3, (0..18).map(|i| i as f32 / 19.0).collect());
        for orientation in 1..=8 {
            let mut values = img().oriented(orientation).px;
            values.sort_by(f32::total_cmp);
            assert_eq!(values, img().px);
        }
        assert_eq!((img().oriented(6).w, img().oriented(6).h), (3, 2));
    }
}
