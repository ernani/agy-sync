use crate::discovery::{
    clean_prompt_text, compute_path_size, discover_project_sessions,
    find_matching_config_project_ids, find_summary_pb_files, is_home_or_root_path,
    normalize_project_path, path_to_file_uri, raw_path_to_file_uri, uri_matches_project,
    DiscoveredSession, ProjectDiscovery,
};
use crate::index_merge::{
    apply_home_remap_str, export_project_manifest, merge_project_manifest,
    purge_sessions_from_harness, remap_conversation_artifacts_custom,
    remap_manifest_custom_strings, remap_manifest_project_ids, MergeStats,
};
use crate::protobuf::{extract_summary_metadata_from_pb, ProtobufSummariesMap};
use anyhow::Result;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

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
    let project_uri = path_to_file_uri(&norm_project);
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
    let project_uri = path_to_file_uri(&norm_project);

    let mut matched_slug: Option<String> = None;

    // 1. Check ~/.gemini/projects.json
    let projects_json_path = gemini_root.join("projects.json");
    if let Ok(content) = fs::read_to_string(&projects_json_path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(map) = val.get("projects").and_then(|p| p.as_object()) {
                for (path_key, slug_val) in map {
                    let key_uri = raw_path_to_file_uri(path_key);
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
                    let root_uri = raw_path_to_file_uri(root_text.trim());
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

/// Deterministically generates a valid RFC-4122 v4 UUID string based on the input seed string.
pub fn generate_project_uuid(seed_str: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h1 = DefaultHasher::new();
    seed_str.hash(&mut h1);
    let u1 = h1.finish();

    let mut h2 = DefaultHasher::new();
    (seed_str, u1).hash(&mut h2);
    let u2 = h2.finish();

    let mut b1 = u1.to_be_bytes();
    let mut b2 = u2.to_be_bytes();

    // Set UUID version to 4 (0100)
    b1[6] = (b1[6] & 0x0f) | 0x40;
    // Set UUID variant to RFC 4122 (10xx)
    b2[0] = (b2[0] & 0x3f) | 0x80;

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b1[0], b1[1], b1[2], b1[3],
        b1[4], b1[5],
        b1[6], b1[7],
        b2[0], b2[1],
        b2[2], b2[3], b2[4], b2[5], b2[6], b2[7]
    )
}

/// Ensures a `config/projects/<id>.json` workspace manifest exists for `project_path`.
/// If an existing manifest matches `project_path`'s URI, returns its ID.
/// Otherwise, generates a deterministic UUID and creates the manifest.
pub fn ensure_workspace_project_manifest(
    gemini_root: &Path,
    project_path: &Path,
    project_name: &str,
) -> Result<String> {
    let project_uri = path_to_file_uri(project_path);

    let matched = crate::discovery::find_matching_config_project_ids(gemini_root, &project_uri);
    if let Some(existing_id) = matched.into_iter().next() {
        return Ok(existing_id);
    }

    let new_id = generate_project_uuid(&project_uri);
    let projects_dir = gemini_root.join("config").join("projects");
    fs::create_dir_all(&projects_dir)?;
    let manifest_path = projects_dir.join(format!("{}.json", new_id));

    let manifest_val = serde_json::json!({
        "id": new_id,
        "name": project_name,
        "projectResources": {
            "resources": [
                {
                    "folderUri": project_uri
                }
            ]
        },
        "settings": {},
        "isWorkspaceOnly": false
    });

    let formatted = serde_json::to_string_pretty(&manifest_val)? + "\n";
    fs::write(&manifest_path, formatted)?;
    Ok(new_id)
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
            content = apply_home_remap_str(&content, from_h, to_h);
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
        for sub in ["history", "tmp"] {
            let pr_file = dst_gemini_root.join(sub).join(slug).join(".project_root");
            if let Ok(content) = fs::read_to_string(&pr_file) {
                let updated = apply_home_remap_str(&content, from_h, to_h);
                if updated != content {
                    let _ = fs::write(&pr_file, updated);
                }
            }
        }
    }

    Ok(())
}

/// Placement mode when migrating or bridging a project between two harnesses (`src` and `dst`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementMode {
    /// Synchronize newest sessions & artifacts so the project coexists in both harnesses.
    CoexistInBoth,
    /// Consolidate newest sessions into the source harness (`src`) and remove from `dst`.
    OnlyInSource,
    /// Consolidate newest sessions into the target harness (`dst`) and remove from `src`.
    OnlyInTarget,
}

/// Summary report comparing two workspace folders on disk (e.g., when a project exists in two locations).
#[derive(Debug, Clone, Default)]
pub struct WorkspaceEqualityReport {
    pub path_a: PathBuf,
    pub path_b: PathBuf,
    pub same_canonical_path: bool,
    pub a_exists: bool,
    pub b_exists: bool,
    pub a_git_head: Option<String>,
    pub b_git_head: Option<String>,
    pub a_git_subject: Option<String>,
    pub b_git_subject: Option<String>,
    pub a_dirty_count: usize,
    pub b_dirty_count: usize,
    pub is_equal: bool,
}

impl WorkspaceEqualityReport {
    pub fn summary_line(&self) -> String {
        if self.same_canonical_path {
            return format!(
                "SAME WORKSPACE FOLDER ({})",
                self.path_a.display()
            );
        }
        if self.is_equal {
            let commit = self.a_git_head.as_deref().unwrap_or("no-git");
            format!(
                "EQUAL (Commit: {}, Dirty: 0 on both {} and {})",
                commit,
                self.path_a.display(),
                self.path_b.display()
            )
        } else {
            let a_info = if !self.a_exists {
                "missing".to_string()
            } else {
                format!(
                    "HEAD={} ({}) dirty={}",
                    self.a_git_head.as_deref().unwrap_or("n/a"),
                    self.a_git_subject.as_deref().unwrap_or(""),
                    self.a_dirty_count
                )
            };
            let b_info = if !self.b_exists {
                "missing".to_string()
            } else {
                format!(
                    "HEAD={} ({}) dirty={}",
                    self.b_git_head.as_deref().unwrap_or("n/a"),
                    self.b_git_subject.as_deref().unwrap_or(""),
                    self.b_dirty_count
                )
            };
            format!(
                "DIFFERENT -> [A: {} => {}] vs [B: {} => {}]",
                self.path_a.display(),
                a_info,
                self.path_b.display(),
                b_info
            )
        }
    }
}

fn inspect_git_folder(path: &Path) -> (Option<String>, Option<String>, usize) {
    if !path.join(".git").exists() {
        return (None, None, 0);
    }
    let head = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    let subj = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["log", "-1", "--format=%s (%ci)"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    let dirty = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count()
        })
        .unwrap_or(0);

    (head, subj, dirty)
}

/// Compares two workspace folders (`path_a` and `path_b`) for git commit, working-tree cleanliness,
/// or byte-size equality.
pub fn compare_workspace_equality(path_a: &Path, path_b: &Path) -> WorkspaceEqualityReport {
    let norm_a = normalize_project_path(path_a);
    let norm_b = normalize_project_path(path_b);
    let same_canonical_path = norm_a == norm_b;
    let a_exists = norm_a.exists();
    let b_exists = norm_b.exists();

    if same_canonical_path {
        let (ah, asub, ad) = inspect_git_folder(&norm_a);
        return WorkspaceEqualityReport {
            path_a: norm_a,
            path_b: norm_b,
            same_canonical_path: true,
            a_exists,
            b_exists,
            a_git_head: ah.clone(),
            b_git_head: ah,
            a_git_subject: asub.clone(),
            b_git_subject: asub,
            a_dirty_count: ad,
            b_dirty_count: ad,
            is_equal: true,
        };
    }

    let (a_git_head, a_git_subject, a_dirty_count) = if a_exists {
        inspect_git_folder(&norm_a)
    } else {
        (None, None, 0)
    };
    let (b_git_head, b_git_subject, b_dirty_count) = if b_exists {
        inspect_git_folder(&norm_b)
    } else {
        (None, None, 0)
    };

    let is_equal = if !a_exists || !b_exists {
        false
    } else if a_git_head.is_some() && b_git_head.is_some() {
        a_git_head == b_git_head && a_dirty_count == 0 && b_dirty_count == 0
    } else {
        compute_path_size(&norm_a) == compute_path_size(&norm_b)
    };

    WorkspaceEqualityReport {
        path_a: norm_a,
        path_b: norm_b,
        same_canonical_path: false,
        a_exists,
        b_exists,
        a_git_head,
        b_git_head,
        a_git_subject,
        b_git_subject,
        a_dirty_count,
        b_dirty_count,
        is_equal,
    }
}

/// Scans `harness_dir` (`conversation_summaries.db`, `*_summaries_proto.pb`, `history.jsonl`,
/// and `../config/projects/*.json` referenced by `project_id`) to return all distinct project
/// workspace paths that have conversations in `harness_dir`.
pub fn discover_all_projects_in_harness(
    harness_dir: &Path,
    gemini_root: &Path,
    local_home: &str,
) -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();

    // Load project_id -> folderUri map from config/projects/*.json
    let mut pid_to_uri: BTreeMap<String, String> = BTreeMap::new();
    let cfg_dir = gemini_root.join("config").join("projects");
    if let Ok(entries) = fs::read_dir(&cfg_dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(txt) = fs::read_to_string(&p) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&txt) {
                    let id = val.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    if let Some(uri) = val
                        .get("projectResources")
                        .and_then(|pr| pr.get("resources"))
                        .and_then(|r| r.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|item| item.get("folderUri"))
                        .and_then(|u| u.as_str())
                    {
                        if !id.is_empty() && !uri.is_empty() {
                            pid_to_uri.insert(id.to_string(), uri.to_string());
                        }
                    }
                }
            }
        }
    }

    let mut raw_uris = Vec::new();
    let db_path = harness_dir.join("conversation_summaries.db");
    if db_path.exists() {
        if let Ok(conn) = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            let _ = conn.busy_timeout(std::time::Duration::from_millis(2000));
            if let Ok(mut stmt) = conn.prepare(
                "SELECT workspace_uris, project_id FROM conversation_summaries WHERE parent_conversation_id = ''",
            ) {
                if let Ok(rows) = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0).unwrap_or_default(),
                        r.get::<_, String>(1).unwrap_or_default(),
                    ))
                }) {
                    for (ws_raw, pid) in rows.flatten() {
                        if !ws_raw.trim().is_empty() {
                            if let Ok(parsed) = serde_json::from_str::<Vec<String>>(&ws_raw) {
                                raw_uris.extend(parsed);
                            } else {
                                raw_uris.push(ws_raw);
                            }
                        } else if let Some(mapped_uri) = pid_to_uri.get(&pid) {
                            raw_uris.push(mapped_uri.clone());
                        }
                    }
                }
            }
        }
    }

    for pb_path in find_summary_pb_files(harness_dir) {
        if let Ok(pb_map) = ProtobufSummariesMap::load_from_path(&pb_path) {
            for raw_bytes in pb_map.entries().values() {
                let (_, uris) = extract_summary_metadata_from_pb(raw_bytes);
                raw_uris.extend(uris);
            }
        }
    }

    let hist_path = harness_dir.join("history.jsonl");
    if hist_path.exists() {
        if let Ok(content) = fs::read_to_string(&hist_path) {
            for line in content.lines() {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(ws) = val
                        .get("workspace")
                        .or_else(|| val.get("cwd"))
                        .and_then(|v| v.as_str())
                    {
                        raw_uris.push(raw_path_to_file_uri(ws));
                    }
                }
            }
        }
    }

    for u in raw_uris {
        let stripped = u.trim().trim_start_matches("file://").trim_end_matches('/');
        if stripped.is_empty() {
            continue;
        }
        let candidate_path = if let Some((_, _, rel)) = crate::discovery::split_home_and_relative(stripped) {
            if rel.is_empty() {
                continue;
            }
            PathBuf::from(format!("{}{}", local_home.trim_end_matches('/'), rel))
        } else {
            PathBuf::from(stripped)
        };
        if !is_home_or_root_path(&candidate_path, local_home) {
            paths.insert(normalize_project_path(&candidate_path));
        }
    }

    Ok(paths)
}

/// Appends any root sessions from `sessions` that are not yet present in `<cli_harness_dir>/history.jsonl`
/// and registers `project_path` in `<cli_harness_dir>/settings.json` (`trustedWorkspaces`).
pub fn sync_cli_history_and_trust(
    cli_harness_dir: &Path,
    project_path: &Path,
    sessions: &BTreeMap<String, DiscoveredSession>,
    dry_run: bool,
) -> Result<usize> {
    if dry_run {
        return Ok(sessions
            .values()
            .filter(|s| s.parent_conversation_id.is_empty())
            .count());
    }
    fs::create_dir_all(cli_harness_dir)?;

    let ws_str = project_path.to_string_lossy().to_string();
    let hist_path = cli_harness_dir.join("history.jsonl");
    let existing_hist = fs::read_to_string(&hist_path).unwrap_or_default();

    let mut added = 0usize;
    let mut new_lines = Vec::new();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1_760_000_000_000);

    for sess in sessions.values() {
        if !sess.parent_conversation_id.is_empty() {
            continue;
        }
        if existing_hist.contains(&sess.conversation_id) {
            continue;
        }
        let display = if !sess.last_prompt_snippet.is_empty() {
            sess.last_prompt_snippet.clone()
        } else if !sess.title.is_empty() {
            sess.title.clone()
        } else {
            format!("Session {}", &sess.conversation_id[..8])
        };
        let entry = serde_json::json!({
            "display": display,
            "timestamp": now_ms,
            "workspace": ws_str,
            "conversationId": sess.conversation_id,
        });
        if let Ok(line) = serde_json::to_string(&entry) {
            new_lines.push(line);
            added += 1;
        }
    }

    if !new_lines.is_empty() {
        let mut combined = existing_hist;
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&new_lines.join("\n"));
        combined.push('\n');
        fs::write(&hist_path, combined)?;
    }

    // Update settings.json trustedWorkspaces if settings.json exists
    let settings_path = cli_harness_dir.join("settings.json");
    if settings_path.exists() {
        if let Ok(content) = fs::read_to_string(&settings_path) {
            if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(arr) = val
                    .get_mut("trustedWorkspaces")
                    .and_then(|v| v.as_array_mut())
                {
                    let already = arr.iter().any(|x| x.as_str() == Some(ws_str.as_str()));
                    if !already {
                        arr.push(serde_json::Value::String(ws_str));
                        if let Ok(pretty) = serde_json::to_string_pretty(&val) {
                            let _ = fs::write(&settings_path, format!("{}\n", pretty));
                        }
                    }
                }
            }
        }
    }

    Ok(added)
}

/// Ensures an `agy-2.0` (`~/.gemini/antigravity`) harness directory has completed onboarding
/// (`settings.json` with `"onboardingComplete": true` + `project_path` in `"trustedWorkspaces"`,
/// and `antigravity_state.pbtxt`) without copying unrelated local workspace paths across hosts.
pub fn ensure_agy20_state_and_trust(
    harness_dir: &Path,
    project_path: &Path,
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        return Ok(());
    }
    fs::create_dir_all(harness_dir)?;

    let ws_str = project_path.to_string_lossy().to_string();
    let settings_path = harness_dir.join("settings.json");
    let mut settings_obj = if settings_path.exists() {
        fs::read_to_string(&settings_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .filter(|v| v.is_object())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if let Some(map) = settings_obj.as_object_mut() {
        map.entry("onboardingComplete".to_string())
            .or_insert(serde_json::Value::Bool(true));
        let arr_val = map
            .entry("trustedWorkspaces".to_string())
            .or_insert_with(|| serde_json::json!([]));
        if !arr_val.is_array() {
            *arr_val = serde_json::json!([]);
        }
        if let Some(arr) = arr_val.as_array_mut() {
            if !ws_str.is_empty() && !arr.iter().any(|x| x.as_str() == Some(ws_str.as_str())) {
                arr.push(serde_json::Value::String(ws_str));
            }
        }
    }

    if let Ok(pretty) = serde_json::to_string_pretty(&settings_obj) {
        let _ = fs::write(&settings_path, format!("{}\n", pretty));
    }

    let state_pbtxt_path = harness_dir.join("antigravity_state.pbtxt");
    if !state_pbtxt_path.exists() {
        let default_pbtxt = concat!(
            "post_onboarding: {\n",
            "  completed_steps: POST_ONBOARDING_STEP_TYPE_MANAGER_WELCOME\n",
            "  completed_steps: POST_ONBOARDING_STEP_TYPE_USAGE_MODE\n",
            "  completed_steps: POST_ONBOARDING_STEP_TYPE_AGENT_CONFIGURATION\n",
            "  completed_steps: POST_ONBOARDING_STEP_TYPE_ADD_WORKSPACE\n",
            "}\n",
            "agent_onboarding_completed: AGENT_ONBOARDING_STATE_COMPLETED\n",
            "migrate_convos_into_projects: MIGRATION_STATUS_COMPLETED\n",
            "migrate_retroactive_projects: RETROACTIVE_MIGRATION_STATUS_COMPLETED_UNNECESSARY\n",
        );
        let _ = fs::write(&state_pbtxt_path, default_pbtxt);
    }

    Ok(())
}

fn copy_session_artifact_rel(src_root: &Path, dst_root: &Path, rel: &str) -> Result<()> {
    let src_full = src_root.join(rel);
    let dst_full = dst_root.join(rel);
    if !src_full.exists() {
        return Ok(());
    }
    if src_full.is_file() {
        if let Some(p) = dst_full.parent() {
            fs::create_dir_all(p)?;
        }
        fs::copy(&src_full, &dst_full)?;
    } else if src_full.is_dir() {
        copy_dir_all_local(&src_full, &dst_full)?;
    }
    Ok(())
}

fn copy_dir_all_local(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)?.flatten() {
        let p = entry.path();
        let target = dst.join(entry.file_name());
        if p.is_dir() {
            copy_dir_all_local(&p, &target)?;
        } else if p.is_file() {
            fs::copy(&p, &target)?;
        }
    }
    Ok(())
}

/// Transfers or bridges a project's sessions between `src_harness_dir` and `dst_harness_dir` on the local machine
/// according to `placement` (`CoexistInBoth`, `OnlyInSource`, or `OnlyInTarget`), optionally relocating the
/// workspace path (`src_project_path` -> `dst_project_path`).
pub fn migrate_project_between_local_harnesses(
    gemini_root: &Path,
    src_harness_dir: &Path,
    dst_harness_dir: &Path,
    src_project_path: &Path,
    dst_project_path: &Path,
    placement: PlacementMode,
    deep_remap: bool,
    dry_run: bool,
) -> Result<(MergeStats, usize)> {
    let norm_src_proj = normalize_project_path(src_project_path);
    let norm_dst_proj = normalize_project_path(dst_project_path);

    let src_disc = discover_project_sessions(&norm_src_proj, src_harness_dir)?;
    let dst_disc = discover_project_sessions(&norm_dst_proj, dst_harness_dir)?;

    let mut all_cids: BTreeSet<String> = src_disc.sessions.keys().cloned().collect();
    all_cids.extend(dst_disc.sessions.keys().cloned());
    if all_cids.is_empty() {
        return Ok((MergeStats::default(), 0));
    }

    let src_sub = src_harness_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("antigravity");
    let dst_sub = dst_harness_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("antigravity-cli");

    if dry_run {
        let stats = MergeStats {
            summaries_upserted: all_cids.len(),
            summaries_skipped_older: 0,
            fts_chunks_updated: all_cids.len(),
            protobuf_entries_updated: all_cids.len(),
        };
        let purged = match placement {
            PlacementMode::CoexistInBoth => 0,
            PlacementMode::OnlyInTarget | PlacementMode::OnlyInSource => all_cids.len(),
        };
        return Ok((stats, purged));
    }

    let proj_name = norm_dst_proj
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let target_pid = ensure_workspace_project_manifest(gemini_root, &norm_dst_proj, proj_name).ok();

    let mut total_stats = MergeStats::default();
    let mut purged_count = 0usize;

    // 1. Copy/merge from src -> dst if CoexistInBoth or OnlyInTarget
    if matches!(placement, PlacementMode::CoexistInBoth | PlacementMode::OnlyInTarget) {
        let mut to_copy_to_dst = BTreeSet::new();
        for cid in &all_cids {
            let s_opt = src_disc.sessions.get(cid);
            let d_opt = dst_disc.sessions.get(cid);
            let copy_it = match (s_opt, d_opt) {
                (Some(s), None) => !s.relative_paths.is_empty() || s.step_count > 0,
                (Some(s), Some(d)) => {
                    s.step_count > d.step_count
                        || (s.step_count == d.step_count
                            && s.last_modified_time >= d.last_modified_time)
                        || d.relative_paths.is_empty()
                        || (d.title.trim().is_empty() && !s.title.trim().is_empty())
                }
                _ => false,
            };
            if copy_it {
                to_copy_to_dst.insert(cid.clone());
                if let Some(s) = s_opt {
                    for rel in &s.relative_paths {
                        copy_session_artifact_rel(src_harness_dir, dst_harness_dir, rel)?;
                    }
                }
            }
        }
        for kp in &src_disc.knowledge_paths {
            copy_session_artifact_rel(src_harness_dir, dst_harness_dir, kp)?;
        }

        if !to_copy_to_dst.is_empty() {
            let mut replacements = vec![(
                format!("/.gemini/{}", src_sub),
                format!("/.gemini/{}", dst_sub),
            )];
            if norm_src_proj != norm_dst_proj {
                replacements.push((
                    norm_src_proj.to_string_lossy().to_string(),
                    norm_dst_proj.to_string_lossy().to_string(),
                ));
            }
            remap_conversation_artifacts_custom(
                dst_harness_dir,
                &to_copy_to_dst,
                &replacements,
                deep_remap,
            )?;
            let mut manifest = export_project_manifest(src_harness_dir, &to_copy_to_dst, None)?;
            remap_manifest_custom_strings(&mut manifest, &replacements);
            if let Some(ref pid) = target_pid {
                remap_manifest_project_ids(&mut manifest, pid);
            }
            let st = merge_project_manifest(dst_harness_dir, &manifest, true)?;
            total_stats.summaries_upserted += st.summaries_upserted;
            total_stats.fts_chunks_updated += st.fts_chunks_updated;
            total_stats.protobuf_entries_updated += st.protobuf_entries_updated;
        }

        if dst_sub == "antigravity-cli" || dst_harness_dir.join("history.jsonl").exists() {
            let refreshed_dst = discover_project_sessions(&norm_dst_proj, dst_harness_dir)?;
            let _ = sync_cli_history_and_trust(
                dst_harness_dir,
                &norm_dst_proj,
                &refreshed_dst.sessions,
                false,
            );
        }
        if dst_sub == "antigravity" {
            let _ = ensure_agy20_state_and_trust(dst_harness_dir, &norm_dst_proj, false);
        }
    }

    // 2. Copy/merge from dst -> src if CoexistInBoth or OnlyInSource
    if matches!(placement, PlacementMode::CoexistInBoth | PlacementMode::OnlyInSource) {
        let mut to_copy_to_src = BTreeSet::new();
        for cid in &all_cids {
            let s_opt = src_disc.sessions.get(cid);
            let d_opt = dst_disc.sessions.get(cid);
            let copy_it = match (s_opt, d_opt) {
                (None, Some(d)) => !d.relative_paths.is_empty() || d.step_count > 0,
                (Some(s), Some(d)) => {
                    d.step_count > s.step_count
                        || (d.step_count == s.step_count
                            && d.last_modified_time > s.last_modified_time)
                        || s.relative_paths.is_empty()
                        || (s.title.trim().is_empty() && !d.title.trim().is_empty())
                }
                _ => false,
            };
            if copy_it {
                to_copy_to_src.insert(cid.clone());
                if let Some(d) = d_opt {
                    for rel in &d.relative_paths {
                        copy_session_artifact_rel(dst_harness_dir, src_harness_dir, rel)?;
                    }
                }
            }
        }
        for kp in &dst_disc.knowledge_paths {
            copy_session_artifact_rel(dst_harness_dir, src_harness_dir, kp)?;
        }

        if !to_copy_to_src.is_empty() {
            let mut replacements = vec![(
                format!("/.gemini/{}", dst_sub),
                format!("/.gemini/{}", src_sub),
            )];
            if norm_src_proj != norm_dst_proj {
                replacements.push((
                    norm_src_proj.to_string_lossy().to_string(),
                    norm_dst_proj.to_string_lossy().to_string(),
                ));
            }
            remap_conversation_artifacts_custom(
                src_harness_dir,
                &to_copy_to_src,
                &replacements,
                deep_remap,
            )?;
            let mut manifest = export_project_manifest(dst_harness_dir, &to_copy_to_src, None)?;
            remap_manifest_custom_strings(&mut manifest, &replacements);
            if let Some(ref pid) = target_pid {
                remap_manifest_project_ids(&mut manifest, pid);
            }
            let st = merge_project_manifest(src_harness_dir, &manifest, true)?;
            total_stats.summaries_upserted += st.summaries_upserted;
            total_stats.fts_chunks_updated += st.fts_chunks_updated;
            total_stats.protobuf_entries_updated += st.protobuf_entries_updated;
        } else if norm_src_proj != norm_dst_proj && !src_disc.sessions.is_empty() {
            // Even if no sessions were newer on dst, relocate src's own sessions to norm_dst_proj
            let src_cids: BTreeSet<String> = src_disc.sessions.keys().cloned().collect();
            let replacements = vec![(
                norm_src_proj.to_string_lossy().to_string(),
                norm_dst_proj.to_string_lossy().to_string(),
            )];
            remap_conversation_artifacts_custom(
                src_harness_dir,
                &src_cids,
                &replacements,
                deep_remap,
            )?;
            let mut manifest = export_project_manifest(src_harness_dir, &src_cids, None)?;
            remap_manifest_custom_strings(&mut manifest, &replacements);
            if let Some(ref pid) = target_pid {
                remap_manifest_project_ids(&mut manifest, pid);
            }
            let st = merge_project_manifest(src_harness_dir, &manifest, true)?;
            total_stats.summaries_upserted += st.summaries_upserted;
            total_stats.fts_chunks_updated += st.fts_chunks_updated;
            total_stats.protobuf_entries_updated += st.protobuf_entries_updated;
        }

        if src_sub == "antigravity-cli" || src_harness_dir.join("history.jsonl").exists() {
            let refreshed_src = discover_project_sessions(&norm_dst_proj, src_harness_dir)?;
            let _ = sync_cli_history_and_trust(
                src_harness_dir,
                &norm_dst_proj,
                &refreshed_src.sessions,
                false,
            );
        }
        if src_sub == "antigravity" {
            let _ = ensure_agy20_state_and_trust(src_harness_dir, &norm_dst_proj, false);
        }
    }

    // 3. If OnlyInTarget, purge from src; if OnlyInSource, purge from dst
    match placement {
        PlacementMode::OnlyInTarget => {
            let src_cids: BTreeSet<String> = src_disc.sessions.keys().cloned().collect();
            purged_count = purge_sessions_from_harness(src_harness_dir, &src_cids, false)?;
        }
        PlacementMode::OnlyInSource => {
            let dst_cids: BTreeSet<String> = dst_disc.sessions.keys().cloned().collect();
            purged_count = purge_sessions_from_harness(dst_harness_dir, &dst_cids, false)?;
        }
        PlacementMode::CoexistInBoth => {}
    }

    Ok((total_stats, purged_count))
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

    #[test]
    fn test_ensure_workspace_project_manifest() -> Result<()> {
        let tmp = TempDir::new()?;
        let gemini_root = tmp.path().join(".gemini");
        let proj_dir = tmp.path().join("code/test-app");
        fs::create_dir_all(&proj_dir)?;

        let id1 = ensure_workspace_project_manifest(&gemini_root, &proj_dir, "test-app")?;
        assert_eq!(id1.len(), 36);
        let cfg_path = gemini_root.join(format!("config/projects/{}.json", id1));
        assert!(cfg_path.exists());

        // Calling it again on the same path returns the same ID without duplicate
        let id2 = ensure_workspace_project_manifest(&gemini_root, &proj_dir, "test-app")?;
        assert_eq!(id1, id2);
        Ok(())
    }

    #[test]
    fn test_migrate_coexist_and_only_in_target_with_relocation() -> Result<()> {
        let tmp = TempDir::new()?;
        let gemini_root = tmp.path().join(".gemini");
        let harness_a = gemini_root.join("custom-store");
        let harness_b = gemini_root.join("antigravity-cli");
        let old_proj = tmp.path().join("Documents/old-projects/demo-service");
        let new_proj = tmp.path().join("Documents/new-projects/demo-service");
        fs::create_dir_all(&harness_a)?;
        fs::create_dir_all(&harness_b)?;
        fs::create_dir_all(&old_proj)?;
        fs::create_dir_all(&new_proj)?;

        let norm_old = normalize_project_path(&old_proj);
        let norm_new = normalize_project_path(&new_proj);
        let old_uri = path_to_file_uri(&norm_old);

        let cid = "11111111-2222-3333-4444-555555555555";
        let db_a = harness_a.join("conversation_summaries.db");
        let conn = Connection::open(&db_a)?;
        crate::index_merge::ensure_summary_db_schema(&conn)?;
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, preview, step_count, last_modified_time, workspace_uris, app_data_dir)
             VALUES (?1, 'Service Design', 'Initial prompt', 25, '2026-11-09T10:00:00Z', ?2, 'custom-store')",
            rusqlite::params![cid, format!("[\"{}\"]", old_uri)],
        )?;
        drop(conn);

        fs::create_dir_all(harness_a.join("conversations"))?;
        fs::write(
            harness_a.join(format!("conversations/{}.db", cid)),
            b"sqlite-payload",
        )?;
        let brain_a = harness_a.join("brain").join(cid);
        fs::create_dir_all(&brain_a)?;
        fs::write(
            brain_a.join("notes.md"),
            format!("Link: {}/.gemini/custom-store/brain/{}/notes.md", tmp.path().display(), cid),
        )?;

        // 1. CoexistInBoth + relocate old_proj -> new_proj
        let (stats, purged) = migrate_project_between_local_harnesses(
            &gemini_root,
            &harness_a,
            &harness_b,
            &norm_old,
            &norm_new,
            PlacementMode::CoexistInBoth,
            false,
            false,
        )?;
        assert_eq!(purged, 0);
        assert!(stats.summaries_upserted >= 1);
        assert!(harness_b.join(format!("conversations/{}.db", cid)).exists());
        assert!(harness_b.join("history.jsonl").exists());

        let notes_b = fs::read_to_string(harness_b.join("brain").join(cid).join("notes.md"))?;
        assert!(notes_b.contains("/.gemini/antigravity-cli/brain/"));

        let disc_b = discover_project_sessions(&norm_new, &harness_b)?;
        assert!(disc_b.sessions.contains_key(cid));

        // 2. OnlyInTarget (removes from harness_a while keeping in harness_b)
        let (_stats2, purged2) = migrate_project_between_local_harnesses(
            &gemini_root,
            &harness_a,
            &harness_b,
            &norm_new,
            &norm_new,
            PlacementMode::OnlyInTarget,
            false,
            false,
        )?;
        assert_eq!(purged2, 1);
        assert!(!harness_a.join(format!("conversations/{}.db", cid)).exists());
        assert!(harness_b.join(format!("conversations/{}.db", cid)).exists());

        Ok(())
    }
}
