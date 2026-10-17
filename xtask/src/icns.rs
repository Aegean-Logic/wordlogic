//! `cargo xtask icns <out.icns> <in.png>…`: pack PNG images into a macOS `.icns`.
//!
//! macOS 10.7 and later read PNG payloads for every modern icon type, so each PNG is stored as is.
//! The type code comes from the PNG's pixel size; a 32, 64, 256, 512 or 1024 px image is written
//! under both its 1x and its @2x slot, the way `iconutil` does. This lets `packaging/icons.sh`
//! write the `.icns` on any platform, not only where `iconutil` exists.

use crate::ico::png_size;
use std::path::Path;

/// The `.icns` type codes that hold a PNG of `size` px: its 1x slot, then its @2x slot.
fn types(size: u32) -> Result<&'static [&'static [u8; 4]], String> {
    Ok(match size {
        16 => &[b"icp4"],
        32 => &[b"icp5", b"ic11"],
        64 => &[b"icp6", b"ic12"],
        128 => &[b"ic07"],
        256 => &[b"ic08", b"ic13"],
        512 => &[b"ic09", b"ic14"],
        1024 => &[b"ic10"],
        _ => return Err(format!("icns images must be 16, 32, 64, 128, 256, 512 or 1024 px, got {size}")),
    })
}

/// An ICNS file holding `pngs`, which must be square and of a size `types` accepts.
pub fn pack(pngs: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    if pngs.is_empty() {
        return Err("need at least one PNG".into());
    }
    let mut body = Vec::new();
    for png in pngs {
        let (w, h) = png_size(png)?;
        if w != h {
            return Err(format!("icon images must be square, got {w}x{h}"));
        }
        for code in types(w)? {
            body.extend_from_slice(*code);
            body.extend_from_slice(&((8 + png.len()) as u32).to_be_bytes());
            body.extend_from_slice(png);
        }
    }
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

pub fn run(args: &[&str]) -> Result<(), String> {
    let [out, inputs @ ..] = args else {
        return Err("usage: cargo xtask icns <out.icns> <in.png>…".into());
    };
    let pngs = inputs.iter().map(|p| std::fs::read(p).map_err(|e| format!("read {p}: {e}"))).collect::<Result<Vec<_>, _>>()?;
    let icns = pack(&pngs)?;
    std::fs::write(Path::new(out), &icns).map_err(|e| format!("write {out}: {e}"))?;
    println!("{out}: {} images, {} bytes", pngs.len(), icns.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_png(size: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        v.extend_from_slice(&size.to_be_bytes());
        v.extend_from_slice(&size.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0, 1, 2, 3, 4]);
        v
    }

    #[test]
    fn packs_header_and_entries() {
        let a = fake_png(16);
        let b = fake_png(32);
        let icns = pack(&[a.clone(), b.clone()]).unwrap();
        let entry = |len: usize| 8 + len;
        let total = 8 + entry(a.len()) + 2 * entry(b.len());
        assert_eq!(&icns[..4], b"icns");
        assert_eq!(u32::from_be_bytes(icns[4..8].try_into().unwrap()) as usize, total);
        assert_eq!(icns.len(), total);
        // 16 px: one icp4 entry.
        assert_eq!(&icns[8..12], b"icp4");
        assert_eq!(u32::from_be_bytes(icns[12..16].try_into().unwrap()) as usize, entry(a.len()));
        assert_eq!(&icns[16..16 + a.len()], &a[..]);
        // 32 px: icp5 (32x32) then ic11 (16x16@2x).
        let at = 16 + a.len();
        assert_eq!(&icns[at..at + 4], b"icp5");
        let at2 = at + entry(b.len());
        assert_eq!(&icns[at2..at2 + 4], b"ic11");
    }

    #[test]
    fn rejects_bad_input() {
        assert!(pack(&[]).is_err());
        assert!(pack(&[fake_png(48)]).is_err());
        assert!(pack(&[b"GIF89a".to_vec()]).is_err());
    }

    #[test]
    fn committed_icon_is_valid() {
        let path = crate::root().join("assets/app-icon/wordcraft.icns");
        let Ok(icns) = std::fs::read(&path) else { return };
        assert_eq!(&icns[..4], b"icns");
        assert_eq!(u32::from_be_bytes([icns[4], icns[5], icns[6], icns[7]]) as usize, icns.len());
    }
}
