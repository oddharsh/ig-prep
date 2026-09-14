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
    let end = off.checked_add(usize::try_from(size).ok()?)?;
    if end > d.len() {
        return None;
    }
    Some(Bx {
        kind,
        body: d.get(off + head..end)?,
        next: end,
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

/// Resolve property associations once for orientation and colour checks.
fn primary_properties(file: &[u8]) -> Option<Vec<Bx<'_>>> {
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

    let mut props = Vec::new();
    let mut off = 0;
    while off < ipco.body.len() {
        let b = box_at(ipco.body, off)?;
        if b.next <= off {
            return None;
        }
        off = b.next;
        props.push(b);
    }
    let ipma = find(iprp.body, b"ipma")?;
    let version = *ipma.body.first()?;
    let flags = u32::from_be_bytes(ipma.body.get(..4)?.try_into().ok()?) & 0x00ff_ffff;
    let wide_index = flags & 1 == 1;
    let count = u32::from_be_bytes(ipma.body.get(4..8)?.try_into().ok()?);
    let mut p = 8usize;

    let mut found = Vec::new();
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
            if item == primary && idx != 0 {
                let b = props.get(idx - 1)?;
                found.push(Bx {
                    kind: b.kind,
                    body: b.body,
                    next: b.next,
                });
            }
        }
    }
    Some(found)
}

pub fn container_transform(file: &[u8]) -> Option<Transform> {
    let mut found: Option<Transform> = None;
    for b in primary_properties(file)? {
        let t = match (&b.kind, b.body.first()) {
            (b"irot", Some(v)) => Transform {
                rotate_cw: (360 - (v & 3) as u16 * 90) % 360,
                mirrored: false,
            },
            (b"imir", Some(v)) => Transform {
                rotate_cw: if v & 1 == 1 { 180 } else { 0 },
                mirrored: true,
            },
            _ => continue,
        };
        found = Some(match found {
            None => t,
            Some(a) => Transform {
                rotate_cw: (a.rotate_cw + t.rotate_cw) % 360,
                mirrored: a.mirrored ^ t.mirrored,
            },
        });
    }
    found
}

/// Refuse known HDR transfers before the platform decoder can silently map them.
/// Inspect only properties associated with the primary image, as for orientation.
pub fn check_sdr(file: &[u8]) -> Result<(), String> {
    if let Some(props) = primary_properties(file) {
        for b in props {
            if b.kind == *b"colr" && b.body.starts_with(b"nclx") {
                let transfer = b
                    .body
                    .get(6..8)
                    .ok_or("HEIF: truncated nclx colour description")?;
                if matches!(u16::from_be_bytes(transfer.try_into().unwrap()), 16 | 18) {
                    return Err("HDR HEIF (PQ/HLG) requires a reviewed SDR tone-mapped export before conversion".into());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exif_and_irot_describe_the_same_turn() {
        // A Fujifilm HIF writes irot angle 1, which is 90 counter-clockwise,
        // and EXIF 8, which is 270 clockwise. They are the same rotation, and
        // a check that called them different would flag every file.
        // irot angle 1 is 90 counter-clockwise, which is 270 clockwise.
        let from_irot = Transform {
            rotate_cw: 270,
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

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut bytes = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        bytes.extend(kind);
        bytes.extend(body);
        bytes
    }
    fn colour_file(transfer: u16, primary: u16) -> Vec<u8> {
        let mut colr = b"nclx".to_vec();
        colr.extend(9u16.to_be_bytes());
        colr.extend(transfer.to_be_bytes());
        colr.extend(9u16.to_be_bytes());
        colr.push(0x80);
        let mut properties = boxed(b"colr", &colr);
        properties.extend(boxed(b"irot", &[1]));
        let mut iprp = boxed(b"ipco", &properties);
        // Item 1 owns colour and rotation. Item 2 (thumbnail) owns neither.
        iprp.extend(boxed(
            b"ipma",
            &[0, 0, 0, 0, 0, 0, 0, 2, 0, 1, 2, 1, 2, 0, 2, 0],
        ));
        let mut pitm = vec![0, 0, 0, 0];
        pitm.extend(primary.to_be_bytes());
        let mut meta = vec![0, 0, 0, 0];
        meta.extend(boxed(b"pitm", &pitm));
        meta.extend(boxed(b"iprp", &iprp));
        boxed(b"meta", &meta)
    }
    #[test]
    fn hdr_check_uses_the_primary_image_colour_properties() {
        for transfer in [16, 18] {
            let bytes = colour_file(transfer, 1);
            assert!(check_sdr(&bytes).is_err());
            assert_eq!(container_transform(&bytes), Transform::from_exif(8));
            assert!(
                check_sdr(&colour_file(transfer, 2)).is_ok(),
                "thumbnail HDR must not reject an SDR primary"
            );
        }
        assert!(check_sdr(&colour_file(13, 1)).is_ok());
    }
    #[test]
    fn truncated_properties_do_not_panic() {
        let bytes = colour_file(13, 1);
        for end in 0..bytes.len() {
            assert!(primary_properties(&bytes[..end]).is_none());
        }
    }
}
