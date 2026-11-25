//! Inputs shared by the shader build script and its regression test.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const INPUTS: &[&str] = &[
    "src",
    "build.rs",
    "fingerprint_inputs.rs",
    "Cargo.toml",
    "../../Cargo.toml",
    "../../Cargo.lock",
];

pub fn field(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn collect(path: PathBuf, files: &mut Vec<PathBuf>) -> io::Result<()> {
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            collect(entry?.path(), files)?;
        }
    } else {
        files.push(path);
    }
    Ok(())
}

pub fn source_inputs(dir: &Path) -> io::Result<(blake3::Hasher, BTreeSet<String>)> {
    let mut files = Vec::new();
    for input in INPUTS {
        collect(dir.join(input), &mut files)?;
    }
    files.sort();
    let mut hash = blake3::Hasher::new_derive_key("VIctory shader build inputs v1");
    let mut switches = BTreeSet::new();
    for path in files {
        let name = path
            .strip_prefix(dir)
            .expect("collected relative to the shader crate");
        let bytes = fs::read(&path)?;
        field(
            &mut hash,
            name.to_string_lossy().replace('\\', "/").as_bytes(),
        );
        field(&mut hash, &bytes);
        // Collect literal switch names, including names passed to helper functions.
        // Require the closing quote to omit prefixes and prose from the list.
        if name.starts_with("src") && path.extension().is_some_and(|s| s == "rs") {
            let source = std::str::from_utf8(&bytes).map_err(io::Error::other)?;
            for (at, _) in source.match_indices("\"VICTORY_") {
                if let Some((name, _)) = source[at + 1..].split_once('"')
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                {
                    switches.insert(name.to_owned());
                }
            }
        }
    }
    Ok((hash, switches))
}
