//! Bounded, link-free archive extraction.
//!
//! Supported formats are ZIP and (optionally gzip-compressed) TAR. Extraction
//! refuses absolute entries, parent traversal, symbolic links, hard links,
//! device nodes and FIFOs, bounds the entry count and the total uncompressed
//! size, and writes every entry through the capability root, so an archive can
//! never touch anything outside its destination directory.

use std::io::{Read, Seek, SeekFrom};

use crate::safe::SafeRoot;
use workspace_core::helper::{self, HelperError};

/// What an archive extraction produced.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Extracted {
    pub entries: u64,
    pub bytes: u64,
}

/// Read at most `cap` bytes of one archive entry.
fn read_entry(reader: &mut impl Read, cap: u64) -> Result<Vec<u8>, HelperError> {
    let mut content = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;

    loop {
        let read = reader.read(&mut buffer).map_err(crate::safe::map_io)?;

        if read == 0 {
            break;
        }

        total += read as u64;

        if total > cap {
            return Err(HelperError::new(
                helper::code::LIMIT,
                "an archive entry exceeds the allowed size",
            ));
        }

        content.extend_from_slice(&buffer[..read]);
    }

    Ok(content)
}

/// Normalise an archive entry name into a safe relative path.
fn entry_path(name: &str) -> Result<String, HelperError> {
    if name.is_empty() || name.contains('\\') {
        return Err(HelperError::new(
            helper::code::ARCHIVE,
            "archive entry has an invalid name",
        ));
    }

    helper::validate_relative_path(name)?;

    let trimmed = name.trim_start_matches("./");

    if trimmed.is_empty() || trimmed == "." {
        return Ok(String::new());
    }

    Ok(trimmed.trim_end_matches('/').to_owned())
}

fn join(destination: &str, name: &str) -> String {
    if destination.is_empty() {
        name.to_owned()
    } else if name.is_empty() {
        destination.to_owned()
    } else {
        format!("{destination}/{name}")
    }
}

/// Extract `archive` into `destination` inside `root`.
pub fn extract(
    root: &SafeRoot,
    destination: &str,
    archive: &mut (impl Read + Seek),
    max_entries: u32,
    max_bytes: u64,
) -> Result<Extracted, HelperError> {
    helper::validate_relative_path(destination)?;

    let mut magic = [0u8; 4];

    let read = archive.read(&mut magic).map_err(crate::safe::map_io)?;

    archive
        .seek(SeekFrom::Start(0))
        .map_err(crate::safe::map_io)?;

    if read == 4 && magic == [0x50, 0x4b, 0x03, 0x04] {
        extract_zip(root, destination, archive, max_entries, max_bytes)
    } else if read >= 2 && magic[0] == 0x1f && magic[1] == 0x8b {
        extract_tar(
            root,
            destination,
            flate2::read::GzDecoder::new(archive),
            max_entries,
            max_bytes,
        )
    } else {
        extract_tar(root, destination, archive, max_entries, max_bytes)
    }
}

fn extract_zip(
    root: &SafeRoot,
    destination: &str,
    archive: &mut (impl Read + Seek),
    max_entries: u32,
    max_bytes: u64,
) -> Result<Extracted, HelperError> {
    let mut archive = zip::ZipArchive::new(archive).map_err(|error| {
        HelperError::new(
            helper::code::ARCHIVE,
            format!("invalid zip archive: {error}"),
        )
    })?;

    if archive.len() > max_entries as usize {
        return Err(HelperError::new(
            helper::code::LIMIT,
            "the archive contains too many entries",
        ));
    }

    let mut extracted = Extracted {
        entries: 0,
        bytes: 0,
    };

    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|error| {
            HelperError::new(helper::code::ARCHIVE, format!("invalid zip entry: {error}"))
        })?;

        let name = entry.name().to_owned();
        let path = entry_path(&name)?;

        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;

            // Only regular files and directories are accepted.
            if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                return Err(HelperError::new(
                    helper::code::ARCHIVE,
                    "the archive contains links or special files",
                ));
            }
        }

        let target = join(destination, &path);

        if entry.is_dir() {
            if !path.is_empty() {
                root.mkdir(&target, true)?;
            }

            continue;
        }

        if path.is_empty() {
            continue;
        }

        let remaining = max_bytes.saturating_sub(extracted.bytes);

        if remaining == 0 {
            return Err(HelperError::new(
                helper::code::LIMIT,
                "the archive exceeds the allowed size",
            ));
        }

        let content = read_entry(&mut entry, remaining)?;

        extracted.bytes += content.len() as u64;
        extracted.entries += 1;

        root.write_unsynced(&target, &content, true, true)?;

        if entry.unix_mode().is_some_and(|mode| mode & 0o111 != 0) {
            root.set_executable(&target)?;
        }
    }

    Ok(extracted)
}

fn extract_tar(
    root: &SafeRoot,
    destination: &str,
    reader: impl Read,
    max_entries: u32,
    max_bytes: u64,
) -> Result<Extracted, HelperError> {
    let mut archive = tar::Archive::new(reader);

    let mut extracted = Extracted {
        entries: 0,
        bytes: 0,
    };

    let entries = archive.entries().map_err(|error| {
        HelperError::new(
            helper::code::ARCHIVE,
            format!("invalid tar archive: {error}"),
        )
    })?;

    for entry in entries {
        let mut entry = entry.map_err(|error| {
            HelperError::new(helper::code::ARCHIVE, format!("invalid tar entry: {error}"))
        })?;

        if extracted.entries >= u64::from(max_entries) {
            return Err(HelperError::new(
                helper::code::LIMIT,
                "the archive contains too many entries",
            ));
        }

        let kind = entry.header().entry_type();

        if !kind.is_file() && !kind.is_dir() {
            return Err(HelperError::new(
                helper::code::ARCHIVE,
                "the archive contains links or special files",
            ));
        }

        let name = entry
            .path()
            .map_err(|error| {
                HelperError::new(helper::code::ARCHIVE, format!("invalid tar path: {error}"))
            })?
            .to_string_lossy()
            .into_owned();

        let path = entry_path(&name)?;
        let target = join(destination, &path);

        if kind.is_dir() {
            if !path.is_empty() {
                root.mkdir(&target, true)?;
            }

            continue;
        }

        if path.is_empty() {
            continue;
        }

        let remaining = max_bytes.saturating_sub(extracted.bytes);

        if remaining == 0 {
            return Err(HelperError::new(
                helper::code::LIMIT,
                "the archive exceeds the allowed size",
            ));
        }

        let content = read_entry(&mut entry, remaining)?;

        extracted.bytes += content.len() as u64;
        extracted.entries += 1;

        root.write_unsynced(&target, &content, true, true)?;

        if entry.header().mode().is_ok_and(|mode| mode & 0o111 != 0) {
            root.set_executable(&target)?;
        }
    }

    Ok(extracted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> (tempfile::TempDir, SafeRoot) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_string_lossy().to_string();
        let root = SafeRoot::open(std::slice::from_ref(&path), &path).unwrap();

        (directory, root)
    }

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buffer = std::io::Cursor::new(Vec::new());

        {
            let mut writer = zip::ZipWriter::new(&mut buffer);

            let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default();

            for (name, content) in entries {
                use std::io::Write;

                writer.start_file(*name, options).unwrap();
                writer.write_all(content).unwrap();
            }

            writer.finish().unwrap();
        }

        buffer.into_inner()
    }

    #[test]
    fn extracts_a_zip_archive() {
        let (_directory, root) = root();

        let mut archive = std::io::Cursor::new(zip_with(&[
            ("project/src/main.rs", b"fn main() {}"),
            ("project/README.md", b"readme"),
        ]));

        let extracted = extract(&root, "", &mut archive, 100, 1024 * 1024).unwrap();

        assert_eq!(extracted.entries, 2);

        let (mut file, _) = root.open_regular("project/src/main.rs").unwrap();
        let mut content = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut content).unwrap();

        assert_eq!(content, b"fn main() {}");
    }

    #[test]
    fn rejects_traversal_and_absolute_entries() {
        let (_directory, root) = root();

        for name in ["../escape.txt", "/etc/passwd"] {
            let mut archive = std::io::Cursor::new(zip_with(&[(name, b"x")]));

            assert!(
                extract(&root, "", &mut archive, 100, 1024).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn enforces_the_size_limit() {
        let (_directory, root) = root();

        let mut archive = std::io::Cursor::new(zip_with(&[("big.bin", &vec![b'x'; 4096])]));

        let error = extract(&root, "", &mut archive, 100, 1024).unwrap_err();

        assert_eq!(error.code, helper::code::LIMIT);
    }

    #[test]
    fn enforces_the_entry_limit() {
        let (_directory, root) = root();

        let mut archive = std::io::Cursor::new(zip_with(&[("a", b"1"), ("b", b"2"), ("c", b"3")]));

        let error = extract(&root, "", &mut archive, 2, 1024).unwrap_err();

        assert_eq!(error.code, helper::code::LIMIT);
    }

    #[test]
    fn malformed_archives_are_rejected_without_panicking() {
        let (_directory, root) = root();

        // Deterministic pseudo-random inputs: a crash or a panic here would be
        // a remotely reachable denial of service, so "returns an error" is the
        // only acceptable outcome.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;

        for length in [0usize, 1, 2, 4, 8, 16, 64, 512, 4096] {
            let mut data = vec![0u8; length];

            for byte in &mut data {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }

            for name in ["evidence.zip", "payload.tar", "unknown.bin"] {
                let mut archive = std::io::Cursor::new(data.clone());
                let result = extract(&root, "", &mut archive, 16, 4096);

                assert!(
                    result.is_err(),
                    "{name} with {length} bytes must be refused"
                );
            }
        }
    }

    #[test]
    fn extracts_a_plain_tar_archive() {
        let (_directory, root) = root();

        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(3);
        header.set_mode(0o644);
        header.set_cksum();

        builder
            .append_data(&mut header, "dir/file.txt", &b"abc"[..])
            .unwrap();

        let data = builder.into_inner().unwrap();
        let mut archive = std::io::Cursor::new(data);

        let extracted = extract(&root, "", &mut archive, 10, 1024).unwrap();

        assert_eq!(extracted.entries, 1);
        assert!(root.stat("dir/file.txt").is_ok());
    }
}
