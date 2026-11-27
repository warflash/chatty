//! 16-bit mono WAV writer for the history audio.

use std::io::Write;

pub fn write(path: &std::path::Path, pcm: &[f32], rate: u32) -> std::io::Result<()> {
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::File::create(path)?.write_all(&out)
}

/// Reads a 16-bit mono WAV written by `write` (used by the bench mode).
pub fn read(path: &std::path::Path) -> std::io::Result<(Vec<f32>, u32)> {
    let b = std::fs::read(path)?;
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "unsupported wav");
    if b.len() < 44 || &b[0..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(bad());
    }
    let mut i = 12;
    let (mut rate, mut channels, mut bits) = (0u32, 0u16, 0u16);
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let len = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        let body = &b[i + 8..(i + 8 + len).min(b.len())];
        if id == b"fmt " && body.len() >= 16 {
            channels = u16::from_le_bytes(body[2..4].try_into().unwrap());
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes(body[14..16].try_into().unwrap());
        } else if id == b"data" {
            if bits != 16 || channels == 0 {
                return Err(bad());
            }
            let samples: Vec<f32> = body
                .chunks_exact(2 * channels as usize)
                .map(|f| i16::from_le_bytes([f[0], f[1]]) as f32 / 32768.0)
                .collect();
            return Ok((samples, rate));
        }
        i += 8 + len + (len & 1);
    }
    Err(bad())
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let p = std::env::temp_dir().join(format!("chatty-wav-{}.wav", std::process::id()));
        super::write(&p, &[0.0, 0.5, -0.5], 16000).unwrap();
        let (s, r) = super::read(&p).unwrap();
        assert_eq!(r, 16000);
        assert_eq!(s.len(), 3);
        assert!((s[1] - 0.5).abs() < 1e-3);
        std::fs::remove_file(p).ok();
    }
}
