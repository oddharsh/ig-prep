# ig-prep

Prepare photographs for Instagram's compression. Reads JPEG, HEIF/HEIC/HIF,
JPEG XL and PNG; writes sRGB JPEG at the size Instagram wants.

The idea is to do the destructive work here, carefully, so that Instagram has
as little left to do as possible. It resizes and re-encodes whatever it gets.
What it does not do is resample an image that already arrives at its target
size.

## The default keeps your framing

Instagram shows frames between 4:5 and 1.91:1. A 3:2 portrait, which is most of
what a Fujifilm or a Leica produces held vertically, is 0.667 and falls outside
that, so something has to give.

The default gives nothing up. **Cropping a portrait to 4:5 removes height only**,
so a frame delivered at exactly Instagram's target width is already the right
width for any vertical crop you drag in the app. You keep the framing decision,
and the expensive step, the 40 megapixel downscale, stays here where it is done
in linear light with Lanczos3 rather than by whatever Instagram uses.

One thing to avoid: **drag, do not pinch-zoom**. Zooming changes the scale and
hands the resample back.

```sh
ig-prep ~/Pictures/selects          # full frame at target width, crop in app
ig-prep --crop --gravity top .      # crop to 4:5 here instead
ig-prep --pad --pad-color 000000 .  # keep the whole frame, pad to 4:5
ig-prep -n .                        # print the plan, write nothing
```

## What is measured and what is guessed

Measured, and visible in the code:

- Reading a HEIF through an uncompressed TIFF intermediate rather than a PNG one
  takes a 5152x7728 frame from 5.96s to 0.45s, because PNG spends that time
  deflating 160 MB nobody keeps.
- Resizing in linear light matters. sRGB is a perceptual encoding, so averaging
  encoded values averages the wrong numbers and darkens edges.
- `sips` does not apply EXIF rotation, so orientation is read separately with
  [exif-sooc](https://github.com/oddharsh/exif-sooc) and applied before anything
  else. A decoder that silently ignores rotation plans a portrait as a landscape.

Guessed, and clearly marked in `src/geometry.rs`:

- **The target width, 1440.** Chosen over 1080 because the failure modes are not
  symmetric: if Instagram wants 1080 it downscales cleanly from 1440, and if it
  wants 1440 and gets 1080 it upscales, inventing detail. Guessing high costs a
  resample, guessing low costs the picture.
- **4:4:4 chroma.** Instagram re-encodes to 4:2:0 regardless, and handing its
  encoder full-resolution chroma should beat handing it chroma already halved
  once. Two successive halvings smear saturated edges.
- **The ratio band itself**, 4:5 to 1.91:1.

None of those three are facts until a round trip measures them: upload a variant
grid, save what Instagram serves back, and score each against its source. That
harness is the obvious next piece and the upload is the one step a program
cannot do for you.

## Speed

Per file on an M-series Mac, 5152x7728 source:

| input | time |
|---|--:|
| JPEG | 0.78s |
| HEIF | 1.34s |

Files are processed in parallel across cores. The resize is the only genuinely
CPU-heavy step and runs through `fast_image_resize`, which is SIMD.

MIT licensed.
