# ig-prep

Prepare photographs for Instagram with explicit colour conversion, precise
downscaling and a single final JPEG encode. Reads JPEG, HEIF/HEIC/HIF, AVIF,
JPEG XL and PNG; writes SDR sRGB JPEG with an embedded colour profile.

```sh
ig-prep ~/Pictures/selects          # full frame; choose the crop in the app
ig-prep --crop --gravity top .      # crop to the nearest supported ratio
ig-prep --pad --pad-color 000000 .  # retain the frame inside a padded canvas
ig-prep -n .                       # report dimensions without writing files
```

## Colour and precision

The decoder reads the source ICC profile and converts samples to floating-point,
linear sRGB. PNG also honours cICP, sRGB, gamma and chromaticity metadata.
Untagged inputs assume sRGB; malformed or unsupported profiles produce errors.

HEIF travels through an uncompressed TIFF intermediate that preserves the
platform decoder's profile and sample depth. JPEG XL travels through PNG.
The TIFF reader supports strips and tiles, and retains 16-bit samples.

Rotation, cropping, Lanczos3 resizing and padding operate on linear samples.
ZenJPEG receives floating-point linear sRGB directly and applies the sRGB transfer
function internally, preserving fractional samples through its transform.
The output is a standard 8-bit JPEG. Colours outside the sRGB gamut clip at export.
Alpha is discarded, as in previous versions; this tool targets opaque photographs.

`--dither` adds deterministic, neutral noise of at most half an 8-bit sRGB step
before encoding. It is off by default. The former explicit 8-bit rounding pass
has been removed, including when this option is enabled. Compare returned skies
and smooth gradients before choosing it; JPEG recompression may remove its benefit.

**HDR policy:** the output is SDR. Detected PQ/HLG colour descriptions are refused
with a request for an SDR export whose highlights you have reviewed. This avoids
silently choosing a tone curve. HDR gain maps are not preserved or scored.

## Framing and size

The ratio band is **3:4 through 1.91:1**. Instagram announced 3:4 support in
[May 2025](https://www.threads.com/@mosseri/post/DKOIbJkRNIb).
A 2:3 camera portrait still needs cropping or padding to fit.

The default `--full` keeps the complete frame at up to 3072 pixels wide.
`--crop` chooses the nearest ratio in the band. `--pad` includes the borders
inside the requested width: a 2:3 portrait at width 3072 becomes a 3072×4096
canvas containing a 2731×4096 photograph. Source pixels never enlarge.

Defaults are **3072 pixels, ZenJPEG quality 99, 4:4:4 chroma**. The width follows
a September 2026 measurement of real posts: Instagram served uploads at their own
width up to 3072, for single posts and carousel items alike, and reduced
full-resolution uploads to 3072×4096. Whether that cap is a width or a 4096 long
edge is untested on frames other than 3:4, and the earlier 1440 default predates
the measurement. Matching dimensions still does not guarantee that Instagram skips
resampling, and repeated chroma subsampling does not necessarily halve resolution
again. Upload clients and served renditions need to be measured.

## Choose the window here

Cropping in the app costs detail. On a September 2026 post the app's crop of a
3072×4608 upload was a resample, 4098.2 source rows squeezed into 4096, so the
phase drifted across the frame: the served image kept 71% of the upload's
Laplacian energy overall and 86% in one band through the middle. The window it
chooses has a fractional height because it is drawn in screen points.

`--pick` moves that choice onto this machine:

```sh
ig-prep --pick -o ig ~/Pictures/selects
```

It opens one page in the default browser, served from a loopback socket by
this process, one photograph at a time. Drag the window, pick 3:4, 4:5, 1:1
or a landscape ratio, zoom if you must (never past the target width, so
nothing enlarges), and export. The export is a whole-pixel window inside the
band, delivered at the target width through the same resize and encode as
every other conversion, so the app has nothing left to crop. Leave the framing
untouched in the app; any adjustment there reintroduces the resample.

## JPEG encoder

The encoder is **ZenJPEG 0.8.4**, using the site's JPEG stack with a
[pinned SharpYUV input fix](https://github.com/oddharsh/zenjpeg/commit/d7ec0944d1ab6a3f4e2dc25bbc3a11461529cd63):

- Standard YCbCr JPEG with an embedded sRGB ICC profile.
- Adaptive quantization and hybrid trellis optimization via `auto_optimize(true)`.
  ZenJPEG enables the hybrid optimizer within its supported quality range.
- Progressive scan search, selected after `auto_optimize` because that call resets
  the scan mode. Optimized Huffman coding and deringing retain upstream defaults.
- Floating-point input throughout; no intermediate 8-bit RGB conversion.

SharpYUV is enabled for `--422` and `--420`. The pinned fix makes its chroma
path interpret linear floating-point samples correctly, preserving their
precision instead of reading their bytes as RGB8. It also corrects u16 and BGR
layouts in the dependency. Default `--444` retains every chroma sample and
produces the same bytes as before this fix.

The dependency's regression tests cover both gamma-aware methods, equivalent
sample layouts, odd dimensions, padded rows and chunked input, using an
independent JPEG decoder. ig-prep also checks decoded colours in all three modes.
See the [source fix and validation notes](https://github.com/oddharsh/zenjpeg/blob/d7ec0944d1ab6a3f4e2dc25bbc3a11461529cd63/docs/SHARPYUV_INPUT_FIX.md).
On the same six local references used below, mean SSIMULACRA2 / Butteraugli
changed from 89.952 / 1.456 to 89.989 / 1.425 for 4:2:2, and from
88.820 / 1.771 to 88.969 / 1.694 for 4:2:0. Sizes changed by less than 0.04%.
These are small local gains; 4:4:4 still scored best. No Instagram round trip
was performed for this fix.

`-q` now uses ZenJPEG's approximate jpegli quality scale. Equal numbers do not
mean equal quality or file size across encoders. The default changes from the
old encoder's q95 to ZenJPEG q99. Existing JPEGs are untouched
until you explicitly convert their sources again. `--dither` now adds noise
without rounding pixels to 8-bit first.

A local comparison used six photographs at width 1440 with identical 16-bit sRGB
PNG references and 4:4:4 sampling. Means from libjxl 0.11.1:

| Encoder setting | SSIMULACRA2 ↑ | Butteraugli ↓ | Bytes per image |
| --- | ---: | ---: | ---: |
| Previous encoder q95 | 87.96 | 1.302 | 1,365,264 |
| Previous encoder q98 | 91.37 | 0.767 | 2,010,767 |
| JPEGli distance 0.15, progressive | 91.50 | 0.594 | 1,977,809 |
| ZenJPEG q99, default | 91.63 | 0.555 | 1,885,407 |
| ZenJPEG q100 | 92.46 | 0.526 | 2,405,402 |

ZenJPEG q99 improved both metrics over the old default on all six images.
Against old q98, it improved SSIMULACRA2 on five and Butteraugli on all six,
with smaller files throughout. JPEGli omitted the standard ICC profile and was
interpreted as sRGB by the metrics. These are local comparisons at the listed
settings, not equal-size encodes or Instagram round trips. Q100 is available
when the additional file size is acceptable.

Progressive scans improve JPEG packing and partial loading. They do not restore
detail lost during resizing or establish that Instagram will skip recompression.
A [2023 study](https://informationsecurity.uibk.ac.at/pdfs/HB2023_IHMMSEC.pdf)
observed distinct progressive scan scripts in Instagram images. That historical
observation does not specify today's upload pipeline, quantization tables or
crop/resize order. Matching those would require measured upload/return pairs.

## Measure an Instagram round trip

```sh
ig-prep --variants --crop -o comparison ~/Pictures/selects
# Upload the files in comparison/uploads/.
# Download the served images into comparison/returned/ under matching filenames.
ig-prep score comparison
```

Each source produces eight variants: 1080/1440 width × ZenJPEG quality 95/99 ×
4:4:4/4:2:0. The quality pair changes with this encoder migration so the experiment
includes the new default.
The directory also contains a manifest identifying the encoder recipe and a
16-bit sRGB PNG reference for each source. Older manifests remain readable;
their absent encoder identifier means unspecified, not the current encoder.
Use a new output directory for every experiment. `--variants` refuses
existing directories, dry runs and overrides of its fixed encoder settings.
Choose `--crop` or `--pad` for sources outside the ratio band.

Use identical framing and upload settings. Record the app version, upload quality
setting, date and post type. Test foliage, skin, saturated edges, grain and skies,
and repeat uploads to distinguish encoder choices from server variability.

Save the actual served image, with its matching upload filename. Screenshots
and intermediary exports introduce another image-processing step. Scoring
supports the converter's input formats; WebP returns need a supported rendition.

The scorer reports missing files, served dimensions, RGB RMSE and local 8×8
luminance SSIM. It normalizes each source group to the smallest returned width,
capped at 1080, and rejects changed aspect ratios. Compare scores within a group.
Inspect framing yourself: a same-ratio crop cannot be detected from dimensions.

The metrics supplement visual inspection. Inspect the full-size returns too:
normalizing to a common size hides any extra detail in higher-resolution images.
Generating variants uploads nothing. `ig-prep simulate` below previews the
servers' encode from measured facts, and is no substitute for a real return.

## Preview the servers' encode

`ig-prep simulate` re-encodes an upload the way Instagram's servers did when
measured in September 2026, so the damage can be seen before posting:

```sh
ig-prep -o ig ~/Pictures/selects
ig-prep simulate -o ig-sim ig
```

Sixteen served renditions from two accounts, with HQ upload on, shared one recipe:
the upload's own width up to 3072, 4:2:0 chroma, progressive scans, optimised
Huffman tables, no ICC profile, and one pair of quantisation tables (luma 2 to 13,
chroma the IJG standard table at about quality 94). The simulation applies those
tables through the common libjpeg chain, triangle chroma upsampling on decode and
2×2 averaging on encode, and writes `<name>.ig.jpg` with the PSNR against the
upload. On four photographs at 1080 wide it agrees with a libjpeg-turbo encode of
the same tables to within 48 to 53 dB and reproduces that chain's SSIMULACRA2 to
0.1. Instagram's exact filters remain unknown, and one measured recipe is a
snapshot rather than a contract.

It does not model the app's crop, which measured as a 1:1 cut at a fractional
vertical offset, nor the downscale applied to uploads wider than 3072, which
measured soft. Uploads wider than the tier are refused rather than guessed at.

The simulation settles the chroma question. After the servers' 4:2:0, a 4:4:4
upload scored SSIMULACRA2 85.3 and Butteraugli 1.67 against the lossless
reference, and a SharpYUV 4:2:0 upload 84.4 and 2.12, on the same four
photographs at 1080 wide. Subsampling once, at the end, beats subsampling twice,
so 4:4:4 stays the default.

## Point a model at it

`ig-prep mcp` speaks [MCP](https://modelcontextprotocol.io) on stdin and stdout,
so an assistant with file access can convert photographs where they already sit.

```jsonc
// claude_desktop_config.json, or any MCP client's server list
{
  "mcpServers": {
    "ig-prep": {
      "command": "ig-prep",
      "args": ["mcp", "--root", "/Users/you/Pictures"]
    }
  }
}
```

Then: *"convert everything in ~/Pictures/selects for Instagram."*

Three tools. `ig_plan` reports what a conversion would do and writes nothing,
`ig_convert` does it, and `ig_check_rotation` is `--check`. They take the same
arguments the flags take and share the same defaults, because both surfaces call
the same function — a tool server that reimplements its own CLI grows a second
set of defaults and then disagrees with its own documentation.

**Photographs never move and never enter the conversation.** The files are
already on the machine the server runs on, so a result carries paths, sizes and
dimensions rather than image data. That is a deliberate limit and not a missing
feature: a converted frame is a few hundred kilobytes, which is roughly a
megabyte of base64, and forty of them would be forty megabytes of a model's
context spent on pixels it cannot look at anyway.

**`--root` is the boundary.** Every path argument, inputs and output directory
alike, is resolved through its symlinks and refused if it lands outside. Without
it the server will convert anything it can read, which is fine for a tool you
drive yourself and is worth thinking about before handing it to an agent.

One call converts at most 200 files, and says how many it left, because a result
listing 200 conversions of a 900-file directory otherwise reads as a complete
run.

The server answers the 2026-07-28 revision and the three before it. Both eras
stay because a legacy client has no fall-forward mechanism: pointed at a
modern-only server it does not negotiate down, it fails.

## --check

A HEIF can state its rotation twice: as a container transform (`irot`, `imir`)
and as an EXIF Orientation tag. Every Fujifilm HIF does, saying the same thing
both ways.

That redundancy is survivable. A viewer that applies one of them is right, and
one that applies both shows the photograph sideways, which is why the same file
can look correct in one app and wrong in another on the same phone. What is not
survivable is the two saying DIFFERENT things, because then no viewer is right
and the file has no orientation anyone can agree on.

```sh
ig-prep --check ~/Pictures/sooc
```

```
160 files: 0 disagree, 114 say the same thing twice, 44 say it once, 2 say nothing
```

It exits non-zero when any file disagrees, so it can gate an import. The
transforms are resolved for the PRIMARY image item specifically, through
`pitm`, `ipma` and `ipco`: a file usually carries a thumbnail with transforms of
its own, and reporting the thumbnail's rotation as the photograph's would be a
confident wrong answer.

Converting a disagreeing file is still the repair: the output has the rotation
baked into its pixels and carries no EXIF, so there is nothing left to
interpret.

## Build and checks

Rust 1.93 or newer is required by ZenJPEG. Git is required to fetch the pinned
source fix. Cargo uses Git's CLI transport; the first fetch also downloads
ZenJPEG's nested reference-code submodules, even though ig-prep does not compile
their C++ tools. Later locked builds use Cargo's cache.

```sh
cargo build --release --locked
cargo test --release --locked
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
```

JPEG and PNG decoding work on every CI platform. HEIF/HEIC/HIF and AVIF need
macOS `sips`; JPEG XL needs `djxl` from libjxl. Tests generate their own images,
including a 16-bit P3 image through `sips` on macOS. Native HEIF decoding needs macOS codec access. A sandbox can block it even
when `sips` reports success, producing an incomplete TIFF that the decoder rejects.
Camera-specific HIF orientation and colour should also be checked on
representative originals.

Conversions use at most two workers to bound memory while processing large
floating-point frames. Earlier speed measurements predate colour management
and should be remeasured before making performance claims.

## Licensing

The ig-prep source retains its MIT licensing. The pinned ZenJPEG dependency
declares `AGPL-3.0-only OR LicenseRef-Imazen-Commercial`; see its
[crate metadata and license files](https://crates.io/crates/zenjpeg/0.8.4).
Builds containing ZenJPEG include those dependency terms and must not be
represented as MIT-only. No commercial license is supplied by this repository.
Bundled ICC profiles are CC0; see [profiles/README.md](profiles/README.md).
