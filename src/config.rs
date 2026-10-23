use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Persistent configuration stored at `$HOME/.config/.agy-sync`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AgySyncConfig {
    pub default_host: Option<String>,
    pub default_port: Option<u16>,
    pub default_user: Option<String>,
    #[serde(default)]
    pub no_remap: bool,
    #[serde(default)]
    pub deep_remap: bool,
    #[serde(default)]
    pub default_harnesses: Vec<String>,
}

/// Normalizes a filesystem path string by stripping Windows `\\?\` verbatim prefixes,
/// converting backslashes (`\`) to forward slashes (`/`), and trimming trailing slashes
/// (unless it is the root `/`).
pub fn normalize_home_path_string(raw: &str) -> String {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix(r"\\?\UNC\")
        .map(|s| format!("//{}", s))
        .unwrap_or_else(|| {
            trimmed
                .strip_prefix(r"\\?\")
                .or_else(|| trimmed.strip_prefix("//?/"))
                .unwrap_or(trimmed)
                .to_string()
        });
    let forward = stripped.replace('\\', "/");
    let no_trail = forward.trim_end_matches('/');
    if no_trail.is_empty() && forward.starts_with('/') {
        "/".to_string()
    } else {
        no_trail.to_string()
    }
}

/// Resolves the local user's `$HOME` directory across `*nix` (macOS/Linux), WSL2, ChromeOS (`crosh`/Crostini),
/// and native Windows (`USERPROFILE` / `HOMEDRIVE` + `HOMEPATH`), normalized with forward slashes.
pub fn resolve_local_home() -> Result<String> {
    if let Ok(h) = std::env::var("HOME") {
        let norm = normalize_home_path_string(&h);
        if !norm.is_empty() {
            return Ok(norm);
        }
    }
    if let Ok(up) = std::env::var("USERPROFILE") {
        let norm = normalize_home_path_string(&up);
        if !norm.is_empty() {
            return Ok(norm);
        }
    }
    if let (Ok(drive), Ok(path)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
        let combined = format!("{}{}", drive.trim(), path.trim());
        let norm = normalize_home_path_string(&combined);
        if !norm.is_empty() {
            return Ok(norm);
        }
    }
    anyhow::bail!(
        "Unable to determine user home directory: neither HOME nor USERPROFILE environment variable is set"
    )
}

/// Returns `$HOME/.config/.agy-sync`.
pub fn config_file_path(home: &str) -> PathBuf {
    Path::new(home).join(".config").join(".agy-sync")
}

fn legacy_config_file_path(home: &str) -> PathBuf {
    Path::new(home).join(".config").join(".gemini-sync")
}

impl AgySyncConfig {
    pub fn load(home: &str) -> Result<Self> {
        let primary = config_file_path(home);
        let path = if primary.exists() {
            primary
        } else {
            let legacy = legacy_config_file_path(home);
            if legacy.exists() {
                legacy
            } else {
                return Ok(Self::default());
            }
        };

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config file {}", path.display()))?;
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        let cfg: Self = serde_json::from_str(&content)
            .with_context(|| format!("Invalid JSON in {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save(&self, home: &str) -> Result<PathBuf> {
        let path = config_file_path(home);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create config directory {}", parent.display())
            })?;
        }
        let serialized = serde_json::to_string_pretty(self)?;
        fs::write(&path, format!("{}\n", serialized))
            .with_context(|| format!("Failed to write {}", path.display()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        }

        Ok(path)
    }
}

/// Parses `[user@]host[:port]` into `(Option<user>, host, Option<port>)`.
pub fn parse_host_port_user(raw: &str) -> (Option<String>, String, Option<u16>) {
    let trimmed = raw.trim();
    let (user, rest) = if let Some((u, r)) = trimmed.split_once('@') {
        let user_opt = (!u.trim().is_empty()).then(|| u.trim().to_string());
        (user_opt, r.trim())
    } else {
        (None, trimmed)
    };

    if let Some((h, p_str)) = rest.rsplit_once(':') {
        if !h.is_empty() && !h.contains(':') {
            if let Ok(port) = p_str.parse::<u16>() {
                return (user, h.to_string(), Some(port));
            }
        }
    }

    (user, rest.to_string(), None)
}

/// Inspects `$HOME/.ssh/config` to check if a specific `Host <alias>` block defines an explicit `Port`.
pub fn detect_port_in_ssh_config(home: &str, target_host: &str) -> Option<u16> {
    let ssh_config_path = Path::new(home).join(".ssh").join("config");
    let content = fs::read_to_string(ssh_config_path).ok()?;
    parse_ssh_config_port(&content, target_host)
}

pub fn parse_ssh_config_port(ssh_config_content: &str, target_host: &str) -> Option<u16> {
    let mut in_matching_specific_host = false;
    for line in ssh_config_content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        if keyword.eq_ignore_ascii_case("Host") {
            let patterns: Vec<&str> = parts.collect();
            in_matching_specific_host = patterns
                .iter()
                .any(|&pat| pat != "*" && pat.eq_ignore_ascii_case(target_host));
        } else if in_matching_specific_host && keyword.eq_ignore_ascii_case("Port") {
            if let Some(p_str) = parts.next() {
                if let Ok(port) = p_str.parse::<u16>() {
                    return Some(port);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_host_port_user() {
        assert_eq!(
            parse_host_port_user("workstation.example.com"),
            (None, "workstation.example.com".to_string(), None)
        );
        assert_eq!(
            parse_host_port_user("workstation.example.com:2222"),
            (None, "workstation.example.com".to_string(), Some(2222))
        );
        assert_eq!(
            parse_host_port_user("alice@workstation.example.com:2200"),
            (
                Some("alice".to_string()),
                "workstation.example.com".to_string(),
                Some(2200)
            )
        );
        assert_eq!(
            parse_host_port_user("bob@devbox"),
            (Some("bob".to_string()), "devbox".to_string(), None)
        );
    }

    #[test]
    fn test_parse_ssh_config_port() {
        let sample = r#"
Host *
    ControlMaster auto

Host devbox
    HostName devbox.example.com
    Port 2222

Host defaultportbox
    HostName default.example.com
"#;
        assert_eq!(parse_ssh_config_port(sample, "devbox"), Some(2222));
        assert_eq!(parse_ssh_config_port(sample, "defaultportbox"), None);
        assert_eq!(parse_ssh_config_port(sample, "unknown"), None);
    }

    #[test]
    fn test_config_load_save_roundtrip() -> Result<()> {
        let tmp = TempDir::new()?;
        let home = tmp.path().to_string_lossy().to_string();

        let loaded_empty = AgySyncConfig::load(&home)?;
        assert_eq!(loaded_empty, AgySyncConfig::default());

        let cfg = AgySyncConfig {
            default_host: Some("remote-workstation".to_string()),
            default_port: Some(22),
            default_user: Some("developer".to_string()),
            no_remap: false,
            deep_remap: true,
            default_harnesses: vec!["agy-2.0".to_string(), "agy-cli".to_string()],
        };
        let saved_path = cfg.save(&home)?;
        assert!(saved_path.ends_with(".config/.agy-sync"));

        let loaded = AgySyncConfig::load(&home)?;
        assert_eq!(loaded, cfg);
        Ok(())
    }
}
