use crate::config::parse_host_port_user;
use crate::discovery::find_summary_pb_files;
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader};
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
            #[cfg(not(windows))]
            {
                if let Some(cp) = control_path {
                    args.push("-o".to_string());
                    args.push(format!("ControlPath={}", cp.display()));
                } else {
                    args.push("-o".to_string());
                    args.push("ControlMaster=auto".to_string());
                    args.push("-o".to_string());
                    args.push("ControlPersist=4h".to_string());
                    args.push("-o".to_string());
                    args.push("ControlPath=/tmp/agy-sync-mux-%C".to_string());
                }
            }
            #[cfg(windows)]
            {
                let _ = control_path;
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
            #[cfg(not(windows))]
            {
                if let Some(cp) = control_path {
                    cmd.push_str(&format!(
                        " -o ControlPath={}",
                        shell_quote(&cp.to_string_lossy())
                    ));
                } else {
                    cmd.push_str(
                        " -o ControlMaster=auto -o ControlPersist=4h -o ControlPath=/tmp/agy-sync-mux-%C",
                    );
                }
            }
            #[cfg(windows)]
            {
                let _ = control_path;
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
    pub remote_has_rsync: bool,
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

static REMOTE_HAS_RSYNC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Records whether the probed remote target environment has `rsync` installed in `$PATH`.
pub fn set_remote_has_rsync(has_rsync: bool) {
    REMOTE_HAS_RSYNC.store(has_rsync, std::sync::atomic::Ordering::Relaxed);
}

/// Returns whether the probed remote target environment has `rsync` available.
pub fn remote_has_rsync() -> bool {
    REMOTE_HAS_RSYNC.load(std::sync::atomic::Ordering::Relaxed)
}

/// Returns `true` when `tar`-over-`ssh` streaming should be used instead of `rsync`
/// (automatically enabled on native Windows when `rsync` is absent, when `rsync` is not in `PATH` locally,
/// when the probed remote host lacks `rsync`, or when `AGY_SYNC_FORCE_TAR=1` is set).
pub fn should_use_tar_transport() -> bool {
    if std::env::var("AGY_SYNC_FORCE_TAR").ok().as_deref() == Some("1") {
        return true;
    }
    if !remote_has_rsync() {
        return true;
    }
    static HAS_RSYNC: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    !*HAS_RSYNC.get_or_init(|| {
        Command::new("rsync")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Streams `rel_paths` from `local_base` to `remote_base` on `endpoint` using `tar -cf -`
/// piped into `tar -xf -` (locally or over `ssh`).
pub fn tar_push_paths(
    endpoint: &RemoteEndpoint,
    local_base: &Path,
    remote_base: &str,
    rel_paths: &[String],
    excludes: &[String],
    update_only: bool,
    dry_run: bool,
) -> Result<()> {
    if dry_run || rel_paths.is_empty() {
        return Ok(());
    }
    let existing_rels: Vec<&String> = rel_paths
        .iter()
        .filter(|r| *r == "." || local_base.join(r.as_str()).exists())
        .collect();
    if existing_rels.is_empty() {
        return Ok(());
    }

    let mut tar_create = Command::new("tar");
    tar_create.arg("-cf").arg("-");
    for ex in excludes {
        tar_create.arg(format!("--exclude={}", ex));
    }
    tar_create.arg("-C").arg(local_base);
    for r in &existing_rels {
        tar_create.arg(r.as_str());
    }
    tar_create.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut src_child = tar_create
        .spawn()
        .context("Failed to spawn local tar for push")?;
    let src_stdout = src_child
        .stdout
        .take()
        .context("Failed to capture local tar stdout")?;
    let mut reader = BufReader::new(src_stdout);

    if reader.fill_buf()?.is_empty() {
        let _ = src_child.wait();
        return Ok(());
    }

    match endpoint {
        RemoteEndpoint::LocalRoot { .. } => {
            fs::create_dir_all(remote_base)?;
            let mut dst_cmd = Command::new("tar");
            dst_cmd.arg("-xf").arg("-");
            if update_only {
                dst_cmd.arg("--keep-newer-files");
            }
            dst_cmd
                .arg("-C")
                .arg(remote_base)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut dst_child = dst_cmd
                .spawn()
                .context("Failed to spawn destination tar for LocalRoot push")?;
            if let Some(mut dst_stdin) = dst_child.stdin.take() {
                let _ = std::io::copy(&mut reader, &mut dst_stdin);
            }
            let _ = dst_child.wait();
        }
        RemoteEndpoint::Ssh { .. } => {
            let target = endpoint.ssh_target().unwrap();
            let keep_flag = if update_only {
                "--keep-newer-files "
            } else {
                ""
            };
            let remote_script = format!(
                "mkdir -p {rb} && (tar -xf - {kf}-C {rb} 2>/dev/null || true)",
                rb = shell_quote(remote_base),
                kf = keep_flag
            );
            let mut ssh_cmd = Command::new("ssh");
            for arg in endpoint.ssh_args(true) {
                ssh_cmd.arg(arg);
            }
            ssh_cmd
                .arg(&target)
                .arg(&remote_script)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit());
            let mut dst_child = ssh_cmd
                .spawn()
                .with_context(|| format!("Failed to spawn ssh tar push to '{}'", target))?;
            if let Some(mut dst_stdin) = dst_child.stdin.take() {
                let _ = std::io::copy(&mut reader, &mut dst_stdin);
            }
            let status = dst_child.wait()?;
            if !status.success() {
                bail!("SSH tar push to '{}' failed (status {})", target, status);
            }
        }
    }

    let _ = src_child.wait();
    Ok(())
}

/// Streams `rel_paths` from `remote_base` on `endpoint` into `local_base` using `tar -cf -`
/// piped into local `tar -xf -`.
pub fn tar_pull_paths(
    endpoint: &RemoteEndpoint,
    remote_base: &str,
    local_base: &Path,
    rel_paths: &[String],
    excludes: &[String],
    update_only: bool,
    dry_run: bool,
) -> Result<()> {
    if dry_run || rel_paths.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(local_base)?;

    let mut src_child = match endpoint {
        RemoteEndpoint::LocalRoot { .. } => {
            let r_path = Path::new(remote_base);
            if !r_path.exists() {
                return Ok(());
            }
            let existing_rels: Vec<&String> = rel_paths
                .iter()
                .filter(|r| *r == "." || r_path.join(r.as_str()).exists())
                .collect();
            if existing_rels.is_empty() {
                return Ok(());
            }
            let mut cmd = Command::new("tar");
            cmd.arg("-cf").arg("-");
            for ex in excludes {
                cmd.arg(format!("--exclude={}", ex));
            }
            cmd.arg("-C").arg(r_path);
            for r in &existing_rels {
                cmd.arg(r.as_str());
            }
            cmd.stdout(Stdio::piped()).stderr(Stdio::null());
            cmd.spawn()
                .context("Failed to spawn source tar for LocalRoot pull")?
        }
        RemoteEndpoint::Ssh { .. } => {
            let target = endpoint.ssh_target().unwrap();
            let mut quoted_excludes = String::new();
            for ex in excludes {
                quoted_excludes.push_str(&format!(" --exclude={}", shell_quote(ex)));
            }
            let quoted_rels: Vec<String> = rel_paths.iter().map(|r| shell_quote(r)).collect();
            let remote_script = format!(
                "if [ -d {rb} ]; then cd {rb} && set -- && for p in {rels}; do if [ -e \"$p\" ]; then set -- \"$@\" \"$p\"; fi; done; if [ $# -gt 0 ]; then tar -cf -{ex} \"$@\"; fi; fi",
                rb = shell_quote(remote_base),
                rels = quoted_rels.join(" "),
                ex = quoted_excludes
            );
            let mut ssh_cmd = Command::new("ssh");
            for arg in endpoint.ssh_args(true) {
                ssh_cmd.arg(arg);
            }
            ssh_cmd
                .arg(&target)
                .arg(&remote_script)
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            ssh_cmd
                .spawn()
                .with_context(|| format!("Failed to spawn ssh tar pull from '{}'", target))?
        }
    };

    let src_stdout = src_child
        .stdout
        .take()
        .context("Failed to capture source tar stdout")?;
    let mut reader = BufReader::new(src_stdout);

    if reader.fill_buf()?.is_empty() {
        let _ = src_child.wait();
        return Ok(());
    }

    let mut dst_cmd = Command::new("tar");
    dst_cmd.arg("-xf").arg("-");
    if update_only {
        dst_cmd.arg("--keep-newer-files");
    }
    dst_cmd
        .arg("-C")
        .arg(local_base)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut dst_child = dst_cmd
        .spawn()
        .context("Failed to spawn local tar for pull extraction")?;
    if let Some(mut dst_stdin) = dst_child.stdin.take() {
        let _ = std::io::copy(&mut reader, &mut dst_stdin);
    }
    let _ = dst_child.wait();
    let _ = src_child.wait();
    Ok(())
}

/// Launches an interactive SSH `ControlMaster` bootstrap session (`-f -N -M -S <sock>`) on Unix/macOS
/// (or an interactive SSH verification on Windows where `ControlMaster` is unsupported by Win32-OpenSSH)
/// inheriting the terminal so the user can enter their password/passphrase/OTP once.
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

    #[cfg(not(windows))]
    {
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
    }

    #[cfg(windows)]
    {
        let _ = control_path;
        let target = endpoint.ssh_target().unwrap();
        let mut cmd = Command::new("ssh");
        for arg in endpoint.ssh_args(false) {
            cmd.arg(arg);
        }
        cmd.arg(&target).arg("printf 'OK'");
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
    }

    Ok(())
}

/// Probes the remote endpoint to determine its `$HOME` directory, check `rsync` availability,
/// and calculate the counterpart project path and `~/.gemini` paths.
pub fn probe_remote(
    endpoint: &RemoteEndpoint,
    local_home: &str,
    local_project_path: &Path,
) -> Result<RemoteProbe> {
    let (remote_home, remote_has_rsync) = match endpoint {
        RemoteEndpoint::LocalRoot { root_home } => {
            let has_r = Command::new("rsync")
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            (
                root_home.to_string_lossy().trim_end_matches('/').to_string(),
                has_r,
            )
        }
        RemoteEndpoint::Ssh { .. } => {
            let target = endpoint.ssh_target().unwrap();
            let probe_cmd = "printf '%s\n' \"$HOME\"; command -v rsync >/dev/null 2>&1 && printf 'RSYNC=1\n' || printf 'RSYNC=0\n'";
            let mut cmd = Command::new("ssh");
            for arg in endpoint.ssh_args(true) {
                cmd.arg(arg);
            }
            cmd.arg(&target).arg(probe_cmd);
            let mut output = cmd
                .output()
                .with_context(|| format!("Failed to execute ssh to host '{}'", target))?;
            if !output.status.success() {
                // Retry without BatchMode=yes (inheriting stderr/stdin) in case a FIDO2 hardware key touch is required
                let mut retry_cmd = Command::new("ssh");
                for arg in endpoint.ssh_args(false) {
                    retry_cmd.arg(arg);
                }
                retry_cmd
                    .arg(&target)
                    .arg(probe_cmd)
                    .stdin(Stdio::inherit())
                    .stderr(Stdio::inherit());
                output = retry_cmd
                    .output()
                    .with_context(|| format!("Failed to execute ssh to host '{}'", target))?;
            }
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!(
                    "SSH probe to '{}' failed (status {}): {}",
                    target,
                    output.status,
                    stderr.trim()
                );
            }
            let out_str = String::from_utf8_lossy(&output.stdout);
            let mut lines = out_str.lines();
            let home_line = lines.next().unwrap_or("").trim().to_string();
            if home_line.is_empty() {
                bail!("SSH probe to '{}' returned empty $HOME", target);
            }
            let has_rsync_line = lines.next().unwrap_or("").trim();
            let has_rsync = has_rsync_line == "RSYNC=1";
            (home_line.trim_end_matches('/').to_string(), has_rsync)
        }
    };

    set_remote_has_rsync(remote_has_rsync);

    let local_proj_norm =
        crate::config::normalize_home_path_string(&local_project_path.to_string_lossy());
    let local_home_norm = crate::config::normalize_home_path_string(local_home);

    let remote_project_path = if !local_home_norm.is_empty()
        && local_proj_norm.len() >= local_home_norm.len()
        && local_proj_norm[..local_home_norm.len()].eq_ignore_ascii_case(&local_home_norm)
    {
        format!("{}{}", remote_home, &local_proj_norm[local_home_norm.len()..])
    } else if let Some((_, _, rel)) = crate::discovery::split_home_and_relative(&local_proj_norm) {
        format!("{}{}", remote_home, rel)
    } else {
        local_proj_norm
    };

    let remote_gemini_root = format!("{}/.gemini", remote_home);

    Ok(RemoteProbe {
        endpoint: endpoint.clone(),
        remote_home,
        remote_gemini_root,
        remote_project_path,
        remote_has_rsync,
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
            if should_use_tar_transport() {
                let _ = tar_pull_paths(
                    &probe.endpoint,
                    &probe.remote_gemini_root,
                    staging_gemini_root,
                    &[
                        "projects.json".to_string(),
                        "config/projects".to_string(),
                    ],
                    &[],
                    false,
                    false,
                );
            } else {
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

                let r_cfg_spec =
                    format!("{}:{}/config/projects/", target, probe.remote_gemini_root);
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

            if should_use_tar_transport() {
                let _ = tar_pull_paths(
                    &probe.endpoint,
                    &remote_dir_str,
                    staging_harness_dir,
                    &files_to_pull,
                    &[],
                    false,
                    false,
                );
            } else {
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

            if should_use_tar_transport() {
                let mut rel_names = Vec::new();
                for src in &staged_files {
                    if src.exists() {
                        if let Some(fname) = src.file_name().and_then(|n| n.to_str()) {
                            rel_names.push(fname.to_string());
                        }
                    }
                }
                tar_push_paths(
                    &probe.endpoint,
                    staging_harness_dir,
                    &remote_dir_str,
                    &rel_names,
                    &[],
                    false,
                    false,
                )?;
            } else {
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
                            let out =
                                cmd.output().context("Failed to run rsync for remote index")?;
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

    if should_use_tar_transport() {
        let dot = vec![".".to_string()];
        if push {
            tar_push_paths(
                &probe.endpoint,
                local_dir,
                &probe.remote_project_path,
                &dot,
                &opts.excludes,
                opts.update_only,
                opts.dry_run,
            )?;
        } else {
            tar_pull_paths(
                &probe.endpoint,
                &probe.remote_project_path,
                local_dir,
                &dot,
                &opts.excludes,
                opts.update_only,
                opts.dry_run,
            )?;
        }
        return Ok(String::new());
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

/// Raises the process soft file-descriptor limit (`RLIMIT_NOFILE`) on Unix/macOS
/// so large session stores and child `rsync` processes do not hit `EMFILE` ("Too many open files"),
/// especially on macOS where the default soft limit is often 256.
pub fn ensure_sufficient_fd_limit() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    unsafe {
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct RLimit {
            rlim_cur: u64,
            rlim_max: u64,
        }
        #[cfg(target_os = "macos")]
        const RLIMIT_NOFILE: i32 = 8;
        #[cfg(target_os = "linux")]
        const RLIMIT_NOFILE: i32 = 7;

        extern "C" {
            fn getrlimit(resource: i32, rlp: *mut RLimit) -> i32;
            fn setrlimit(resource: i32, rlp: *const RLimit) -> i32;
        }

        let mut lim = RLimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if getrlimit(RLIMIT_NOFILE, &mut lim) == 0 && lim.rlim_cur < 10240 {
            for &target in &[65536u64, 32768, 24576, 10240, 4096] {
                let desired = if lim.rlim_max > 0 {
                    target.min(lim.rlim_max)
                } else {
                    target
                };
                if desired <= lim.rlim_cur {
                    continue;
                }
                let next = RLimit {
                    rlim_cur: desired,
                    rlim_max: lim.rlim_max,
                };
                if setrlimit(RLIMIT_NOFILE, &next) == 0 {
                    break;
                }
            }
        }
    }
}

const RSYNC_FILES_FROM_BATCH_SIZE: usize = 64;

/// Synchronizes a specific list of relative paths inside a harness directory (or `.gemini` root when `harness_subdir == ""`)
/// using `rsync -avz -r --files-from`, batched in chunks of at most 64 paths to bound open file descriptors.
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

    ensure_sufficient_fd_limit();

    let remote_dir_str = probe.remote_harness_dir(harness_subdir);
    if should_use_tar_transport() {
        for chunk in relative_paths.chunks(RSYNC_FILES_FROM_BATCH_SIZE) {
            if push {
                tar_push_paths(
                    &probe.endpoint,
                    local_root_dir,
                    &remote_dir_str,
                    chunk,
                    &[],
                    update_only,
                    dry_run,
                )?;
            } else {
                tar_pull_paths(
                    &probe.endpoint,
                    &remote_dir_str,
                    local_root_dir,
                    chunk,
                    &[],
                    update_only,
                    dry_run,
                )?;
            }
        }
        return Ok(String::new());
    }

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

    for chunk in relative_paths.chunks(RSYNC_FILES_FROM_BATCH_SIZE) {
        let list_file = tempfile::NamedTempFile::new()?;
        let list_content = chunk.join("\n") + "\n";
        fs::write(list_file.path(), list_content)?;

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
    }
    Ok(String::new())
}

/// Attempts a graceful restart of known agent web hub services (such as `rshelld-jetski-hub`)
/// so that updated SQLite summaries and protobuf caches are immediately reloaded in the UI.
pub fn reload_remote_agent_services_if_present(endpoint: &RemoteEndpoint) -> Result<()> {
    if matches!(endpoint, RemoteEndpoint::LocalRoot { .. }) {
        return Ok(());
    }
    let script = "systemctl --user is-active --quiet rshelld-jetski-hub 2>/dev/null && systemctl --user restart rshelld-jetski-hub 2>/dev/null || true";
    let _ = run_remote_script(endpoint, script);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_tar_push_and_pull_fallback_local_root() -> Result<()> {
        let tmp = TempDir::new()?;
        let local_dir = tmp.path().join("local_ws");
        let remote_dir = tmp.path().join("remote_ws");
        let pulled_dir = tmp.path().join("pulled_ws");
        fs::create_dir_all(local_dir.join("src"))?;
        fs::write(local_dir.join("src/main.rs"), b"fn main() {}")?;
        fs::write(local_dir.join("ignored.tmp"), b"ignore-me")?;

        let ep = RemoteEndpoint::LocalRoot {
            root_home: tmp.path().to_path_buf(),
        };
        let remote_str = remote_dir.to_string_lossy().to_string();

        tar_push_paths(
            &ep,
            &local_dir,
            &remote_str,
            &[".".to_string()],
            &["ignored.tmp".to_string()],
            false,
            false,
        )?;
        assert!(remote_dir.join("src/main.rs").exists());
        assert!(!remote_dir.join("ignored.tmp").exists());

        tar_pull_paths(
            &ep,
            &remote_str,
            &pulled_dir,
            &["src/main.rs".to_string()],
            &[],
            false,
            false,
        )?;
        assert_eq!(
            fs::read_to_string(pulled_dir.join("src/main.rs"))?,
            "fn main() {}"
        );
        Ok(())
    }

    #[test]
    fn test_probe_remote_windows_and_chromeos_paths() -> Result<()> {
        let ep = RemoteEndpoint::LocalRoot {
            root_home: PathBuf::from("/home/devuser"),
        };
        let win_probe = probe_remote(
            &ep,
            r"C:\Users\devuser",
            Path::new(r"C:\Users\devuser\Documents\my-repo"),
        )?;
        assert_eq!(
            win_probe.remote_project_path,
            "/home/devuser/Documents/my-repo"
        );

        let chromeos_probe = probe_remote(
            &ep,
            "/home/devuser",
            Path::new("/mnt/chromeos/MyFiles/Documents/my-repo"),
        )?;
        assert_eq!(
            chromeos_probe.remote_project_path,
            "/home/devuser/Documents/my-repo"
        );
        Ok(())
    }

    #[test]
    fn test_remote_rsync_detection_and_tar_fallback() {
        set_remote_has_rsync(true);
        // With remote_has_rsync = true, behavior depends on local rsync/env
        let default_mode = should_use_tar_transport();

        // If remote has no rsync, should_use_tar_transport must always return true
        set_remote_has_rsync(false);
        assert!(should_use_tar_transport());

        // Restore
        set_remote_has_rsync(true);
        assert_eq!(should_use_tar_transport(), default_mode);
    }
}
