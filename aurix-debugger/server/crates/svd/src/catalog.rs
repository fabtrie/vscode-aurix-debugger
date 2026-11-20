//! The SVD files bundled with the debugger (`svd-index.json`): which file, and which extra files
//! merged into it, describe a device.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{Device, Error, Extra};

/// The index file of an SVD directory.
pub const INDEX: &str = "svd-index.json";

/// The devices of an SVD directory.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub entries: Vec<Entry>,
}

/// A device's SVD file and the files merged into it.
#[derive(Clone, Debug)]
pub struct Entry {
    /// The file's name without `.svd.gz` (`TC37XPD`).
    pub id: String,
    /// Device name as a launch configuration's `device` gives it (`TC37x`).
    pub device: String,
    pub label: String,
    pub path: PathBuf,
    pub device_names: Vec<String>,
    pub extra: Vec<ExtraFile>,
}

#[derive(Clone, Debug)]
pub struct ExtraFile {
    pub path: PathBuf,
    /// Device peripherals it replaces.
    pub replace: Vec<String>,
    /// Its peripherals that are not merged.
    pub exclude: Vec<String>,
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()).unwrap_or_default()
}

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// The file an index path names, in `dir` (the index lists them relative to the extension:
/// `bin/svd/TC37XPD.svd.gz`).
fn file_in(dir: &Path, path: &str) -> PathBuf {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    dir.join(name)
}

/// `TC37x_EXT` and `tc37xext` are the same device.
fn same(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.chars().filter(|&c| c != '_' && c != '-').flat_map(char::to_lowercase).collect::<String>();
    norm(a) == norm(b)
}

impl Catalog {
    /// The devices listed in `dir/svd-index.json` whose files exist (as the extension read it).
    pub fn load(dir: &Path) -> Result<Catalog, Error> {
        let index = dir.join(INDEX);
        let text_ = std::fs::read_to_string(&index).map_err(|e| Error::Svd(format!("{}: {e}", index.display())))?;
        let v: Value = serde_json::from_str(&text_).map_err(|e| Error::Svd(format!("{}: {e}", index.display())))?;
        let mut entries = Vec::new();
        for e in v["entries"].as_array().into_iter().flatten() {
            let Some(rel) = text(&e["path"]) else { continue };
            let path = file_in(dir, &rel);
            if !path.is_file() {
                continue;
            }
            let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let fallback = file_name.strip_suffix(".svd.gz").or_else(|| file_name.strip_suffix(".svd")).unwrap_or(&file_name).to_string();
            let id = text(&e["id"]).unwrap_or(fallback);
            let device = text(&e["device"]).unwrap_or_else(|| id.clone());
            let label = text(&e["label"]).unwrap_or_else(|| id.clone());
            let extra = e["extraSvd"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| {
                    let (p, x) = match x {
                        Value::String(s) => (s.clone(), &Value::Null),
                        _ => (text(&x["path"])?, x),
                    };
                    let path = file_in(dir, &p);
                    path.is_file().then(|| ExtraFile { path, replace: strings(&x["replace"]), exclude: strings(&x["exclude"]) })
                })
                .collect();
            entries.push(Entry { id, device, label, path, device_names: strings(&e["deviceNames"]), extra });
        }
        Ok(Catalog { entries })
    }

    /// The entry of a device name (`TC37x`, case-insensitive), else of an SVD id (`TC37XPD`) or
    /// another name it lists. Underscores do not count, so an OpenOCD chip name (`tc37x_ext`)
    /// finds its device (`TC37xEXT`).
    pub fn find(&self, device: &str) -> Option<&Entry> {
        let d = device.trim();
        if d.is_empty() {
            return None;
        }
        self.entries
            .iter()
            .find(|e| e.device.eq_ignore_ascii_case(d))
            .or_else(|| self.entries.iter().find(|e| std::iter::once(&e.id).chain(&e.device_names).any(|n| n.eq_ignore_ascii_case(d))))
            .or_else(|| self.entries.iter().find(|e| std::iter::once(&e.device).chain(&e.device_names).any(|n| same(n, d))))
    }
}

impl Entry {
    /// The device file with its extra files merged.
    pub fn load(&self) -> Result<Device, Error> {
        let extras: Vec<Extra<&Path>> = self.extra.iter().map(|x| Extra { source: x.path.as_path(), replace: &x.replace, exclude: &x.exclude }).collect();
        crate::load(&self.path, &extras)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_as_the_extension_and_openocd_give_them() {
        let entry = |id: &str, device: &str| Entry { id: id.into(), device: device.into(), label: id.into(), path: PathBuf::new(), device_names: vec![], extra: vec![] };
        let c = Catalog { entries: vec![entry("TC37XPD", "TC37x"), entry("TC37XEXTPD", "TC37xEXT"), entry("TC4DXAPD", "TC4Dx")] };
        assert_eq!(c.find("tc37x").map(|e| e.id.as_str()), Some("TC37XPD"));
        assert_eq!(c.find("TC37XPD").map(|e| e.id.as_str()), Some("TC37XPD"));
        assert_eq!(c.find("tc37x_ext").map(|e| e.id.as_str()), Some("TC37XEXTPD"));
        assert_eq!(c.find("tc4dx").map(|e| e.id.as_str()), Some("TC4DXAPD"));
        assert!(c.find("tc49x").is_none());
        assert!(c.find(" ").is_none());
    }
}
