use crate::discovery::{
    clean_prompt_text, compute_path_size, discover_project_sessions,
    find_matching_config_project_ids, find_summary_pb_files, normalize_project_path,
    uri_matches_project, ProjectDiscovery,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Harnesses stored under `~/.gemini/`, including dynamic discovery of any custom session subdirectory.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum HarnessKind {
    /// `~/.gemini/antigravity` (Agy 2.0 UI & VS Code Plugin)
    Agy20,
    /// `~/.gemini/antigravity-cli`
    AgyCli,
    /// `~/.gemini/antigravity-ide`
    AgyIde,
    /// `~/.gemini/config/projects/*.json` + `plugin_data`
    AgyIdePlugins,
    /// `~/.gemini/{projects.json, history/<slug>, tmp/<slug>}`
    GeminiCliLegacy,
    /// Any additional session-based harness directory discovered dynamically under `~/.gemini/<subdir>`
    CustomSessionDir(String),
}

impl HarnessKind {
    /// Returns the built-in public harness kinds.
    pub fn builtin() -> Vec<HarnessKind> {
        vec![
            HarnessKind::Agy20,
            HarnessKind::AgyCli,
            HarnessKind::AgyIde,
            HarnessKind::AgyIdePlugins,
            HarnessKind::GeminiCliLegacy,
        ]
    }

    /// Returns all harnesses available under `gemini_root`, automatically including any additional
    /// subdirectories on disk that contain `conversation_summaries.db`, `*_summaries_proto.pb`, `conversations/`, or `brain/`.
    pub fn discover_available_kinds(gemini_root: &Path) -> Vec<HarnessKind> {
        let mut kinds = Vec::new();

        // Scan for any custom session directories on disk first
        if let Ok(entries) = fs::read_dir(gemini_root) {
            let mut custom_names = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if name.starts_with('.')
                    || matches!(
                        name,
                        "antigravity"
                            | "antigravity-cli"
                            | "antigravity-ide"
                            | "config"
                            | "history"
                            | "tmp"
                            | "bin"
                            | "logs"
                            | "extensions"
                            | "skills"
                            | "memory"
                    )
                {
                    continue;
                }
                let is_session_harness = path.join("conversation_summaries.db").exists()
                    || path.join("conversations").is_dir()
                    || path.join("brain").is_dir()
                    || !find_summary_pb_files(&path).is_empty();
                if is_session_harness {
                    custom_names.push(name.to_string());
                }
            }
            custom_names.sort();
            for name in custom_names {
                kinds.push(HarnessKind::CustomSessionDir(name));
            }
        }

        kinds.extend(Self::builtin());
        kinds
    }

    pub fn cli_id(&self) -> &str {
        match self {
            HarnessKind::Agy20 => "agy-2.0",
            HarnessKind::AgyCli => "agy-cli",
            HarnessKind::AgyIde => "agy-ide",
            HarnessKind::AgyIdePlugins => "agy-plugins",
            HarnessKind::GeminiCliLegacy => "gemini-cli",
            HarnessKind::CustomSessionDir(name) => name.as_str(),
        }
    }

    pub fn short_name(&self) -> String {
        match self {
            HarnessKind::Agy20 => "agy (2.0 / vscode plugin)".to_string(),
            HarnessKind::AgyCli => "agy-cli".to_string(),
            HarnessKind::AgyIde => "agy (ide)".to_string(),
            HarnessKind::AgyIdePlugins => "agy (ide plugins & project config)".to_string(),
            HarnessKind::GeminiCliLegacy => "gemini-cli (legacy)".to_string(),
            HarnessKind::CustomSessionDir(name) => format!("{} (session store)", name),
        }
    }

    pub fn display_path(&self) -> String {
        match self {
            HarnessKind::Agy20 => "~/.gemini/antigravity".to_string(),
            HarnessKind::AgyCli => "~/.gemini/antigravity-cli".to_string(),
            HarnessKind::AgyIde => "~/.gemini/antigravity-ide".to_string(),
            HarnessKind::AgyIdePlugins => "~/.gemini/config/projects".to_string(),
            HarnessKind::GeminiCliLegacy => "~/.gemini/{projects.json,history,tmp}".to_string(),
            HarnessKind::CustomSessionDir(name) => format!("~/.gemini/{}", name),
        }
    }

    /// Returns the subdirectory name inside `~/.gemini` for session-based harnesses.
    pub fn session_subdir(&self) -> Option<&str> {
        match self {
            HarnessKind::Agy20 => Some("antigravity"),
            HarnessKind::AgyCli => Some("antigravity-cli"),
            HarnessKind::AgyIde => Some("antigravity-ide"),
            HarnessKind::CustomSessionDir(name) => Some(name.as_str()),
            HarnessKind::AgyIdePlugins | HarnessKind::GeminiCliLegacy => None,
        }
    }

    pub fn from_cli_id(raw: &str) -> Option<HarnessKind> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "agy-2.0" | "agy2" | "agy" | "antigravity" | "vscode" => Some(HarnessKind::Agy20),
            "agy-cli" | "antigravity-cli" => Some(HarnessKind::AgyCli),
            "agy-ide" | "antigravity-ide" => Some(HarnessKind::AgyIde),
            "agy-plugins" | "plugins" | "config" => Some(HarnessKind::AgyIdePlugins),
            "gemini-cli" | "legacy" => Some(HarnessKind::GeminiCliLegacy),
            other => {
                if other
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    Some(HarnessKind::CustomSessionDir(other.to_string()))
                } else {
                    None
                }
            }
        }
    }
}

/// Discovery summary for a single harness inside `~/.gemini/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessDiscovery {
    pub kind: HarnessKind,
    pub harness_dir: PathBuf,
    pub has_interactions: bool,
    pub session_count: usize,
    pub total_bytes: u64,
    pub latest_modified_time: String,
    pub latest_conversation_id: String,
    pub latest_prompt_snippet: String,
    /// Populated for session-based harnesses
    pub project_discovery: Option<ProjectDiscovery>,
    /// Relative paths inside `~/.gemini` for config/legacy harnesses
    pub gemini_root_relative_paths: Vec<String>,
    /// Matched `project_id`s in `config/projects/<id>.json`
    pub config_project_ids: BTreeSet<String>,
    /// Matched slug in `projects.json` / `history/<slug>` / `tmp/<slug>`
    pub legacy_project_slug: Option<String>,
}

/// Discovers project interactions across the specified harnesses under `gemini_root`.
pub fn discover_harnesses(
    project_path: &Path,
    gemini_root: &Path,
    kinds: &[HarnessKind],
) -> Result<Vec<HarnessDiscovery>> {
    let mut results = Vec::with_capacity(kinds.len());
    for kind in kinds {
        results.push(discover_single_harness(kind, project_path, gemini_root)?);
    }
    Ok(results)
}

/// Discovers project interactions for a single `HarnessKind` under `gemini_root`.
pub fn discover_single_harness(
    kind: &HarnessKind,
    project_path: &Path,
    gemini_root: &Path,
) -> Result<HarnessDiscovery> {
    if let Some(sub) = kind.session_subdir() {
        let harness_dir = gemini_root.join(sub);
        let disc = discover_project_sessions(project_path, &harness_dir)?;
        let has_interactions = !disc.sessions.is_empty() || !disc.knowledge_paths.is_empty();
        let session_count = disc.sessions.len();
        let total_bytes = disc.total_bytes;

        let (latest_modified_time, latest_conversation_id, latest_prompt_snippet) =
            if let Some(latest) = disc.latest_session() {
                let snippet = if !latest.last_prompt_snippet.is_empty() {
                    latest.last_prompt_snippet.clone()
                } else {
                    disc.sessions
                        .values()
                        .filter(|s| !s.last_prompt_snippet.is_empty())
                        .max_by(|a, b| a.last_modified_time.cmp(&b.last_modified_time))
                        .map(|s| s.last_prompt_snippet.clone())
                        .unwrap_or_else(|| {
                            if !latest.title.is_empty() {
                                latest.title.clone()
                            } else {
                                "(Session without text prompt preview)".to_string()
                            }
                        })
                };
                (
                    latest.last_modified_time.clone(),
                    latest.conversation_id.clone(),
                    snippet,
                )
            } else {
                (String::new(), String::new(), String::new())
            };

        return Ok(HarnessDiscovery {
            kind: kind.clone(),
            harness_dir,
            has_interactions,
            session_count,
            total_bytes,
            latest_modified_time,
            latest_conversation_id,
            latest_prompt_snippet,
            project_discovery: Some(disc),
            gemini_root_relative_paths: Vec::new(),
            config_project_ids: BTreeSet::new(),
            legacy_project_slug: None,
        });
    }

    match kind {
        HarnessKind::AgyIdePlugins => discover_agy_ide_plugins(project_path, gemini_root),
        HarnessKind::GeminiCliLegacy => discover_gemini_cli_legacy(project_path, gemini_root),
        _ => unreachable!(),
    }
}

fn format_system_time_epoch(path: &Path) -> String {
    let Ok(meta) = fs::metadata(path) else {
        return String::new();
    };
    let Ok(mtime) = meta.modified() else {
        return String::new();
    };
    let Ok(dur) = mtime.duration_since(UNIX_EPOCH) else {
        return String::new();
    };
    format!("epoch:{}s", dur.as_secs())
}

fn discover_agy_ide_plugins(project_path: &Path, gemini_root: &Path) -> Result<HarnessDiscovery> {
    let norm_project = normalize_project_path(project_path);
    let project_uri = format!("file://{}", norm_project.display());
    let matched_ids = find_matching_config_project_ids(gemini_root, &project_uri);

    let mut rel_paths = Vec::new();
    let mut total_bytes = 0u64;
    let mut latest_time = String::new();
    let mut latest_id = String::new();
    let mut latest_snippet = String::new();

    for id in &matched_ids {
        let rel = format!("config/projects/{}.json", id);
        let full = gemini_root.join(&rel);
        if full.exists() {
            total_bytes += compute_path_size(&full);
            let mut mtime_str = format_system_time_epoch(&full);
            let mut name_label = id.clone();
            if let Ok(content) = fs::read_to_string(&full) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(name) = val.get("name").and_then(|v| v.as_str()) {
                        name_label = format!("{} ({})", name, id);
                    }
                    if let Some(updated_at) = val.get("updatedAt").and_then(|v| v.as_str()) {
                        mtime_str = updated_at.to_string();
                    }
                }
            }
            if mtime_str >= latest_time {
                latest_time = mtime_str;
                latest_id = id.clone();
                latest_snippet = format!("IDE workspace config registered for {}", name_label);
            }
            rel_paths.push(rel);
        }
    }

    Ok(HarnessDiscovery {
        kind: HarnessKind::AgyIdePlugins,
        harness_dir: gemini_root.join("config"),
        has_interactions: !rel_paths.is_empty(),
        session_count: rel_paths.len(),
        total_bytes,
        latest_modified_time: latest_time,
        latest_conversation_id: latest_id,
        latest_prompt_snippet: latest_snippet,
        project_discovery: None,
        gemini_root_relative_paths: rel_paths,
        config_project_ids: matched_ids,
        legacy_project_slug: None,
    })
}

fn discover_gemini_cli_legacy(
    project_path: &Path,
    gemini_root: &Path,
) -> Result<HarnessDiscovery> {
    let norm_project = normalize_project_path(project_path);
    let norm_project_str = norm_project.to_string_lossy().to_string();
    let project_uri = format!("file://{}", norm_project_str);

    let mut matched_slug: Option<String> = None;

    // 1. Check ~/.gemini/projects.json
    let projects_json_path = gemini_root.join("projects.json");
    if let Ok(content) = fs::read_to_string(&projects_json_path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(map) = val.get("projects").and_then(|p| p.as_object()) {
                for (path_key, slug_val) in map {
                    let key_uri = format!("file://{}", path_key.trim_end_matches('/'));
                    if uri_matches_project(&key_uri, &project_uri) {
                        if let Some(slug) = slug_val.as_str() {
                            matched_slug = Some(slug.to_string());
                            break;
                        }
                    }
                }
            }
        }
    }

    // 2. Also scan ~/.gemini/history/*/.project_root and ~/.gemini/tmp/*/.project_root
    if matched_slug.is_none() {
        for sub in ["history", "tmp"] {
            let sub_dir = gemini_root.join(sub);
            let Ok(entries) = fs::read_dir(sub_dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let pr_file = entry.path().join(".project_root");
                if let Ok(root_text) = fs::read_to_string(&pr_file) {
                    let root_uri = format!("file://{}", root_text.trim().trim_end_matches('/'));
                    if uri_matches_project(&root_uri, &project_uri) {
                        matched_slug = Some(entry.file_name().to_string_lossy().to_string());
                        break;
                    }
                }
            }
            if matched_slug.is_some() {
                break;
            }
        }
    }

    let mut rel_paths = Vec::new();
    let mut total_bytes = 0u64;
    let mut latest_time = String::new();
    let mut latest_snippet = String::new();
    let mut session_count = 0usize;

    if let Some(ref slug) = matched_slug {
        session_count = 1;
        for sub in ["history", "tmp"] {
            let rel = format!("{}/{}", sub, slug);
            let full = gemini_root.join(&rel);
            if full.exists() {
                total_bytes += compute_path_size(&full);
                let t = format_system_time_epoch(&full);
                if t > latest_time {
                    latest_time = t;
                }
                rel_paths.push(rel);
            }
        }
        let logs_json = gemini_root.join("tmp").join(slug).join("logs.json");
        if let Ok(content) = fs::read_to_string(&logs_json) {
            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&content) {
                for item in arr.iter().rev() {
                    if item.get("type").and_then(|v| v.as_str()) == Some("user") {
                        if let Some(msg) = item.get("message").and_then(|v| v.as_str()) {
                            let cleaned = clean_prompt_text(msg, 96);
                            if !cleaned.is_empty() {
                                latest_snippet = cleaned;
                                if let Some(ts) = item.get("timestamp").and_then(|v| v.as_str()) {
                                    latest_time = ts.to_string();
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
        if latest_snippet.is_empty() {
            latest_snippet = format!("Legacy gemini-cli workspace state (slug: '{}')", slug);
        }
    }

    let has_interactions = matched_slug.is_some();

    Ok(HarnessDiscovery {
        kind: HarnessKind::GeminiCliLegacy,
        harness_dir: gemini_root.to_path_buf(),
        has_interactions,
        session_count,
        total_bytes,
        latest_modified_time: latest_time,
        latest_conversation_id: matched_slug.clone().unwrap_or_default(),
        latest_prompt_snippet: latest_snippet,
        project_discovery: None,
        gemini_root_relative_paths: rel_paths,
        config_project_ids: BTreeSet::new(),
        legacy_project_slug: matched_slug,
    })
}

/// Synchronizes `config/projects/<id>.json` files between two `~/.gemini` roots,
/// rewriting `from_home` -> `to_home` in `folderUri`.
pub fn sync_config_project_files(
    src_gemini_root: &Path,
    dst_gemini_root: &Path,
    project_ids: &BTreeSet<String>,
    remap_homes: Option<(&str, &str)>,
) -> Result<usize> {
    if project_ids.is_empty() {
        return Ok(0);
    }
    let dst_projects_dir = dst_gemini_root.join("config").join("projects");
    fs::create_dir_all(&dst_projects_dir)?;
    let mut count = 0;

    for id in project_ids {
        let src_file = src_gemini_root
            .join("config")
            .join("projects")
            .join(format!("{}.json", id));
        if !src_file.exists() {
            continue;
        }
        let mut content = fs::read_to_string(&src_file)?;
        if let Some((from_h, to_h)) = remap_homes {
            let f_trim = from_h.trim_end_matches('/');
            let t_trim = to_h.trim_end_matches('/');
            if !f_trim.is_empty() && f_trim != t_trim {
                content = content.replace(f_trim, t_trim);
            }
        }
        let dst_file = dst_projects_dir.join(format!("{}.json", id));
        fs::write(&dst_file, content)?;
        count += 1;
    }
    Ok(count)
}

/// Upserts a `(project_path -> slug)` mapping into `dst_gemini_root/projects.json`
/// and rewrites `.project_root` files in `history/<slug>` and `tmp/<slug>` when `$HOME` remapping is active.
pub fn merge_legacy_gemini_cli_metadata(
    dst_gemini_root: &Path,
    target_project_path: &str,
    slug: &str,
    remap_homes: Option<(&str, &str)>,
) -> Result<()> {
    fs::create_dir_all(dst_gemini_root)?;
    let projects_json_path = dst_gemini_root.join("projects.json");

    let mut root_val: serde_json::Value = if projects_json_path.exists() {
        let content = fs::read_to_string(&projects_json_path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({ "projects": {} }))
    } else {
        serde_json::json!({ "projects": {} })
    };

    if root_val.get("projects").and_then(|p| p.as_object()).is_none() {
        root_val["projects"] = serde_json::json!({});
    }
    if let Some(obj) = root_val.get_mut("projects").and_then(|p| p.as_object_mut()) {
        obj.insert(
            target_project_path.trim_end_matches('/').to_string(),
            serde_json::Value::String(slug.to_string()),
        );
    }
    let formatted = serde_json::to_string_pretty(&root_val)? + "\n";
    fs::write(&projects_json_path, formatted)?;

    if let Some((from_h, to_h)) = remap_homes {
        let f_trim = from_h.trim_end_matches('/');
        let t_trim = to_h.trim_end_matches('/');
        if !f_trim.is_empty() && f_trim != t_trim {
            for sub in ["history", "tmp"] {
                let pr_file = dst_gemini_root.join(sub).join(slug).join(".project_root");
                if let Ok(content) = fs::read_to_string(&pr_file) {
                    let updated = content.replace(f_trim, t_trim);
                    if updated != content {
                        let _ = fs::write(&pr_file, updated);
                    }
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_discover_agy_ide_plugins_and_legacy_cli() -> Result<()> {
        let tmp = TempDir::new()?;
        let gemini_root = tmp.path().join(".gemini");
        let proj_dir = tmp.path().join("workspace/demo-app");
        fs::create_dir_all(&proj_dir)?;
        let norm_proj = normalize_project_path(&proj_dir);

        // 1. Setup config/projects/abc-123.json
        let cfg_dir = gemini_root.join("config/projects");
        fs::create_dir_all(&cfg_dir)?;
        let cfg_json = serde_json::json!({
            "id": "abc-123",
            "name": "demo-app",
            "projectResources": {
                "resources": [
                    { "folderUri": format!("file://{}", norm_proj.display()) }
                ]
            }
        });
        fs::write(
            cfg_dir.join("abc-123.json"),
            serde_json::to_string_pretty(&cfg_json)?,
        )?;

        // 2. Setup projects.json and history/demo-app/.project_root
        let projects_json = serde_json::json!({
            "projects": {
                norm_proj.to_string_lossy().to_string(): "demo-app"
            }
        });
        fs::write(
            gemini_root.join("projects.json"),
            serde_json::to_string_pretty(&projects_json)?,
        )?;
        let hist_dir = gemini_root.join("history/demo-app");
        fs::create_dir_all(&hist_dir)?;
        fs::write(
            hist_dir.join(".project_root"),
            norm_proj.to_string_lossy().as_bytes(),
        )?;

        let kinds = HarnessKind::discover_available_kinds(&gemini_root);
        let all = discover_harnesses(&proj_dir, &gemini_root, &kinds)?;
        assert_eq!(all.len(), 5);

        let plugins = all
            .iter()
            .find(|h| h.kind == HarnessKind::AgyIdePlugins)
            .unwrap();
        assert!(plugins.has_interactions);
        assert!(plugins.config_project_ids.contains("abc-123"));

        let legacy = all
            .iter()
            .find(|h| h.kind == HarnessKind::GeminiCliLegacy)
            .unwrap();
        assert!(legacy.has_interactions);
        assert_eq!(legacy.legacy_project_slug.as_deref(), Some("demo-app"));

        Ok(())
    }
}
