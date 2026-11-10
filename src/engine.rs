use crate::config::SyncStateLedger;
use crate::discovery::{
    discover_project_sessions, hydrate_missing_counterpart_sessions, is_home_or_root_path,
    normalize_project_path, path_to_file_uri, ProjectDiscovery,
};
use crate::harnesses::{
    discover_single_harness, ensure_agy20_state_and_trust, merge_legacy_gemini_cli_metadata,
    sync_config_project_files, HarnessKind,
};
use crate::index_merge::{
     apply_home_remap_str, export_project_manifest, find_sessions_with_home_prefix,
    home_search_needles, merge_project_manifest, normalize_harness_app_data_dir,
    purge_sessions_from_harness, remap_conversation_artifacts, MergeStats,
};
use crate::transport::{
    ensure_sufficient_fd_limit, probe_remote, push_staged_remote_harness_index,
    reload_remote_agent_services_if_present, remote_checkpoint_or_clean_harness_wals,
    remote_purge_sessions_and_workspace, rsync_harness_relative_paths, rsync_workspace_dir,
    stage_remote_gemini_metadata, stage_remote_harness_index, RemoteEndpoint, RemoteProbe,
    RsyncTransferOpts,
};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn log_progress(msg: &str) {
    println!("==> {}", msg);
    let _ = io::stdout().flush();
}

fn log_substep(msg: &str) {
    println!("  • {}", msg);
    let _ = io::stdout().flush();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    Inspect,
    Sync,
    Push,
    Pull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAction {
    PushToRemote,
    PullToLocal,
    InSyncDeltaCheck,
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub mode: SyncMode,
    pub project_path: PathBuf,
    pub local_home: String,
    pub local_gemini_dir: PathBuf,
    pub selected_harnesses: Vec<HarnessKind>,
    pub endpoint: Option<RemoteEndpoint>,
    pub no_remap: bool,
    pub deep_remap: bool,
    pub dry_run: bool,
    pub delete: bool,
    pub include_build_dirs: bool,
    pub extra_excludes: Vec<String>,
}

impl EngineConfig {
    /// Resolves the root `~/.gemini` folder.
    pub fn local_gemini_root(&self) -> PathBuf {
        if self.local_gemini_dir.file_name().and_then(|n| n.to_str()) == Some(".gemini") {
            return self.local_gemini_dir.clone();
        }
        if let Some(parent) = self.local_gemini_dir.parent() {
            if parent.file_name().and_then(|n| n.to_str()) == Some(".gemini") {
                return parent.to_path_buf();
            }
        }
        self.local_gemini_dir.clone()
    }

    /// Resolves the local directory for a specific `HarnessKind`.
    pub fn local_dir_for_harness(&self, kind: &HarnessKind) -> PathBuf {
        let root = self.local_gemini_root();
        match kind.session_subdir() {
            Some(sub) => root.join(sub),
            None => match kind {
                HarnessKind::AgyIdePlugins => root.join("config"),
                HarnessKind::GeminiCliLegacy => root,
                _ => unreachable!(),
            },
        }
    }
}

/// Detailed step-delta information for a single conversation transferred during sync.
#[derive(Debug, Clone)]
pub struct SessionSyncDetail {
    pub conversation_id: String,
    pub title: String,
    pub parent_conversation_id: String,
    pub agent_name: String,
    pub before_steps: i64,
    pub after_steps: i64,
    pub total_bytes: u64,
}

impl SessionSyncDetail {
    pub fn display_title(&self) -> String {
        if !self.title.is_empty() {
            self.title.clone()
        } else if !self.parent_conversation_id.is_empty() {
            if self.agent_name.is_empty() {
                "(Subagent)".to_string()
            } else {
                format!("(Subagent: {})", self.agent_name)
            }
        } else {
            "(Root Session)".to_string()
        }
    }
}

/// Synchronization report for an individual harness.
#[derive(Debug, Clone)]
pub struct HarnessSyncReport {
    pub kind: HarnessKind,
    pub local_harness_dir: PathBuf,
    pub remote_harness_dir: String,
    pub local_discovery: Option<ProjectDiscovery>,
    pub remote_discovery: Option<ProjectDiscovery>,
    pub session_actions: BTreeMap<String, SessionAction>,
    pub pushed_sessions: Vec<SessionSyncDetail>,
    pub pulled_sessions: Vec<SessionSyncDetail>,
    pub local_merge_stats: MergeStats,
    pub remote_merge_stats: MergeStats,
    pub metadata_items_pushed: usize,
    pub metadata_items_pulled: usize,
}

#[derive(Debug, Clone)]
pub struct SyncReport {
    pub local_discovery: ProjectDiscovery,
    pub remote_probe: Option<RemoteProbe>,
    pub remote_discovery: Option<ProjectDiscovery>,
    pub session_actions: BTreeMap<String, SessionAction>,
    pub pushed_cids: BTreeSet<String>,
    pub pulled_cids: BTreeSet<String>,
    pub local_merge_stats: MergeStats,
    pub remote_merge_stats: MergeStats,
    pub harness_reports: Vec<HarnessSyncReport>,
}

pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GiB", b / GB)
    } else if b >= MB {
        format!("{:.2} MiB", b / MB)
    } else if b >= KB {
        format!("{:.1} KiB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// Builds the list of `rsync --exclude` patterns for the workspace folder.
fn build_workspace_excludes(config: &EngineConfig) -> Vec<String> {
    let mut excludes = vec![".DS_Store".to_string(), ".git/index.lock".to_string()];
    if !config.include_build_dirs {
        excludes.extend([
            "target".to_string(),
            "node_modules".to_string(),
            ".venv".to_string(),
            "__pycache__".to_string(),
            ".terraform/providers".to_string(),
            "terraform-provider-*".to_string(),
            ".terragrunt-cache".to_string(),
        ]);
    }
    excludes.extend(config.extra_excludes.iter().cloned());
    excludes
}

fn current_terraform_os_prefix() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin_",
        "linux" => "linux_",
        "windows" => "windows_",
        other => other,
    }
}

/// Self-heals a synchronized project workspace for Terraform / Terragrunt portability:
/// 1. When `!include_build_dirs`, removes disposable `.terragrunt-cache` directories and any
///    foreign-OS `.terraform/providers/**/<os>_<arch>` binary directories (e.g. `linux_amd64` on macOS),
///    while preserving `.terraform/terraform.tfstate`, `.terragrunt-local-state/`, `.parallel-state/`, and `*.tfstate*`.
/// 2. When `foreign_home` is provided, rewrites `foreign_home` -> `local_home` inside any `.tfstate` or
///    `.tfstate.backup` files so local backend state pointers remain valid across machines.
pub fn self_heal_workspace_terraform_artifacts(
    workspace_dir: &Path,
    foreign_home: Option<&str>,
    local_home: &str,
    include_build_dirs: bool,
    dry_run: bool,
) -> Result<(usize, usize)> {
    if !workspace_dir.exists() {
        return Ok((0, 0));
    }
    let mut removed_dirs = 0usize;
    let mut remapped_states = 0usize;
    let native_os_prefix = current_terraform_os_prefix();

    fn walk_tf(
        dir: &Path,
        in_tf_providers: bool,
        native_os_prefix: &str,
        foreign_home: Option<&str>,
        local_home: &str,
        include_build_dirs: bool,
        dry_run: bool,
        removed_dirs: &mut usize,
        remapped_states: &mut usize,
    ) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if p.is_dir() {
                if matches!(name, ".git" | "target" | "node_modules" | ".venv" | "__pycache__") {
                    continue;
                }
                if !include_build_dirs && name == ".terragrunt-cache" {
                    if !dry_run && fs::remove_dir_all(&p).is_ok() {
                        println!("  [self-heal] Removed .terragrunt-cache: {}", p.display());
                    }
                    *removed_dirs += 1;
                    continue;
                }
                if !include_build_dirs && in_tf_providers {
                    let is_os_arch_dir = (name.starts_with("linux_")
                        || name.starts_with("darwin_")
                        || name.starts_with("windows_")
                        || name.starts_with("freebsd_"))
                        && !name.starts_with(native_os_prefix);
                    if is_os_arch_dir {
                        if !dry_run && fs::remove_dir_all(&p).is_ok() {
                            println!(
                                "  [self-heal] Removed foreign-OS Terraform provider dir: {}",
                                p.display()
                            );
                        }
                        *removed_dirs += 1;
                        continue;
                    }
                }
                let next_in_providers = in_tf_providers
                    || (name == "providers"
                        && dir.file_name().and_then(|n| n.to_str()) == Some(".terraform"));
                walk_tf(
                    &p,
                    next_in_providers,
                    native_os_prefix,
                    foreign_home,
                    local_home,
                    include_build_dirs,
                    dry_run,
                    removed_dirs,
                    remapped_states,
                );
            } else if p.is_file() {
                if let Some(fh) = foreign_home.filter(|s| !s.is_empty() && *s != local_home) {
                    if name.ends_with(".tfstate") || name.ends_with(".tfstate.backup") {
                        if let Ok(content) = fs::read_to_string(&p) {
                            let updated = apply_home_remap_str(&content, fh, local_home);
                            if updated != content {
                                if !dry_run {
                                    let _ = fs::write(&p, updated);
                                }
                                *remapped_states += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    walk_tf(
        workspace_dir,
        false,
        native_os_prefix,
        foreign_home,
        local_home,
        include_build_dirs,
        dry_run,
        &mut removed_dirs,
        &mut remapped_states,
    );

    Ok((removed_dirs, remapped_states))
}

/// Copies specific relative files from `src_root` into `dst_root` so they can be remapped in-place
/// before pushing to Remote without altering the local machine's files.
fn stage_files_for_push_remap(
    src_root: &Path,
    dst_root: &Path,
    relative_paths: &[String],
    deep_remap: bool,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut direct_paths = Vec::new();
    let mut remapped_paths = Vec::new();

    for rel in relative_paths {
        let needs_staging = rel.starts_with("conversations/")
            || rel.starts_with("implicit/")
            || (deep_remap && rel.starts_with("brain/"));

        if needs_staging {
            let src_full = src_root.join(rel);
            let dst_full = dst_root.join(rel);
            if src_full.is_file() {
                if let Some(parent) = dst_full.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::copy(&src_full, &dst_full)?;
                remapped_paths.push(rel.clone());
            } else if src_full.is_dir() {
                copy_dir_recursive(&src_full, &dst_full)?;
                remapped_paths.push(rel.clone());
            }
        } else {
            direct_paths.push(rel.clone());
        }
    }

    Ok((direct_paths, remapped_paths))
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let p = entry.path();
        let target = dst.join(entry.file_name());
        if p.is_dir() {
            copy_dir_recursive(&p, &target)?;
        } else if p.is_file() {
            fs::copy(&p, &target)?;
        }
    }
    Ok(())
}

/// Prints a comprehensive `en-US` summary report of the discovered project sessions and bidirectional sync results.
pub fn print_report(report: &SyncReport, config: &EngineConfig) {
    println!("================================================================================");
    println!(" Agy Multi-Harness Project & Session Synchronization Engine (agy-sync)");
    println!("================================================================================");
    println!(
        " Local Workspace : {}",
        report.local_discovery.project_path.display()
    );
    println!(
        " Local Gemini Dir: {}",
        config.local_gemini_root().display()
    );
    let harness_names: Vec<&str> = report
        .harness_reports
        .iter()
        .map(|h| h.kind.cli_id())
        .collect();
    println!(" Active Harnesses: {}", harness_names.join(", "));

    if let Some(ref probe) = report.remote_probe {
        println!(" Remote Endpoint : {}", probe.endpoint.display_label());
        println!(" Remote Workspace: {}", probe.remote_project_path);
        println!(" Remote Gemini   : {}", probe.remote_gemini_root);
        let remap_active = !config.no_remap && config.local_home != probe.remote_home;
        println!(
            " $HOME Remapping : {}",
            if remap_active {
                format!("ACTIVE ({} <-> {})", config.local_home, probe.remote_home)
            } else {
                "DISABLED (--no-remap or identical $HOME)".to_string()
            }
        );
    }

    for hr in &report.harness_reports {
        println!("--------------------------------------------------------------------------------");
        println!(
            " Harness: {} [{}]",
            hr.kind.short_name(),
            hr.kind.display_path()
        );

        if let Some(ref l_disc) = hr.local_discovery {
            let mut all_cids: BTreeSet<String> = l_disc.sessions.keys().cloned().collect();
            if let Some(ref r_disc) = hr.remote_discovery {
                all_cids.extend(r_disc.sessions.keys().cloned());
            }

            if all_cids.is_empty() {
                println!("  No correlated sessions found in this harness for the project.");
            } else {
                println!(
                    "  Correlated Sessions ({} total, Local Size: {}):",
                    all_cids.len(),
                    format_bytes(l_disc.total_bytes)
                );
                for cid in &all_cids {
                    let l_sess = l_disc.sessions.get(cid);
                    let r_sess = hr
                        .remote_discovery
                        .as_ref()
                        .and_then(|d| d.sessions.get(cid));
                    let title = l_sess
                        .map(|s| s.title.as_str())
                        .or_else(|| r_sess.map(|s| s.title.as_str()))
                        .unwrap_or("(Untitled)");
                    let sess_ref = l_sess.or(r_sess);
                    let display_title = if !title.is_empty() {
                        title.to_string()
                    } else if let Some(s) = sess_ref {
                        if !s.parent_conversation_id.is_empty() {
                            if s.agent_name.is_empty() {
                                "(Subagent)".to_string()
                            } else {
                                format!("(Subagent: {})", s.agent_name)
                            }
                        } else {
                            "(Root Session)".to_string()
                        }
                    } else {
                        "(Untitled)".to_string()
                    };
                    let parent_tag = sess_ref
                        .filter(|s| {
                            !s.parent_conversation_id.is_empty()
                                && s.parent_conversation_id.len() >= 8
                        })
                        .map(|s| format!(" [child of {}]", &s.parent_conversation_id[..8]))
                        .unwrap_or_default();

                    let l_steps = l_sess
                        .map(|s| {
                            format!("{} steps ({})", s.step_count, format_bytes(s.total_bytes))
                        })
                        .unwrap_or_else(|| "absent".to_string());
                    let r_steps = r_sess
                        .map(|s| format!("{} steps", s.step_count))
                        .unwrap_or_else(|| {
                            if hr.remote_discovery.is_some() {
                                "absent".to_string()
                            } else {
                                "not probed".to_string()
                            }
                        });

                    let action_str = match hr.session_actions.get(cid) {
                        Some(SessionAction::PushToRemote) => "PUSH (Local -> Remote)",
                        Some(SessionAction::PullToLocal) => "PULL (Remote -> Local)",
                        Some(SessionAction::InSyncDeltaCheck) => "SYNC (Delta Check)",
                        None => "LOCAL_INSPECT",
                    };

                    println!(
                        "   • {} | {}{}\n       Local: {} | Remote: {} | Action: {}",
                        cid, display_title, parent_tag, l_steps, r_steps, action_str
                    );
                }
            }

            if !l_disc.knowledge_paths.is_empty() {
                println!(
                    "  Correlated Knowledge Topics: {:?}",
                    l_disc.knowledge_paths
                );
            }
        } else {
            println!(
                "  Metadata items synced -> Outbound (Pushed): {} | Inbound (Pulled): {}",
                hr.metadata_items_pushed, hr.metadata_items_pulled
            );
        }
    }

    if config.mode != SyncMode::Inspect {
        println!("================================================================================");
        println!(
            " BIDIRECTIONAL SYNC SUMMARY{}",
            if config.dry_run { " (DRY-RUN)" } else { "" }
        );
        println!("--------------------------------------------------------------------------------");

        // 1. Outbound (Local -> Remote)
        let total_pushed: usize = report
            .harness_reports
            .iter()
            .map(|h| h.pushed_sessions.len())
            .sum();
        println!(
            " Outbound (Local -> Remote): {} conversation(s) pushed",
            total_pushed
        );
        for hr in &report.harness_reports {
            for s in &hr.pushed_sessions {
                let delta = (s.after_steps - s.before_steps).max(0);
                println!(
                    "   [{}] {} | {}\n       Steps: {} -> {} (+{} steps, {})",
                    hr.kind.cli_id(),
                    s.conversation_id,
                    s.display_title(),
                    s.before_steps,
                    s.after_steps,
                    delta,
                    format_bytes(s.total_bytes)
                );
            }
            if hr.metadata_items_pushed > 0 && hr.local_discovery.is_none() {
                println!(
                    "   [{}] {} workspace metadata file(s) pushed",
                    hr.kind.cli_id(),
                    hr.metadata_items_pushed
                );
            }
        }

        println!("--------------------------------------------------------------------------------");
        // 2. Inbound (Remote -> Local)
        let total_pulled: usize = report
            .harness_reports
            .iter()
            .map(|h| h.pulled_sessions.len())
            .sum();
        println!(
            " Inbound (Remote -> Local): {} conversation(s) pulled",
            total_pulled
        );
        for hr in &report.harness_reports {
            for s in &hr.pulled_sessions {
                let delta = (s.after_steps - s.before_steps).max(0);
                println!(
                    "   [{}] {} | {}\n       Steps: {} -> {} (+{} steps, {})",
                    hr.kind.cli_id(),
                    s.conversation_id,
                    s.display_title(),
                    s.before_steps,
                    s.after_steps,
                    delta,
                    format_bytes(s.total_bytes)
                );
            }
            if hr.metadata_items_pulled > 0 && hr.local_discovery.is_none() {
                println!(
                    "   [{}] {} workspace metadata file(s) pulled",
                    hr.kind.cli_id(),
                    hr.metadata_items_pulled
                );
            }
        }

        println!("--------------------------------------------------------------------------------");
        println!(
            " Global Index Merge Totals -> Remote Upserted: {} (FTS: {}, Protobuf: {}) | Local Upserted: {} (FTS: {}, Protobuf: {})",
            report.remote_merge_stats.summaries_upserted,
            report.remote_merge_stats.fts_chunks_updated,
            report.remote_merge_stats.protobuf_entries_updated,
            report.local_merge_stats.summaries_upserted,
            report.local_merge_stats.fts_chunks_updated,
            report.local_merge_stats.protobuf_entries_updated,
        );
    }
    println!("================================================================================");
}

/// Synchronizes a single session-based harness (`agy-2.0`, `agy-cli`, `agy-ide`, or a custom session store).
fn run_session_harness(
    kind: &HarnessKind,
    config: &EngineConfig,
    norm_project: &Path,
    probe_opt: Option<&RemoteProbe>,
    staging_gemini_root: &Path,
    remap_enabled: bool,
) -> Result<HarnessSyncReport> {
    let subdir = kind.session_subdir().unwrap_or("antigravity");
    let local_harness_dir = config.local_dir_for_harness(kind);
    if config.mode != SyncMode::Inspect {
        log_progress(&format!(
            "[2/4] Discovering & comparing sessions in harness '{}' ({}) ...",
            kind.cli_id(),
            kind.display_path()
        ));
    }
    let mut local_discovery = discover_project_sessions(norm_project, &local_harness_dir)?;

    let Some(probe) = probe_opt else {
        return Ok(HarnessSyncReport {
            kind: kind.clone(),
            local_harness_dir,
            remote_harness_dir: String::new(),
            local_discovery: Some(local_discovery),
            remote_discovery: None,
            session_actions: BTreeMap::new(),
            pushed_sessions: Vec::new(),
            pulled_sessions: Vec::new(),
            local_merge_stats: MergeStats::default(),
            remote_merge_stats: MergeStats::default(),
            metadata_items_pushed: 0,
            metadata_items_pulled: 0,
        });
    };

    if matches!(kind, HarnessKind::CustomSessionDir(_))
        && !probe.remote_existing_subdirs.contains(subdir)
    {
        if config.mode != SyncMode::Inspect {
            log_substep(&format!(
                "Skipping custom harness '{}' ({}) — directory is not present on remote host '{}'.",
                kind.cli_id(),
                kind.display_path(),
                probe.endpoint.display_label()
            ));
        }
        return Ok(HarnessSyncReport {
            kind: kind.clone(),
            local_harness_dir,
            remote_harness_dir: String::new(),
            local_discovery: Some(local_discovery),
            remote_discovery: None,
            session_actions: BTreeMap::new(),
            pushed_sessions: Vec::new(),
            pulled_sessions: Vec::new(),
            local_merge_stats: MergeStats::default(),
            remote_merge_stats: MergeStats::default(),
            metadata_items_pushed: 0,
            metadata_items_pulled: 0,
        });
    }

    let remote_harness_dir = probe.remote_harness_dir(subdir);
    let staging_remote_harness = staging_gemini_root.join(subdir);
    if config.mode != SyncMode::Inspect {
        log_substep(&format!(
            "Staging remote session index from {}:{} ...",
            probe.endpoint.display_label(),
            remote_harness_dir
        ));
    }
    stage_remote_harness_index(probe, subdir, &staging_remote_harness)?;

    let mut remote_discovery = discover_project_sessions(
        Path::new(&probe.remote_project_path),
        &staging_remote_harness,
    )?;

    let mut session_actions = BTreeMap::new();
    let mut pushed_cids = BTreeSet::new();
    let mut pulled_cids = BTreeSet::new();
    let mut delta_both_cids = BTreeSet::new();
    let mut pushed_sessions = Vec::new();
    let mut pulled_sessions = Vec::new();

    let mut all_cids: BTreeSet<String> = local_discovery.sessions.keys().cloned().collect();
    all_cids.extend(remote_discovery.sessions.keys().cloned());

    hydrate_missing_counterpart_sessions(&mut local_discovery, &all_cids);
    hydrate_missing_counterpart_sessions(&mut remote_discovery, &all_cids);

    let local_gemini_root = config.local_gemini_root();
    let mut ledger = SyncStateLedger::load(&local_gemini_root);
    let proj_key = SyncStateLedger::canonical_project_key(norm_project);
    let is_tty = io::stdin().is_terminal();

    let mut local_purge_cids = BTreeSet::new();
    let mut remote_purge_cids = BTreeSet::new();

    for cid in &all_cids {
        let l_opt = local_discovery.sessions.get(cid);
        let r_opt = remote_discovery.sessions.get(cid);

        let previously_synced_both = ledger
            .projects
            .get(&proj_key)
            .and_then(|p| p.sessions.get(cid))
            .is_some_and(|s| {
                s.present_on_local
                    && s.present_on_remote
                    && (s.remote_synced_harnesses.contains(kind.cli_id())
                        || (s.remote_synced_harnesses.is_empty()
                            && s.harnesses.contains(kind.cli_id())))
            });

        let action = match config.mode {
            SyncMode::Push => {
                if l_opt.is_some() {
                    SessionAction::PushToRemote
                } else {
                    continue;
                }
            }
            SyncMode::Pull => {
                if r_opt.is_some() {
                    SessionAction::PullToLocal
                } else {
                    continue;
                }
            }
            SyncMode::Inspect => match (l_opt, r_opt) {
                (Some(_), None) => SessionAction::PushToRemote,
                (None, Some(_)) => SessionAction::PullToLocal,
                (Some(l), Some(r)) => {
                    if l.step_count > r.step_count
                        || (l.step_count == r.step_count
                            && l.last_modified_time > r.last_modified_time)
                    {
                        SessionAction::PushToRemote
                    } else if r.step_count > l.step_count
                        || (r.step_count == l.step_count
                            && r.last_modified_time > l.last_modified_time)
                    {
                        SessionAction::PullToLocal
                    } else {
                        SessionAction::InSyncDeltaCheck
                    }
                }
                (None, None) => unreachable!(),
            },
            SyncMode::Sync => match (l_opt, r_opt) {
                (Some(l), None) => {
                    if previously_synced_both && is_tty {
                        print!(
                            "  [?] Conversation '{}' ({}, {} steps) was previously synced, \
                             but is now deleted on Remote.\n      \
                             -> [d] Delete from Local as well, [r] Restore (push) to Remote, [s] Skip [default: s]: ",
                            if l.title.is_empty() {
                                "(Untitled)"
                            } else {
                                &l.title
                            },
                            cid,
                            l.step_count
                        );
                        let _ = io::stdout().flush();
                        let mut line = String::new();
                        let _ = io::stdin().lock().read_line(&mut line);
                        match line.trim().to_ascii_lowercase().as_str() {
                            "d" | "delete" => {
                                local_purge_cids.insert(cid.clone());
                                ledger.remove_session(norm_project, cid);
                                continue;
                            }
                            "r" | "restore" => SessionAction::PushToRemote,
                            _ => continue,
                        }
                    } else {
                        SessionAction::PushToRemote
                    }
                }
                (None, Some(r)) => {
                    if previously_synced_both && is_tty {
                        print!(
                            "  [?] Conversation '{}' ({}, {} steps) previously existed on Local, \
                             but was deleted locally.\n      \
                             -> [d] Delete from Remote as well, [r] Restore (pull) to Local, [s] Skip [default: s]: ",
                            if r.title.is_empty() {
                                "(Untitled)"
                            } else {
                                &r.title
                            },
                            cid,
                            r.step_count
                        );
                        let _ = io::stdout().flush();
                        let mut line = String::new();
                        let _ = io::stdin().lock().read_line(&mut line);
                        match line.trim().to_ascii_lowercase().as_str() {
                            "d" | "delete" => {
                                remote_purge_cids.insert(cid.clone());
                                ledger.remove_session(norm_project, cid);
                                continue;
                            }
                            "r" | "restore" => SessionAction::PullToLocal,
                            _ => continue,
                        }
                    } else {
                        SessionAction::PullToLocal
                    }
                }
                (Some(l), Some(r)) => {
                    if l.step_count > r.step_count
                        || (l.step_count == r.step_count
                            && l.last_modified_time > r.last_modified_time)
                    {
                        SessionAction::PushToRemote
                    } else if r.step_count > l.step_count
                        || (r.step_count == l.step_count
                            && r.last_modified_time > l.last_modified_time)
                    {
                        SessionAction::PullToLocal
                    } else {
                        SessionAction::InSyncDeltaCheck
                    }
                }
                (None, None) => unreachable!(),
            },
        };

        match action {
            SessionAction::PushToRemote => {
                pushed_cids.insert(cid.clone());
                if let Some(l) = l_opt {
                    pushed_sessions.push(SessionSyncDetail {
                        conversation_id: cid.clone(),
                        title: l.title.clone(),
                        parent_conversation_id: l.parent_conversation_id.clone(),
                        agent_name: l.agent_name.clone(),
                        before_steps: r_opt.map(|r| r.step_count).unwrap_or(0),
                        after_steps: l.step_count,
                        total_bytes: l.total_bytes,
                    });
                }
            }
            SessionAction::PullToLocal => {
                pulled_cids.insert(cid.clone());
                if let Some(r) = r_opt {
                    pulled_sessions.push(SessionSyncDetail {
                        conversation_id: cid.clone(),
                        title: r.title.clone(),
                        parent_conversation_id: r.parent_conversation_id.clone(),
                        agent_name: r.agent_name.clone(),
                        before_steps: l_opt.map(|l| l.step_count).unwrap_or(0),
                        after_steps: r.step_count,
                        total_bytes: r.total_bytes,
                    });
                }
            }
            SessionAction::InSyncDeltaCheck => {
                delta_both_cids.insert(cid.clone());
            }
        }
        session_actions.insert(cid.clone(), action);
    }

    if config.mode == SyncMode::Inspect {
        return Ok(HarnessSyncReport {
            kind: kind.clone(),
            local_harness_dir,
            remote_harness_dir,
            local_discovery: Some(local_discovery),
            remote_discovery: Some(remote_discovery),
            session_actions,
            pushed_sessions,
            pulled_sessions,
            local_merge_stats: MergeStats::default(),
            remote_merge_stats: MergeStats::default(),
            metadata_items_pushed: 0,
            metadata_items_pulled: 0,
        });
    }

    log_substep(&format!(
        "Found {} correlated session(s) in '{}' (Local total: {}): {} to push, {} to pull, {} in-sync delta check",
        all_cids.len(),
        kind.cli_id(),
        format_bytes(local_discovery.total_bytes),
        pushed_cids.len(),
        pulled_cids.len(),
        delta_both_cids.len()
    ));

    log_progress(&format!(
        "[3/4] Synchronizing session artifacts & global indexes for '{}' ...",
        kind.cli_id()
    ));

    let mut local_merge_stats = MergeStats::default();
    let mut remote_merge_stats = MergeStats::default();

    // 1. Pull Sessions (Remote -> Local)
    let mut pull_rel_set = BTreeSet::new();
    for cid in &pulled_cids {
        if let Some(sess) = remote_discovery.sessions.get(cid) {
            for rel in &sess.relative_paths {
                pull_rel_set.insert(rel.clone());
            }
        }
    }
    for cid in &delta_both_cids {
        if let Some(sess) = remote_discovery.sessions.get(cid) {
            for rel in &sess.relative_paths {
                // When step_count and last_modified_time are already equal on Local and Remote,
                // only delta-sync non-database/non-protobuf artifacts (e.g. brain/<cid> markdown files)
                // so machine-remapped conversations/<cid>.db and implicit/<cid>.pb are never clobbered.
                if !rel.starts_with("conversations/")
                    && !rel.starts_with("implicit/")
                    && !rel.starts_with("context_state/")
                {
                    pull_rel_set.insert(rel.clone());
                }
            }
        }
    }
    for kp in &remote_discovery.knowledge_paths {
        pull_rel_set.insert(kp.clone());
    }

    if !pull_rel_set.is_empty() {
        log_substep(&format!(
            "Pulling {} session artifact path(s) from Remote -> Local ...",
            pull_rel_set.len()
        ));
        remote_checkpoint_or_clean_harness_wals(probe, subdir, &pulled_cids, false, config.dry_run)?;
        let pull_paths: Vec<String> = pull_rel_set.into_iter().collect();
        let update_only = config.mode == SyncMode::Sync;
        rsync_harness_relative_paths(
            probe,
            subdir,
            &local_harness_dir,
            &pull_paths,
            false,
            update_only,
            config.dry_run,
        )?;

        if !config.dry_run {
            for cid in &pulled_cids {
                let _ = fs::remove_file(
                    local_harness_dir
                        .join("conversations")
                        .join(format!("{}.db-wal", cid)),
                );
                let _ = fs::remove_file(
                    local_harness_dir
                        .join("conversations")
                        .join(format!("{}.db-shm", cid)),
                );
            }
            if remap_enabled {
                let mut pull_remap_cids = pulled_cids.clone();
                pull_remap_cids.extend(delta_both_cids.iter().cloned());
                remap_conversation_artifacts(
                    &local_harness_dir,
                    &pull_remap_cids,
                    &probe.remote_home,
                    &config.local_home,
                    config.deep_remap,
                )?;
            }
        }
    }

    if !pulled_cids.is_empty() && !config.dry_run {
        log_substep(&format!(
            "Merging {} pulled session(s) into local SQLite & Protobuf indexes ...",
            pulled_cids.len()
        ));
        let remap_pair =
            remap_enabled.then_some((probe.remote_home.as_str(), config.local_home.as_str()));
        let mut remote_manifest =
            export_project_manifest(&staging_remote_harness, &pulled_cids, remap_pair)?;
        if let Some(ref pid) = remote_discovery.matched_project_ids.iter().next().cloned() {
            crate::index_merge::remap_manifest_project_ids(&mut remote_manifest, pid);
        }
        let force = config.mode == SyncMode::Pull;
        local_merge_stats =
            merge_project_manifest(&local_harness_dir, &remote_manifest, force)?;
        if subdir == "antigravity" {
            let _ = ensure_agy20_state_and_trust(&local_harness_dir, norm_project, config.dry_run);
        }

        // Also ensure any config/projects/<id>.json referenced by pulled sessions is synced locally
        if !remote_discovery.matched_project_ids.is_empty() {
            let _ = sync_config_project_files(
                staging_gemini_root,
                &config.local_gemini_root(),
                &remote_discovery.matched_project_ids,
                remap_pair,
            );
        }
    }

    // Self-heal any local session rows that still contain unremapped remote $HOME URIs
    if remap_enabled && !config.dry_run {
        let mut self_heal_cids =
            find_sessions_with_home_prefix(&local_harness_dir, &probe.remote_home);
        let remote_needles = home_search_needles(&probe.remote_home);
        for (cid, sess) in &local_discovery.sessions {
            if sess
                .workspace_uris
                .iter()
                .any(|u| remote_needles.iter().any(|n| u.contains(n)))
            {
                self_heal_cids.insert(cid.clone());
            }
        }
        if !self_heal_cids.is_empty() {
            log_substep(&format!(
                "Normalizing {} local session(s) with unremapped remote $HOME paths ...",
                self_heal_cids.len()
            ));
            remap_conversation_artifacts(
                &local_harness_dir,
                &self_heal_cids,
                &probe.remote_home,
                &config.local_home,
                config.deep_remap,
            )?;
            let remap_pair = Some((probe.remote_home.as_str(), config.local_home.as_str()));
            let heal_manifest =
                export_project_manifest(&local_harness_dir, &self_heal_cids, remap_pair)?;
            let heal_stats = merge_project_manifest(&local_harness_dir, &heal_manifest, true)?;
            local_merge_stats.summaries_upserted += heal_stats.summaries_upserted;
            local_merge_stats.fts_chunks_updated += heal_stats.fts_chunks_updated;
            local_merge_stats.protobuf_entries_updated += heal_stats.protobuf_entries_updated;
        }
    }

    // 2. Push Sessions (Local -> Remote)
    let mut push_rel_set = BTreeSet::new();
    for cid in &pushed_cids {
        if let Some(sess) = local_discovery.sessions.get(cid) {
            for rel in &sess.relative_paths {
                push_rel_set.insert(rel.clone());
            }
        }
    }
    for cid in &delta_both_cids {
        if let Some(sess) = local_discovery.sessions.get(cid) {
            for rel in &sess.relative_paths {
                if !rel.starts_with("conversations/")
                    && !rel.starts_with("implicit/")
                    && !rel.starts_with("context_state/")
                {
                    push_rel_set.insert(rel.clone());
                }
            }
        }
    }
    for kp in &local_discovery.knowledge_paths {
        push_rel_set.insert(kp.clone());
    }

    if !push_rel_set.is_empty() {
        log_substep(&format!(
            "Pushing {} session artifact path(s) from Local -> Remote ({}) ...",
            push_rel_set.len(),
            format_bytes(local_discovery.total_bytes)
        ));
        let push_paths: Vec<String> = push_rel_set.into_iter().collect();
        let update_only = config.mode == SyncMode::Sync;

        if remap_enabled && !config.dry_run {
            let push_remap_root = staging_gemini_root.join(format!("push_remap_{}", subdir));
            fs::create_dir_all(&push_remap_root)?;
            let (direct_paths, remapped_paths) = stage_files_for_push_remap(
                &local_harness_dir,
                &push_remap_root,
                &push_paths,
                config.deep_remap,
            )?;

            if !direct_paths.is_empty() {
                rsync_harness_relative_paths(
                    probe,
                    subdir,
                    &local_harness_dir,
                    &direct_paths,
                    true,
                    update_only,
                    config.dry_run,
                )?;
            }
            if !remapped_paths.is_empty() {
                let mut stage_cids = pushed_cids.clone();
                stage_cids.extend(delta_both_cids.iter().cloned());
                remap_conversation_artifacts(
                    &push_remap_root,
                    &stage_cids,
                    &config.local_home,
                    &probe.remote_home,
                    config.deep_remap,
                )?;
                rsync_harness_relative_paths(
                    probe,
                    subdir,
                    &push_remap_root,
                    &remapped_paths,
                    true,
                    update_only,
                    config.dry_run,
                )?;
            }
        } else {
            rsync_harness_relative_paths(
                probe,
                subdir,
                &local_harness_dir,
                &push_paths,
                true,
                update_only,
                config.dry_run,
            )?;
        }

        remote_checkpoint_or_clean_harness_wals(probe, subdir, &pushed_cids, true, config.dry_run)?;
    }

    if !pushed_cids.is_empty() && !config.dry_run {
        log_substep(&format!(
            "Merging {} pushed session(s) into remote SQLite & Protobuf indexes ...",
            pushed_cids.len()
        ));
        let remap_pair =
            remap_enabled.then_some((config.local_home.as_str(), probe.remote_home.as_str()));
        let mut local_manifest =
            export_project_manifest(&local_harness_dir, &pushed_cids, remap_pair)?;

        // Ensure workspace project manifest exists locally and map target_project_id
        let target_project_id = local_discovery
            .matched_project_ids
            .iter()
            .next()
            .cloned()
            .or_else(|| remote_discovery.matched_project_ids.iter().next().cloned())
            .or_else(|| {
                let proj_name = norm_project
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("project");
                crate::harnesses::ensure_workspace_project_manifest(
                    &config.local_gemini_root(),
                    norm_project,
                    proj_name,
                )
                .ok()
            });

        if let Some(ref pid) = target_project_id {
            crate::index_merge::remap_manifest_project_ids(&mut local_manifest, pid);
        }

        let force = config.mode == SyncMode::Push;
        remote_merge_stats =
            merge_project_manifest(&staging_remote_harness, &local_manifest, force)?;
        if subdir == "antigravity" {
            let _ = ensure_agy20_state_and_trust(
                &staging_remote_harness,
                Path::new(&probe.remote_project_path),
                config.dry_run,
            );
        }
        push_staged_remote_harness_index(probe, subdir, &staging_remote_harness, config.dry_run)?;

        let mut all_project_ids = local_discovery.matched_project_ids.clone();
        if let Some(ref pid) = target_project_id {
            all_project_ids.insert(pid.clone());
        }

        if !all_project_ids.is_empty() {
            let push_cfg_root = staging_gemini_root.join("push_cfg_overlay");
            let count = sync_config_project_files(
                &config.local_gemini_root(),
                &push_cfg_root,
                &all_project_ids,
                remap_pair,
            )?;
            if count > 0 {
                let cfg_rels: Vec<String> = all_project_ids
                    .iter()
                    .map(|id| format!("config/projects/{}.json", id))
                    .collect();
                let _ = rsync_harness_relative_paths(
                    probe,
                    "",
                    &push_cfg_root,
                    &cfg_rels,
                    true,
                    false,
                    config.dry_run,
                );
            }
        }

        let _ = reload_remote_agent_services_if_present(&probe.endpoint);
    } else if subdir == "antigravity"
        && !all_cids.is_empty()
        && !config.dry_run
        && config.mode != SyncMode::Inspect
        && (!staging_remote_harness
            .join("antigravity_state.pbtxt")
            .exists()
            || !staging_remote_harness.join("settings.json").exists())
    {
        let _ = ensure_agy20_state_and_trust(
            &staging_remote_harness,
            Path::new(&probe.remote_project_path),
            config.dry_run,
        );
        let _ = push_staged_remote_harness_index(
            probe,
            subdir,
            &staging_remote_harness,
            config.dry_run,
        );
    }

    if !local_purge_cids.is_empty() {
        log_substep(&format!(
            "Purging {} deleted session(s) from local harness '{}' ...",
            local_purge_cids.len(),
            kind.cli_id()
        ));
        let _ = purge_sessions_from_harness(&local_harness_dir, &local_purge_cids, config.dry_run);
    }
    if !remote_purge_cids.is_empty() {
        log_substep(&format!(
            "Purging {} deleted session(s) from remote harness '{}' ...",
            remote_purge_cids.len(),
            kind.cli_id()
        ));
        let _ = remote_purge_sessions_and_workspace(
            probe,
            subdir,
            &staging_remote_harness,
            &remote_purge_cids,
            false,
            config.dry_run,
        );
    }

    if !config.dry_run {
        let _ = normalize_harness_app_data_dir(&local_harness_dir);
        for cid in &all_cids {
            if local_purge_cids.contains(cid) || remote_purge_cids.contains(cid) {
                continue;
            }
            let l_opt = local_discovery.sessions.get(cid);
            let r_opt = remote_discovery.sessions.get(cid);
            let sess_ref = l_opt.or(r_opt);
            if let Some(s) = sess_ref {
                let max_steps = l_opt
                    .map(|x| x.step_count)
                    .unwrap_or(0)
                    .max(r_opt.map(|x| x.step_count).unwrap_or(0));
                let on_local = l_opt.is_some() || pulled_cids.contains(cid);
                let on_remote = r_opt.is_some() || pushed_cids.contains(cid);
                ledger.record_session(
                    norm_project,
                    &probe.remote_project_path,
                    kind.cli_id(),
                    cid,
                    &s.title,
                    max_steps,
                    &s.parent_conversation_id,
                    on_local,
                    on_remote,
                );
            }
        }
        let _ = ledger.save(&local_gemini_root);
    }

    Ok(HarnessSyncReport {
        kind: kind.clone(),
        local_harness_dir,
        remote_harness_dir,
        local_discovery: Some(local_discovery),
        remote_discovery: Some(remote_discovery),
        session_actions,
        pushed_sessions,
        pulled_sessions,
        local_merge_stats,
        remote_merge_stats,
        metadata_items_pushed: 0,
        metadata_items_pulled: 0,
    })
}

/// Synchronizes `AgyIdePlugins` (`~/.gemini/config/projects/<id>.json`) or `GeminiCliLegacy` (`projects.json`, `history/<slug>`, `tmp/<slug>`).
fn run_metadata_harness(
    kind: &HarnessKind,
    config: &EngineConfig,
    norm_project: &Path,
    probe_opt: Option<&RemoteProbe>,
    staging_gemini_root: &Path,
    remap_enabled: bool,
) -> Result<HarnessSyncReport> {
    let local_gemini_root = config.local_gemini_root();
    let local_h = discover_single_harness(kind, norm_project, &local_gemini_root)?;

    let Some(probe) = probe_opt else {
        return Ok(HarnessSyncReport {
            kind: kind.clone(),
            local_harness_dir: local_h.harness_dir,
            remote_harness_dir: String::new(),
            local_discovery: None,
            remote_discovery: None,
            session_actions: BTreeMap::new(),
            pushed_sessions: Vec::new(),
            pulled_sessions: Vec::new(),
            local_merge_stats: MergeStats::default(),
            remote_merge_stats: MergeStats::default(),
            metadata_items_pushed: 0,
            metadata_items_pulled: 0,
        });
    };

    stage_remote_gemini_metadata(probe, staging_gemini_root)?;
    let remote_h = discover_single_harness(
        kind,
        Path::new(&probe.remote_project_path),
        staging_gemini_root,
    )?;

    if config.mode == SyncMode::Inspect {
        return Ok(HarnessSyncReport {
            kind: kind.clone(),
            local_harness_dir: local_h.harness_dir,
            remote_harness_dir: probe.remote_gemini_root.clone(),
            local_discovery: None,
            remote_discovery: None,
            session_actions: BTreeMap::new(),
            pushed_sessions: Vec::new(),
            pulled_sessions: Vec::new(),
            local_merge_stats: MergeStats::default(),
            remote_merge_stats: MergeStats::default(),
            metadata_items_pushed: 0,
            metadata_items_pulled: 0,
        });
    }

    let mut pushed_items = 0usize;
    let mut pulled_items = 0usize;

    match kind {
        HarnessKind::AgyIdePlugins => {
            if matches!(config.mode, SyncMode::Sync | SyncMode::Pull)
                && !remote_h.config_project_ids.is_empty()
                && !config.dry_run
            {
                let remap_pair = remap_enabled
                    .then_some((probe.remote_home.as_str(), config.local_home.as_str()));
                pulled_items = sync_config_project_files(
                    staging_gemini_root,
                    &local_gemini_root,
                    &remote_h.config_project_ids,
                    remap_pair,
                )?;
            }
            if matches!(config.mode, SyncMode::Sync | SyncMode::Push)
                && !local_h.config_project_ids.is_empty()
            {
                let remap_pair = remap_enabled
                    .then_some((config.local_home.as_str(), probe.remote_home.as_str()));
                let push_cfg_root = staging_gemini_root.join("push_plugins_overlay");
                pushed_items = sync_config_project_files(
                    &local_gemini_root,
                    &push_cfg_root,
                    &local_h.config_project_ids,
                    remap_pair,
                )?;
                if pushed_items > 0 {
                    let rels: Vec<String> = local_h
                        .config_project_ids
                        .iter()
                        .map(|id| format!("config/projects/{}.json", id))
                        .collect();
                    rsync_harness_relative_paths(
                        probe,
                        "",
                        &push_cfg_root,
                        &rels,
                        true,
                        config.mode == SyncMode::Sync,
                        config.dry_run,
                    )?;
                }
            }
        }
        HarnessKind::GeminiCliLegacy => {
            let update_only = config.mode == SyncMode::Sync;
            if matches!(config.mode, SyncMode::Sync | SyncMode::Pull) {
                if let Some(ref slug) = remote_h.legacy_project_slug {
                    let pull_rels = vec![format!("history/{}", slug), format!("tmp/{}", slug)];
                    let _ = rsync_harness_relative_paths(
                        probe,
                        "",
                        &local_gemini_root,
                        &pull_rels,
                        false,
                        update_only,
                        config.dry_run,
                    );
                    if !config.dry_run {
                        let remap_pair = remap_enabled
                            .then_some((probe.remote_home.as_str(), config.local_home.as_str()));
                        merge_legacy_gemini_cli_metadata(
                            &local_gemini_root,
                            &norm_project.to_string_lossy(),
                            slug,
                            remap_pair,
                        )?;
                    }
                    pulled_items = 1;
                }
            }
            if matches!(config.mode, SyncMode::Sync | SyncMode::Push) {
                if let Some(ref slug) = local_h.legacy_project_slug {
                    if !local_h.gemini_root_relative_paths.is_empty() {
                        rsync_harness_relative_paths(
                            probe,
                            "",
                            &local_gemini_root,
                            &local_h.gemini_root_relative_paths,
                            true,
                            update_only,
                            config.dry_run,
                        )?;
                    }
                    if !config.dry_run {
                        let remap_pair = remap_enabled
                            .then_some((config.local_home.as_str(), probe.remote_home.as_str()));
                        merge_legacy_gemini_cli_metadata(
                            staging_gemini_root,
                            &probe.remote_project_path,
                            slug,
                            remap_pair,
                        )?;
                        rsync_harness_relative_paths(
                            probe,
                            "",
                            staging_gemini_root,
                            &["projects.json".to_string()],
                            true,
                            false,
                            config.dry_run,
                        )?;
                    }
                    pushed_items = 1;
                }
            }
        }
        _ => unreachable!(),
    }

    Ok(HarnessSyncReport {
        kind: kind.clone(),
        local_harness_dir: local_h.harness_dir,
        remote_harness_dir: probe.remote_gemini_root.clone(),
        local_discovery: None,
        remote_discovery: None,
        session_actions: BTreeMap::new(),
        pushed_sessions: Vec::new(),
        pulled_sessions: Vec::new(),
        local_merge_stats: MergeStats::default(),
        remote_merge_stats: MergeStats::default(),
        metadata_items_pushed: pushed_items,
        metadata_items_pulled: pulled_items,
    })
}

/// Executes the configured `Inspect`, `Sync`, `Push`, or `Pull` operation across all `config.selected_harnesses`.
pub fn run_engine(config: &EngineConfig) -> Result<SyncReport> {
    ensure_sufficient_fd_limit();

    let norm_project = normalize_project_path(&config.project_path);
    let norm_gemini_root = normalize_project_path(&config.local_gemini_root());

    if config.mode != SyncMode::Inspect {
        if is_home_or_root_path(&norm_project, &config.local_home)
            || norm_project.starts_with(&norm_gemini_root)
        {
            bail!(
                "Refusing to synchronize '{}' directly as a project workspace (it is $HOME or ~/.gemini). \
                 Please run agy-sync from inside a specific project folder or pass -p <project-dir>.",
                norm_project.display()
            );
        }
    }

    let harnesses = if config.selected_harnesses.is_empty() {
        vec![HarnessKind::Agy20]
    } else {
        config.selected_harnesses.clone()
    };

    let probe_opt = if let Some(ref ep) = config.endpoint {
        if config.mode != SyncMode::Inspect {
            log_progress(&format!(
                "[1/4] Probing remote endpoint {} ...",
                ep.display_label()
            ));
        }
        let p = probe_remote(ep, &config.local_home, &norm_project)?;
        if config.mode != SyncMode::Inspect {
            log_substep(&format!("Local Workspace : {}", norm_project.display()));
            log_substep(&format!("Remote Workspace: {}", p.remote_project_path));
            log_substep(&format!(
                "$HOME Remapping : {} <-> {}",
                config.local_home, p.remote_home
            ));
        }
        Some(p)
    } else if config.mode != SyncMode::Inspect {
        bail!("A remote endpoint (--remote) is required for sync/push/pull");
    } else {
        None
    };

    let remap_enabled = probe_opt.as_ref().is_some_and(|p| {
        !config.no_remap
            && !config.local_home.is_empty()
            && !p.remote_home.is_empty()
            && config.local_home != p.remote_home
    });

    let staging_dir = TempDir::new()?;
    let staging_gemini_root = staging_dir.path().join("remote_gemini");
    fs::create_dir_all(&staging_gemini_root)?;

    let mut harness_reports = Vec::with_capacity(harnesses.len());
    for kind in &harnesses {
        let hr = if kind.session_subdir().is_some() {
            run_session_harness(
                kind,
                config,
                &norm_project,
                probe_opt.as_ref(),
                &staging_gemini_root,
                remap_enabled,
            )?
        } else {
            run_metadata_harness(
                kind,
                config,
                &norm_project,
                probe_opt.as_ref(),
                &staging_gemini_root,
                remap_enabled,
            )?
        };
        harness_reports.push(hr);
    }

    // 4. Synchronize the project workspace folder via rsync -avz --partial --progress
    if config.mode != SyncMode::Inspect {
        let probe = probe_opt.as_ref().unwrap();
        let excludes = build_workspace_excludes(config);
        log_progress(&format!(
            "[4/4] Synchronizing project workspace folder ({}) ...",
            norm_project.display()
        ));
        match config.mode {
            SyncMode::Push => {
                let opts = RsyncTransferOpts {
                    update_only: false,
                    delete: config.delete,
                    dry_run: config.dry_run,
                    excludes,
                };
                log_substep("Pushing workspace files (Local -> Remote) ...");
                rsync_workspace_dir(probe, &norm_project, true, &opts)?;
            }
            SyncMode::Pull => {
                let opts = RsyncTransferOpts {
                    update_only: false,
                    delete: config.delete,
                    dry_run: config.dry_run,
                    excludes,
                };
                log_substep("Pulling workspace files (Remote -> Local) ...");
                rsync_workspace_dir(probe, &norm_project, false, &opts)?;
            }
            SyncMode::Sync => {
                let opts = RsyncTransferOpts {
                    update_only: true,
                    delete: false,
                    dry_run: config.dry_run,
                    excludes,
                };
                log_substep("Pushing newer workspace files (Local -> Remote) ...");
                rsync_workspace_dir(probe, &norm_project, true, &opts)?;
                log_substep("Pulling newer workspace files (Remote -> Local) ...");
                rsync_workspace_dir(probe, &norm_project, false, &opts)?;
            }
            SyncMode::Inspect => unreachable!(),
        }

        let foreign_home_opt = remap_enabled.then_some(probe.remote_home.as_str());
        let _ = self_heal_workspace_terraform_artifacts(
            &norm_project,
            foreign_home_opt,
            &config.local_home,
            config.include_build_dirs,
            config.dry_run,
        );
    }

    let mut pushed_cids = BTreeSet::new();
    let mut pulled_cids = BTreeSet::new();
    let mut session_actions = BTreeMap::new();
    let mut local_merge_stats = MergeStats::default();
    let mut remote_merge_stats = MergeStats::default();

    for hr in &harness_reports {
        for s in &hr.pushed_sessions {
            pushed_cids.insert(s.conversation_id.clone());
        }
        for s in &hr.pulled_sessions {
            pulled_cids.insert(s.conversation_id.clone());
        }
        for (k, v) in &hr.session_actions {
            session_actions.insert(k.clone(), *v);
        }
        local_merge_stats.summaries_upserted += hr.local_merge_stats.summaries_upserted;
        local_merge_stats.fts_chunks_updated += hr.local_merge_stats.fts_chunks_updated;
        local_merge_stats.protobuf_entries_updated += hr.local_merge_stats.protobuf_entries_updated;
        remote_merge_stats.summaries_upserted += hr.remote_merge_stats.summaries_upserted;
        remote_merge_stats.fts_chunks_updated += hr.remote_merge_stats.fts_chunks_updated;
        remote_merge_stats.protobuf_entries_updated +=
            hr.remote_merge_stats.protobuf_entries_updated;
    }

    let fallback_harness_dir = config.local_dir_for_harness(&harnesses[0]);
    let primary_local_discovery = harness_reports
        .iter()
        .find_map(|h| h.local_discovery.clone())
        .unwrap_or_else(|| {
            discover_project_sessions(&norm_project, &fallback_harness_dir).unwrap_or(
                ProjectDiscovery {
                    project_path: norm_project.clone(),
                    project_uri: path_to_file_uri(&norm_project),
                    harness_dir: fallback_harness_dir,
                    sessions: BTreeMap::new(),
                    knowledge_paths: Vec::new(),
                    matched_project_ids: BTreeSet::new(),
                    all_relative_paths: Vec::new(),
                    total_bytes: 0,
                },
            )
        });

    let primary_remote_discovery = harness_reports
        .iter()
        .find_map(|h| h.remote_discovery.clone());

    Ok(SyncReport {
        local_discovery: primary_local_discovery,
        remote_probe: probe_opt,
        remote_discovery: primary_remote_discovery,
        session_actions,
        pushed_cids,
        pulled_cids,
        local_merge_stats,
        remote_merge_stats,
        harness_reports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_terraform_state_preservation_and_leftover_self_healing() -> Result<()> {
        let tmp = TempDir::new()?;
        let ws = tmp.path().join("project");
        let tg_cache = ws.join("infra/uat/.terragrunt-cache/abc/def");
        let tg_state_dir = ws.join("infra/uat/.terragrunt-local-state");
        let tf_dir = ws.join("terraform/.terraform");
        let foreign_os = if std::env::consts::OS == "macos" {
            "linux_amd64"
        } else {
            "darwin_arm64"
        };
        let native_os = if std::env::consts::OS == "macos" {
            "darwin_arm64"
        } else {
            "linux_amd64"
        };
        let foreign_prov = tf_dir
            .join("providers/registry.terraform.io/hashicorp/google/8.10.0")
            .join(foreign_os);
        let native_prov = tf_dir
            .join("providers/registry.terraform.io/hashicorp/google/8.10.0")
            .join(native_os);

        fs::create_dir_all(&tg_cache)?;
        fs::create_dir_all(&tg_state_dir)?;
        fs::create_dir_all(&foreign_prov)?;
        fs::create_dir_all(&native_prov)?;

        fs::write(tg_cache.join("backend_tg.tf"), b"# cached")?;
        fs::write(
            tg_state_dir.join("terraform.tfstate"),
            r#"{"version":4,"outputs":{"path":"/home/remoteuser/project/out"}}"#,
        )?;
        fs::write(
            tf_dir.join("terraform.tfstate"),
            r#"{"version":3,"backend":{"type":"local","config":{"path":"/home/remoteuser/project/infra/uat/.terragrunt-local-state/terraform.tfstate"}}}"#,
        )?;
        fs::write(foreign_prov.join("terraform-provider-google"), b"elf-bin")?;
        fs::write(native_prov.join("terraform-provider-google"), b"native-bin")?;

        let (removed_dirs, remapped_states) = self_heal_workspace_terraform_artifacts(
            &ws,
            Some("/home/remoteuser"),
            "/Users/localdev",
            false,
            false,
        )?;
        assert_eq!(removed_dirs, 2);
        assert_eq!(remapped_states, 2);

        // .terragrunt-cache and foreign provider binary dir are gone
        assert!(!ws.join("infra/uat/.terragrunt-cache").exists());
        assert!(!foreign_prov.exists());

        // Native provider binary and all .tfstate files are preserved and $HOME-remapped
        assert!(native_prov.exists());
        let backend_tfstate = fs::read_to_string(tf_dir.join("terraform.tfstate"))?;
        assert!(backend_tfstate.contains("/Users/localdev/project/infra/uat/.terragrunt-local-state/terraform.tfstate"));
        let local_tfstate = fs::read_to_string(tg_state_dir.join("terraform.tfstate"))?;
        assert!(local_tfstate.contains("/Users/localdev/project/out"));

        Ok(())
    }
}
