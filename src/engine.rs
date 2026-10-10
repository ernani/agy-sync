use crate::discovery::{discover_project_sessions, normalize_project_path, ProjectDiscovery};
use crate::harnesses::{
    discover_single_harness, merge_legacy_gemini_cli_metadata, sync_config_project_files,
    HarnessKind,
};
use crate::index_merge::{
    export_project_manifest, merge_project_manifest, remap_conversation_artifacts, MergeStats,
};
use crate::transport::{
    probe_remote, push_staged_remote_harness_index, remote_checkpoint_or_clean_harness_wals,
    rsync_harness_relative_paths, rsync_workspace_dir, stage_remote_gemini_metadata,
    stage_remote_harness_index, RemoteEndpoint, RemoteProbe, RsyncTransferOpts,
};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

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
            "target/".to_string(),
            "node_modules/".to_string(),
            ".venv/".to_string(),
            "__pycache__/".to_string(),
        ]);
    }
    excludes.extend(config.extra_excludes.iter().cloned());
    excludes
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
    let local_discovery = discover_project_sessions(norm_project, &local_harness_dir)?;

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

    let remote_harness_dir = probe.remote_harness_dir(subdir);
    let staging_remote_harness = staging_gemini_root.join(subdir);
    stage_remote_harness_index(probe, subdir, &staging_remote_harness)?;

    let remote_discovery = discover_project_sessions(
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

    for cid in &all_cids {
        let l_opt = local_discovery.sessions.get(cid);
        let r_opt = remote_discovery.sessions.get(cid);

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
            SyncMode::Inspect | SyncMode::Sync => match (l_opt, r_opt) {
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
                pull_rel_set.insert(rel.clone());
            }
        }
    }
    for kp in &remote_discovery.knowledge_paths {
        pull_rel_set.insert(kp.clone());
    }

    if !pull_rel_set.is_empty() {
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
                remap_conversation_artifacts(
                    &local_harness_dir,
                    &pulled_cids,
                    &probe.remote_home,
                    &config.local_home,
                    config.deep_remap,
                )?;
            }
        }
    }

    if !pulled_cids.is_empty() && !config.dry_run {
        let remap_pair =
            remap_enabled.then_some((probe.remote_home.as_str(), config.local_home.as_str()));
        let remote_manifest =
            export_project_manifest(&staging_remote_harness, &pulled_cids, remap_pair)?;
        let force = config.mode == SyncMode::Pull;
        local_merge_stats =
            merge_project_manifest(&local_harness_dir, &remote_manifest, force)?;

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
                push_rel_set.insert(rel.clone());
            }
        }
    }
    for kp in &local_discovery.knowledge_paths {
        push_rel_set.insert(kp.clone());
    }

    if !push_rel_set.is_empty() {
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
        let remap_pair =
            remap_enabled.then_some((config.local_home.as_str(), probe.remote_home.as_str()));
        let local_manifest =
            export_project_manifest(&local_harness_dir, &pushed_cids, remap_pair)?;
        let force = config.mode == SyncMode::Push;
        remote_merge_stats =
            merge_project_manifest(&staging_remote_harness, &local_manifest, force)?;
        push_staged_remote_harness_index(probe, subdir, &staging_remote_harness, config.dry_run)?;

        if !local_discovery.matched_project_ids.is_empty() {
            let push_cfg_root = staging_gemini_root.join("push_cfg_overlay");
            let count = sync_config_project_files(
                &config.local_gemini_root(),
                &push_cfg_root,
                &local_discovery.matched_project_ids,
                remap_pair,
            )?;
            if count > 0 {
                let cfg_rels: Vec<String> = local_discovery
                    .matched_project_ids
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
    let norm_project = normalize_project_path(&config.project_path);
    let harnesses = if config.selected_harnesses.is_empty() {
        vec![HarnessKind::Agy20]
    } else {
        config.selected_harnesses.clone()
    };

    let probe_opt = if let Some(ref ep) = config.endpoint {
        Some(probe_remote(ep, &config.local_home, &norm_project)?)
    } else if config.mode != SyncMode::Inspect {
        bail!("A remote endpoint (--remote) is required for sync/push/pull");
    } else {
        None
    };

    // 1. If performing Sync/Push/Pull, synchronize the project workspace folder first via rsync -avz
    if config.mode != SyncMode::Inspect {
        let probe = probe_opt.as_ref().unwrap();
        let excludes = build_workspace_excludes(config);
        match config.mode {
            SyncMode::Push => {
                let opts = RsyncTransferOpts {
                    update_only: false,
                    delete: config.delete,
                    dry_run: config.dry_run,
                    excludes,
                };
                rsync_workspace_dir(probe, &norm_project, true, &opts)?;
            }
            SyncMode::Pull => {
                let opts = RsyncTransferOpts {
                    update_only: false,
                    delete: config.delete,
                    dry_run: config.dry_run,
                    excludes,
                };
                rsync_workspace_dir(probe, &norm_project, false, &opts)?;
            }
            SyncMode::Sync => {
                let opts = RsyncTransferOpts {
                    update_only: true,
                    delete: false,
                    dry_run: config.dry_run,
                    excludes,
                };
                rsync_workspace_dir(probe, &norm_project, true, &opts)?;
                rsync_workspace_dir(probe, &norm_project, false, &opts)?;
            }
            SyncMode::Inspect => unreachable!(),
        }
    }

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
                    project_uri: format!("file://{}", norm_project.display()),
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
