//! The hub's settings: `hub.toml` (see [`crate::paths::hub_toml`]), then the command line.
//!
//! ```toml
//! bind = "127.0.0.1"   # "0.0.0.0" serves other machines too: needs `token`
//! port = 3001
//! token = "..."        # clients from other machines send `Authorization: Bearer <token>`
//! allow_tcl = true     # false: other machines may not run Tcl commands (Tcl can run programs)
//! idle_exit_min = 0    # exit after this many minutes without sessions and MCP clients (0: never)
//! mcp_idle_min = 30    # end an MCP session after this many minutes without a request
//! worker = "..."       # aurix-dbg executable; default: the one next to aurix-hub
//! tcl_dir = "..."      # OpenOCD scripts; default: openocd\tcl next to aurix-hub
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HubConfig {
    pub bind: String,
    pub port: u16,
    pub token: Option<String>,
    pub allow_tcl: bool,
    pub idle_exit_min: u64,
    pub mcp_idle_min: u64,
    pub worker: Option<PathBuf>,
    pub tcl_dir: Option<PathBuf>,
}

impl Default for HubConfig {
    fn default() -> HubConfig {
        HubConfig {
            bind: "127.0.0.1".into(),
            port: api_spec::HUB_PORT,
            token: None,
            allow_tcl: true,
            idle_exit_min: 0,
            mcp_idle_min: 30,
            worker: None,
            tcl_dir: None,
        }
    }
}

impl HubConfig {
    /// `hub.toml`, or the defaults when there is none. `AURIX_HUB_PORT` overrides the port.
    pub fn load() -> Result<HubConfig, String> {
        let mut cfg = Self::load_file(&paths::hub_toml())?;
        if let Some(p) = std::env::var("AURIX_HUB_PORT").ok().and_then(|p| p.parse().ok()) {
            cfg.port = p;
        }
        Ok(cfg)
    }

    pub fn load_file(path: &Path) -> Result<HubConfig, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HubConfig::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The aurix-dbg executable the workers run.
    pub fn worker_exe(&self) -> PathBuf {
        self.worker.clone().unwrap_or_else(|| paths::exe_dir().join(paths::exe_name("aurix-dbg")))
    }

    /// OpenOCD's scripts, passed to every worker as the first search directory.
    pub fn tcl_dir(&self) -> Option<PathBuf> {
        let d = self.tcl_dir.clone().unwrap_or_else(|| self.worker_exe().parent().map(|p| p.join("openocd").join("tcl")).unwrap_or_default());
        d.is_dir().then_some(d)
    }

    /// Whether the hub listens only on this machine.
    pub fn loopback_only(&self) -> bool {
        matches!(self.bind.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]")
    }

    /// The token, when one is set.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref().filter(|t| !t.is_empty())
    }

    /// Refuses a setup that would expose the debugger without a token.
    pub fn check(&self) -> Result<(), String> {
        if !self.loopback_only() && self.token().is_none() {
            return Err(format!(
                "hub.toml binds to {} without a token: every machine that can reach it could flash the board and run programs here. Set `token` (aurix-hub install --remote creates one).",
                self.bind
            ));
        }
        Ok(())
    }
}

/// A random hex string of `bytes` bytes (session IDs, MCP session IDs, tokens).
pub fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    if getrandom::fill(&mut b).is_err() {
        // No OS randomness: time and process ID are unique enough for IDs (never for a token,
        // which `install --remote` creates and which fails loudly instead).
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
        let seed = t ^ ((std::process::id() as u128) << 64);
        for (i, x) in b.iter_mut().enumerate() {
            *x = (seed >> ((i % 16) * 8)) as u8 ^ (i as u8).wrapping_mul(151);
        }
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposed_without_token_is_refused() {
        let mut c = HubConfig::default();
        assert!(c.check().is_ok());
        c.bind = "0.0.0.0".into();
        assert!(c.check().is_err());
        c.token = Some("abc".into());
        assert!(c.check().is_ok());
        let parsed: HubConfig = toml::from_str("port = 4000\nallow_tcl = false\n").unwrap();
        assert_eq!((parsed.port, parsed.allow_tcl, parsed.bind.as_str()), (4000, false, "127.0.0.1"));
        assert_eq!(random_hex(8).len(), 16);
        assert_ne!(random_hex(8), random_hex(8));
    }
}
