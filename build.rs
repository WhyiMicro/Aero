//! Embeds the Aero icon into aero.exe, so Explorer, the Start Menu and the taskbar show it.

use std::path::PathBuf;

include!("src/icon.rs");

/// Builds a multi-size .ico (32-bit BMP entries, alpha channel used, empty AND mask).
fn ico(sizes: &[usize]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&s| {
            let rgba = icon_rgba(s, true);
            let mask_row = s.div_ceil(32) * 4;
            let mut img = Vec::new();
            for v in [40u32, s as u32, (s * 2) as u32] {
                img.extend(v.to_le_bytes()); // biSize, biWidth, biHeight (colour + mask)
            }
            img.extend(1u16.to_le_bytes()); // planes
            img.extend(32u16.to_le_bytes()); // bit count
            img.extend([0u8; 24]); // compression, sizes, resolution, palette: all 0
            for y in (0..s).rev() {
                // bottom-up BGRA
                for x in 0..s {
                    let i = (y * s + x) * 4;
                    img.extend([rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
                }
            }
            img.extend(vec![0u8; mask_row * s]);
            img
        })
        .collect();

    let mut out = Vec::new();
    out.extend([0u8, 0, 1, 0]); // reserved, type = icon
    out.extend((sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len();
    for (&s, img) in sizes.iter().zip(&images) {
        let dim = if s >= 256 { 0 } else { s as u8 };
        out.extend([dim, dim, 0, 0]);
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend((img.len() as u32).to_le_bytes());
        out.extend((offset as u32).to_le_bytes());
        offset += img.len();
    }
    for img in images {
        out.extend(img);
    }
    out
}

fn main() {
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico_path = out.join("aero.ico");
    std::fs::write(&ico_path, ico(&[16, 20, 24, 32, 40, 48, 64, 256])).unwrap();
    let rc_path = out.join("aero.rc");
    let ico_str = ico_path.display().to_string().replace('\\', "/");
    std::fs::write(&rc_path, format!("1 ICON \"{ico_str}\"\n")).unwrap();
    embed_resource::compile(&rc_path, embed_resource::NONE).manifest_optional().unwrap();
}
