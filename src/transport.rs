use crate::config::parse_host_port_user;
use crate::discovery::find_summary_pb_files;
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Represents the remote endpoint for synchronization:
/// either an SSH host (`[user@]host[:port]`) or a local filesystem root (`local:/path/to/home`).
#[derive(Debug, Clone)]
pub enum RemoteEndpoint {
    Ssh {
        host: String,
        port: Option<u16>,
        user: Option<String>,
        control_path: Option<PathBuf>,
    },
    LocalRoot {
        root_home: PathBuf,
    },
}

impl RemoteEndpoint {
    /// Parses a remote specification string (`local:/path` or `[user@]host[:port]`).
    pub fn from_raw_spec(raw: &str) -> Self {
        if let Some(local_path) = raw.strip_prefix("local:") {
            RemoteEndpoint::LocalRoot {
                root_home: PathBuf::from(local_path),
            }
        } else {
            let (user, host, port) = parse_host_port_user(raw);
            RemoteEndpoint::Ssh {
                host,
                port,
                user,
                control_path: None,
            }
        }
    }

    /// Formats the endpoint for human display in reports.
    pub fn display_label(&self) -> String {
        match self {
            RemoteEndpoint::Ssh {
                host,
                port,
                user,
                ..
            } => {
                let u_prefix = user
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|u| format!("{}@", u))
                    .unwrap_or_default();
                let p_suffix = port.map(|p| format!(":{}", p)).unwrap_or_default();
                format!("ssh://{}{}{}", u_prefix, host, p_suffix)
            }
            RemoteEndpoint::LocalRoot { root_home } => format!("local://{}", root_home.display()),
        }
    }

    /// Returns `[user@]host` for `ssh` and `rsync` target specifications.
    pub fn ssh_target(&self) -> Option<String> {
        match self {
            RemoteEndpoint::Ssh { host, user, .. } => {
                if let Some(u) = user.as_deref().filter(|s| !s.is_empty()) {
                    Some(format!("{}@{}", u, host))
                } else {
                    Some(host.clone())
                }
            }
            RemoteEndpoint::LocalRoot { .. } => None,
        }
    }

    /// Builds the argument vector for `ssh` invocations.
    pub fn ssh_args(&self, batch_mode: bool) -> Vec<String> {
        let mut args = vec![
            "-o".to_string(),
            "RemoteCommand=none".to_string(),
            "-o".to_string(),
            "RequestTTY=no".to_string(),
            "-o".to_string(),
            "ConnectTimeout=10".to_string(),
        ];
        if batch_mode {
            args.push("-o".to_string());
            args.push("BatchMode=yes".to_string());
        }
        if let RemoteEndpoint::Ssh {
            port, control_path, ..
        } = self
        {
            if let Some(p) = port {
                args.push("-p".to_string());
                args.push(p.to_string());
            }
            if let Some(cp) = control_path {
                args.push("-o".to_string());
                args.push(format!("ControlPath={}", cp.display()));
            } else {
                args.push("-o".to_string());
                args.push("ControlMaster=auto".to_string());
                args.push("-o".to_string());
                args.push("ControlPersist=60".to_string());
                args.push("-o".to_string());
                args.push("ControlPath=/tmp/agy-sync-mux-%C".to_string());
            }
        }
        args
    }

    /// Builds the `-e` transport string for `rsync` over SSH.
    pub fn rsync_ssh_transport_cmd(&self) -> String {
        let mut cmd = String::from("ssh -o RemoteCommand=none -o RequestTTY=no");
        if let RemoteEndpoint::Ssh {
            port, control_path, ..
        } = self
        {
            if let Some(p) = port {
                cmd.push_str(&format!(" -p {}", p));
            }
            if let Some(cp) = control_path {
                cmd.push_str(&format!(
                    " -o ControlPath={}",
                    shell_quote(&cp.to_string_lossy())
                ));
            } else {
                cmd.push_str(
                    " -o ControlMaster=auto -o ControlPersist=60 -o ControlPath=/tmp/agy-sync-mux-%C",
                );
            }
        }
        cmd
    }
}

/// Information probed from the remote environment.
#[derive(Debug, Clone)]
pub struct RemoteProbe {
    pub endpoint: RemoteEndpoint,
    pub remote_home: String,
    pub remote_gemini_root: String,
    pub remote_project_path: String,
}

impl RemoteProbe {
    pub fn remote_harness_dir(&self, subdir: &str) -> String {
        if subdir.is_empty() || subdir == "." {
            self.remote_gemini_root.clone()
        } else {
            format!("{}/{}", self.remote_gemini_root, subdir.trim_matches('/'))
        }
    }
}

/// Safely single-quotes a string for POSIX `sh` execution over SSH.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Tests whether non-interactive SSH authentication (`BatchMode=yes`) succeeds for `endpoint`.
pub fn check_ssh_auth(endpoint: &RemoteEndpoint) -> Result<bool> {
    match endpoint {
        RemoteEndpoint::LocalRoot { .. } => Ok(true),
        RemoteEndpoint::Ssh { .. } => {
            let target = endpoint.ssh_target().unwrap();
            let mut cmd = Command::new("ssh");
            for arg in endpoint.ssh_args(true) {
                cmd.arg(arg);
            }
            cmd.arg(&target).arg("printf 'OK'");
            let output = cmd
                .output()
                .with_context(|| format!("Failed to invoke ssh for '{}'", target))?;
            Ok(output.status.success())
        }
    }
}

/// Launches an interactive SSH `ControlMaster` bootstrap session (`-f -N -M -S <sock>`)
/// inheriting the terminal so the user can enter their password/passphrase/OTP once,
/// and all subsequent `ssh` and `rsync` commands reuse the authenticated socket.
pub fn authenticate_ssh_interactive(
    endpoint: &mut RemoteEndpoint,
    user_override: Option<String>,
) -> Result<()> {
    let RemoteEndpoint::Ssh {
        user, control_path, ..
    } = endpoint
    else {
        return Ok(());
    };

    if let Some(u) = user_override.filter(|s| !s.trim().is_empty()) {
        *user = Some(u.trim().to_string());
    }

    let pid = std::process::id();
    let sock_path = PathBuf::from(format!("/tmp/agy-sync-ssh-{}.sock", pid));
    if sock_path.exists() {
        let _ = fs::remove_file(&sock_path);
    }
    *control_path = Some(sock_path.clone());

    let target = endpoint.ssh_target().unwrap();
    let mut cmd = Command::new("ssh");
    cmd.arg("-f")
        .arg("-N")
        .arg("-M")
        .arg("-S")
        .arg(&sock_path)
        .arg("-o")
        .arg("ControlPersist=4h");
    for arg in endpoint.ssh_args(false) {
        cmd.arg(arg);
    }
    cmd.arg(&target);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let status = cmd.status().with_context(|| {
        format!(
            "Failed to start interactive SSH authentication to '{}'",
            target
        )
    })?;
    if !status.success() {
        bail!(
            "Interactive SSH authentication to '{}' failed (status {})",
            target,
            status
        );
    }
    Ok(())
}

/// Probes the remote endpoint to determine its `$HOME` directory and calculate the counterpart
/// project path and `~/.gemini` paths.
pub fn probe_remote(
    endpoint: &RemoteEndpoint,
    local_home: &str,
    local_project_path: &Path,
) -> Result<RemoteProbe> {
    let remote_home = match endpoint {
        RemoteEndpoint::LocalRoot { root_home } => {
            root_home.to_string_lossy().trim_end_matches('/').to_string()
        }
        RemoteEndpoint::Ssh { .. } => {
            let target = endpoint.ssh_target().unwrap();
            let mut cmd = Command::new("ssh");
            for arg in endpoint.ssh_args(true) {
                cmd.arg(arg);
            }
            cmd.arg(&target).arg("printf '%s' \"$HOME\"");
            let output = cmd
                .output()
                .with_context(|| format!("Failed to execute ssh to host '{}'", target))?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!(
                    "SSH probe to '{}' failed (status {}): {}",
                    target,
                    output.status,
                    stderr.trim()
                );
            }
            let out_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if out_str.is_empty() {
                bail!("SSH probe to '{}' returned empty $HOME", target);
            }
            out_str.trim_end_matches('/').to_string()
        }
    };

    let local_proj_str = local_project_path.to_string_lossy();
    let local_home_trimmed = local_home.trim_end_matches('/');

    let remote_project_path = if let Some(rel) = local_proj_str.strip_prefix(local_home_trimmed) {
        format!("{}{}", remote_home, rel)
    } else {
        local_proj_str.to_string()
    };

    let remote_gemini_root = format!("{}/.gemini", remote_home);

    Ok(RemoteProbe {
        endpoint: endpoint.clone(),
        remote_home,
        remote_gemini_root,
        remote_project_path,
    })
}

/// Runs a shell script on the remote endpoint (or locally if `LocalRoot`).
pub fn run_remote_script(endpoint: &RemoteEndpoint, script: &str) -> Result<String> {
    match endpoint {
        RemoteEndpoint::LocalRoot { .. } => {
            let output = Command::new("sh")
                .arg("-c")
                .arg(script)
                .output()
                .context("Failed to execute local sh script")?;
            if !output.status.success() {
                bail!(
                    "Local script failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
        RemoteEndpoint::Ssh { .. } => {
            let target = endpoint.ssh_target().unwrap();
            let mut cmd = Command::new("ssh");
            for arg in endpoint.ssh_args(true) {
                cmd.arg(arg);
            }
            cmd.arg(&target).arg(script);
            let output = cmd
                .output()
                .with_context(|| format!("Failed to run remote script on {}", target))?;
            if !output.status.success() {
                bail!(
                    "Remote command on '{}' failed: {}",
                    target,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        }
    }
}

/// Stages the remote `~/.gemini` config (`config/projects/*.json`, `projects.json`, and `history/*/.project_root`)
/// into `staging_gemini_root` so project-ID and legacy-slug resolution works identically on remote staging.
pub fn stage_remote_gemini_metadata(
    probe: &RemoteProbe,
    staging_gemini_root: &Path,
) -> Result<()> {
    fs::create_dir_all(staging_gemini_root.join("config/projects"))?;
    match &probe.endpoint {
        RemoteEndpoint::LocalRoot { .. } => {
            let r_root = Path::new(&probe.remote_gemini_root);
            let r_proj_json = r_root.join("projects.json");
            if r_proj_json.exists() {
                fs::copy(&r_proj_json, staging_gemini_root.join("projects.json"))?;
            }
            let r_cfg_projects = r_root.join("config/projects");
            if let Ok(entries) = fs::read_dir(&r_cfg_projects) {
                for entry in entries.flatten() {
                    if entry.path().is_file() {
                        fs::copy(
                            entry.path(),
                            staging_gemini_root
                                .join("config/projects")
                                .join(entry.file_name()),
                        )?;
                    }
                }
            }
            for sub in ["history", "tmp"] {
                let r_sub = r_root.join(sub);
                if let Ok(entries) = fs::read_dir(&r_sub) {
                    for entry in entries.flatten() {
                        if entry.path().is_dir() {
                            let dst_slug = staging_gemini_root.join(sub).join(entry.file_name());
                            fs::create_dir_all(&dst_slug)?;
                            let pr = entry.path().join(".project_root");
                            if pr.exists() {
                                fs::copy(&pr, dst_slug.join(".project_root"))?;
                            }
                        }
                    }
                }
            }
        }
        RemoteEndpoint::Ssh { .. } => {
            let target = probe.endpoint.ssh_target().unwrap();
            let transport_cmd = probe.endpoint.rsync_ssh_transport_cmd();
            let r_proj_spec = format!("{}:{}/projects.json", target, probe.remote_gemini_root);
            let _ = Command::new("rsync")
                .arg("-az")
                .arg("-e")
                .arg(&transport_cmd)
                .arg(&r_proj_spec)
                .arg(staging_gemini_root.join("projects.json"))
                .output();

            let r_cfg_spec = format!("{}:{}/config/projects/", target, probe.remote_gemini_root);
            let _ = Command::new("rsync")
                .arg("-az")
                .arg("-e")
                .arg(&transport_cmd)
                .arg(&r_cfg_spec)
                .arg(format!(
                    "{}/",
                    staging_gemini_root.join("config/projects").display()
                ))
                .output();
        }
    }
    Ok(())
}

/// Stages a remote session-based harness index (`conversation_summaries.db`, `*_summaries_proto.pb`,
/// `history.jsonl`, `knowledge/*/metadata.json`, and directory skeleton) into `staging_harness_dir`.
pub fn stage_remote_harness_index(
    probe: &RemoteProbe,
    harness_subdir: &str,
    staging_harness_dir: &Path,
) -> Result<()> {
    fs::create_dir_all(staging_harness_dir)?;
    let remote_dir_str = probe.remote_harness_dir(harness_subdir);

    // Also stage parent ../config/projects so project_id resolution works for Agy 2.0 / VS Code
    if let Some(parent_gemini) = staging_harness_dir.parent() {
        let _ = stage_remote_gemini_metadata(probe, parent_gemini);
    }

    match &probe.endpoint {
        RemoteEndpoint::LocalRoot { .. } => {
            let remote_dir = Path::new(&remote_dir_str);
            if !remote_dir.exists() {
                return Ok(());
            }
            let db_file = remote_dir.join("conversation_summaries.db");
            if db_file.exists() {
                if let Ok(conn) = rusqlite::Connection::open(&db_file) {
                    let _ = conn.busy_timeout(std::time::Duration::from_millis(2000));
                    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
                }
                fs::copy(&db_file, staging_harness_dir.join("conversation_summaries.db"))?;
            }
            for pb_file in find_summary_pb_files(remote_dir) {
                if let Some(fname) = pb_file.file_name() {
                    fs::copy(&pb_file, staging_harness_dir.join(fname))?;
                }
            }
            let hist_file = remote_dir.join("history.jsonl");
            if hist_file.exists() {
                fs::copy(&hist_file, staging_harness_dir.join("history.jsonl"))?;
            }
            for sub in [
                "conversations",
                "brain",
                "annotations",
                "implicit",
                "browser_recordings",
                "html_artifacts",
                "context_state",
            ] {
                let src_sub = remote_dir.join(sub);
                let dst_sub = staging_harness_dir.join(sub);
                if let Ok(entries) = fs::read_dir(&src_sub) {
                    fs::create_dir_all(&dst_sub)?;
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let dst_item = dst_sub.join(&name);
                        if entry.path().is_dir() {
                            fs::create_dir_all(&dst_item)?;
                        } else {
                            fs::write(&dst_item, b"")?;
                        }
                    }
                }
            }
            let src_know = remote_dir.join("knowledge");
            if let Ok(entries) = fs::read_dir(&src_know) {
                for entry in entries.flatten() {
                    let meta = entry.path().join("metadata.json");
                    if meta.exists() {
                        let dst_topic =
                            staging_harness_dir.join("knowledge").join(entry.file_name());
                        fs::create_dir_all(&dst_topic)?;
                        fs::copy(&meta, dst_topic.join("metadata.json"))?;
                    }
                }
            }
        }
        RemoteEndpoint::Ssh { .. } => {
            let target = probe.endpoint.ssh_target().unwrap();
            let transport_cmd = probe.endpoint.rsync_ssh_transport_cmd();
            let r_dir = shell_quote(&remote_dir_str);
            let prep_script = format!(
                "mkdir -p {rj} && cd {rj} && \
                 if [ -f conversation_summaries.db ] && command -v sqlite3 >/dev/null 2>&1; then \
                   sqlite3 conversation_summaries.db 'PRAGMA busy_timeout=2000; PRAGMA wal_checkpoint(TRUNCATE);' >/dev/null 2>&1 || true; \
                 fi && \
                 for p in *_summaries_proto.pb; do \
                   if [ -f \"$p\" ]; then printf 'PB:%s\\n' \"$p\"; fi; \
                 done && \
                 for d in conversations brain annotations implicit browser_recordings html_artifacts context_state; do \
                   if [ -d \"$d\" ]; then \
                     for f in \"$d\"/*; do \
                       if [ -d \"$f\" ]; then printf 'D:%s\\n' \"$f\"; \
                       elif [ -f \"$f\" ]; then printf 'F:%s\\n' \"$f\"; fi; \
                     done; \
                   fi; \
                 done",
                rj = r_dir
            );
            let mut pb_files = Vec::new();
            if let Ok(listing) = run_remote_script(&probe.endpoint, &prep_script) {
                for line in listing.lines() {
                    if let Some(pb) = line.strip_prefix("PB:") {
                        pb_files.push(pb.to_string());
                    } else if let Some(rel) = line.strip_prefix("D:") {
                        let _ = fs::create_dir_all(staging_harness_dir.join(rel));
                    } else if let Some(rel) = line.strip_prefix("F:") {
                        if let Some(parent) = staging_harness_dir.join(rel).parent() {
                            let _ = fs::create_dir_all(parent);
                        }
                        let _ = fs::write(staging_harness_dir.join(rel), b"");
                    }
                }
            }

            let mut files_to_pull = vec![
                "conversation_summaries.db".to_string(),
                "history.jsonl".to_string(),
            ];
            files_to_pull.extend(pb_files);

            for fname in files_to_pull {
                let remote_spec = format!("{}:{}/{}", target, remote_dir_str, fname);
                let mut cmd = Command::new("rsync");
                cmd.arg("-az")
                    .arg("-e")
                    .arg(&transport_cmd)
                    .arg(&remote_spec)
                    .arg(staging_harness_dir.join(&fname));
                let _ = cmd.output();
            }
        }
    }

    Ok(())
}

/// Checkpoints SQLite WALs on the remote machine for specific `conversation_ids` before pulling them,
/// or cleans up stale `.db-wal`/`.db-shm` after pushing updated `.db` files to the remote machine.
pub fn remote_checkpoint_or_clean_harness_wals(
    probe: &RemoteProbe,
    harness_subdir: &str,
    conversation_ids: &BTreeSet<String>,
    clean_after_push: bool,
    dry_run: bool,
) -> Result<()> {
    if dry_run || conversation_ids.is_empty() {
        return Ok(());
    }
    let remote_dir_str = probe.remote_harness_dir(harness_subdir);

    match &probe.endpoint {
        RemoteEndpoint::LocalRoot { .. } => {
            let remote_dir = Path::new(&remote_dir_str);
            if clean_after_push {
                for cid in conversation_ids {
                    let _ = fs::remove_file(
                        remote_dir
                            .join("conversations")
                            .join(format!("{}.db-wal", cid)),
                    );
                    let _ = fs::remove_file(
                        remote_dir
                            .join("conversations")
                            .join(format!("{}.db-shm", cid)),
                    );
                }
            } else {
                crate::discovery::checkpoint_sqlite_dbs(remote_dir, conversation_ids);
            }
        }
        RemoteEndpoint::Ssh { .. } => {
            let r_dir = shell_quote(&remote_dir_str);
            let mut cmds = Vec::new();
            for cid in conversation_ids {
                if !crate::discovery::is_uuid(cid) {
                    continue;
                }
                if clean_after_push {
                    cmds.push(format!(
                        "rm -fv {rj}/conversations/{cid}.db-wal {rj}/conversations/{cid}.db-shm >/dev/null 2>&1 || true",
                        rj = r_dir,
                        cid = cid
                    ));
                } else {
                    cmds.push(format!(
                        "if [ -f {rj}/conversations/{cid}.db ] && command -v sqlite3 >/dev/null 2>&1; then \
                           sqlite3 {rj}/conversations/{cid}.db 'PRAGMA busy_timeout=1500; PRAGMA wal_checkpoint(PASSIVE);' >/dev/null 2>&1 || true; \
                         fi",
                        rj = r_dir,
                        cid = cid
                    ));
                }
            }
            if !cmds.is_empty() {
                let _ = run_remote_script(&probe.endpoint, &cmds.join(" && "));
            }
        }
    }
    Ok(())
}

/// Pushes the merged `conversation_summaries.db` and `*_summaries_proto.pb` from `staging_harness_dir`
/// back to the remote harness directory, backing up the previous remote files first.
pub fn push_staged_remote_harness_index(
    probe: &RemoteProbe,
    harness_subdir: &str,
    staging_harness_dir: &Path,
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        return Ok(());
    }
    let remote_dir_str = probe.remote_harness_dir(harness_subdir);

    let mut staged_files = vec![staging_harness_dir.join("conversation_summaries.db")];
    staged_files.extend(find_summary_pb_files(staging_harness_dir));

    match &probe.endpoint {
        RemoteEndpoint::LocalRoot { .. } => {
            let remote_dir = Path::new(&remote_dir_str);
            fs::create_dir_all(remote_dir)?;
            crate::index_merge::backup_global_indexes(remote_dir)?;

            for src in staged_files {
                if src.exists() {
                    if let Some(fname) = src.file_name() {
                        let dst = remote_dir.join(fname);
                        fs::copy(&src, &dst)?;
                    }
                }
            }
            let _ = fs::remove_file(remote_dir.join("conversation_summaries.db-wal"));
            let _ = fs::remove_file(remote_dir.join("conversation_summaries.db-shm"));
        }
        RemoteEndpoint::Ssh { .. } => {
            let target = probe.endpoint.ssh_target().unwrap();
            let transport_cmd = probe.endpoint.rsync_ssh_transport_cmd();
            let r_dir = shell_quote(&remote_dir_str);
            let backup_script = format!(
                "mkdir -p {rj}/.sync_backups/latest && \
                 cp -f {rj}/conversation_summaries.db {rj}/.sync_backups/latest/ 2>/dev/null || true && \
                 for p in {rj}/*_summaries_proto.pb; do \
                   if [ -f \"$p\" ]; then cp -f \"$p\" {rj}/.sync_backups/latest/ 2>/dev/null || true; fi; \
                 done",
                rj = r_dir
            );
            run_remote_script(&probe.endpoint, &backup_script)?;

            for src in staged_files {
                if src.exists() {
                    if let Some(fname) = src.file_name().and_then(|n| n.to_str()) {
                        let remote_spec = format!("{}:{}/{}", target, remote_dir_str, fname);
                        let mut cmd = Command::new("rsync");
                        cmd.arg("-az")
                            .arg("-e")
                            .arg(&transport_cmd)
                            .arg(&src)
                            .arg(&remote_spec);
                        let out = cmd.output().context("Failed to run rsync for remote index")?;
                        if !out.status.success() {
                            bail!(
                                "Failed to push {} to remote: {}",
                                fname,
                                String::from_utf8_lossy(&out.stderr).trim()
                            );
                        }
                    }
                }
            }

            let cleanup_wal = format!(
                "rm -fv {rj}/conversation_summaries.db-wal {rj}/conversation_summaries.db-shm >/dev/null 2>&1 || true",
                rj = r_dir
            );
            let _ = run_remote_script(&probe.endpoint, &cleanup_wal);
        }
    }

    Ok(())
}

/// Options for running an `rsync` directory or `--files-from` transfer.
#[derive(Debug, Clone)]
pub struct RsyncTransferOpts {
    pub update_only: bool,
    pub delete: bool,
    pub dry_run: bool,
    pub excludes: Vec<String>,
}

/// Synchronizes a workspace folder between `local_dir` and `probe.remote_project_path`.
/// If `push` is true: Local -> Remote. If `push` is false: Remote -> Local.
pub fn rsync_workspace_dir(
    probe: &RemoteProbe,
    local_dir: &Path,
    push: bool,
    opts: &RsyncTransferOpts,
) -> Result<String> {
    if !opts.dry_run {
        if push {
            match &probe.endpoint {
                RemoteEndpoint::LocalRoot { .. } => {
                    fs::create_dir_all(&probe.remote_project_path)?;
                }
                RemoteEndpoint::Ssh { .. } => {
                    let script = format!("mkdir -p {}", shell_quote(&probe.remote_project_path));
                    run_remote_script(&probe.endpoint, &script)?;
                }
            }
        } else {
            fs::create_dir_all(local_dir)?;
        }
    }

    let local_slash = format!("{}/", local_dir.to_string_lossy().trim_end_matches('/'));
    let remote_slash = match &probe.endpoint {
        RemoteEndpoint::LocalRoot { .. } => {
            if !push && !Path::new(&probe.remote_project_path).exists() {
                return Ok(String::new());
            }
            format!("{}/", probe.remote_project_path.trim_end_matches('/'))
        }
        RemoteEndpoint::Ssh { .. } => {
            let target = probe.endpoint.ssh_target().unwrap();
            if !push {
                let script = format!("mkdir -p {}", shell_quote(&probe.remote_project_path));
                let _ = run_remote_script(&probe.endpoint, &script);
            }
            format!(
                "{}:{}/",
                target,
                probe.remote_project_path.trim_end_matches('/')
            )
        }
    };

    let mut cmd = Command::new("rsync");
    cmd.arg("-avz")
        .arg("-h")
        .arg("--partial")
        .arg("--progress");
    if opts.update_only {
        cmd.arg("-u");
    }
    if opts.delete {
        cmd.arg("--delete");
    }
    if opts.dry_run {
        cmd.arg("--dry-run");
    }
    for ex in &opts.excludes {
        cmd.arg("--exclude").arg(ex);
    }
    if matches!(probe.endpoint, RemoteEndpoint::Ssh { .. }) {
        cmd.arg("-e").arg(probe.endpoint.rsync_ssh_transport_cmd());
    }

    if push {
        cmd.arg(&local_slash).arg(&remote_slash);
    } else {
        cmd.arg(&remote_slash).arg(&local_slash);
    }

    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    let status = cmd.status().context("Failed to execute rsync for workspace")?;
    if !status.success() {
        bail!("Workspace rsync failed (status {})", status);
    }
    Ok(String::new())
}

/// Synchronizes a specific list of relative paths inside a harness directory (or `.gemini` root when `harness_subdir == ""`)
/// using `rsync -avz -r --files-from`.
pub fn rsync_harness_relative_paths(
    probe: &RemoteProbe,
    harness_subdir: &str,
    local_root_dir: &Path,
    relative_paths: &[String],
    push: bool,
    update_only: bool,
    dry_run: bool,
) -> Result<String> {
    if relative_paths.is_empty() {
        return Ok(String::new());
    }

    let list_file = tempfile::NamedTempFile::new()?;
    let list_content = relative_paths.join("\n") + "\n";
    fs::write(list_file.path(), list_content)?;

    let remote_dir_str = probe.remote_harness_dir(harness_subdir);
    let local_slash = format!("{}/", local_root_dir.to_string_lossy().trim_end_matches('/'));
    let remote_slash = match &probe.endpoint {
        RemoteEndpoint::LocalRoot { .. } => {
            if !dry_run {
                fs::create_dir_all(&remote_dir_str)?;
            }
            format!("{}/", remote_dir_str.trim_end_matches('/'))
        }
        RemoteEndpoint::Ssh { .. } => {
            let target = probe.endpoint.ssh_target().unwrap();
            if !dry_run {
                let script = format!("mkdir -p {}", shell_quote(&remote_dir_str));
                run_remote_script(&probe.endpoint, &script)?;
            }
            format!("{}:{}/", target, remote_dir_str.trim_end_matches('/'))
        }
    };

    let mut cmd = Command::new("rsync");
    cmd.arg("-avz")
        .arg("-h")
        .arg("--partial")
        .arg("--progress")
        .arg("-r")
        .arg("--files-from")
        .arg(list_file.path());

    if update_only {
        cmd.arg("-u");
    }
    if dry_run {
        cmd.arg("--dry-run");
    }
    if matches!(probe.endpoint, RemoteEndpoint::Ssh { .. }) {
        cmd.arg("-e").arg(probe.endpoint.rsync_ssh_transport_cmd());
    }

    if push {
        cmd.arg(&local_slash).arg(&remote_slash);
    } else {
        cmd.arg(&remote_slash).arg(&local_slash);
    }

    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    let status = cmd
        .status()
        .context("Failed to execute rsync --files-from for harness artifacts")?;
    if !status.success() {
        bail!("Harness artifacts rsync failed (status {})", status);
    }
    Ok(String::new())
}
