use crate::protobuf::{extract_summary_metadata_from_pb, ProtobufSummariesMap};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Summary metadata for a single discovered conversation belonging to the project.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredSession {
    pub conversation_id: String,
    pub title: String,
    pub step_count: i64,
    pub last_modified_time: String,
    pub parent_conversation_id: String,
    pub agent_name: String,
    pub status: String,
    pub project_id: String,
    pub workspace_uris: Vec<String>,
    pub last_prompt_snippet: String,
    pub is_orphan_brain: bool,
    /// Relative paths inside the harness directory belonging to this specific conversation
    pub relative_paths: Vec<String>,
    /// Total size in bytes on disk across all artifacts for this conversation
    pub total_bytes: u64,
}

/// Complete discovery report for a project workspace inside a harness directory (e.g. `~/.gemini/antigravity`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDiscovery {
    pub project_path: PathBuf,
    pub project_uri: String,
    pub harness_dir: PathBuf,
    pub sessions: BTreeMap<String, DiscoveredSession>,
    /// Knowledge directories (`knowledge/<topic>`) referencing any conversation in `sessions`
    pub knowledge_paths: Vec<String>,
    /// Matched `project_id` JSON config files (`config/projects/<id>.json` relative to `~/.gemini`)
    pub matched_project_ids: BTreeSet<String>,
    /// All relative paths inside `harness_dir` to sync via `rsync --files-from`
    pub all_relative_paths: Vec<String>,
    pub total_bytes: u64,
}

impl ProjectDiscovery {
    /// Returns the most recently modified session (by `last_modified_time`, then `step_count`).
    pub fn latest_session(&self) -> Option<&DiscoveredSession> {
        self.sessions.values().max_by(|a, b| {
            a.last_modified_time
                .cmp(&b.last_modified_time)
                .then_with(|| a.step_count.cmp(&b.step_count))
        })
    }
}

/// Normalize a project path (strip trailing slashes, canonicalize if existing).
pub fn normalize_project_path(path: &Path) -> PathBuf {
    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = canonical.to_string_lossy().trim_end_matches('/').to_string();
    PathBuf::from(if s.is_empty() { "/" } else { &s })
}

/// Extracts `(home_dir, username, relative_path)` from a filesystem path or `file://` URI
/// for standard Unix/macOS home directories (`/Users/<user>[/...]` or `[/usr/...]/home/<user>[/...]`).
pub fn split_home_and_relative(raw: &str) -> Option<(&str, &str, &str)> {
    let path = raw
        .strip_prefix("file://")
        .unwrap_or(raw)
        .trim_end_matches('/');
    if let Some(after_users) = path.strip_prefix("/Users/") {
        if after_users.is_empty() {
            return None;
        }
        return match after_users.find('/') {
            Some(idx) => {
                let user = &after_users[..idx];
                let home = &path[..7 + idx];
                let rel = &after_users[idx..];
                Some((home, user, rel))
            }
            None => Some((path, after_users, "")),
        };
    }
    if path.starts_with("/home/") || path.starts_with("/usr/") {
        if let Some(pos) = path.find("/home/") {
            let after_home = &path[pos + 6..];
            if after_home.is_empty() {
                return None;
            }
            return match after_home.find('/') {
                Some(idx) => {
                    let user = &after_home[..idx];
                    let home = &path[..pos + 6 + idx];
                    let rel = &after_home[idx..];
                    Some((home, user, rel))
                }
                None => Some((path, after_home, "")),
            };
        }
    }
    None
}

/// Returns true if `path` is a user `$HOME` directory root (or filesystem root `/`).
pub fn is_home_or_root_path(path: &Path, local_home: &str) -> bool {
    let norm = normalize_project_path(path);
    let s = norm.to_string_lossy();
    let trimmed = s.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" || trimmed == "/Users" || trimmed == "/home" {
        return true;
    }
    if !local_home.is_empty() && trimmed == local_home.trim_end_matches('/') {
        return true;
    }
    if let Some((_, _, rel)) = split_home_and_relative(trimmed) {
        if rel.is_empty() {
            return true;
        }
    }
    false
}

/// Check if a workspace URI string matches the target `project_uri` (exact match or sub-directory).
///
/// Special cases:
/// 1. If `project_uri` is a user's `$HOME` root (`rel == ""`), only an exact match is allowed
///    so running in `$HOME` never sweeps up child projects under `~/Documents/...`.
/// 2. If `uri` and `project_uri` belong to the same username (`user_u == user_p`) under different
///    OS `$HOME` mount points (e.g. macOS `/Users/<user>/...` vs Linux `[.../]/home/<user>/...`),
///    their relative project paths are compared so unremapped sessions are still discovered.
pub fn uri_matches_project(uri: &str, project_uri: &str) -> bool {
    let u = uri.trim_end_matches('/');
    let p = project_uri.trim_end_matches('/');

    if let (Some((_, user_u, rel_u)), Some((_, user_p, rel_p))) =
        (split_home_and_relative(u), split_home_and_relative(p))
    {
        if rel_p.is_empty() {
            return user_u == user_p && rel_u.is_empty();
        }
        if user_u == user_p {
            if rel_u == rel_p {
                return true;
            }
            if let Some(rest) = rel_u.strip_prefix(rel_p) {
                return rest.starts_with('/');
            }
            return false;
        }
    }

    if u == p {
        return true;
    }
    if p == "file://" || p == "file:///" || p == "/" {
        return false;
    }
    if let Some(rest) = u.strip_prefix(p) {
        return rest.starts_with('/');
    }
    false
}

/// Validates whether a string is a standard 36-char lowercase hex UUID.
pub fn is_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    for (i, b) in s.bytes().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if b != b'-' {
                    return false;
                }
            }
            _ => {
                if !b.is_ascii_hexdigit() {
                    return false;
                }
            }
        }
    }
    true
}

/// Returns all `*_summaries_proto.pb` files inside `harness_dir`.
pub fn find_summary_pb_files(harness_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(harness_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with("_summaries_proto.pb") {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    out
}

/// Passively checkpoints SQLite WAL files for `conversation_summaries.db` and matched `conversations/<id>.db`
/// so all committed WAL frames are flushed into the main `.db` file prior to `rsync`.
pub fn checkpoint_sqlite_dbs(harness_dir: &Path, conversation_ids: &BTreeSet<String>) {
    let main_db = harness_dir.join("conversation_summaries.db");
    checkpoint_single_db(&main_db);
    for cid in conversation_ids {
        let conv_db = harness_dir.join("conversations").join(format!("{}.db", cid));
        checkpoint_single_db(&conv_db);
    }
}

fn checkpoint_single_db(db_path: &Path) {
    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    if db_path.exists() && wal_path.exists() {
        if let Ok(conn) = Connection::open(db_path) {
            let _ = conn.busy_timeout(std::time::Duration::from_millis(1500));
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        }
    }
}

/// Recursively computes the byte size of a file or directory.
pub fn compute_path_size(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_file() {
        return meta.len();
    }
    if meta.is_dir() {
        let mut total = 0;
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                total += compute_path_size(&entry.path());
            }
        }
        return total;
    }
    0
}

/// Strips XML wrapper tags like `<USER_REQUEST>...</USER_REQUEST>` and collapses whitespace.
pub fn clean_prompt_text(raw: &str, max_chars: usize) -> String {
    let mut text = raw.to_string();
    if let Some(start) = text.find("<USER_REQUEST>") {
        text = text[start + "<USER_REQUEST>".len()..].to_string();
    }
    if let Some(end) = text.find("</USER_REQUEST>") {
        text = text[..end].to_string();
    }
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > max_chars {
        let truncated: String = collapsed.chars().take(max_chars).collect();
        format!("{}...", truncated)
    } else {
        collapsed
    }
}

/// Extracts the most recent user prompt snippet and optional timestamp for a conversation.
pub fn extract_last_user_prompt(
    harness_dir: &Path,
    cid: &str,
    db_preview: &str,
    history_prompts: &BTreeMap<String, String>,
) -> (Option<String>, String) {
    // 1. Check brain/<cid>/.system_generated/logs/transcript.jsonl for the last USER_INPUT step
    let transcript_path = harness_dir
        .join("brain")
        .join(cid)
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl");
    if let Ok(content) = fs::read_to_string(&transcript_path) {
        let mut last_created_at = None;
        let mut last_prompt = None;
        for line in content.lines().rev() {
            if line.contains("\"USER_INPUT\"") {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    if val.get("type").and_then(|v| v.as_str()) == Some("USER_INPUT") {
                        if let Some(c) = val.get("content").and_then(|v| v.as_str()) {
                            let cleaned = clean_prompt_text(c, 96);
                            if !cleaned.is_empty() {
                                last_prompt = Some(cleaned);
                                last_created_at = val
                                    .get("created_at")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                break;
                            }
                        }
                    }
                }
            }
        }
        if let Some(prompt) = last_prompt {
            return (last_created_at, prompt);
        }
    }

    // 2. Fallback to history.jsonl prompt if available
    if let Some(hist) = history_prompts.get(cid) {
        let cleaned = clean_prompt_text(hist, 96);
        if !cleaned.is_empty() {
            return (None, cleaned);
        }
    }

    // 3. Fallback to conversation_summaries.preview
    let cleaned_preview = clean_prompt_text(db_preview, 96);
    (None, cleaned_preview)
}

/// Scans `~/.gemini/config/projects/*.json` to find project IDs whose `folderUri` matches `project_uri`.
pub fn find_matching_config_project_ids(
    gemini_root: &Path,
    project_uri: &str,
) -> BTreeSet<String> {
    let mut matched = BTreeSet::new();
    let projects_dir = gemini_root.join("config").join("projects");
    let Ok(entries) = fs::read_dir(projects_dir) else {
        return matched;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) else {
            continue;
        };
        let Some(id) = val.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(resources) = val
            .get("projectResources")
            .and_then(|pr| pr.get("resources"))
            .and_then(|r| r.as_array())
        {
            for res in resources {
                if let Some(f_uri) = res.get("folderUri").and_then(|u| u.as_str()) {
                    if uri_matches_project(f_uri, project_uri) {
                        matched.insert(id.to_string());
                    }
                }
            }
        }
    }
    matched
}

/// Scans `brain/` directories that are not already in `matched_ids` to see if any file inside
/// them references `project_path_str` or `project_uri`.
fn scan_orphan_brains(
    brain_root: &Path,
    already_known: &BTreeSet<String>,
    project_path_str: &str,
    project_uri: &str,
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let Ok(entries) = fs::read_dir(brain_root) else {
        return found;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_uuid(name) || already_known.contains(name) {
            continue;
        }
        if dir_mentions_project(&path, project_path_str, project_uri, 0) {
            found.insert(name.to_string());
        }
    }
    found
}

fn dir_mentions_project(
    dir: &Path,
    project_path_str: &str,
    project_uri: &str,
    depth: usize,
) -> bool {
    if depth > 3 {
        return false;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if dir_mentions_project(&path, project_path_str, project_uri, depth + 1) {
                return true;
            }
        } else if path.is_file() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "md" | "json" | "jsonl" | "txt") {
                if let Ok(mut f) = fs::File::open(&path) {
                    let mut buf = vec![0u8; 256 * 1024];
                    if let Ok(n) = f.read(&mut buf) {
                        let s = String::from_utf8_lossy(&buf[..n]);
                        if s.contains(project_uri) || s.contains(project_path_str) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

/// Scans `transcript.jsonl` inside a session's `brain/<id>/.system_generated/logs/`
/// to discover any subagent conversation IDs (`conversation://<uuid>`).
fn extract_subagents_from_transcript(brain_session_dir: &Path) -> BTreeSet<String> {
    let mut subagents = BTreeSet::new();
    let transcript = brain_session_dir
        .join(".system_generated")
        .join("logs")
        .join("transcript.jsonl");
    let Ok(content) = fs::read_to_string(transcript) else {
        return subagents;
    };
    for part in content.split("conversation://").skip(1) {
        if part.len() >= 36 {
            let candidate = &part[..36];
            if is_uuid(candidate) {
                subagents.insert(candidate.to_string());
            }
        }
    }
    subagents
}

#[derive(Debug, Clone)]
struct RawDbRow {
    conversation_id: String,
    title: String,
    preview: String,
    step_count: i64,
    last_modified_time: String,
    parent_conversation_id: String,
    agent_name: String,
    status: String,
    project_id: String,
    workspace_uris: Vec<String>,
}

/// Discovers all sessions, subagents, brain directories, SQLite databases, annotations,
/// implicit protobufs, browser recordings, and knowledge items correlated with `project_path`
/// inside `harness_dir` (e.g., `~/.gemini/antigravity` or `~/.gemini/antigravity-cli`).
pub fn discover_project_sessions(
    project_path: &Path,
    harness_dir: &Path,
) -> Result<ProjectDiscovery> {
    let norm_project = normalize_project_path(project_path);
    let project_path_str = norm_project.to_string_lossy().to_string();
    let project_uri = format!("file://{}", project_path_str);

    // Check parent ~/.gemini/config/projects/*.json if harness_dir is inside ~/.gemini/<harness>
    let matched_project_ids = if let Some(gemini_root) = harness_dir.parent() {
        find_matching_config_project_ids(gemini_root, &project_uri)
    } else {
        BTreeSet::new()
    };

    // Parse history.jsonl if present (used by antigravity-cli)
    let mut history_prompts: BTreeMap<String, String> = BTreeMap::new();
    let mut history_matched_cids: BTreeSet<String> = BTreeSet::new();
    let history_jsonl_path = harness_dir.join("history.jsonl");
    if let Ok(content) = fs::read_to_string(&history_jsonl_path) {
        for line in content.lines() {
            let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(sid) = val.get("sessionId").and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(prompt) = val.get("prompt").and_then(|v| v.as_str()) {
                history_prompts.insert(sid.to_string(), prompt.to_string());
            }
            if let Some(cwd) = val.get("cwd").and_then(|v| v.as_str()) {
                let cwd_uri = format!("file://{}", cwd.trim_end_matches('/'));
                if uri_matches_project(&cwd_uri, &project_uri) {
                    history_matched_cids.insert(sid.to_string());
                }
            }
        }
    }

    let mut all_db_rows: BTreeMap<String, RawDbRow> = BTreeMap::new();
    let mut matched_ids: BTreeSet<String> = BTreeSet::new();

    // 1. Query conversation_summaries.db if present
    let summary_db_path = harness_dir.join("conversation_summaries.db");
    if summary_db_path.exists() {
        let conn = Connection::open_with_flags(
            &summary_db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("Failed to open {}", summary_db_path.display()))?;
        let _ = conn.busy_timeout(std::time::Duration::from_millis(3000));

        if let Ok(mut stmt) = conn.prepare(
            "SELECT conversation_id, title, preview, step_count, last_modified_time, workspace_uris, status, agent_name, parent_conversation_id, project_id
             FROM conversation_summaries",
        ) {
            if let Ok(rows) = stmt.query_map([], |row| {
                let cid: String = row.get(0)?;
                let title: String = row.get(1)?;
                let preview: String = row.get(2)?;
                let step_count: i64 = row.get(3)?;
                let last_modified_time: String = row.get(4)?;
                let ws_raw: String = row.get(5)?;
                let status: String = row.get(6)?;
                let agent_name: String = row.get(7)?;
                let parent_id: String = row.get(8)?;
                let project_id: String = row.get(9)?;
                let workspace_uris: Vec<String> = if ws_raw.trim().is_empty() {
                    Vec::new()
                } else {
                    serde_json::from_str(&ws_raw).unwrap_or_else(|_| vec![ws_raw])
                };
                Ok(RawDbRow {
                    conversation_id: cid,
                    title,
                    preview,
                    step_count,
                    last_modified_time,
                    parent_conversation_id: parent_id,
                    agent_name,
                    status,
                    project_id,
                    workspace_uris,
                })
            }) {
                for r in rows.flatten() {
                    let uri_match = r
                        .workspace_uris
                        .iter()
                        .any(|u| uri_matches_project(u, &project_uri));
                    let pid_match = !r.project_id.is_empty()
                        && matched_project_ids.contains(&r.project_id);
                    if uri_match || pid_match {
                        matched_ids.insert(r.conversation_id.clone());
                    }
                    all_db_rows.insert(r.conversation_id.clone(), r);
                }
            };
        };
    }

    // 2. Also inspect any `*_summaries_proto.pb` files for sessions not already in `conversation_summaries.db`
    for pb_path in find_summary_pb_files(harness_dir) {
        if let Ok(pb_map) = ProtobufSummariesMap::load_from_path(&pb_path) {
            for (cid, raw_bytes) in pb_map.entries() {
                let (title_opt, uris) = extract_summary_metadata_from_pb(raw_bytes);
                let uri_match = uris.iter().any(|u| uri_matches_project(u, &project_uri));
                if uri_match {
                    matched_ids.insert(cid.clone());
                }
                all_db_rows.entry(cid.clone()).or_insert_with(|| RawDbRow {
                    conversation_id: cid.clone(),
                    title: title_opt.unwrap_or_default(),
                    preview: String::new(),
                    step_count: 1,
                    last_modified_time: String::new(),
                    parent_conversation_id: String::new(),
                    agent_name: String::new(),
                    status: "pb_index".to_string(),
                    project_id: String::new(),
                    workspace_uris: uris,
                });
            }
        }
    }

    // 3. Include any sessions matched via `history.jsonl`
    for cid in &history_matched_cids {
        matched_ids.insert(cid.clone());
        all_db_rows.entry(cid.clone()).or_insert_with(|| RawDbRow {
            conversation_id: cid.clone(),
            title: String::new(),
            preview: history_prompts.get(cid).cloned().unwrap_or_default(),
            step_count: 1,
            last_modified_time: String::new(),
            parent_conversation_id: String::new(),
            agent_name: String::new(),
            status: "history_jsonl".to_string(),
            project_id: String::new(),
            workspace_uris: vec![project_uri.clone()],
        });
    }

    // 4. Transitive closure over parent_conversation_id AND transcript subagent links
    loop {
        let prev_len = matched_ids.len();

        // Check SQLite parent_conversation_id in both directions (children of matched, or parent of matched)
        for row in all_db_rows.values() {
            if !row.parent_conversation_id.is_empty() {
                if matched_ids.contains(&row.parent_conversation_id) {
                    matched_ids.insert(row.conversation_id.clone());
                } else if matched_ids.contains(&row.conversation_id)
                    && all_db_rows.contains_key(&row.parent_conversation_id)
                {
                    matched_ids.insert(row.parent_conversation_id.clone());
                }
            }
        }

        // Check transcript.jsonl inside brain/<id> for conversation://<uuid> links
        let current_snapshot: Vec<String> = matched_ids.iter().cloned().collect();
        for cid in current_snapshot {
            let brain_dir = harness_dir.join("brain").join(&cid);
            if brain_dir.is_dir() {
                for sub_id in extract_subagents_from_transcript(&brain_dir) {
                    matched_ids.insert(sub_id);
                }
            }
        }

        if matched_ids.len() == prev_len {
            break;
        }
    }

    // 5. Scan orphan brain/<uuid> directories that weren't in SQLite DB
    let is_home_root = is_home_or_root_path(&norm_project, "");
    let mut known_ids: BTreeSet<String> = all_db_rows.keys().cloned().collect();
    known_ids.extend(matched_ids.iter().cloned());
    let orphan_ids = if is_home_root {
        BTreeSet::new()
    } else {
        scan_orphan_brains(
            &harness_dir.join("brain"),
            &known_ids,
            &project_path_str,
            &project_uri,
        )
    };
    for oid in &orphan_ids {
        matched_ids.insert(oid.clone());
    }

    // 6. Build DiscoveredSession entries and collect all relative paths + byte sizes
    let mut sessions = BTreeMap::new();
    let mut all_rel_set = BTreeSet::new();
    let mut total_bytes: u64 = 0;

    for cid in &matched_ids {
        let mut rel_paths = Vec::new();
        let mut sess_bytes: u64 = 0;

        let candidate_rels = [
            format!("conversations/{}.db", cid),
            format!("brain/{}", cid),
            format!("annotations/{}.pbtxt", cid),
            format!("implicit/{}.pb", cid),
            format!("browser_recordings/{}", cid),
            format!("html_artifacts/{}", cid),
            format!("context_state/{}.pb", cid),
        ];

        for rel in candidate_rels {
            let full = harness_dir.join(&rel);
            if full.exists() {
                sess_bytes += compute_path_size(&full);
                rel_paths.push(rel.clone());
                all_rel_set.insert(rel);
            }
        }

        total_bytes += sess_bytes;

        if let Some(row) = all_db_rows.get(cid) {
            let (ts_opt, last_prompt_snippet) =
                extract_last_user_prompt(harness_dir, cid, &row.preview, &history_prompts);
            let effective_time = if !row.last_modified_time.is_empty() {
                row.last_modified_time.clone()
            } else {
                ts_opt.unwrap_or_default()
            };
            sessions.insert(
                cid.clone(),
                DiscoveredSession {
                    conversation_id: cid.clone(),
                    title: row.title.clone(),
                    step_count: row.step_count,
                    last_modified_time: effective_time,
                    parent_conversation_id: row.parent_conversation_id.clone(),
                    agent_name: row.agent_name.clone(),
                    status: row.status.clone(),
                    project_id: row.project_id.clone(),
                    workspace_uris: row.workspace_uris.clone(),
                    last_prompt_snippet,
                    is_orphan_brain: false,
                    relative_paths: rel_paths,
                    total_bytes: sess_bytes,
                },
            );
        } else {
            let (ts_opt, last_prompt_snippet) =
                extract_last_user_prompt(harness_dir, cid, "", &history_prompts);
            sessions.insert(
                cid.clone(),
                DiscoveredSession {
                    conversation_id: cid.clone(),
                    title: "(Orphan / Subagent Session)".to_string(),
                    step_count: 0,
                    last_modified_time: ts_opt.unwrap_or_default(),
                    parent_conversation_id: String::new(),
                    agent_name: String::new(),
                    status: "orphan_brain".to_string(),
                    project_id: String::new(),
                    workspace_uris: vec![project_uri.clone()],
                    last_prompt_snippet,
                    is_orphan_brain: orphan_ids.contains(cid),
                    relative_paths: rel_paths,
                    total_bytes: sess_bytes,
                },
            );
        }
    }

    // 7. Scan knowledge/*/metadata.json for references to any matched conversation_id or project_uri
    let mut knowledge_paths = Vec::new();
    let knowledge_root = harness_dir.join("knowledge");
    if let Ok(entries) = fs::read_dir(&knowledge_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let meta_file = path.join("metadata.json");
            let Ok(meta_str) = fs::read_to_string(&meta_file) else {
                continue;
            };
            let matches_knowledge = (!is_home_root && meta_str.contains(&project_uri))
                || matched_ids.iter().any(|cid| meta_str.contains(cid));
            if matches_knowledge {
                if let Some(topic_name) = path.file_name().and_then(|n| n.to_str()) {
                    let rel = format!("knowledge/{}", topic_name);
                    total_bytes += compute_path_size(&path);
                    knowledge_paths.push(rel.clone());
                    all_rel_set.insert(rel);
                }
            }
        }
    }
    knowledge_paths.sort();

    // Checkpoint local SQLite WALs for discovered sessions so `.db` files are up to date
    if !matched_ids.is_empty() {
        checkpoint_sqlite_dbs(harness_dir, &matched_ids);
    }

    Ok(ProjectDiscovery {
        project_path: norm_project,
        project_uri,
        harness_dir: harness_dir.to_path_buf(),
        sessions,
        knowledge_paths,
        matched_project_ids,
        all_relative_paths: all_rel_set.into_iter().collect(),
        total_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::TempDir;

    #[test]
    fn test_discover_project_sessions_full_graph_and_last_prompt() -> Result<()> {
        let tmp = TempDir::new()?;
        let harness_dir = tmp.path().join(".gemini/antigravity");
        let proj_dir = tmp.path().join("workspace/my-project");
        fs::create_dir_all(&harness_dir)?;
        fs::create_dir_all(&proj_dir)?;

        let norm_proj = normalize_project_path(&proj_dir);
        let proj_uri = format!("file://{}", norm_proj.display());

        let db_path = harness_dir.join("conversation_summaries.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(
            "CREATE TABLE conversation_summaries (
                conversation_id TEXT PRIMARY KEY,
                title TEXT DEFAULT '',
                preview TEXT DEFAULT '',
                step_count INTEGER DEFAULT 0,
                created_time TEXT DEFAULT '',
                last_modified_time TEXT DEFAULT '',
                workspace_uris TEXT DEFAULT '',
                status TEXT DEFAULT '',
                agent_name TEXT DEFAULT '',
                parent_conversation_id TEXT DEFAULT '',
                project_id TEXT DEFAULT '',
                last_user_input_time TEXT DEFAULT '',
                raw_summary BLOB
            );",
        )?;

        let root_id = "11111111-1111-1111-1111-111111111111";
        let sub_id = "22222222-2222-2222-2222-222222222222";
        let transcript_sub_id = "33333333-3333-3333-3333-333333333333";
        let other_id = "99999999-9999-9999-9999-999999999999";

        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, preview, step_count, last_modified_time, workspace_uris, parent_conversation_id)
             VALUES (?1, 'Root Session', 'Initial preview', 42, '2026-10-10T01:00:00Z', ?2, '')",
            params![root_id, format!("[\"{}\"]", proj_uri)],
        )?;
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, step_count, last_modified_time, workspace_uris, parent_conversation_id)
             VALUES (?1, 'Subagent Session', 10, '2026-10-10T01:05:00Z', '[]', ?2)",
            params![sub_id, root_id],
        )?;
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, step_count, last_modified_time, workspace_uris, parent_conversation_id)
             VALUES (?1, 'Other Project Session', 99, '2026-10-10T01:05:00Z', '[\"file:///other/project\"]', '')",
            params![other_id],
        )?;
        drop(conn);

        // Create artifacts on disk
        fs::create_dir_all(harness_dir.join("conversations"))?;
        fs::write(
            harness_dir.join(format!("conversations/{}.db", root_id)),
            b"sqlite-root",
        )?;
        fs::write(
            harness_dir.join(format!("conversations/{}.db", sub_id)),
            b"sqlite-sub",
        )?;

        let root_logs = harness_dir
            .join("brain")
            .join(root_id)
            .join(".system_generated/logs");
        fs::create_dir_all(&root_logs)?;
        fs::write(
            root_logs.join("transcript.jsonl"),
            format!(
                "{{\"type\":\"USER_INPUT\",\"created_at\":\"2026-10-10T01:10:00Z\",\"content\":\"<USER_REQUEST>Please build the sync engine</USER_REQUEST>\"}}\nSee subagent [research](conversation://{}) for details\n",
                transcript_sub_id
            ),
        )?;

        let t_sub_brain = harness_dir.join("brain").join(transcript_sub_id);
        fs::create_dir_all(&t_sub_brain)?;
        fs::write(t_sub_brain.join("notes.md"), b"subagent notes")?;

        let know_dir = harness_dir.join("knowledge/sync_topic");
        fs::create_dir_all(&know_dir)?;
        fs::write(
            know_dir.join("metadata.json"),
            format!("{{\"conversation_id\": \"{}\"}}", root_id),
        )?;

        let disc = discover_project_sessions(&proj_dir, &harness_dir)?;
        assert_eq!(disc.sessions.len(), 3);
        assert!(disc.sessions.contains_key(root_id));
        assert!(disc.sessions.contains_key(sub_id));
        assert!(disc.sessions.contains_key(transcript_sub_id));
        assert!(!disc.sessions.contains_key(other_id));
        assert_eq!(
            disc.sessions[root_id].last_prompt_snippet,
            "Please build the sync engine"
        );
        assert_eq!(disc.knowledge_paths, vec!["knowledge/sync_topic"]);

        Ok(())
    }

    #[test]
    fn test_uri_matches_project_home_guard_and_cross_os_relative_match() {
        // 1. $HOME root must NOT prefix-match child projects
        assert!(uri_matches_project(
            "file:///Users/alice",
            "file:///Users/alice"
        ));
        assert!(!uri_matches_project(
            "file:///Users/alice/Documents/my-project",
            "file:///Users/alice"
        ));
        assert!(!uri_matches_project(
            "file:///usr/local/corp/home/alice/Documents/my-project",
            "file:///usr/local/corp/home/alice"
        ));

        // 2. Normal project matches exact and subdirectories
        assert!(uri_matches_project(
            "file:///Users/alice/Documents/my-project",
            "file:///Users/alice/Documents/my-project"
        ));
        assert!(uri_matches_project(
            "file:///Users/alice/Documents/my-project/sub",
            "file:///Users/alice/Documents/my-project"
        ));
        assert!(!uri_matches_project(
            "file:///Users/alice/Documents/my-project-2",
            "file:///Users/alice/Documents/my-project"
        ));

        // 3. Cross-OS home mount points for the same username & relative project path match
        assert!(uri_matches_project(
            "file:///usr/local/corp/home/alice/Documents/my-project",
            "file:///Users/alice/Documents/my-project"
        ));
        assert!(uri_matches_project(
            "file:///Users/alice/Documents/my-project/sub",
            "file:///usr/local/corp/home/alice/Documents/my-project"
        ));
        assert!(!uri_matches_project(
            "file:///usr/local/corp/home/bob/Documents/my-project",
            "file:///Users/alice/Documents/my-project"
        ));
    }
}
