use agy_sync::config::{config_file_path, detect_port_in_ssh_config, AgySyncConfig};
use agy_sync::discovery::normalize_project_path;
use agy_sync::engine::{print_report, run_engine, EngineConfig, SyncMode};
use agy_sync::harnesses::{discover_harnesses, HarnessKind};
use agy_sync::interactive::run_interactive_wizard;
use agy_sync::transport::RemoteEndpoint;
use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use std::env;
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "agy-sync",
    version,
    about = "Intelligent multi-harness project & session synchronization engine for the Agy ecosystem (~/.gemini)",
    arg_required_else_help = true
)]
struct Cli {
    /// Launch interactive step-by-step synchronization wizard (en-US)
    #[arg(short = 'i', long, global = true)]
    interactive: bool,

    /// Path to the project workspace folder (defaults to current working directory)
    #[arg(short, long, global = true)]
    project: Option<PathBuf>,

    /// Remote SSH host (`[user@]host[:port]`) or `local:/path/to/home`.
    /// Defaults to `default_host` in `$HOME/.config/.agy-sync` if configured.
    #[arg(short, long, global = true, env = "AGY_SYNC_REMOTE")]
    remote: Option<String>,

    /// Explicit SSH port override (defaults to port in host spec, `~/.ssh/config`, `$HOME/.config/.agy-sync`, or 22)
    #[arg(long, global = true)]
    port: Option<u16>,

    /// Explicit SSH username override
    #[arg(short = 'u', long, global = true)]
    user: Option<String>,

    /// Harnesses to include (`agy-2.0`, `agy-cli`, `agy-ide`, `agy-plugins`, `gemini-cli`, custom subdir name, or `all`).
    /// Can be specified multiple times or comma-separated. Defaults to all harnesses with activity for the project.
    #[arg(long, global = true, value_delimiter = ',')]
    harness: Vec<String>,

    /// Override local `~/.gemini` root directory path
    #[arg(long, global = true, env = "AGY_SYNC_DIR")]
    gemini_dir: Option<PathBuf>,

    /// Disable automatic `$HOME` path remapping
    #[arg(long, global = true)]
    no_remap: bool,

    /// Also rewrite `$HOME` paths inside historical conversation `steps` blobs and `transcript.jsonl`
    #[arg(long, global = true)]
    deep_remap: bool,

    /// Preview sync actions and `rsync` delta without modifying local or remote files
    #[arg(short = 'n', long, global = true)]
    dry_run: bool,

    /// Allow `rsync --delete` on the workspace folder during unidirectional `push` or `pull`
    #[arg(long, global = true)]
    delete: bool,

    /// Include OS-specific build/dependency directories (`target/`, `node_modules/`, `.venv/`, `__pycache__/`)
    #[arg(long, global = true)]
    include_build_dirs: bool,

    /// Additional `rsync --exclude` patterns for the workspace folder
    #[arg(short, long, global = true)]
    exclude: Vec<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug, Clone)]
enum Commands {
    /// Discover and list all correlated sessions, subagents, SQLite DBs, and artifacts across `~/.gemini` harnesses
    Inspect {
        /// Also probe the remote host over SSH to compare remote session step counts
        #[arg(long)]
        probe_remote: bool,
    },
    /// Bidirectional synchronization (Local <-> Remote): newest steps/files win, surgical DB/Protobuf merge
    Sync,
    /// Unidirectional push (Local -> Remote) for the project folder and selected harnesses
    Push,
    /// Unidirectional pull (Remote -> Local) for the project folder and selected harnesses
    Pull,
    /// View or update persistent defaults stored in `$HOME/.config/.agy-sync`
    Config {
        /// Display current configuration in `$HOME/.config/.agy-sync`
        #[arg(long)]
        show: bool,
        /// Persist default remote hostname (`[user@]host[:port]`)
        #[arg(long)]
        set_remote: Option<String>,
        /// Persist default SSH port
        #[arg(long)]
        set_port: Option<u16>,
        /// Persist default SSH username
        #[arg(long)]
        set_user: Option<String>,
    },
}

fn resolve_remote_endpoint(
    cli_remote: Option<String>,
    cli_port: Option<u16>,
    cli_user: Option<String>,
    home_dir: &str,
) -> Result<RemoteEndpoint> {
    let saved = AgySyncConfig::load(home_dir).unwrap_or_default();
    let raw = cli_remote
        .or_else(|| env::var("AGY_SYNC_REMOTE").ok().filter(|s| !s.is_empty()))
        .or(saved.default_host)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No remote host specified. Pass `-r <host>`, run with `-i` (interactive mode), \
                 or set a default via `agy-sync config --set-remote <host>` (saved in ~/.config/.agy-sync)."
            )
        })?;

    let mut ep = RemoteEndpoint::from_raw_spec(&raw);
    if let RemoteEndpoint::Ssh {
        ref host,
        ref mut port,
        ref mut user,
        ..
    } = ep
    {
        if let Some(u) = cli_user {
            *user = Some(u);
        } else if user.is_none() {
            *user = saved.default_user;
        }

        if let Some(p) = cli_port {
            *port = Some(p);
        } else if port.is_none() {
            *port = detect_port_in_ssh_config(home_dir, host).or(saved.default_port);
        }
    }
    Ok(ep)
}

fn resolve_harnesses(
    raw_flags: &[String],
    project_path: &Path,
    local_gemini_root: &Path,
) -> Result<Vec<HarnessKind>> {
    let available = HarnessKind::discover_available_kinds(local_gemini_root);
    if !raw_flags.is_empty() {
        let mut out = Vec::new();
        for item in raw_flags {
            if item.eq_ignore_ascii_case("all") {
                return Ok(available);
            }
            let Some(k) = HarnessKind::from_cli_id(item) else {
                bail!(
                    "Unknown harness '{}'. Valid values: agy-2.0, agy-cli, agy-ide, agy-plugins, gemini-cli, or all",
                    item
                );
            };
            if !out.contains(&k) {
                out.push(k);
            }
        }
        return Ok(out);
    }

    // Default: include all harnesses that have local activity for this project
    let norm = normalize_project_path(project_path);
    let discovered = discover_harnesses(&norm, local_gemini_root, &available)?;
    let active: Vec<HarnessKind> = discovered
        .into_iter()
        .filter(|d| d.has_interactions)
        .map(|d| d.kind)
        .collect();
    if active.is_empty() {
        Ok(vec![HarnessKind::Agy20])
    } else {
        Ok(active)
    }
}

fn main() -> Result<()> {
    if env::args_os().len() <= 1 {
        let mut cmd = Cli::command();
        cmd.print_help()?;
        println!();
        return Ok(());
    }

    let cli = Cli::parse();

    let local_home = env::var("HOME").context("HOME environment variable is not set")?;

    if let Some(Commands::Config {
        show,
        set_remote,
        set_port,
        set_user,
    }) = cli.command
    {
        let mut cfg = AgySyncConfig::load(&local_home)?;
        let mut modified = false;
        if let Some(r) = set_remote {
            cfg.default_host = if r.trim().is_empty() { None } else { Some(r) };
            modified = true;
        }
        if let Some(p) = set_port {
            cfg.default_port = Some(p);
            modified = true;
        }
        if let Some(u) = set_user {
            cfg.default_user = if u.trim().is_empty() { None } else { Some(u) };
            modified = true;
        }
        if modified {
            let path = cfg.save(&local_home)?;
            println!("Saved configuration to {}", path.display());
        }
        if show || !modified {
            let path = config_file_path(&local_home);
            println!("Configuration file: {}", path.display());
            println!(
                "  default_host   : {}",
                cfg.default_host.as_deref().unwrap_or("(not set)")
            );
            println!(
                "  default_port   : {}",
                cfg.default_port
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "(auto / 22)".to_string())
            );
            println!(
                "  default_user   : {}",
                cfg.default_user.as_deref().unwrap_or("(default ssh user)")
            );
        }
        return Ok(());
    }

    let local_gemini_dir = cli
        .gemini_dir
        .unwrap_or_else(|| PathBuf::from(&local_home).join(".gemini"));

    let project_path = match cli.project {
        Some(p) => p,
        None => env::current_dir().context("Failed to determine current working directory")?,
    };

    let mode = match cli.command.clone().unwrap_or(Commands::Sync) {
        Commands::Inspect { .. } => SyncMode::Inspect,
        Commands::Sync => SyncMode::Sync,
        Commands::Push => SyncMode::Push,
        Commands::Pull => SyncMode::Pull,
        Commands::Config { .. } => unreachable!(),
    };

    let mut base_config = EngineConfig {
        mode,
        project_path: project_path.clone(),
        local_home: local_home.clone(),
        local_gemini_dir,
        selected_harnesses: Vec::new(),
        endpoint: None,
        no_remap: cli.no_remap,
        deep_remap: cli.deep_remap,
        dry_run: cli.dry_run,
        delete: cli.delete,
        include_build_dirs: cli.include_build_dirs,
        extra_excludes: cli.exclude,
    };

    if cli.interactive {
        return run_interactive_wizard(base_config, cli.remote);
    }

    let local_gemini_root = base_config.local_gemini_root();
    base_config.selected_harnesses =
        resolve_harnesses(&cli.harness, &project_path, &local_gemini_root)?;

    let endpoint = match cli.command.unwrap_or(Commands::Sync) {
        Commands::Inspect { probe_remote } => {
            if probe_remote {
                Some(resolve_remote_endpoint(
                    cli.remote,
                    cli.port,
                    cli.user,
                    &local_home,
                )?)
            } else {
                None
            }
        }
        Commands::Sync | Commands::Push | Commands::Pull => Some(resolve_remote_endpoint(
            cli.remote,
            cli.port,
            cli.user,
            &local_home,
        )?),
        Commands::Config { .. } => unreachable!(),
    };
    base_config.endpoint = endpoint;

    let report = run_engine(&base_config)?;
    print_report(&report, &base_config);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_args_renders_help_menu() {
        let mut cmd = Cli::command();
        let help = cmd.render_help().to_string();
        assert!(help.contains("Usage: agy-sync"));
        assert!(help.contains("Commands:"));
        assert!(help.contains("Options:"));
    }
}
