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

## Speed

Per file on an M-series Mac, 5152x7728 source:

| input | time |
|---|--:|
| JPEG | 0.78s |
| HEIF | 1.34s |

Files are processed in parallel across cores. The resize is the only genuinely
CPU-heavy step and runs through `fast_image_resize`, which is SIMD.

MIT licensed.
