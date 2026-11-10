use crate::index_merge::apply_home_remap_str;
use crate::transport::{
    probe_remote, run_remote_script, shell_quote, should_use_tar_transport, tar_pull_paths,
    tar_push_paths, RemoteEndpoint, RemoteProbe,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::Command;
use std::time::UNIX_EPOCH;

/// Categories of global customizations discovered across `$HOME/.gemini` and `$HOME/.agents`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatalogCategory {
    Skill,
    Plugin,
    Extension,
    Agent,
}

impl CatalogCategory {
    pub fn label(&self) -> &'static str {
        match self {
            CatalogCategory::Skill => "Skill",
            CatalogCategory::Plugin => "Plugin",
            CatalogCategory::Extension => "Extension",
            CatalogCategory::Agent => "Agent",
        }
    }

    pub fn plural_title(&self) -> &'static str {
        match self {
            CatalogCategory::Skill => "Global Skills (~/.agents/skills & ~/.gemini/config/skills)",
            CatalogCategory::Plugin => "Global Plugins (~/.gemini/config/plugins)",
            CatalogCategory::Extension => "Global Extensions (~/.gemini/extensions)",
            CatalogCategory::Agent => "Global Agents (~/.gemini/config/agents)",
        }
    }

    pub fn parse_filter_list(raw: &[String]) -> Result<BTreeSet<CatalogCategory>> {
        let mut set = BTreeSet::new();
        if raw.is_empty() {
            set.insert(CatalogCategory::Skill);
            set.insert(CatalogCategory::Plugin);
            set.insert(CatalogCategory::Extension);
            set.insert(CatalogCategory::Agent);
            return Ok(set);
        }
        for tok in raw {
            for part in tok.split(',') {
                let p = part.trim().to_ascii_lowercase();
                if p.is_empty() {
                    continue;
                }
                match p.as_str() {
                    "all" => {
                        set.insert(CatalogCategory::Skill);
                        set.insert(CatalogCategory::Plugin);
                        set.insert(CatalogCategory::Extension);
                        set.insert(CatalogCategory::Agent);
                    }
                    "skill" | "skills" => {
                        set.insert(CatalogCategory::Skill);
                    }
                    "plugin" | "plugins" => {
                        set.insert(CatalogCategory::Plugin);
                    }
                    "extension" | "extensions" | "ext" => {
                        set.insert(CatalogCategory::Extension);
                    }
                    "agent" | "agents" => {
                        set.insert(CatalogCategory::Agent);
                    }
                    other => bail!(
                        "Unknown catalog kind '{}'. Valid values: skills, plugins, extensions, agents, all",
                        other
                    ),
                }
            }
        }
        Ok(set)
    }
}

/// Metadata for a single discovered global skill, plugin, extension, or agent on one host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogItemMeta {
    pub category: CatalogCategory,
    pub name: String,
    /// Relative path under `$HOME` (e.g. `.agents/skills/foo` or `.gemini/config/plugins/bar`).
    pub rel_home_path: String,
    /// Whether this item is a single file (e.g. `.md` agent) rather than a directory.
    pub is_file: bool,
    /// Declared version (e.g. `1.13.0`) if found in manifest JSON or `SKILL.md` frontmatter.
    pub version: Option<String>,
    /// Source file from which `version` was extracted.
    pub version_source: Option<String>,
    /// Number of regular files (excluding build/dependency caches).
    pub file_count: usize,
    /// Latest modification timestamp (seconds since UNIX epoch).
    pub latest_mtime_secs: u64,
    /// `$HOME`-normalized SHA-256 content digest (first 12 hex chars).
    pub content_digest: String,
    /// True when the item itself is a symlink pointing outside `$HOME`.
    pub is_external_symlink: bool,
    /// Symlink target if `is_external_symlink` is true.
    pub external_symlink_target: Option<String>,
    /// Additional `$HOME`-relative symlink locations pointing to this skill (e.g. `.gemini/skills/foo`).
    pub skill_symlink_rels: Vec<String>,
    /// Non-standard root mount prefixes (e.g. `/custom_depot`) referenced inside text files.
    pub referenced_custom_roots: Vec<String>,
    /// Lowercase text preview token set or raw sample check used for unshared-harness quarantine.
    pub referenced_gemini_subdirs: Vec<String>,
}

/// Comparison status between Local (`A`) and Remote (`B`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CatalogDiffStatus {
    Identical,
    LocalNewer,
    RemoteNewer,
    Diverged,
    OnlyLocal,
    OnlyRemote,
}

impl CatalogDiffStatus {
    pub fn short_label(&self) -> &'static str {
        match self {
            CatalogDiffStatus::Identical => "IDENTICAL",
            CatalogDiffStatus::LocalNewer => "LOCAL NEWER",
            CatalogDiffStatus::RemoteNewer => "REMOTE NEWER",
            CatalogDiffStatus::Diverged => "DIVERGED",
            CatalogDiffStatus::OnlyLocal => "ONLY LOCAL (A)",
            CatalogDiffStatus::OnlyRemote => "ONLY REMOTE (B)",
        }
    }
}

/// Action proposed or chosen for a catalog entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CatalogAction {
    SkipIdentical,
    KeepLocalPush,
    KeepRemotePull,
    KeepAsIs,
    RemoveBoth,
    RemoveLocalOnly,
    RemoveRemoteOnly,
}

impl CatalogAction {
    pub fn description(&self) -> &'static str {
        match self {
            CatalogAction::SkipIdentical => "Skip (already identical)",
            CatalogAction::KeepLocalPush => "Keep Local [A -> Push to B]",
            CatalogAction::KeepRemotePull => "Keep Remote [B -> Pull to A]",
            CatalogAction::KeepAsIs => "Keep As-Is on Each Host",
            CatalogAction::RemoveBoth => "Remove from BOTH Local & Remote",
            CatalogAction::RemoveLocalOnly => "Remove from Local (A) Only",
            CatalogAction::RemoveRemoteOnly => "Remove from Remote (B) Only",
        }
    }

    pub fn prompt_code(&self) -> &'static str {
        match self {
            CatalogAction::SkipIdentical | CatalogAction::KeepAsIs => "k",
            CatalogAction::KeepLocalPush => "a",
            CatalogAction::KeepRemotePull => "b",
            CatalogAction::RemoveBoth => "d",
            CatalogAction::RemoveLocalOnly => "x",
            CatalogAction::RemoveRemoteOnly => "y",
        }
    }
}

/// Planned comparison & recommendation entry for one `(CatalogCategory, name)` key.
#[derive(Debug, Clone)]
pub struct CatalogPlanEntry {
    pub category: CatalogCategory,
    pub name: String,
    pub local: Option<CatalogItemMeta>,
    pub remote: Option<CatalogItemMeta>,
    pub status: CatalogDiffStatus,
    pub suggested_action: CatalogAction,
    pub quarantined: bool,
    pub quarantine_reason: Option<String>,
    pub superseded_by: Option<String>,
}

/// Pure-Rust FIPS 180-4 SHA-256 implementation so zero external hash crates are needed.
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64) * 8;
    let mut padded = Vec::with_capacity(data.len() + 72);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0x00);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let off = i * 4;
            *word = u32::from_be_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    format!(
        "{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]
    )
}

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    "target",
    ".DS_Store",
];

const STANDARD_POSIX_ROOTS: &[&str] = &[
    "usr",
    "bin",
    "sbin",
    "etc",
    "var",
    "tmp",
    "opt",
    "dev",
    "proc",
    "sys",
    "lib",
    "lib64",
    "home",
    "Users",
    "Applications",
    "Library",
    "System",
    "private",
    "Volumes",
    "mnt",
    "run",
    "srv",
    "media",
];

fn is_text_inspectable(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    [
        ".md", ".json", ".py", ".ts", ".js", ".mjs", ".cjs", ".sh", ".toml", ".yaml", ".yml",
        ".txt",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

fn scan_non_portable_markers_in_text(
    text: &str,
    custom_roots: &mut BTreeSet<String>,
    gemini_subdirs: &mut BTreeSet<String>,
) {
    // 1. Detect `/.gemini/<subdir>/` references so unshared custom session stores can be quarantined
    let needle = "/.gemini/";
    let mut rest = text;
    while let Some(idx) = rest.find(needle) {
        let after = &rest[idx + needle.len()..];
        let seg: String = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !seg.is_empty()
            && !matches!(
                seg.as_str(),
                "antigravity"
                    | "antigravity-cli"
                    | "antigravity-ide"
                    | "config"
                    | "skills"
                    | "extensions"
                    | "bin"
                    | "tmp"
                    | "history"
                    | "memory"
            )
        {
            gemini_subdirs.insert(seg);
        }
        rest = after;
    }

    // 2. Detect absolute `/<root>/<subpath>` mount references (e.g. host-mounted depots) outside standard OS dirs
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 4 < bytes.len() {
        if bytes[i] == b'/'
            && (i == 0
                || matches!(
                    bytes[i - 1],
                    b'"' | b'\'' | b' ' | b'\t' | b'\n' | b'=' | b'(' | b'['
                ))
            && (i == 0 || bytes[i - 1] != b'/')
        {
            let start = i + 1;
            let mut end = start;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'-')
            {
                end += 1;
            }
            if end > start && end < bytes.len() && bytes[end] == b'/' {
                // Ensure there is a second path component with at least 2 chars (avoid `/a/b` regexes or `/v1/` API paths)
                let after_slash = end + 1;
                let mut seg2_end = after_slash;
                while seg2_end < bytes.len()
                    && (bytes[seg2_end].is_ascii_alphanumeric()
                        || bytes[seg2_end] == b'_'
                        || bytes[seg2_end] == b'-')
                {
                    seg2_end += 1;
                }
                let root_tok = &text[start..end];
                let seg2_tok = &text[after_slash..seg2_end];
                if root_tok.len() >= 4
                    && seg2_tok.len() >= 3
                    && !root_tok.starts_with('v')
                    && root_tok.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                    && matches!(seg2_tok, "src" | "bin" | "data" | "obj" | "releases" | "depot")
                    && !STANDARD_POSIX_ROOTS.contains(&root_tok)
                {
                    custom_roots.insert(format!("/{}", root_tok));
                }
            }
            i = end;
        } else {
            i += 1;
        }
    }
}

fn extract_version_from_skill_md(path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    for line in raw.lines().take(30) {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("version:") {
            let v = rest.trim().trim_matches('"').trim_matches('\'').trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn extract_item_version(item_path: &Path, is_file: bool) -> (Option<String>, Option<String>) {
    if is_file {
        if let Some(v) = extract_version_from_skill_md(item_path) {
            return (
                Some(v),
                item_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string()),
            );
        }
        return (None, None);
    }

    for mf in [
        "plugin.json",
        ".claude-plugin/plugin.json",
        "gemini-extension.json",
        "package.json",
        "extension.json",
        "manifest.json",
    ] {
        let mp = item_path.join(mf);
        if mp.is_file() {
            if let Ok(content) = fs::read_to_string(&mp) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(v) = val.get("version").and_then(|x| x.as_str()) {
                        let clean = v.trim();
                        if !clean.is_empty() {
                            return (Some(clean.to_string()), Some(mf.to_string()));
                        }
                    }
                }
            }
        }
    }

    let root_skill = item_path.join("SKILL.md");
    if root_skill.is_file() {
        if let Some(v) = extract_version_from_skill_md(&root_skill) {
            return (Some(v), Some("SKILL.md".to_string()));
        }
    }

    let nested_skills_dir = item_path.join("skills");
    if let Ok(entries) = fs::read_dir(nested_skills_dir) {
        let mut names: Vec<_> = entries.flatten().collect();
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let sm = e.path().join("SKILL.md");
            if sm.is_file() {
                if let Some(v) = extract_version_from_skill_md(&sm) {
                    return (Some(v), Some("skills/*/SKILL.md".to_string()));
                }
            }
        }
    }

    (None, None)
}

fn walk_item_files(
    base_path: &Path,
    cur_path: &Path,
    home_bytes: &[u8],
    rel_digests: &mut Vec<(String, String)>,
    max_mtime: &mut u64,
    file_count: &mut usize,
    custom_roots: &mut BTreeSet<String>,
    gemini_subdirs: &mut BTreeSet<String>,
) {
    let Ok(entries) = fs::read_dir(cur_path) else {
        return;
    };
    let mut sorted: Vec<_> = entries.flatten().collect();
    sorted.sort_by_key(|e| e.file_name());

    for entry in sorted {
        let fname_os = entry.file_name();
        let Some(fname) = fname_os.to_str() else {
            continue;
        };
        if SKIP_DIRS.contains(&fname) {
            continue;
        }
        let full = entry.path();
        let Ok(meta) = fs::symlink_metadata(&full) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            walk_item_files(
                base_path,
                &full,
                home_bytes,
                rel_digests,
                max_mtime,
                file_count,
                custom_roots,
                gemini_subdirs,
            );
        } else if meta.is_file() {
            *file_count += 1;
            if let Ok(m) = meta.modified() {
                if let Ok(dur) = m.duration_since(UNIX_EPOCH) {
                    if dur.as_secs() > *max_mtime {
                        *max_mtime = dur.as_secs();
                    }
                }
            }
            if let Ok(raw) = fs::read(&full) {
                let norm = replace_bytes(&raw, home_bytes, b"$HOME");
                let h = sha256_hex(&norm);
                let rel = full
                    .strip_prefix(base_path)
                    .unwrap_or(&full)
                    .to_string_lossy()
                    .replace('\\', "/");
                rel_digests.push((rel, h[..12].to_string()));
                if raw.len() <= 300_000 && is_text_inspectable(fname) {
                    if let Ok(txt) = std::str::from_utf8(&raw) {
                        scan_non_portable_markers_in_text(txt, custom_roots, gemini_subdirs);
                    }
                }
            }
        }
    }
}

fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if &haystack[i..i + needle.len()] == needle {
            out.extend_from_slice(replacement);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out.extend_from_slice(&haystack[i..]);
    out
}

fn build_item_meta(
    home_dir: &Path,
    category: CatalogCategory,
    name: &str,
    rel_home_path: &str,
    skill_symlink_rels: Vec<String>,
) -> Option<CatalogItemMeta> {
    let full = home_dir.join(rel_home_path);
    let sym_meta = fs::symlink_metadata(&full).ok()?;
    let home_str = home_dir.to_string_lossy().trim_end_matches('/').to_string();
    let home_bytes = home_str.as_bytes();

    let mut is_external_symlink = false;
    let mut external_symlink_target = None;
    if sym_meta.file_type().is_symlink() {
        if let Ok(link_target) = fs::read_link(&full) {
            let resolved = if link_target.is_absolute() {
                link_target.clone()
            } else {
                full.parent().unwrap_or(home_dir).join(&link_target)
            };
            let resolved_str = resolved.to_string_lossy().to_string();
            if !resolved_str.starts_with(&home_str) {
                is_external_symlink = true;
                external_symlink_target = Some(resolved_str);
            }
        }
    }

    let real_meta = fs::metadata(&full).ok()?;
    let is_file = real_meta.is_file();
    if !is_file && !real_meta.is_dir() {
        return None;
    }

    let (version, version_source) = extract_item_version(&full, is_file);
    let mut rel_digests = Vec::new();
    let mut max_mtime = 0u64;
    let mut file_count = 0usize;
    let mut custom_roots = BTreeSet::new();
    let mut gemini_subdirs = BTreeSet::new();

    if let Some(ref ext_t) = external_symlink_target {
        scan_non_portable_markers_in_text(ext_t, &mut custom_roots, &mut gemini_subdirs);
    }

    if is_file {
        file_count = 1;
        if let Ok(m) = real_meta.modified() {
            if let Ok(dur) = m.duration_since(UNIX_EPOCH) {
                max_mtime = dur.as_secs();
            }
        }
        if let Ok(raw) = fs::read(&full) {
            let norm = replace_bytes(&raw, home_bytes, b"$HOME");
            let h = sha256_hex(&norm);
            rel_digests.push((name.to_string(), h[..12].to_string()));
            if raw.len() <= 300_000 {
                if let Ok(txt) = std::str::from_utf8(&raw) {
                    scan_non_portable_markers_in_text(txt, &mut custom_roots, &mut gemini_subdirs);
                }
            }
        }
    } else {
        walk_item_files(
            &full,
            &full,
            home_bytes,
            &mut rel_digests,
            &mut max_mtime,
            &mut file_count,
            &mut custom_roots,
            &mut gemini_subdirs,
        );
    }

    rel_digests.sort_by(|a, b| a.0.cmp(&b.0));
    let mut combined = String::new();
    for (r, h) in &rel_digests {
        combined.push_str(r);
        combined.push(':');
        combined.push_str(h);
        combined.push('\n');
    }
    let content_digest = sha256_hex(combined.as_bytes())[..10].to_string();

    Some(CatalogItemMeta {
        category,
        name: name.to_string(),
        rel_home_path: rel_home_path.to_string(),
        is_file,
        version,
        version_source,
        file_count,
        latest_mtime_secs: max_mtime,
        content_digest,
        is_external_symlink,
        external_symlink_target,
        skill_symlink_rels,
        referenced_custom_roots: custom_roots.into_iter().collect(),
        referenced_gemini_subdirs: gemini_subdirs.into_iter().collect(),
    })
}

/// Scans global skills, plugins, extensions, and agents under `home_dir`.
pub fn scan_catalog_at_home(
    home_dir: &Path,
    categories: &BTreeSet<CatalogCategory>,
) -> Result<Vec<CatalogItemMeta>> {
    let mut items = Vec::new();

    // 1. Skills: primary store `.agents/skills/<name>`, plus standalone dirs in `.gemini/config/skills` & `.gemini/skills`
    if categories.contains(&CatalogCategory::Skill) {
        let mut skill_links_by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut standalone_skill_rels: BTreeMap<String, String> = BTreeMap::new();

        for link_root_rel in [".gemini/skills", ".gemini/config/skills"] {
            let link_root = home_dir.join(link_root_rel);
            if let Ok(entries) = fs::read_dir(&link_root) {
                for entry in entries.flatten() {
                    let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                        continue;
                    };
                    if name.starts_with('.') {
                        continue;
                    }
                    let rel_path = format!("{}/{}", link_root_rel, name);
                    let p = entry.path();
                    if let Ok(sm) = fs::symlink_metadata(&p) {
                        if sm.file_type().is_symlink() {
                            skill_links_by_name
                                .entry(name.clone())
                                .or_default()
                                .push(rel_path.clone());
                            // If symlink points somewhere OTHER than `.agents/skills/<name>`, record as standalone if `.agents/skills/<name>` is absent
                            if !home_dir.join(".agents/skills").join(&name).exists() {
                                standalone_skill_rels.entry(name).or_insert(rel_path);
                            }
                        } else if sm.is_dir() && !home_dir.join(".agents/skills").join(&name).exists()
                        {
                            standalone_skill_rels.entry(name).or_insert(rel_path);
                        }
                    }
                }
            }
        }

        let mut seen_skills = BTreeSet::new();
        let agents_skills = home_dir.join(".agents/skills");
        if let Ok(entries) = fs::read_dir(&agents_skills) {
            let mut sorted: Vec<_> = entries.flatten().collect();
            sorted.sort_by_key(|e| e.file_name());
            for entry in sorted {
                let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                    continue;
                };
                if name.starts_with('.') {
                    continue;
                }
                let rel = format!(".agents/skills/{}", name);
                let links = skill_links_by_name.remove(&name).unwrap_or_default();
                if let Some(meta) =
                    build_item_meta(home_dir, CatalogCategory::Skill, &name, &rel, links)
                {
                    seen_skills.insert(name);
                    items.push(meta);
                }
            }
        }

        for (name, rel) in standalone_skill_rels {
            if seen_skills.contains(&name) {
                continue;
            }
            let links = skill_links_by_name.remove(&name).unwrap_or_default();
            if let Some(meta) = build_item_meta(home_dir, CatalogCategory::Skill, &name, &rel, links)
            {
                items.push(meta);
            }
        }
    }

    // 2. Plugins: `.gemini/config/plugins/<name>`
    if categories.contains(&CatalogCategory::Plugin) {
        let plugins_dir = home_dir.join(".gemini/config/plugins");
        if let Ok(entries) = fs::read_dir(&plugins_dir) {
            let mut sorted: Vec<_> = entries.flatten().collect();
            sorted.sort_by_key(|e| e.file_name());
            for entry in sorted {
                let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                    continue;
                };
                if name.starts_with('.') {
                    continue;
                }
                let rel = format!(".gemini/config/plugins/{}", name);
                if let Some(meta) =
                    build_item_meta(home_dir, CatalogCategory::Plugin, &name, &rel, Vec::new())
                {
                    items.push(meta);
                }
            }
        }
    }

    // 3. Extensions: `.gemini/extensions/<name>` (directories only; skip `extension-enablement.json`)
    if categories.contains(&CatalogCategory::Extension) {
        let ext_dir = home_dir.join(".gemini/extensions");
        if let Ok(entries) = fs::read_dir(&ext_dir) {
            let mut sorted: Vec<_> = entries.flatten().collect();
            sorted.sort_by_key(|e| e.file_name());
            for entry in sorted {
                let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                    continue;
                };
                if name.starts_with('.') || !entry.path().is_dir() {
                    continue;
                }
                let rel = format!(".gemini/extensions/{}", name);
                if let Some(meta) = build_item_meta(
                    home_dir,
                    CatalogCategory::Extension,
                    &name,
                    &rel,
                    Vec::new(),
                ) {
                    items.push(meta);
                }
            }
        }
    }

    // 4. Agents: `.gemini/config/agents/<name>` (both agent directories and `.md` agent definitions, skipping `README.md`)
    if categories.contains(&CatalogCategory::Agent) {
        let agents_dir = home_dir.join(".gemini/config/agents");
        if let Ok(entries) = fs::read_dir(&agents_dir) {
            let mut sorted: Vec<_> = entries.flatten().collect();
            sorted.sort_by_key(|e| e.file_name());
            for entry in sorted {
                let Some(name) = entry.file_name().to_str().map(|s| s.to_string()) else {
                    continue;
                };
                if name.starts_with('.') || name.eq_ignore_ascii_case("README.md") {
                    continue;
                }
                let p = entry.path();
                if p.is_dir() || (p.is_file() && name.ends_with(".md")) {
                    let rel = format!(".gemini/config/agents/{}", name);
                    if let Some(meta) = build_item_meta(
                        home_dir,
                        CatalogCategory::Agent,
                        &name,
                        &rel,
                        Vec::new(),
                    ) {
                        items.push(meta);
                    }
                }
            }
        }
    }

    Ok(items)
}

/// Scans the remote endpoint's global skills, plugins, extensions, and agents in a single SSH pass.
pub fn scan_remote_catalog(
    probe: &RemoteProbe,
    categories: &BTreeSet<CatalogCategory>,
) -> Result<(Vec<CatalogItemMeta>, BTreeSet<String>)> {
    match &probe.endpoint {
        RemoteEndpoint::LocalRoot { root_home } => {
            let items = scan_catalog_at_home(root_home, categories)?;
            let root_dirs = BTreeSet::new();
            Ok((items, root_dirs))
        }
        RemoteEndpoint::Ssh { .. } => {
            let want_skill = categories.contains(&CatalogCategory::Skill);
            let want_plugin = categories.contains(&CatalogCategory::Plugin);
            let want_ext = categories.contains(&CatalogCategory::Extension);
            let want_agent = categories.contains(&CatalogCategory::Agent);

            let py_script = format!(
                r#"python3 - <<'PY'
import os, glob, json, hashlib

home = os.path.expanduser("~").rstrip("/")
home_bytes = home.encode("utf-8")
SKIP_DIRS = {{".git", "node_modules", ".venv", "venv", "__pycache__", "target", ".DS_Store"}}
STD_ROOTS = {{"usr","bin","sbin","etc","var","tmp","opt","dev","proc","sys","lib","lib64","home","Users","Applications","Library","System","private","Volumes","mnt","run","srv","media"}}
TEXT_EXTS = (".md",".json",".py",".ts",".js",".mjs",".cjs",".sh",".toml",".yaml",".yml",".txt")

want_skill = {ws}
want_plugin = {wp}
want_ext = {we}
want_agent = {wa}

def scan_markers(txt, custom_roots, gemini_subs):
    needle = "/.gemini/"
    pos = 0
    while True:
        idx = txt.find(needle, pos)
        if idx == -1:
            break
        after = txt[idx + len(needle):]
        seg = []
        for ch in after:
            if ch.isalnum() or ch in "-_":
                seg.append(ch)
            else:
                break
        s = "".join(seg)
        if s and s not in ("antigravity","antigravity-cli","antigravity-ide","config","skills","extensions","bin","tmp","history","memory"):
            gemini_subs.add(s)
        pos = idx + len(needle)

def extract_skill_md_ver(p):
    try:
        with open(p, "r", encoding="utf-8", errors="ignore") as f:
            for i, line in enumerate(f):
                if i >= 30:
                    break
                t = line.strip()
                if t.startswith("version:"):
                    v = t.split(":", 1)[1].strip().strip('"').strip("'").strip()
                    if v:
                        return v
    except Exception:
        pass
    return None

def extract_ver(item_path, is_file):
    if is_file:
        v = extract_skill_md_ver(item_path)
        return (v, os.path.basename(item_path) if v else None)
    for mf in ["plugin.json", ".claude-plugin/plugin.json", "gemini-extension.json", "package.json", "extension.json", "manifest.json"]:
        mp = os.path.join(item_path, mf)
        if os.path.isfile(mp):
            try:
                d = json.load(open(mp, "r", encoding="utf-8"))
                if isinstance(d, dict) and isinstance(d.get("version"), str) and d["version"].strip():
                    return (d["version"].strip(), mf)
            except Exception:
                pass
    sm = os.path.join(item_path, "SKILL.md")
    if os.path.isfile(sm):
        v = extract_skill_md_ver(sm)
        if v:
            return (v, "SKILL.md")
    for sub_sm in sorted(glob.glob(os.path.join(item_path, "skills/*/SKILL.md"))):
        v = extract_skill_md_ver(sub_sm)
        if v:
            return (v, "skills/*/SKILL.md")
    return (None, None)

def build_meta(cat, name, rel_path, skill_links):
    full = os.path.join(home, rel_path)
    if not os.path.lexists(full):
        return None
    is_ext_sym = False
    ext_target = None
    if os.path.islink(full):
        t = os.readlink(full)
        resolved = t if os.path.isabs(t) else os.path.normpath(os.path.join(os.path.dirname(full), t))
        if not resolved.startswith(home):
            is_ext_sym = True
            ext_target = resolved
    if not os.path.exists(full):
        return None
    is_file = os.path.isfile(full)
    if not is_file and not os.path.isdir(full):
        return None
    ver, ver_src = extract_ver(full, is_file)
    rel_digests = []
    max_m = 0
    fcount = 0
    custom_roots = set()
    gemini_subs = set()
    if ext_target:
        parts = ext_target.strip("/").split("/")
        if len(parts) >= 2 and parts[0] not in STD_ROOTS:
            custom_roots.add("/" + parts[0])
        scan_markers(ext_target, custom_roots, gemini_subs)
    if is_file:
        fcount = 1
        st = os.stat(full)
        max_m = int(st.st_mtime)
        raw = open(full, "rb").read()
        norm = raw.replace(home_bytes, b"$HOME")
        rel_digests.append((name, hashlib.sha256(norm).hexdigest()[:12]))
        if len(raw) <= 300000:
            scan_markers(raw.decode("utf-8", errors="ignore"), custom_roots, gemini_subs)
    else:
        for r, dirs, files in os.walk(full):
            dirs[:] = sorted([d for d in dirs if d not in SKIP_DIRS])
            for fn in sorted(files):
                if fn in SKIP_DIRS:
                    continue
                fp = os.path.join(r, fn)
                if os.path.islink(fp):
                    continue
                try:
                    st = os.stat(fp)
                    if not os.path.isfile(fp):
                        continue
                    fcount += 1
                    if int(st.st_mtime) > max_m:
                        max_m = int(st.st_mtime)
                    raw = open(fp, "rb").read()
                    norm = raw.replace(home_bytes, b"$HOME")
                    rel = os.path.relpath(fp, full).replace("\\", "/")
                    rel_digests.append((rel, hashlib.sha256(norm).hexdigest()[:12]))
                    if len(raw) <= 300000 and fn.lower().endswith(TEXT_EXTS):
                        scan_markers(raw.decode("utf-8", errors="ignore"), custom_roots, gemini_subs)
                except Exception:
                    pass
    rel_digests.sort(key=lambda x: x[0])
    comb = "".join(f"{{r}}:{{h}}\n" for r, h in rel_digests).encode("utf-8")
    digest = hashlib.sha256(comb).hexdigest()[:10]
    return {{
        "category": cat,
        "name": name,
        "rel_home_path": rel_path,
        "is_file": is_file,
        "version": ver,
        "version_source": ver_src,
        "file_count": fcount,
        "latest_mtime_secs": max_m,
        "content_digest": digest,
        "is_external_symlink": is_ext_sym,
        "external_symlink_target": ext_target,
        "skill_symlink_rels": skill_links,
        "referenced_custom_roots": sorted(custom_roots),
        "referenced_gemini_subdirs": sorted(gemini_subs),
    }}

items = []
if want_skill:
    links_by_name = {{}}
    standalone = {{}}
    for lr in [".gemini/skills", ".gemini/config/skills"]:
        p = os.path.join(home, lr)
        if os.path.isdir(p):
            for n in sorted(os.listdir(p)):
                if n.startswith("."):
                    continue
                rel = f"{{lr}}/{{n}}"
                fp = os.path.join(p, n)
                has_agent_dir = os.path.exists(os.path.join(home, ".agents/skills", n))
                if os.path.islink(fp):
                    links_by_name.setdefault(n, []).append(rel)
                    if not has_agent_dir:
                        standalone.setdefault(n, rel)
                elif os.path.isdir(fp) and not has_agent_dir:
                    standalone.setdefault(n, rel)
    seen = set()
    ag_p = os.path.join(home, ".agents/skills")
    if os.path.isdir(ag_p):
        for n in sorted(os.listdir(ag_p)):
            if n.startswith("."):
                continue
            m = build_meta("skill", n, f".agents/skills/{{n}}", links_by_name.pop(n, []))
            if m:
                seen.add(n)
                items.append(m)
    for n, rel in sorted(standalone.items()):
        if n in seen:
            continue
        m = build_meta("skill", n, rel, links_by_name.pop(n, []))
        if m:
            items.append(m)

if want_plugin:
    p = os.path.join(home, ".gemini/config/plugins")
    if os.path.isdir(p):
        for n in sorted(os.listdir(p)):
            if not n.startswith("."):
                m = build_meta("plugin", n, f".gemini/config/plugins/{{n}}", [])
                if m:
                    items.append(m)

if want_ext:
    p = os.path.join(home, ".gemini/extensions")
    if os.path.isdir(p):
        for n in sorted(os.listdir(p)):
            fp = os.path.join(p, n)
            if not n.startswith(".") and os.path.isdir(fp):
                m = build_meta("extension", n, f".gemini/extensions/{{n}}", [])
                if m:
                    items.append(m)

if want_agent:
    p = os.path.join(home, ".gemini/config/agents")
    if os.path.isdir(p):
        for n in sorted(os.listdir(p)):
            if n.startswith(".") or n.lower() == "readme.md":
                continue
            fp = os.path.join(p, n)
            if os.path.isdir(fp) or (os.path.isfile(fp) and n.endswith(".md")):
                m = build_meta("agent", n, f".gemini/config/agents/{{n}}", [])
                if m:
                    items.append(m)

top_roots = sorted(["/" + d for d in os.listdir("/") if os.path.isdir("/" + d)])
print(json.dumps({{"items": items, "top_roots": top_roots}}))
PY"#,
                ws = if want_skill { "True" } else { "False" },
                wp = if want_plugin { "True" } else { "False" },
                we = if want_ext { "True" } else { "False" },
                wa = if want_agent { "True" } else { "False" },
            );

            let raw_out = run_remote_script(&probe.endpoint, &py_script)
                .context("Failed to inventory remote global skills/plugins/extensions")?;

            #[derive(Deserialize)]
            struct RemotePayload {
                items: Vec<CatalogItemMeta>,
                #[serde(default)]
                top_roots: Vec<String>,
            }

            let parsed: RemotePayload = serde_json::from_str(raw_out.trim())
                .context("Failed to parse JSON catalog inventory from remote host")?;
            Ok((parsed.items, parsed.top_roots.into_iter().collect()))
        }
    }
}

fn parse_semver_tuple(raw: &str) -> Option<(u64, u64, u64)> {
    let trimmed = raw
        .trim()
        .trim_start_matches('v')
        .trim_start_matches('V')
        .split(['-', '+'])
        .next()?
        .trim();
    let parts: Vec<&str> = trimmed.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let major = parts[0].parse::<u64>().ok()?;
    let minor = if parts.len() >= 2 {
        parts[1].parse::<u64>().ok()?
    } else {
        0
    };
    let patch = if parts.len() == 3 {
        parts[2].parse::<u64>().ok()?
    } else {
        0
    };
    Some((major, minor, patch))
}

fn format_epoch_short(secs: u64) -> String {
    if secs == 0 {
        return "n/a".to_string();
    }
    // Pure-Rust civil-from-days (Howard Hinnant algorithm) for UTC YYYY-MM-DD HH:MM
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", year, m, d, hour, min)
}

/// Builds a prioritized reconciliation & suggestion plan from Local (`A`) and Remote (`B`) inventories.
pub fn build_catalog_plan(
    local_items: &[CatalogItemMeta],
    remote_items: &[CatalogItemMeta],
    local_gemini_subdirs: &BTreeSet<String>,
    remote_gemini_subdirs: &BTreeSet<String>,
    remote_top_roots: &BTreeSet<String>,
    force_external_quarantine: bool,
) -> Vec<CatalogPlanEntry> {
    let mut local_map: BTreeMap<(CatalogCategory, String), CatalogItemMeta> = BTreeMap::new();
    let mut remote_map: BTreeMap<(CatalogCategory, String), CatalogItemMeta> = BTreeMap::new();
    let mut all_keys: BTreeSet<(CatalogCategory, String)> = BTreeSet::new();

    for it in local_items {
        let key = (it.category, it.name.clone());
        all_keys.insert(key.clone());
        local_map.insert(key, it.clone());
    }
    for it in remote_items {
        let key = (it.category, it.name.clone());
        all_keys.insert(key.clone());
        remote_map.insert(key, it.clone());
    }

    // Custom session directories present locally that do NOT exist on remote (e.g. external VM target)
    let unshared_local_stores: BTreeSet<String> = local_gemini_subdirs
        .difference(remote_gemini_subdirs)
        .filter(|s| {
            !matches!(
                s.as_str(),
                "antigravity"
                    | "antigravity-cli"
                    | "antigravity-ide"
                    | "config"
                    | "skills"
                    | "extensions"
                    | "bin"
                    | "tmp"
                    | "history"
                    | "logs"
                    | "memory"
            )
        })
        .cloned()
        .collect();

    let is_external_remote = force_external_quarantine || !unshared_local_stores.is_empty();

    let mut entries = Vec::new();

    for key in &all_keys {
        let (category, ref name) = *key;
        let l_opt = local_map.get(key).cloned();
        let r_opt = remote_map.get(key).cloned();

        // Check superseded `<prefix>-<base>` pattern against the counterpart
        let mut superseded_by = None;
        if let Some((prefix, base)) = name.split_once('-') {
            if prefix.len() >= 3
                && base.len() >= 4
                && prefix.chars().all(|c| c.is_ascii_lowercase())
            {
                let base_key = (category, base.to_string());
                if l_opt.is_none() && r_opt.is_some() {
                    if let (Some(r_item), Some(l_base)) = (&r_opt, local_map.get(&base_key)) {
                        if l_base.latest_mtime_secs >= r_item.latest_mtime_secs {
                            superseded_by = Some(base.to_string());
                        }
                    }
                } else if r_opt.is_none() && l_opt.is_some() {
                    if let (Some(l_item), Some(r_base)) = (&l_opt, remote_map.get(&base_key)) {
                        if r_base.latest_mtime_secs >= l_item.latest_mtime_secs {
                            superseded_by = Some(base.to_string());
                        }
                    }
                }
            }
        }

        // Evaluate quarantine / host-bound conditions
        let mut quarantined = false;
        let mut quarantine_reason = None;

        if let Some(ref l) = l_opt {
            if l.is_external_symlink {
                quarantined = true;
                quarantine_reason = Some(format!(
                    "Local symlink outside $HOME ({})",
                    l.external_symlink_target.as_deref().unwrap_or("external")
                ));
            } else if is_external_remote {
                let lower_name = l.name.to_ascii_lowercase();
                for store_tok in &unshared_local_stores {
                    let st_low = store_tok.to_ascii_lowercase();
                    if lower_name.contains(&st_low)
                        || l.referenced_gemini_subdirs
                            .iter()
                            .any(|x| x.eq_ignore_ascii_case(&st_low))
                    {
                        quarantined = true;
                        quarantine_reason = Some(format!(
                            "References local-only harness '{}' absent on remote",
                            store_tok
                        ));
                        break;
                    }
                }
                if !quarantined && !l.referenced_custom_roots.is_empty() {
                    for root in &l.referenced_custom_roots {
                        if !remote_top_roots.is_empty() && !remote_top_roots.contains(root) {
                            quarantined = true;
                            quarantine_reason = Some(format!(
                                "References non-portable mount '{}' absent on remote",
                                root
                            ));
                            break;
                        } else if force_external_quarantine {
                            quarantined = true;
                            quarantine_reason =
                                Some(format!("References host mount '{}'", root));
                            break;
                        }
                    }
                }
            }
        }

        if !quarantined {
            if let Some(ref r) = r_opt {
                if r.is_external_symlink {
                    quarantined = true;
                    quarantine_reason = Some(format!(
                        "Remote symlink outside $HOME ({})",
                        r.external_symlink_target.as_deref().unwrap_or("external")
                    ));
                } else if !r.referenced_custom_roots.is_empty() {
                    for root in &r.referenced_custom_roots {
                        if !Path::new(root).exists() {
                            quarantined = true;
                            quarantine_reason = Some(format!(
                                "References remote mount '{}' absent locally",
                                root
                            ));
                            break;
                        }
                    }
                }
            }
        }

        let (status, suggested_action) = match (&l_opt, &r_opt) {
            (Some(l), Some(r)) => {
                if l.content_digest == r.content_digest {
                    (CatalogDiffStatus::Identical, CatalogAction::SkipIdentical)
                } else {
                    let semver_cmp = match (&l.version, &r.version) {
                        (Some(lv), Some(rv)) => {
                            match (parse_semver_tuple(lv), parse_semver_tuple(rv)) {
                                (Some(lt), Some(rt)) if lt != rt => Some(lt.cmp(&rt)),
                                _ => None,
                            }
                        }
                        _ => None,
                    };

                    if let Some(ord) = semver_cmp {
                        if ord.is_gt() {
                            (
                                CatalogDiffStatus::LocalNewer,
                                if quarantined {
                                    CatalogAction::KeepAsIs
                                } else {
                                    CatalogAction::KeepLocalPush
                                },
                            )
                        } else {
                            (
                                CatalogDiffStatus::RemoteNewer,
                                if quarantined {
                                    CatalogAction::KeepAsIs
                                } else {
                                    CatalogAction::KeepRemotePull
                                },
                            )
                        }
                    } else if l.latest_mtime_secs > r.latest_mtime_secs + 2 {
                        (
                            CatalogDiffStatus::LocalNewer,
                            if quarantined {
                                CatalogAction::KeepAsIs
                            } else {
                                CatalogAction::KeepLocalPush
                            },
                        )
                    } else if r.latest_mtime_secs > l.latest_mtime_secs + 2 {
                        (
                            CatalogDiffStatus::RemoteNewer,
                            if quarantined {
                                CatalogAction::KeepAsIs
                            } else {
                                CatalogAction::KeepRemotePull
                            },
                        )
                    } else {
                        (CatalogDiffStatus::Diverged, CatalogAction::KeepAsIs)
                    }
                }
            }
            (Some(_), None) => {
                let act = if quarantined {
                    CatalogAction::KeepAsIs
                } else if superseded_by.is_some() {
                    CatalogAction::RemoveLocalOnly
                } else {
                    CatalogAction::KeepLocalPush
                };
                (CatalogDiffStatus::OnlyLocal, act)
            }
            (None, Some(_)) => {
                let act = if quarantined {
                    CatalogAction::KeepAsIs
                } else if superseded_by.is_some() {
                    CatalogAction::RemoveRemoteOnly
                } else {
                    CatalogAction::KeepRemotePull
                };
                (CatalogDiffStatus::OnlyRemote, act)
            }
            (None, None) => unreachable!(),
        };

        entries.push(CatalogPlanEntry {
            category,
            name: name.clone(),
            local: l_opt,
            remote: r_opt,
            status,
            suggested_action,
            quarantined,
            quarantine_reason,
            superseded_by,
        });
    }

    entries
}

fn format_side_summary(meta: Option<&CatalogItemMeta>) -> String {
    let Some(m) = meta else {
        return "absent".to_string();
    };
    let ver = m.version.as_deref().unwrap_or("unversioned");
    format!(
        "{} ({}, {}f, #{})",
        ver,
        format_epoch_short(m.latest_mtime_secs),
        m.file_count,
        &m.content_digest[..6.min(m.content_digest.len())]
    )
}

/// Prints the grouped Global Skills / Plugins / Extensions / Agents comparison & suggestion report.
pub fn print_catalog_plan(
    entries: &[CatalogPlanEntry],
    local_home: &str,
    probe: &RemoteProbe,
    show_identical: bool,
) {
    println!("================================================================================");
    println!(" Agy Global Skills, Plugins, Extensions & Agents Comparison Plan");
    println!("================================================================================");
    println!(" Local Host (A)  : {}", local_home);
    println!(
        " Remote Host (B) : {}:{}",
        probe.endpoint.display_label(),
        probe.remote_home
    );
    let identical_count = entries
        .iter()
        .filter(|e| e.status == CatalogDiffStatus::Identical)
        .count();
    let actionable_count = entries.len().saturating_sub(identical_count);
    println!(
        " Inventory Total : {} item(s) ({} identical, {} differing / host-unique)",
        entries.len(),
        identical_count,
        actionable_count
    );
    println!("================================================================================");

    for cat in [
        CatalogCategory::Skill,
        CatalogCategory::Plugin,
        CatalogCategory::Extension,
        CatalogCategory::Agent,
    ] {
        let cat_entries: Vec<_> = entries
            .iter()
            .filter(|e| {
                e.category == cat && (show_identical || e.status != CatalogDiffStatus::Identical)
            })
            .collect();
        let cat_identical = entries
            .iter()
            .filter(|e| e.category == cat && e.status == CatalogDiffStatus::Identical)
            .count();

        if cat_entries.is_empty() && cat_identical == 0 {
            continue;
        }

        println!();
        println!(
            "--- {} ({} differing, {} identical) ---",
            cat.plural_title(),
            cat_entries.len(),
            cat_identical
        );
        if cat_entries.is_empty() {
            println!("  All items in this category are byte-for-byte identical.");
            continue;
        }

        for e in cat_entries {
            let mut badges = Vec::new();
            if e.quarantined {
                badges.push(format!(
                    "QUARANTINED: {}",
                    e.quarantine_reason.as_deref().unwrap_or("host-bound")
                ));
            }
            if let Some(ref sup) = e.superseded_by {
                badges.push(format!("superseded by '{}'", sup));
            }
            let badge_str = if badges.is_empty() {
                String::new()
            } else {
                format!(" [{}]", badges.join("; "))
            };

            println!("  • {} [{}]{}", e.name, e.status.short_label(), badge_str);
            println!(
                "      Local (A)  : {}",
                format_side_summary(e.local.as_ref())
            );
            println!(
                "      Remote (B) : {}",
                format_side_summary(e.remote.as_ref())
            );
            println!(
                "      Suggestion : [{}] {}",
                e.suggested_action.prompt_code(),
                e.suggested_action.description()
            );
        }
    }
    println!();
    println!("================================================================================");
}

fn copy_and_remap_dir_or_file(
    src: &Path,
    dst: &Path,
    from_home: &str,
    to_home: &str,
) -> Result<()> {
    if src.is_file() {
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        let raw = fs::read(src)?;
        if raw.len() <= 512_000 {
            if let Ok(txt) = std::str::from_utf8(&raw) {
                let remapped = apply_home_remap_str(txt, from_home, to_home);
                fs::write(dst, remapped.as_bytes())?;
                return Ok(());
            }
        }
        fs::copy(src, dst)?;
        return Ok(());
    }

    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)?.flatten() {
        let fname_os = entry.file_name();
        let Some(fname) = fname_os.to_str() else {
            continue;
        };
        if SKIP_DIRS.contains(&fname) {
            continue;
        }
        let p = entry.path();
        let Ok(sm) = fs::symlink_metadata(&p) else {
            continue;
        };
        if sm.file_type().is_symlink() {
            continue;
        }
        let target = dst.join(fname);
        if sm.is_dir() {
            copy_and_remap_dir_or_file(&p, &target, from_home, to_home)?;
        } else if sm.is_file() {
            copy_and_remap_dir_or_file(&p, &target, from_home, to_home)?;
        }
    }
    Ok(())
}

fn remove_local_path_verbose(target: &Path) -> Result<()> {
    if fs::symlink_metadata(target).is_err() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let out = Command::new("rm").arg("-rfv").arg(target).output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            if !line.trim().is_empty() {
                println!("    [rm] {}", line.trim());
            }
        }
    }
    #[cfg(not(unix))]
    {
        if target.is_dir() {
            fs::remove_dir_all(target)?;
        } else {
            fs::remove_file(target)?;
        }
        println!("    [rm] {}", target.display());
    }
    Ok(())
}

fn create_skill_symlink_local(home_dir: &Path, link_rel: &str, canonical_rel: &str) {
    let link_full = home_dir.join(link_rel);
    let canonical_full = home_dir.join(canonical_rel);
    if link_full == canonical_full {
        return;
    }
    if let Some(p) = link_full.parent() {
        let _ = fs::create_dir_all(p);
    }
    if fs::symlink_metadata(&link_full).is_ok() {
        let _ = remove_local_path_verbose(&link_full);
    }
    #[cfg(unix)]
    {
        let _ = std::os::unix::fs::symlink(&canonical_full, &link_full);
    }
}

fn ensure_extension_enabled_local(home_dir: &Path, ext_name: &str) {
    let enablement_path = home_dir.join(".gemini/extensions/extension-enablement.json");
    if !enablement_path.exists() {
        return;
    }
    if let Ok(raw) = fs::read_to_string(&enablement_path) {
        if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(obj) = val.as_object_mut() {
                obj.entry(ext_name.to_string()).or_insert_with(|| {
                    serde_json::json!({
                        "overrides": ["*"]
                    })
                });
                if let Ok(pretty) = serde_json::to_string_pretty(&val) {
                    let _ = fs::write(&enablement_path, format!("{}\n", pretty));
                }
            }
        }
    }
}

/// Applies a single chosen `CatalogAction` for `entry` across Local (`A`) and Remote (`B`).
pub fn execute_catalog_action(
    entry: &CatalogPlanEntry,
    action: CatalogAction,
    local_home: &Path,
    probe: &RemoteProbe,
    dry_run: bool,
) -> Result<()> {
    if matches!(action, CatalogAction::SkipIdentical | CatalogAction::KeepAsIs) {
        return Ok(());
    }

    let local_home_str = local_home.to_string_lossy().trim_end_matches('/').to_string();

    match action {
        CatalogAction::KeepLocalPush => {
            let Some(ref l) = entry.local else {
                return Ok(());
            };
            println!(
                "  [PUSH A -> B] {} '{}' ({})",
                entry.category.label(),
                entry.name,
                l.rel_home_path
            );
            if dry_run {
                return Ok(());
            }

            let stage_tmp = tempfile::TempDir::new()?;
            let staged_item = stage_tmp.path().join(&l.rel_home_path);
            copy_and_remap_dir_or_file(
                &local_home.join(&l.rel_home_path),
                &staged_item,
                &local_home_str,
                &probe.remote_home,
            )?;

            match &probe.endpoint {
                RemoteEndpoint::LocalRoot { root_home } => {
                    let dst = root_home.join(&l.rel_home_path);
                    if fs::symlink_metadata(&dst).is_ok() {
                        remove_local_path_verbose(&dst)?;
                    }
                    copy_and_remap_dir_or_file(
                        &staged_item,
                        &dst,
                        &probe.remote_home,
                        &probe.remote_home,
                    )?;
                    if entry.category == CatalogCategory::Skill {
                        let mut links = l.skill_symlink_rels.clone();
                        if links.is_empty() && l.rel_home_path.starts_with(".agents/skills/") {
                            links.push(format!(".gemini/config/skills/{}", entry.name));
                        }
                        for lr in links {
                            create_skill_symlink_local(root_home, &lr, &l.rel_home_path);
                        }
                    } else if entry.category == CatalogCategory::Extension {
                        ensure_extension_enabled_local(root_home, &entry.name);
                    }
                }
                RemoteEndpoint::Ssh { .. } => {
                    let remote_dst = format!("{}/{}", probe.remote_home, l.rel_home_path);
                    let r_dst_q = shell_quote(&remote_dst);
                    let prep = format!(
                        "rm -rfv {rd} >/dev/null 2>&1 || true; mkdir -p $(dirname {rd})",
                        rd = r_dst_q
                    );
                    run_remote_script(&probe.endpoint, &prep)?;

                    let rels = vec![l.rel_home_path.clone()];
                    let skip_vec: Vec<String> = SKIP_DIRS.iter().map(|s| s.to_string()).collect();
                    if should_use_tar_transport() {
                        tar_push_paths(
                            &probe.endpoint,
                            stage_tmp.path(),
                            &probe.remote_home,
                            &rels,
                            &skip_vec,
                            false,
                            false,
                        )?;
                    } else {
                        let target = probe.endpoint.ssh_target().unwrap();
                        let transport_cmd = probe.endpoint.rsync_ssh_transport_cmd();
                        let mut cmd = Command::new("rsync");
                        cmd.arg("-az")
                            .arg("--delete")
                            .arg("-e")
                            .arg(&transport_cmd);
                        for ex in SKIP_DIRS {
                            cmd.arg("--exclude").arg(ex);
                        }
                        if l.is_file {
                            cmd.arg(&staged_item)
                                .arg(format!("{}:{}", target, remote_dst));
                        } else {
                            cmd.arg(format!("{}/", staged_item.display()))
                                .arg(format!("{}:{}/", target, remote_dst));
                        }
                        let out = cmd.output().context("Failed to run rsync for catalog push")?;
                        if !out.status.success() {
                            bail!(
                                "Catalog rsync push failed for '{}': {}",
                                entry.name,
                                String::from_utf8_lossy(&out.stderr).trim()
                            );
                        }
                    }

                    if entry.category == CatalogCategory::Skill {
                        let mut links = l.skill_symlink_rels.clone();
                        if links.is_empty() && l.rel_home_path.starts_with(".agents/skills/") {
                            links.push(format!(".gemini/config/skills/{}", entry.name));
                        }
                        for lr in links {
                            if lr != l.rel_home_path {
                                let link_abs = format!("{}/{}", probe.remote_home, lr);
                                let link_q = shell_quote(&link_abs);
                                let sym_cmd = format!(
                                    "mkdir -p $(dirname {lq}) && rm -rfv {lq} >/dev/null 2>&1 || true && ln -s {rd} {lq}",
                                    lq = link_q,
                                    rd = r_dst_q
                                );
                                let _ = run_remote_script(&probe.endpoint, &sym_cmd);
                            }
                        }
                    }
                }
            }
        }
        CatalogAction::KeepRemotePull => {
            let Some(ref r) = entry.remote else {
                return Ok(());
            };
            println!(
                "  [PULL B -> A] {} '{}' ({})",
                entry.category.label(),
                entry.name,
                r.rel_home_path
            );
            if dry_run {
                return Ok(());
            }

            let stage_tmp = tempfile::TempDir::new()?;
            match &probe.endpoint {
                RemoteEndpoint::LocalRoot { root_home } => {
                    copy_and_remap_dir_or_file(
                        &root_home.join(&r.rel_home_path),
                        &stage_tmp.path().join(&r.rel_home_path),
                        &probe.remote_home,
                        &local_home_str,
                    )?;
                }
                RemoteEndpoint::Ssh { .. } => {
                    let rels = vec![r.rel_home_path.clone()];
                    let skip_vec: Vec<String> = SKIP_DIRS.iter().map(|s| s.to_string()).collect();
                    tar_pull_paths(
                        &probe.endpoint,
                        &probe.remote_home,
                        stage_tmp.path(),
                        &rels,
                        &skip_vec,
                        false,
                        false,
                    )?;
                }
            }

            let pulled_src = stage_tmp.path().join(&r.rel_home_path);
            let local_dst = local_home.join(&r.rel_home_path);
            if fs::symlink_metadata(&local_dst).is_ok() {
                remove_local_path_verbose(&local_dst)?;
            }
            copy_and_remap_dir_or_file(&pulled_src, &local_dst, &probe.remote_home, &local_home_str)?;

            if entry.category == CatalogCategory::Skill {
                let mut links = r.skill_symlink_rels.clone();
                if links.is_empty() && r.rel_home_path.starts_with(".agents/skills/") {
                    links.push(format!(".gemini/skills/{}", entry.name));
                    links.push(format!(".gemini/config/skills/{}", entry.name));
                }
                for lr in links {
                    create_skill_symlink_local(local_home, &lr, &r.rel_home_path);
                }
            } else if entry.category == CatalogCategory::Extension {
                ensure_extension_enabled_local(local_home, &entry.name);
            }
        }
        CatalogAction::RemoveLocalOnly | CatalogAction::RemoveBoth => {
            if let Some(ref l) = entry.local {
                println!(
                    "  [REMOVE LOCAL (A)] {} '{}' ({})",
                    entry.category.label(),
                    entry.name,
                    l.rel_home_path
                );
                if !dry_run {
                    remove_local_path_verbose(&local_home.join(&l.rel_home_path))?;
                    for lr in &l.skill_symlink_rels {
                        remove_local_path_verbose(&local_home.join(lr))?;
                    }
                }
            }
            if action == CatalogAction::RemoveBoth {
                remove_remote_item(entry, probe, dry_run)?;
            }
        }
        CatalogAction::RemoveRemoteOnly => {
            remove_remote_item(entry, probe, dry_run)?;
        }
        CatalogAction::SkipIdentical | CatalogAction::KeepAsIs => {}
    }

    Ok(())
}

fn remove_remote_item(entry: &CatalogPlanEntry, probe: &RemoteProbe, dry_run: bool) -> Result<()> {
    let Some(ref r) = entry.remote else {
        return Ok(());
    };
    println!(
        "  [REMOVE REMOTE (B)] {} '{}' ({})",
        entry.category.label(),
        entry.name,
        r.rel_home_path
    );
    if dry_run {
        return Ok(());
    }
    match &probe.endpoint {
        RemoteEndpoint::LocalRoot { root_home } => {
            remove_local_path_verbose(&root_home.join(&r.rel_home_path))?;
            for lr in &r.skill_symlink_rels {
                remove_local_path_verbose(&root_home.join(lr))?;
            }
        }
        RemoteEndpoint::Ssh { .. } => {
            let mut targets = vec![shell_quote(&format!("{}/{}", probe.remote_home, r.rel_home_path))];
            for lr in &r.skill_symlink_rels {
                targets.push(shell_quote(&format!("{}/{}", probe.remote_home, lr)));
            }
            let script = format!("rm -rfv {}", targets.join(" "));
            let out = run_remote_script(&probe.endpoint, &script)?;
            for line in out.lines().take(20) {
                if !line.trim().is_empty() {
                    println!("    [remote rm] {}", line.trim());
                }
            }
        }
    }
    Ok(())
}

/// Runs the global `agy-sync skills` reconciliation command (dry-run report, batch suggestion mode, or interactive wizard).
pub fn run_catalog_command(
    local_home: &Path,
    local_gemini_root: &Path,
    endpoint: &RemoteEndpoint,
    categories: &BTreeSet<CatalogCategory>,
    dry_run: bool,
    show_identical: bool,
    force_external_quarantine: bool,
    apply_suggestions: bool,
) -> Result<()> {
    let local_home_str = local_home.to_string_lossy().to_string();
    let probe = probe_remote(endpoint, &local_home_str, local_home)?;

    println!("==> [1/3] Scanning Local global skills, plugins, extensions & agents ...");
    let local_items = scan_catalog_at_home(local_home, categories)?;

    println!(
        "==> [2/3] Scanning Remote global skills, plugins, extensions & agents on '{}' ...",
        probe.endpoint.display_label()
    );
    let (remote_items, remote_top_roots) = scan_remote_catalog(&probe, categories)?;

    let mut local_gemini_subdirs = BTreeSet::new();
    if let Ok(entries) = fs::read_dir(local_gemini_root) {
        for e in entries.flatten() {
            if e.path().is_dir() {
                if let Some(n) = e.file_name().to_str() {
                    local_gemini_subdirs.insert(n.to_string());
                }
            }
        }
    }

    let plan = build_catalog_plan(
        &local_items,
        &remote_items,
        &local_gemini_subdirs,
        &probe.remote_existing_subdirs,
        &remote_top_roots,
        force_external_quarantine,
    );

    print_catalog_plan(&plan, &local_home_str, &probe, show_identical);

    let actionable: Vec<&CatalogPlanEntry> = plan
        .iter()
        .filter(|e| e.status != CatalogDiffStatus::Identical)
        .collect();

    if actionable.is_empty() {
        println!(" All selected global skills, plugins, extensions, and agents are in sync!");
        return Ok(());
    }

    if dry_run {
        println!(" [DRY-RUN] Preview complete. Re-run without --dry-run to apply or review interactively.");
        return Ok(());
    }

    if apply_suggestions {
        println!("==> [3/3] Applying safe Keep-Local / Keep-Remote suggestions ...");
        let mut applied = 0usize;
        for entry in &actionable {
            if !entry.quarantined
                && matches!(
                    entry.suggested_action,
                    CatalogAction::KeepLocalPush | CatalogAction::KeepRemotePull
                )
            {
                execute_catalog_action(entry, entry.suggested_action, local_home, &probe, false)?;
                applied += 1;
            }
        }
        println!(" Completed {} automatic catalog synchronization(s).", applied);
        return Ok(());
    }

    if !io::stdin().is_terminal() {
        println!(" Non-interactive terminal detected; pass --apply-suggestions to execute safe syncs or run in a TTY.");
        return Ok(());
    }

    println!(" Choose how to reconcile the {} differing item(s):", actionable.len());
    println!("   [i] Step-by-step interactive review (confirm A / B / Keep Both / Remove per item)");
    println!("   [p] Auto-apply all non-quarantined Keep-A / Keep-B sync suggestions, then review removals");
    println!("   [q] Quit without making changes");
    print!(" Selection [default: i]: ");
    io::stdout().flush()?;
    let mut mode_line = String::new();
    io::stdin().lock().read_line(&mut mode_line)?;
    let mode_choice = mode_line.trim().to_ascii_lowercase();
    if mode_choice == "q" || mode_choice == "quit" {
        println!(" Aborted without changes.");
        return Ok(());
    }

    let auto_sync_safe = mode_choice == "p";
    let mut changes = 0usize;

    for (idx, entry) in actionable.iter().enumerate() {
        if auto_sync_safe
            && !entry.quarantined
            && matches!(
                entry.suggested_action,
                CatalogAction::KeepLocalPush | CatalogAction::KeepRemotePull
            )
        {
            execute_catalog_action(entry, entry.suggested_action, local_home, &probe, false)?;
            changes += 1;
            continue;
        }

        println!();
        println!(
            " [{}/{}] {} '{}' — {}",
            idx + 1,
            actionable.len(),
            entry.category.label(),
            entry.name,
            entry.status.short_label()
        );
        println!(
            "   Local (A)  : {}",
            format_side_summary(entry.local.as_ref())
        );
        println!(
            "   Remote (B) : {}",
            format_side_summary(entry.remote.as_ref())
        );
        if entry.quarantined {
            println!(
                "   Notice     : QUARANTINED ({})",
                entry.quarantine_reason.as_deref().unwrap_or("host-bound")
            );
        }
        if let Some(ref sup) = entry.superseded_by {
            println!("   Notice     : Likely superseded by newer '{}'", sup);
        }
        println!(
            "   Options    : [a] Keep Local (push A->B) | [b] Keep Remote (pull B->A) | [k] Keep As-Is | [d] Remove Both | [x] Remove A | [y] Remove B | [q] Quit"
        );
        print!(
            "   Action [Enter = '{}' ({})]: ",
            entry.suggested_action.prompt_code(),
            entry.suggested_action.description()
        );
        io::stdout().flush()?;
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        let ans = line.trim().to_ascii_lowercase();
        if ans == "q" || ans == "quit" {
            println!(" Stopping interactive review.");
            break;
        }

        let chosen = if ans.is_empty() {
            entry.suggested_action
        } else {
            match ans.as_str() {
                "a" => CatalogAction::KeepLocalPush,
                "b" => CatalogAction::KeepRemotePull,
                "k" | "s" => CatalogAction::KeepAsIs,
                "d" => CatalogAction::RemoveBoth,
                "x" => CatalogAction::RemoveLocalOnly,
                "y" => CatalogAction::RemoveRemoteOnly,
                _ => entry.suggested_action,
            }
        };

        if !matches!(chosen, CatalogAction::KeepAsIs | CatalogAction::SkipIdentical) {
            execute_catalog_action(entry, chosen, local_home, &probe, false)?;
            changes += 1;
        }
    }

    println!();
    println!(
        "==> Global catalog reconciliation complete ({} item action(s) executed).",
        changes
    );
    Ok(())
}
