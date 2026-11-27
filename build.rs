//! Embeds `assets/chatty.ico` as the executable's icon (resource id 1).
//!
//! Writes the compiled resource (.res) directly and hands it to the MSVC
//! linker, so the build needs neither rc.exe nor a crate for it.

use std::path::PathBuf;

fn main() {
    let ico_path = "assets/chatty.ico";
    println!("cargo:rerun-if-changed={ico_path}");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    let ico = std::fs::read(ico_path).expect("assets/chatty.ico");
    let res = icon_res(&ico);
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("chatty.res");
    std::fs::write(&out, res).unwrap();
    println!("cargo:rustc-link-arg-bins={}", out.display());
}

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const LANG_EN_US: u16 = 0x0409;

/// A .res holding each image of the .ico as an RT_ICON and the directory as
/// RT_GROUP_ICON 1.
fn icon_res(ico: &[u8]) -> Vec<u8> {
    let u16_at = |i: usize| u16::from_le_bytes([ico[i], ico[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([ico[i], ico[i + 1], ico[i + 2], ico[i + 3]]);
    assert!(u16_at(0) == 0 && u16_at(2) == 1, "not an .ico");
    let count = u16_at(4) as usize;

    // An empty first entry marks a 32-bit .res file.
    let mut res = Vec::new();
    entry(&mut res, 0, 0, &[], 0);
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&(count as u16).to_le_bytes());
    for n in 0..count {
        let d = 6 + n * 16;
        let (size, offset) = (u32_at(d + 8) as usize, u32_at(d + 12) as usize);
        let id = n as u16 + 1;
        entry(&mut res, RT_ICON, id, &ico[offset..offset + size], 0x1010);
        // GRPICONDIRENTRY: the ICONDIRENTRY's first 12 bytes, then the id.
        group.extend_from_slice(&ico[d..d + 12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(&mut res, RT_GROUP_ICON, 1, &group, 0x1030);
    res
}

fn entry(res: &mut Vec<u8>, kind: u16, id: u16, data: &[u8], flags: u16) {
    let header_size: u32 = 32;
    res.extend_from_slice(&(data.len() as u32).to_le_bytes());
    res.extend_from_slice(&header_size.to_le_bytes());
    for v in [0xFFFF, kind, 0xFFFF, id] {
        res.extend_from_slice(&v.to_le_bytes());
    }
    res.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    res.extend_from_slice(&flags.to_le_bytes()); // MemoryFlags
    res.extend_from_slice(&(if kind == 0 { 0 } else { LANG_EN_US }).to_le_bytes());
    res.extend_from_slice(&0u32.to_le_bytes()); // Version
    res.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    res.extend_from_slice(data);
    while res.len() % 4 != 0 {
        res.push(0);
    }
}
