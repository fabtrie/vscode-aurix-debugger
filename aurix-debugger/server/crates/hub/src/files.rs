//! Files uploaded by clients on other machines (`POST /api/v1/files`).
//!
//! The session operations take paths on the hub machine. A remote client uploads an ELF or HEX
//! file and gets an ID `file:<sha256>/<name>`, which it passes wherever a path goes; the hub
//! replaces IDs in every JSON request by the cached file's path before forwarding it.

use std::path::PathBuf;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::paths;

const PREFIX: &str = "file:";

/// Stores `data` as `name`; returns the ID and the path.
pub fn store(name: &str, data: &[u8]) -> Result<(String, PathBuf), String> {
    let name = sanitize(name);
    let hash: String = Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect();
    let dir = paths::cache_dir().join(&hash);
    let path = dir.join(&name);
    if !path.is_file() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp = dir.join(format!("{name}.part"));
        std::fs::write(&tmp, data).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    Ok((format!("{PREFIX}{hash}/{name}"), path))
}

/// A file name without directories or characters Windows refuses.
fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = base.chars().map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c }).collect();
    let clean = clean.trim_matches(['.', ' ']).to_string();
    if clean.is_empty() {
        "upload.bin".into()
    } else {
        clean
    }
}

/// The cached path of an ID, if it is one of a stored file.
pub fn resolve_id(s: &str) -> Option<PathBuf> {
    let rest = s.strip_prefix(PREFIX)?;
    let (hash, name) = rest.split_once('/')?;
    if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) || name.contains(['/', '\\']) || name == ".." {
        return None;
    }
    let p = paths::cache_dir().join(hash).join(name);
    p.is_file().then_some(p)
}

/// Replaces file IDs in every string of `v` (also `id@N` as `load_symbols` writes them).
pub fn resolve_ids(v: &mut Value) -> Result<(), String> {
    match v {
        Value::String(s) if s.starts_with(PREFIX) => {
            let (id, suffix) = match s.rsplit_once('@').filter(|(_, n)| n.split(',').all(|x| x.trim().parse::<u32>().is_ok())) {
                Some((id, n)) => (id.to_string(), format!("@{n}")),
                None => (s.clone(), String::new()),
            };
            let p = resolve_id(&id).ok_or_else(|| format!("unknown file ID {id} (upload it with POST /api/v1/files)"))?;
            *s = format!("{}{suffix}", p.to_string_lossy());
        }
        Value::Array(a) => a.iter_mut().try_for_each(resolve_ids)?,
        Value::Object(m) => m.values_mut().try_for_each(resolve_ids)?,
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(sanitize(r"C:\x\app.elf"), "app.elf");
        assert_eq!(sanitize("a:b?.hex"), "a_b_.hex");
        assert_eq!(sanitize(".."), "upload.bin");
        assert!(resolve_id("file:abc/x").is_none());
        let mut v = serde_json::json!({ "files": ["plain.elf"] });
        resolve_ids(&mut v).unwrap();
        assert_eq!(v["files"][0], "plain.elf");
        let mut v = serde_json::json!(format!("file:{}/x.elf", "0".repeat(64)));
        assert!(resolve_ids(&mut v).is_err());
    }
}
