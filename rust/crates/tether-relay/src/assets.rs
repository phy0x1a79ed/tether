//! Serving the launcher script and the client binaries.
//!
//! The relay hosts these so the whole tool lives behind one mount and one
//! upstream. That is not filing convenience: the owner's very first contact
//! with tether is a piped command, and every extra host in that line is another
//! name they have to trust and another thing that can be down when they need
//! help.
//!
//! The public host has no toolchain and builds nothing. These files arrive as
//! artifacts from a build box, into a directory outside the checkout — a deploy
//! cleans untracked files, and a multi-megabyte binary inside the tree would
//! not survive one.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The most a single asset may be, so a misconfigured directory cannot be
/// turned into a memory exhaustion by asking for whatever is in it.
const MAX_ASSET: u64 = 64 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum AssetError {
    /// No such asset, this host has no asset directory, or the name was not one
    /// this service would ever serve. One answer for all three: a caller
    /// probing the filesystem learns the same thing as a caller with a typo.
    NotFound,
    TooLarge,
}

/// Whether a path segment is a name this service will look up.
///
/// An allow-list rather than a search for `..`, because the interesting
/// traversals are the encodings nobody thinks of. A name is lowercase-ish
/// ASCII, dots and dashes, and must not begin with a dot.
fn nameable(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 64
        && !segment.starts_with('.')
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

/// Read one asset, named by path segments relative to the asset root.
pub fn read(root: Option<&Path>, segments: &[&str]) -> Result<Vec<u8>, AssetError> {
    let root = root.ok_or(AssetError::NotFound)?;
    if segments.is_empty() || !segments.iter().all(|s| nameable(s)) {
        return Err(AssetError::NotFound);
    }
    let mut path = PathBuf::from(root);
    for segment in segments {
        path.push(segment);
    }
    let meta = fs::metadata(&path).map_err(|_| AssetError::NotFound)?;
    if !meta.is_file() {
        return Err(AssetError::NotFound);
    }
    if meta.len() > MAX_ASSET {
        return Err(AssetError::TooLarge);
    }
    fs::read(&path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => AssetError::NotFound,
        _ => AssetError::NotFound,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tether-assets-{}", std::process::id()));
        let _ = fs::create_dir_all(dir.join("bin"));
        fs::write(dir.join("tether"), b"#!/bin/sh\n").unwrap();
        fs::write(dir.join("bin").join("tether-linux-x86_64"), b"ELF").unwrap();
        fs::write(dir.parent().unwrap().join("tether-secret"), b"no").unwrap();
        dir
    }

    #[test]
    fn a_named_asset_is_read() {
        let dir = scratch();
        assert_eq!(read(Some(&dir), &["tether"]).unwrap(), b"#!/bin/sh\n");
        assert_eq!(
            read(Some(&dir), &["bin", "tether-linux-x86_64"]).unwrap(),
            b"ELF"
        );
    }

    #[test]
    fn a_host_with_no_assets_says_there_is_nothing_here() {
        assert_eq!(read(None, &["tether"]), Err(AssetError::NotFound));
    }

    #[test]
    fn traversal_is_refused_by_the_name_rule_before_the_filesystem_sees_it() {
        let dir = scratch();
        for name in ["..", ".", "../tether-secret", "", ".hidden", "a/b", "a\\b"] {
            assert_eq!(
                read(Some(&dir), &[name]),
                Err(AssetError::NotFound),
                "{name}"
            );
        }
        assert_eq!(
            read(Some(&dir), &["..", "tether-secret"]),
            Err(AssetError::NotFound)
        );
    }

    #[test]
    fn a_directory_is_not_an_asset() {
        let dir = scratch();
        assert_eq!(read(Some(&dir), &["bin"]), Err(AssetError::NotFound));
    }

    #[test]
    fn an_absent_file_and_a_refused_name_answer_the_same_way() {
        let dir = scratch();
        assert_eq!(read(Some(&dir), &["nope"]), Err(AssetError::NotFound));
        assert_eq!(read(Some(&dir), &["no pe"]), Err(AssetError::NotFound));
    }
}
