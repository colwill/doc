//! What a run made, as one file to download: a `.zip` or a `.tar.gz` of every file it rendered,
//! under one directory named after what was created. It is how a template is used with no git
//! host at all, and how anything a template published can be had again as it was made.

use std::io::Write;

use chrono::{DateTime, Datelike, Timelike, Utc};
use flate2::Compression;
use flate2::write::{DeflateEncoder, GzEncoder};

use crate::files::Rendered;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Zip,
    TarGz,
}

impl Format {
    /// `zip` or `tar.gz`, as a route or a query names it.
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "zip" => Some(Self::Zip),
            "tar.gz" | "tgz" => Some(Self::TarGz),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::TarGz => "tar.gz",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Zip => "application/zip",
            Self::TarGz => "application/gzip",
        }
    }
}

/// The directory everything goes under: what was created, as a file name may say it.
pub fn root(name: &str) -> String {
    let kept: String = name
        .chars()
        .map(|letter| match letter.is_ascii_alphanumeric() || matches!(letter, '-' | '_' | '.') {
            true => letter,
            false => '-',
        })
        .collect();
    match kept.trim_matches(['-', '.']) {
        "" => "files".to_string(),
        kept => kept.to_string(),
    }
}

/// The files, packed.
pub fn pack(format: Format, root: &str, files: &[Rendered], at: DateTime<Utc>) -> Vec<u8> {
    match format {
        Format::Zip => zip(root, files, at),
        Format::TarGz => tar_gz(root, files, at),
    }
}

/// A script stays one a person can run.
fn mode(path: &str) -> u32 {
    match path.ends_with(".sh") {
        true => 0o755,
        false => 0o644,
    }
}

fn tar_gz(root: &str, files: &[Rendered], at: DateTime<Utc>) -> Vec<u8> {
    let mut archive = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
    archive.mode(tar::HeaderMode::Deterministic);
    for file in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(file.bytes.len() as u64);
        header.set_mode(mode(&file.path));
        header.set_mtime(u64::try_from(at.timestamp()).unwrap_or_default());
        // `append_data` writes a long path as GNU's extension, where `set_path` would refuse it.
        let path = format!("{root}/{}", file.path);
        if let Err(err) = archive.append_data(&mut header, &path, file.bytes.as_slice()) {
            tracing::warn!(%err, path, "a file was left out of the archive");
        }
    }
    archive.into_inner().and_then(|gzip| gzip.finish()).unwrap_or_else(|err| {
        tracing::warn!(%err, "the archive could not be finished");
        Vec::new()
    })
}

/// A zip of deflated entries. A template makes at most 400 files and 4 MiB, so nothing here needs
/// Zip64; names are marked UTF-8, and each entry says it came from Unix with its permissions.
fn zip(root: &str, files: &[Rendered], at: DateTime<Utc>) -> Vec<u8> {
    let (time, date) = dos(at);
    let mut out = Vec::new();
    let mut central = Vec::new();
    let mut entries: u16 = 0;
    for file in files {
        let name = format!("{root}/{}", file.path);
        let mut crc = flate2::Crc::new();
        crc.update(&file.bytes);
        let mut deflater = DeflateEncoder::new(Vec::new(), Compression::default());
        if deflater.write_all(&file.bytes).is_err() {
            continue;
        }
        let Ok(packed) = deflater.finish() else { continue };
        let (Ok(size), Ok(packed_size), Ok(name_length), Ok(offset)) = (
            u32::try_from(file.bytes.len()),
            u32::try_from(packed.len()),
            u16::try_from(name.len()),
            u32::try_from(out.len()),
        ) else {
            continue;
        };
        // The fields both headers share, from `version needed` to the name's length.
        let mut shared = Vec::with_capacity(26);
        shared.extend_from_slice(&20u16.to_le_bytes()); // version needed: 2.0, for deflate
        shared.extend_from_slice(&0x0800u16.to_le_bytes()); // the name is UTF-8
        shared.extend_from_slice(&8u16.to_le_bytes()); // deflated
        shared.extend_from_slice(&time.to_le_bytes());
        shared.extend_from_slice(&date.to_le_bytes());
        shared.extend_from_slice(&crc.sum().to_le_bytes());
        shared.extend_from_slice(&packed_size.to_le_bytes());
        shared.extend_from_slice(&size.to_le_bytes());
        shared.extend_from_slice(&name_length.to_le_bytes());
        shared.extend_from_slice(&0u16.to_le_bytes()); // no extra field

        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&shared);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&packed);

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&((3u16 << 8) | 20).to_le_bytes()); // made on Unix, zip 2.0
        central.extend_from_slice(&shared);
        central.extend_from_slice(&0u16.to_le_bytes()); // no comment
        central.extend_from_slice(&0u16.to_le_bytes()); // disk 0
        central.extend_from_slice(&0u16.to_le_bytes()); // no internal attributes
        central.extend_from_slice(&((0o100_000 | mode(&file.path)) << 16).to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
        entries = entries.saturating_add(1);
    }
    let start = u32::try_from(out.len()).unwrap_or(u32::MAX);
    let length = u32::try_from(central.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // the disk the directory starts on
    out.extend_from_slice(&entries.to_le_bytes());
    out.extend_from_slice(&entries.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // no comment
    out
}

/// A time as MS-DOS kept it, which is what a zip's headers hold: to two seconds, from 1980.
fn dos(at: DateTime<Utc>) -> (u16, u16) {
    let year = u16::try_from(at.year().clamp(1980, 2107) - 1980).unwrap_or_default();
    let time = (at.hour() as u16) << 11 | (at.minute() as u16) << 5 | (at.second() as u16 / 2);
    let date = year << 9 | (at.month() as u16) << 5 | at.day() as u16;
    (time, date)
}
