//! Loaded symbol files (MemoryMap.set): each context has an ordered set of ELF
//! files (application, bootmanager, FBL, ...), the first one being the primary.
//!
//! ELF files are numbered in load order for the whole process: symbol IDs name
//! their ELF (`E<n>/...`), so they stay valid whichever context they came from.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use aurix_core::log_info;
use symbols::{Function, LineInfo, Symbols};

/// An ELF of a symbol set: its global number and symbols.
pub type Elf = (usize, Arc<Symbols>);

/// The ELF files of a context, primary first.
#[derive(Default)]
pub struct SymbolSet {
    pub elfs: Vec<Elf>,
}

impl SymbolSet {
    pub fn primary(&self) -> Option<&Elf> {
        self.elfs.first()
    }

    /// The ELF with code (a function or line entry) at `addr`.
    pub fn for_addr(&self, addr: u32) -> Option<&Elf> {
        self.elfs
            .iter()
            .find(|(_, s)| s.function_at(addr).is_some())
            .or_else(|| self.elfs.iter().find(|(_, s)| s.line_at(addr).is_some()))
    }

    /// ELF files in lookup order for a scope: the ELF of `pc` first, then the others.
    pub fn ordered(&self, pc: Option<u32>) -> Vec<Elf> {
        let first = pc.and_then(|pc| self.for_addr(pc)).map(|e| e.0);
        let mut out: Vec<Elf> = self.elfs.iter().filter(|e| Some(e.0) == first).cloned().collect();
        out.extend(self.elfs.iter().filter(|e| Some(e.0) != first).cloned());
        out
    }

    pub fn function_at(&self, addr: u32) -> Option<&Function> {
        self.elfs.iter().find_map(|(_, s)| s.function_at(addr))
    }

    /// Function at `addr` as (ELF number, function index).
    pub fn function_index_at(&self, addr: u32) -> Option<(usize, usize)> {
        self.elfs.iter().find_map(|(n, s)| s.function_index_at(addr).map(|i| (*n, i)))
    }

    pub fn line_at(&self, addr: u32) -> Option<LineInfo> {
        match self.for_addr(addr) {
            Some((_, s)) => s.line_at(addr),
            None => None,
        }
    }

    /// Line entries overlapping `start..end`, from every file. A disassembly window often starts
    /// in data or a gap (MCS `.var` areas between the tasks), so asking only the file that owns
    /// `start` would leave the whole window without source. The owner of `start` comes first, so
    /// its entry wins where two files describe the same address.
    pub fn lines_in_range(&self, start: u32, end: u32) -> Vec<LineInfo> {
        self.ordered(Some(start)).iter().flat_map(|(_, s)| s.lines_in_range(start, end)).collect()
    }

    /// Addresses for a source line in all ELF files: the nearest line with code
    /// at or after `line` over all files, and its addresses.
    pub fn resolve_line(&self, file: &str, line: u32) -> Result<(u32, Vec<u32>), String> {
        let mut first_err = None;
        let mut best: Option<(u32, Vec<u32>)> = None;
        for (_, s) in &self.elfs {
            match s.resolve_line(file, line) {
                Ok((l, addrs)) => match &mut best {
                    Some((bl, ba)) if *bl == l => ba.extend(addrs),
                    Some((bl, _)) if *bl < l => {}
                    _ => best = Some((l, addrs)),
                },
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        match best {
            Some((l, mut addrs)) => {
                addrs.sort_unstable();
                addrs.dedup();
                Ok((l, addrs))
            }
            None => Err(first_err.unwrap_or_else(|| "No symbols loaded".into())),
        }
    }

    /// Address for a location expression (a number or a function name), primary ELF first.
    pub fn resolve_location(&self, loc: &str) -> Option<u32> {
        self.elfs.iter().find_map(|(_, s)| s.resolve_location(loc))
    }
}

#[derive(Default)]
pub struct SymbolStore {
    /// Loaded ELF files, indexed by their number.
    elfs: Vec<Arc<Symbols>>,
    /// Path -> (modification time, length, ELF number).
    cache: HashMap<PathBuf, (Option<SystemTime>, u64, usize)>,
    sets: HashMap<String, Arc<SymbolSet>>,
    /// TriCore core IDs that run an ELF (`symbolFiles` entry `path@N`), by ELF number: their
    /// core-local memory holds the ELF's variables linked to the local DSPR/PSPR alias.
    run_cores: HashMap<usize, Vec<u32>>,
}

impl SymbolStore {
    pub fn elf(&self, n: usize) -> Option<Arc<Symbols>> {
        self.elfs.get(n).cloned()
    }

    pub fn set(&self, ctx: &str) -> Option<Arc<SymbolSet>> {
        self.sets.get(ctx).cloned()
    }

    /// The TriCore core IDs that run ELF `n`, first the one whose core-local memory is meant on
    /// the other cores (empty: not configured).
    pub fn run_cores(&self, n: usize) -> &[u32] {
        self.run_cores.get(&n).map_or(&[], Vec::as_slice)
    }

    /// Sets the cores that run the loaded ELF `path` (see [`Self::run_cores`]).
    pub fn set_run_cores(&mut self, path: &Path, cores: Vec<u32>) {
        let Some(&(_, _, n)) = self.cache.get(path) else { return };
        if cores.is_empty() {
            self.run_cores.remove(&n);
        } else {
            self.run_cores.insert(n, cores);
        }
    }

    /// Contexts with a symbol set.
    pub fn contexts(&self) -> Vec<String> {
        self.sets.keys().cloned().collect()
    }

    /// Loads the files (cached) and makes them the symbol set of `ctx`.
    pub fn assign(&mut self, ctx: &str, paths: &[PathBuf]) -> Result<Arc<SymbolSet>, String> {
        let mut set = SymbolSet::default();
        for p in paths {
            let n = self.load(p)?;
            if !set.elfs.iter().any(|e| e.0 == n) {
                set.elfs.push((n, self.elfs[n].clone()));
            }
        }
        let set = Arc::new(set);
        self.sets.insert(ctx.to_string(), set.clone());
        Ok(set)
    }

    fn load(&mut self, path: &Path) -> Result<usize, String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let (mtime, len) = (meta.modified().ok(), meta.len());
        if let Some((m, l, n)) = self.cache.get(path) {
            if *m == mtime && *l == len {
                return Ok(*n);
            }
        }
        // GTM MCS programs have no debug information: the source itself is assembled.
        let is_asm = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("asm"));
        let s = Arc::new(if is_asm {
            let t0 = std::time::Instant::now();
            let mut s = symbols::mcs_asm::assemble(path, &[])?.symbols(path);
            s.load_time = t0.elapsed();
            s
        } else {
            Symbols::load(path)?
        });
        log_info!(
            "Symbols: loaded {} in {:.1}s ({} functions, {} globals, {} source files)",
            path.display(),
            s.load_time.as_secs_f64(),
            s.functions.len(),
            s.globals.len(),
            s.file_count()
        );
        self.elfs.push(s);
        let n = self.elfs.len() - 1;
        self.cache.insert(path.to_path_buf(), (mtime, len, n));
        Ok(n)
    }
}

/// Symbol ID of `id` in ELF `n`.
pub fn elf_id(n: usize, id: &str) -> String {
    format!("E{n}/{id}")
}

/// Splits `E<n>/<id>` into (n, id).
pub fn split_elf_id(id: &str) -> Option<(usize, &str)> {
    let (n, rest) = id.strip_prefix('E')?.split_once('/')?;
    Some((n.parse().ok()?, rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert_eq!(elf_id(2, "GV:12"), "E2/GV:12");
        assert_eq!(split_elf_id("E2/GV:12"), Some((2, "GV:12")));
        assert_eq!(split_elf_id("E10/LV:21.3803@S0.D0.C0.F2"), Some((10, "LV:21.3803@S0.D0.C0.F2")));
        assert_eq!(split_elf_id("GV:12"), None);
        assert_eq!(split_elf_id("EX/GV:1"), None);
    }

    /// `symbolFiles` `path@N`: the cores are kept per loaded file, and an empty list clears them.
    #[test]
    fn run_cores_by_file() {
        let dir = std::env::temp_dir().join(format!("aurix-runcores-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.asm"), dir.join("b.asm"));
        std::fs::write(&a, "start:\n  jmp start\n").unwrap();
        std::fs::write(&b, "start:\n  jmp start\n").unwrap();
        let mut store = SymbolStore::default();
        store.assign("S0.D0.C0", &[a.clone(), b.clone()]).unwrap();
        store.set_run_cores(&b, vec![1, 2]);
        store.set_run_cores(&dir.join("unknown.asm"), vec![3]);
        assert_eq!(store.run_cores(0), &[] as &[u32]);
        assert_eq!(store.run_cores(1), &[1, 2]);
        store.set_run_cores(&b, Vec::new());
        assert!(store.run_cores(1).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A disassembly window that starts in data (an MCS `.var`) still gets the source lines of the
    /// code in it: asking only the file that owns the window start used to return nothing at all.
    #[test]
    fn lines_in_range_from_data() {
        let dir = std::env::temp_dir().join(format!("aurix-symstore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("t.asm");
        std::fs::write(&src, ".org 0x10\nvar1: .var 7\n.org 0x20\nstart:\n  movl R0, 1\n  jmp start\n").unwrap();
        let prog = symbols::mcs_asm::assemble(&src, &[]).unwrap();
        let set = SymbolSet { elfs: vec![(0, Arc::new(prog.symbols(&src)))] };

        assert!(set.for_addr(0x10).is_none(), "0x10 is data, owned by no code");
        let lines: Vec<(u32, u32)> = set.lines_in_range(0x10, 0x28).iter().map(|l| (l.start, l.line)).collect();
        assert_eq!(lines, vec![(0x20, 5), (0x24, 6)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Application plus bootmanager: addresses map to their ELF, lines resolve in both.
    #[test]
    fn real_elf_set() {
        let app = PathBuf::from("C:/workspace/app/build/bin/app_dev.elf");
        let bm = PathBuf::from("C:/workspace/bootloader/build/bin/bootloader_dev.elf");
        if !app.exists() || !bm.exists() {
            eprintln!("test ELF files not found, skipping");
            return;
        }
        let mut store = SymbolStore::default();
        let set = store.assign("S0.D0.C0", &[app.clone(), bm.clone()]).unwrap();
        assert_eq!(set.elfs.iter().map(|e| e.0).collect::<Vec<_>>(), vec![0, 1]);
        // Same files for another core: loaded once.
        let set1 = store.assign("S0.D0.C1", &[app, bm]).unwrap();
        assert_eq!(set1.elfs.iter().map(|e| e.0).collect::<Vec<_>>(), vec![0, 1]);
        assert!(store.elf(2).is_none());

        let (app_s, bm_s) = (&set.elfs[0].1, &set.elfs[1].1);
        let f = bm_s
            .functions
            .iter()
            .find(|f| app_s.function_at(f.low).is_none() && bm_s.line_at(f.low).is_some())
            .expect("a bootmanager function outside the application");
        assert_eq!(set.for_addr(f.low).map(|e| e.0), Some(1));
        assert_eq!(set.ordered(Some(f.low)).first().map(|e| e.0), Some(1));
        assert_eq!(set.ordered(None).first().map(|e| e.0), Some(0));
        assert_eq!(set.function_at(f.low).map(|x| x.name.as_str()), Some(f.name.as_str()));
        assert_eq!(set.function_index_at(f.low).map(|x| x.0), Some(1));
        let li = set.line_at(f.low).expect("bootmanager line");
        let (line, addrs) = set.resolve_line(&li.file, li.line).unwrap();
        assert_eq!(line, li.line);
        assert!(addrs.iter().any(|a| f.contains(*a)), "{addrs:08x?}");
        assert!(set.resolve_line("no_such_file_anywhere.c", 1).is_err());
    }
}
