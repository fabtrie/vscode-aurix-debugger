//! Loaded symbol files (MemoryMap.set), cached by path and modification time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use aurix_core::log_info;
use symbols::Symbols;

#[derive(Default)]
pub struct SymbolStore {
    cache: HashMap<PathBuf, (Option<SystemTime>, u64, Arc<Symbols>)>,
    active: Option<Arc<Symbols>>,
}

impl SymbolStore {
    pub fn active(&self) -> Option<Arc<Symbols>> {
        self.active.clone()
    }

    pub fn load(&mut self, path: &Path) -> Result<Arc<Symbols>, String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (mtime, len) = (meta.modified().ok(), meta.len());
        if let Some((m, l, s)) = self.cache.get(path) {
            if *m == mtime && *l == len {
                self.active = Some(s.clone());
                return Ok(s.clone());
            }
        }
        let s = Arc::new(Symbols::load(path)?);
        log_info!(
            "Symbols: loaded {} in {:.1}s ({} functions, {} globals, {} source files)",
            path.display(),
            s.load_time.as_secs_f64(),
            s.functions.len(),
            s.globals.len(),
            s.file_count()
        );
        self.cache.insert(path.to_path_buf(), (mtime, len, s.clone()));
        self.active = Some(s.clone());
        Ok(s)
    }
}
