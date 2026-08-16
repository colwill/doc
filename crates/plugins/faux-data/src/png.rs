//! A small bar chart as a PNG, written by hand so faux pages have real images to attach: a white
//! plate, a bar for each value, the last bar darker. Truecolour, unfiltered rows, zlib-compressed.

use std::io::Write;

use flate2::Compression;
use flate2::write::ZlibEncoder;

const WIDTH: u32 = 280;
const HEIGHT: u32 = 100;
const GAP: u32 = 6;

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = match crc & 1 {
                1 => (crc >> 1) ^ 0xedb8_8320,
                _ => crc >> 1,
            };
        }
    }
    !crc
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let crc = crc32(&png[start..]);
    png.extend_from_slice(&crc.to_be_bytes());
}

/// `values` from 0 to 1, one bar each, in `ink`.
pub fn bars(values: &[f64], ink: (u8, u8, u8)) -> Vec<u8> {
    let count = values.len().max(1) as u32;
    let width = (WIDTH - GAP * (count + 1)) / count;
    let mut rows = Vec::with_capacity(((WIDTH * 3 + 1) * HEIGHT) as usize);
    for y in 0..HEIGHT {
        rows.push(0);
        for x in 0..WIDTH {
            let bar = (x >= GAP)
                .then(|| (x - GAP) / (width + GAP))
                .filter(|bar| *bar < count && (x - GAP) % (width + GAP) < width);
            let filled = bar.is_some_and(|bar| {
                let value = values[bar as usize].clamp(0.02, 1.0);
                f64::from(HEIGHT - y) <= value * f64::from(HEIGHT - GAP)
            });
            let last = bar == Some(count - 1);
            let (r, g, b) = match (filled, last) {
                (false, _) => (255, 255, 255),
                (true, false) => ink,
                (true, true) => (ink.0 / 2, ink.1 / 2, ink.2 / 2),
            };
            rows.extend_from_slice(&[r, g, b]);
        }
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    let compressed = encoder.write_all(&rows).and_then(|()| encoder.finish()).unwrap_or_default();
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&WIDTH.to_be_bytes());
    header.extend_from_slice(&HEIGHT.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &compressed);
    chunk(&mut png, b"IEND", &[]);
    png
}
