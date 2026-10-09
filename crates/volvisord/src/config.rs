//! Daemon configuration.
//!
//! Configuration is explicit and validated at startup; defaults are
//! conservative. No secret material is ever logged (SPEC-0002 section 9).

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::Deserialize;

use crate::DaemonError;

/// Provider backend selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// In-memory fake provider (testing only; never production).
    Fake,
    /// Native-local LVM provider (P1 prototype).
    Lvm,
}

/// Daemon configuration (TOML file at `--config`).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// HTTP bind address for the Volume API v2 surface.
    pub listen: SocketAddr,
    /// Directory holding the intent journal and lock file.
    pub journal_dir: PathBuf,
    /// Selected provider backend.
    pub provider: ProviderKind,
    /// LVM volume-group prefix used for claimed pools (LVM provider only).
    pub lvm_vg_prefix: Option<String>,
    /// Static bearer token for privileged admin operations. Empty disables
    /// admin endpoints (fail closed). Never logged.
    #[serde(default)]
    pub admin_token: Option<String>,
    /// Maximum request body size in bytes.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
}

fn default_max_body_bytes() -> usize {
    1 << 20
}

impl Config {
    /// Load and validate configuration from a TOML file.
    ///
    /// # Errors
    /// Returns [`DaemonError::Config`] when the file is unreadable,
    /// malformed or fails validation.
    pub fn load(path: &std::path::Path) -> Result<Self, DaemonError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| DaemonError::Config(format!("cannot read {}: {e}", path.display())))?;
        let cfg: Self = toml::from_str(&raw)
            .map_err(|e| DaemonError::Config(format!("cannot parse {}: {e}", path.display())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate cross-field constraints.
    fn validate(&self) -> Result<(), DaemonError> {
        if self.lvm_vg_prefix.is_none() && self.provider == ProviderKind::Lvm {
            return Err(DaemonError::Config(
                "lvm_vg_prefix is required for the lvm provider".to_owned(),
            ));
        }
        if let Some(prefix) = &self.lvm_vg_prefix {
            if prefix.is_empty() || prefix.len() > 64 {
                return Err(DaemonError::Config(
                    "lvm_vg_prefix must be 1..=64 characters".to_owned(),
                ));
            }
            if !prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(DaemonError::Config(
                    "lvm_vg_prefix may contain only alnum, '-' and '_'".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_toml() -> String {
        "\
listen = \"127.0.0.1:8787\"
journal_dir = \"/var/lib/volvisor/journal\"
provider = \"lvm\"
lvm_vg_prefix = \"volvisor\"
"
        .to_owned()
    }

    #[test]
    fn parses_minimal_config() {
        let cfg: Config = toml::from_str(&minimal_toml()).expect("parse");
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.provider, ProviderKind::Lvm);
        assert_eq!(cfg.max_body_bytes, 1 << 20);
    }

    #[test]
    fn lvm_provider_requires_prefix() {
        let raw = "\
listen = \"127.0.0.1:8787\"
journal_dir = \"/j\"
provider = \"lvm\"
";
        let cfg: Config = toml::from_str(raw).expect("parse");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let raw = minimal_toml() + "surprise = 1\n";
        assert!(toml::from_str::<Config>(&raw).is_err());
    }

    #[test]
    fn rejects_bad_prefix() {
        let raw = minimal_toml().replace("volvisor", "bad prefix!");
        let cfg: Config = toml::from_str(&raw).expect("parse");
        assert!(cfg.validate().is_err());
    }
}
