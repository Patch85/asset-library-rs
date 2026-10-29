//! Reading Dungeondraft asset packs (`.dungeondraft_pack`) in place.
//!
//! A pack is a Godot 3 resource pack (PCK v1): a `GDPC` header, a table of
//! `res://packs/<id>/...` paths with offsets and sizes, then the files. Images are stored as
//! plain PNG/WebP under `textures/<kind>/`; `pack.json` names the pack and
//! `data/default.dungeondraft_tags` lists Dungeondraft's tags per image.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use serde_json::Value;

pub const EXTENSION: &str = "dungeondraft_pack";

pub struct Entry {
    /// Path inside the pack, relative to `res://packs/<id>/` (e.g. `textures/objects/x.png`).
    pub path: String,
    pub offset: u64,
    pub size: u64,
}

pub struct Pack {
    pub name: String,
    pub author: String,
    pub version: String,
    pub entries: Vec<Entry>,
    /// Image path (as in `entries`) to its Dungeondraft tags, already cleaned up.
    pub tags: HashMap<String, Vec<String>>,
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn u32le(r: &mut impl Read) -> io::Result<u32> {
    let mut b = [0; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

fn u64le(r: &mut impl Read) -> io::Result<u64> {
    let mut b = [0; 8];
    r.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

/// The raw file table: (full `res://` path, offset, size).
fn file_table(path: &Path) -> io::Result<Vec<(String, u64, u64)>> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut r = BufReader::new(file);
    let mut magic = [0; 4];
    r.read_exact(&mut magic)?;
    if &magic != b"GDPC" {
        return Err(invalid("not a Dungeondraft pack (no GDPC header)"));
    }
    let format = u32le(&mut r)?;
    if format != 1 {
        return Err(invalid(format!("unsupported pack format {format} (expected Godot 3, format 1)")));
    }
    r.seek(SeekFrom::Current(12 + 64))?; // engine version, reserved
    let count = u32le(&mut r)?;
    let mut out = Vec::with_capacity(count.min(1 << 20) as usize);
    for _ in 0..count {
        let n = u32le(&mut r)? as usize;
        if n > 4096 {
            return Err(invalid("corrupt pack file table"));
        }
        let mut name = vec![0; n];
        r.read_exact(&mut name)?;
        let name = String::from_utf8_lossy(&name).trim_end_matches('\0').to_string();
        let (offset, size) = (u64le(&mut r)?, u64le(&mut r)?);
        r.seek(SeekFrom::Current(16))?; // md5
        if offset.checked_add(size).is_none_or(|end| end > len) {
            return Err(invalid(format!("entry {name} runs past the end of the pack")));
        }
        out.push((name, offset, size));
    }
    Ok(out)
}

pub fn read_entry(pack: &Path, offset: u64, size: u64) -> io::Result<Vec<u8>> {
    let mut f = File::open(pack)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0; size as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Parse JSON that may have trailing commas (some packs' tag files do).
pub fn lenient_json(text: &str) -> serde_json::Result<Value> {
    let mut out = String::with_capacity(text.len());
    let (mut in_str, mut escaped) = (false, false);
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if in_str {
            out.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        if c == '"' {
            in_str = true;
        } else if c == ',' {
            let next = chars[i + 1..].iter().find(|c| !c.is_whitespace());
            if matches!(next, Some(']' | '}')) {
                continue;
            }
        }
        out.push(c);
    }
    serde_json::from_str(&out)
}

/// A Dungeondraft tag as a library tag: leading `.` and `[XX] ` prefixes dropped; tags that
/// only matter inside Dungeondraft are skipped.
pub fn clean_tag(raw: &str) -> Option<String> {
    let mut t = raw.trim().trim_start_matches('.').trim();
    if t.starts_with('[')
        && let Some(end) = t.find(']')
    {
        t = t[end + 1..].trim();
    }
    let t = crate::db::normalize_tag(t);
    (!t.is_empty() && !["colorable", "testing"].contains(&t.as_str())).then_some(t)
}

pub fn open(path: &Path) -> io::Result<Pack> {
    let table = file_table(path)?;
    let prefix = table
        .iter()
        .find_map(|(n, _, _)| {
            let rest = n.strip_prefix("res://packs/")?;
            let (id, _) = rest.split_once('/')?;
            Some(format!("res://packs/{id}/"))
        })
        .ok_or_else(|| invalid("no res://packs/<id>/ files in the pack"))?;
    let entries: Vec<Entry> = table
        .into_iter()
        .filter_map(|(n, offset, size)| Some(Entry { path: n.strip_prefix(&prefix)?.to_string(), offset, size }))
        .collect();
    let json = |name: &str| -> Option<Value> {
        let e = entries.iter().find(|e| e.path == name)?;
        let bytes = read_entry(path, e.offset, e.size).ok()?;
        lenient_json(&String::from_utf8_lossy(&bytes)).ok()
    };
    let meta = json("pack.json").unwrap_or_default();
    let text = |k: &str| meta.get(k).and_then(Value::as_str).unwrap_or_default().trim().to_string();
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut tags: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(Value::Object(by_tag)) = json("data/default.dungeondraft_tags").and_then(|v| v.get("tags").cloned()) {
        for (tag, paths) in by_tag {
            let Some(tag) = clean_tag(&tag) else { continue };
            for p in paths.as_array().into_iter().flatten().filter_map(Value::as_str) {
                let list = tags.entry(p.to_string()).or_default();
                if !list.contains(&tag) {
                    list.push(tag.clone());
                }
            }
        }
    }
    Ok(Pack {
        name: Some(text("name")).filter(|n| !n.is_empty()).unwrap_or(stem),
        author: text("author"),
        version: text("version"),
        entries,
        tags,
    })
}

/// For an image inside `textures/`: (Dungeondraft kind, library asset type, folders below the
/// kind folder). `None` for anything else.
pub fn classify(inner: &str) -> Option<(&str, &'static str, Vec<&str>)> {
    let rest = inner.strip_prefix("textures/")?;
    let ext = Path::new(rest).extension()?.to_str()?.to_lowercase();
    if !crate::config::IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    let mut parts: Vec<&str> = rest.split('/').collect();
    parts.pop(); // file name
    let kind = *parts.first()?;
    let asset_type = match kind {
        "objects" => "prop",
        "terrain" | "patterns" | "materials" => "texture",
        "tilesets" => "tile",
        _ => "other",
    };
    Some((kind, asset_type, parts[1..].to_vec()))
}

/// Singular kind name for the Kind property: `walls` -> `wall`, `terrain` stays.
pub fn kind_value(kind: &str) -> String {
    kind.strip_suffix('s').filter(|_| kind != "terrain").unwrap_or(kind).to_string()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Write a minimal Godot 3 PCK with the given `res://` files (for tests).
    pub fn write_pack(path: &Path, files: &[(&str, &[u8])]) {
        let header_len = 4 + 16 + 64 + 4 + files.iter().map(|(n, _)| 4 + n.len() + 32).sum::<usize>();
        let mut out = Vec::new();
        out.extend(b"GDPC");
        for v in [1u32, 3, 2, 1] {
            out.extend(v.to_le_bytes());
        }
        out.extend([0; 64]);
        out.extend((files.len() as u32).to_le_bytes());
        let mut offset = header_len as u64;
        for (name, data) in files {
            out.extend((name.len() as u32).to_le_bytes());
            out.extend(name.as_bytes());
            out.extend(offset.to_le_bytes());
            out.extend((data.len() as u64).to_le_bytes());
            out.extend([0; 16]);
            offset += data.len() as u64;
        }
        for (_, data) in files {
            out.extend(*data);
        }
        std::fs::write(path, out).unwrap();
    }

    #[test]
    fn reads_pack_metadata_entries_and_tags() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("Test.dungeondraft_pack");
        let tags = br#"{"tags": {"Furniture": ["textures/objects/Home/chair_1x1.png",], ".Garden": ["textures/objects/Home/chair_1x1.png"], "Colorable": ["textures/objects/Home/chair_1x1.png"], "[FA] Trees": ["textures/terrain/grass.png"]}, "sets": {},}"#;
        write_pack(&p, &[
            ("res://packs/AbC123.json", b"{}"),
            ("res://packs/AbC123/pack.json", br#"{"name": "Test Pack", "id": "AbC123", "version": "2", "author": "Me"}"#),
            ("res://packs/AbC123/data/default.dungeondraft_tags", tags),
            ("res://packs/AbC123/textures/objects/Home/chair_1x1.png", b"PNGDATA"),
            ("res://packs/AbC123/textures/terrain/grass.png", b"GRASS"),
        ]);
        let pack = open(&p).unwrap();
        assert_eq!((pack.name.as_str(), pack.author.as_str(), pack.version.as_str()), ("Test Pack", "Me", "2"));
        let chair = pack.entries.iter().find(|e| e.path == "textures/objects/Home/chair_1x1.png").unwrap();
        assert_eq!(read_entry(&p, chair.offset, chair.size).unwrap(), b"PNGDATA");
        assert_eq!(pack.tags["textures/objects/Home/chair_1x1.png"], ["furniture", "garden"]);
        assert_eq!(pack.tags["textures/terrain/grass.png"], ["trees"]);
    }

    #[test]
    fn classifies_and_cleans() {
        assert_eq!(classify("textures/objects/Home/Beds/bed.webp"), Some(("objects", "prop", vec!["Home", "Beds"])));
        assert_eq!(classify("textures/walls/w.png").map(|c| c.1), Some("other"));
        assert_eq!(classify("textures/tilesets/simple/t.png").map(|c| c.1), Some("tile"));
        assert!(classify("textures/materials/x.psd").is_none());
        assert!(classify("data/tilesets/x.png").is_none());
        assert_eq!(kind_value("walls"), "wall");
        assert_eq!(kind_value("terrain"), "terrain");
        assert_eq!(clean_tag("[BW] Gems & Crystals"), Some("gems-&-crystals".into()));
        assert!(clean_tag("Testing").is_none());
        assert!(lenient_json(r#"{"a": "x,]", "b": [1,2,],}"#).is_ok());
    }

    /// `DDPACK_DIR=<folder> cargo test -p asset-library real_packs -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_packs() {
        let dir = std::env::var("DDPACK_DIR").expect("set DDPACK_DIR");
        let (mut packs, mut images, mut tagged) = (0, 0, 0);
        let mut by_type: HashMap<&str, usize> = HashMap::new();
        for (path, _) in crate::scanner::iter_files(Path::new(&dir)).1 {
            let pack = open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            packs += 1;
            for e in &pack.entries {
                if let Some((_, t, _)) = classify(&e.path) {
                    images += 1;
                    *by_type.entry(t).or_default() += 1;
                    tagged += usize::from(pack.tags.contains_key(&e.path));
                }
            }
        }
        eprintln!("{packs} packs, {images} images ({tagged} with Dungeondraft tags): {by_type:?}");
    }

    #[test]
    fn rejects_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.dungeondraft_pack");
        std::fs::write(&p, b"PK\x03\x04 a zip").unwrap();
        assert!(open(&p).is_err());
    }
}
