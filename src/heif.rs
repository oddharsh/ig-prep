//! Reading a HEIF's own idea of how the image should be turned.
//!
//! A HEIF can say "rotate this" twice: once as a container transform (`irot`
//! and `imir`, properties attached to the image item) and once as an EXIF
//! Orientation tag. A Fujifilm HIF writes both, saying the same thing each way.
//!
//! That redundancy is the interop hazard. Applying either is correct; applying
//! BOTH turns the picture sideways, and which a viewer does is not something
//! the file can control. When the two say DIFFERENT things it is worse, because
//! then no viewer is right and the photograph has no defined orientation at
//! all. That second case is what `--check` is for.
//!
//! The properties are resolved for the PRIMARY item specifically. A file
//! commonly carries a thumbnail with its own transforms, and reporting the
//! thumbnail's rotation as the photograph's would be a confident wrong answer.

/// A rotation and a mirror, which is enough to compare two ways of saying the
/// same thing. Rotation is clockwise degrees, applied after the mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transform {
    pub rotate_cw: u16,
    pub mirrored: bool,
}

impl Transform {
    pub const NONE: Transform = Transform {
        rotate_cw: 0,
        mirrored: false,
    };

    /// EXIF Orientation, 1 through 8.
    pub fn from_exif(v: u16) -> Option<Transform> {
        let t = |rotate_cw, mirrored| {
            Some(Transform {
                rotate_cw,
                mirrored,
            })
        };
        match v {
            1 => t(0, false),
            2 => t(0, true),
            3 => t(180, false),
            // A vertical flip is a horizontal flip turned half way round.
            4 => t(180, true),
            5 => t(270, true),
            6 => t(90, false),
            7 => t(90, true),
            8 => t(270, false),
            _ => None,
        }
    }

    pub fn describe(&self) -> String {
        match (self.rotate_cw, self.mirrored) {
            (0, false) => "none".into(),
            (r, false) => format!("rotate {r} CW"),
            (0, true) => "mirror".into(),
            (r, true) => format!("mirror + rotate {r} CW"),
        }
    }
}

struct Bx<'a> {
    kind: [u8; 4],
    body: &'a [u8],
    next: usize,
}

fn box_at(d: &[u8], off: usize) -> Option<Bx<'_>> {
    let size = u32::from_be_bytes(d.get(off..off + 4)?.try_into().ok()?) as u64;
    let kind: [u8; 4] = d.get(off + 4..off + 8)?.try_into().ok()?;
    let (size, head) = match size {
        1 => (
            u64::from_be_bytes(d.get(off + 8..off + 16)?.try_into().ok()?),
            16,
        ),
        0 => ((d.len() - off) as u64, 8),
        n => (n, 8),
    };
    if size < head as u64 {
        return None;
    }
    let end = (off + size as usize).min(d.len());
    Some(Bx {
        kind,
        body: d.get(off + head..end)?,
        next: off + size as usize,
    })
}

fn find<'a>(d: &'a [u8], kind: &[u8; 4]) -> Option<Bx<'a>> {
    let mut off = 0;
    while off + 8 <= d.len() {
        let b = box_at(d, off)?;
        if &b.kind == kind {
            return Some(b);
        }
        if b.next <= off {
            return None;
        }
        off = b.next;
    }
    None
}

/// The transform the container declares for the primary image.
///
/// Returns None when the file carries no transform properties, which is the
/// unambiguous case and the one that needs no report.
pub fn container_transform(file: &[u8]) -> Option<Transform> {
    let meta = find(file, b"meta")?;
    let kids = meta.body.get(4..)?; // meta is a FullBox

    let primary = find(kids, b"pitm").and_then(|b| {
        let v = *b.body.first()?;
        if v == 0 {
            Some(u16::from_be_bytes(b.body.get(4..6)?.try_into().ok()?) as u32)
        } else {
            Some(u32::from_be_bytes(b.body.get(4..8)?.try_into().ok()?))
        }
    })?;

    let iprp = find(kids, b"iprp")?;
    let ipco = find(iprp.body, b"ipco")?;

    // Properties are addressed by their 1-based index in ipco, so the whole
    // list has to be walked even though only two kinds are interesting.
    let mut props: Vec<Option<Transform>> = vec![None];
    let mut off = 0;
    while off + 8 <= ipco.body.len() {
        let Some(b) = box_at(ipco.body, off) else {
            break;
        };
        props.push(match (&b.kind, b.body.first()) {
            (b"irot", Some(v)) => {
                // irot counts counter-clockwise; everything here is clockwise.
                let ccw = (v & 3) as u16 * 90;
                Some(Transform {
                    rotate_cw: (360 - ccw) % 360,
                    mirrored: false,
                })
            }
            (b"imir", Some(v)) => Some(Transform {
                // axis 0 mirrors about a vertical axis, which is a left-right
                // flip; axis 1 is top-bottom, the same flip turned half round.
                rotate_cw: if v & 1 == 1 { 180 } else { 0 },
                mirrored: true,
            }),
            _ => None,
        });
        if b.next <= off {
            break;
        }
        off = b.next;
    }

    let ipma = find(iprp.body, b"ipma")?;
    let version = *ipma.body.first()?;
    let flags = u32::from_be_bytes([0, ipma.body[1], ipma.body[2], ipma.body[3]]);
    let wide_index = flags & 1 == 1;
    let count = u32::from_be_bytes(ipma.body.get(4..8)?.try_into().ok()?);
    let mut p = 8usize;

    let mut found: Option<Transform> = None;
    for _ in 0..count {
        let item = if version < 1 {
            let v = u16::from_be_bytes(ipma.body.get(p..p + 2)?.try_into().ok()?) as u32;
            p += 2;
            v
        } else {
            let v = u32::from_be_bytes(ipma.body.get(p..p + 4)?.try_into().ok()?);
            p += 4;
            v
        };
        let n = *ipma.body.get(p)?;
        p += 1;
        for _ in 0..n {
            let idx = if wide_index {
                let v = u16::from_be_bytes(ipma.body.get(p..p + 2)?.try_into().ok()?) & 0x7fff;
                p += 2;
                v as usize
            } else {
                let v = (*ipma.body.get(p)? & 0x7f) as usize;
                p += 1;
                v
            };
            if item == primary {
                if let Some(Some(t)) = props.get(idx) {
                    found = Some(match found {
                        None => *t,
                        // Two transform properties compose. Mirrors do not
                        // commute with rotation, and the property order is the
                        // order they are listed in, so this composes in that
                        // order rather than sorting them.
                        Some(a) => Transform {
                            rotate_cw: (a.rotate_cw + t.rotate_cw) % 360,
                            mirrored: a.mirrored ^ t.mirrored,
                        },
                    });
                }
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_and_irot_describe_the_same_turn() {
        // A Fujifilm HIF writes irot angle 1, which is 90 counter-clockwise,
        // and EXIF 8, which is 270 clockwise. They are the same rotation, and
        // a check that called them different would flag every file.
        let from_irot = Transform {
            rotate_cw: (360 - 90) % 360,
            mirrored: false,
        };
        assert_eq!(Transform::from_exif(8), Some(from_irot));
    }

    #[test]
    fn every_exif_value_maps() {
        for v in 1..=8u16 {
            assert!(Transform::from_exif(v).is_some(), "orientation {v}");
        }
        assert_eq!(Transform::from_exif(9), None);
    }

    #[test]
    fn a_vertical_flip_is_a_horizontal_flip_turned_around() {
        assert_eq!(
            Transform::from_exif(4),
            Transform::from_exif(2).map(|t| Transform {
                rotate_cw: 180,
                ..t
            })
        );
    }
}
