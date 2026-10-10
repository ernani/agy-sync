use crate::config::{detect_port_in_ssh_config, AgySyncConfig};
use crate::discovery::normalize_project_path;
use crate::engine::{format_bytes, print_report, run_engine, EngineConfig, SyncMode};
use crate::harnesses::{discover_harnesses, HarnessKind};
use crate::transport::{authenticate_ssh_interactive, check_ssh_auth, RemoteEndpoint};
use anyhow::{bail, Result};
use std::io::{self, BufRead, Write};

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{}", prompt);
    io::stdout().flush()?;
    let stdin = io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn prompt_yes_no(prompt: &str, default_yes: bool) -> Result<bool> {
    let ans = prompt_line(prompt)?;
    if ans.is_empty() {
        return Ok(default_yes);
    }
    match ans.to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => Ok(default_yes),
    }
}

/// Runs the interactive (`-i` / `--interactive`) synchronization wizard in `en-US`.
pub fn run_interactive_wizard(
    mut base_config: EngineConfig,
    cli_remote_override: Option<String>,
) -> Result<()> {
    let norm_project = normalize_project_path(&base_config.project_path);
    let local_gemini_root = base_config.local_gemini_root();

    println!("================================================================================");
    println!(" Agy Interactive Multi-Harness Synchronization Wizard (agy-sync -i)");
    println!("================================================================================");
    println!(" Detected Project Workspace : {}", norm_project.display());
    println!(" Local Gemini Root Directory: {}", local_gemini_root.display());
    println!("--------------------------------------------------------------------------------");
    println!(" Scanning harnesses in ~/.gemini for project activity...");
    println!();

    let available_kinds = HarnessKind::discover_available_kinds(&local_gemini_root);
    let all_discoveries = discover_harnesses(&norm_project, &local_gemini_root, &available_kinds)?;

    println!(" Harness Activity Overview:");
    for d in &all_discoveries {
        if d.has_interactions {
            println!(
                "  [DETECTED] {:<34} ({} item(s), {})",
                d.kind.short_name(),
                d.session_count,
                format_bytes(d.total_bytes)
            );
        } else {
            println!(
                "  [  none  ] {:<34} (no activity for this project)",
                d.kind.short_name()
            );
        }
    }
    println!("--------------------------------------------------------------------------------");

    let mut selected_harnesses = Vec::new();
    let mut ignored_harnesses = Vec::new();

    let active_discoveries: Vec<_> = all_discoveries.iter().filter(|d| d.has_interactions).collect();
    if active_discoveries.is_empty() {
        println!(" No existing harness sessions were found locally for this project.");
        let check_remote = prompt_yes_no(
            " Would you like to select harnesses to pull/sync from the remote host anyway? [y/N]: ",
            false,
        )?;
        if check_remote {
            for d in &all_discoveries {
                let yn = prompt_yes_no(
                    &format!(
                        "  -> Include '{}' ({}) in sync? [y/N]: ",
                        d.kind.short_name(),
                        d.kind.display_path()
                    ),
                    false,
                )?;
                if yn {
                    selected_harnesses.push(d.kind.clone());
                } else {
                    ignored_harnesses.push(d.kind.clone());
                }
            }
        }
    } else {
        println!(" Please review each detected harness and confirm whether to synchronize it:");
        println!();
        for d in active_discoveries {
            println!(
                "  • Harness      : {} [{}]",
                d.kind.short_name(),
                d.kind.display_path()
            );
            println!(
                "    Sessions     : {} ({})",
                d.session_count,
                format_bytes(d.total_bytes)
            );
            if !d.latest_modified_time.is_empty() {
                println!(
                    "    Latest Event : {} (ID: {})",
                    d.latest_modified_time,
                    if d.latest_conversation_id.is_empty() {
                        "n/a"
                    } else {
                        &d.latest_conversation_id
                    }
                );
            }
            if !d.latest_prompt_snippet.is_empty() {
                println!("    Last Prompt  : \"{}\"", d.latest_prompt_snippet);
            }
            let include = prompt_yes_no(
                &format!(
                    "    -> Synchronize '{}' for this project? [y/N]: ",
                    d.kind.short_name()
                ),
                false,
            )?;
            if include {
                selected_harnesses.push(d.kind.clone());
            } else {
                ignored_harnesses.push(d.kind.clone());
            }
            println!();
        }
    }

    if selected_harnesses.is_empty() {
        println!(" No harnesses were selected for synchronization.");
        let ws_only = prompt_yes_no(
            " Do you want to abort synchronization completely? [Y/n]: ",
            true,
        )?;
        if ws_only {
            println!(" Synchronization aborted by user.");
            return Ok(());
        }
    }

    // Step 2: Remote Hostname & SSH Port Resolution
    println!("--------------------------------------------------------------------------------");
    println!(" Remote Host & SSH Connection Setup");
    println!("--------------------------------------------------------------------------------");

    let home_str = base_config.local_home.as_str();
    let mut saved_cfg = AgySyncConfig::load(home_str).unwrap_or_default();

    let default_host = cli_remote_override
        .or_else(|| saved_cfg.default_host.clone())
        .unwrap_or_default();

    let raw_remote = if !default_host.is_empty() {
        let input = prompt_line(&format!(
            " Enter remote SSH host (or local:/path) [default: {}]: ",
            default_host
        ))?;
        if input.is_empty() {
            default_host
        } else {
            input
        }
    } else {
        let input = prompt_line(
            " Enter remote SSH host (e.g. workstation.example.com or user@host:port): ",
        )?;
        if input.is_empty() {
            bail!("Remote hostname cannot be empty.");
        }
        input
    };

    let mut endpoint = RemoteEndpoint::from_raw_spec(&raw_remote);

    if let RemoteEndpoint::Ssh {
        ref host,
        ref mut port,
        ref mut user,
        ..
    } = endpoint
    {
        if user.is_none() {
            if let Some(ref saved_user) = saved_cfg.default_user {
                *user = Some(saved_user.clone());
            }
        }

        if let Some(inline_port) = *port {
            println!(
                "  [i] Using SSH port {} from host specification.",
                inline_port
            );
        } else if let Some(ssh_cfg_port) = detect_port_in_ssh_config(home_str, host) {
            println!(
                "  [i] Detected SSH port {} in ~/.ssh/config for host '{}'.",
                ssh_cfg_port, host
            );
            *port = Some(ssh_cfg_port);
        } else if let Some(saved_port) = saved_cfg.default_port {
            *port = Some(saved_port);
        }
    }

    // Step 3: Verify SSH Authentication & Fallback to Port/Interactive Login if Needed
    if matches!(endpoint, RemoteEndpoint::Ssh { .. }) {
        println!(
            "  [*] Verifying SSH connectivity and authentication to {}...",
            endpoint.display_label()
        );
        let auth_ok = check_ssh_auth(&endpoint).unwrap_or(false);
        if auth_ok {
            println!("  [OK] SSH authentication verified via active key / ControlMaster.");
        } else {
            if let RemoteEndpoint::Ssh { ref mut port, .. } = endpoint {
                if port.is_none() {
                    let def_port = saved_cfg.default_port.unwrap_or(22);
                    let port_str =
                        prompt_line(&format!("  Enter SSH port [default: {}]: ", def_port))?;
                    if !port_str.is_empty() {
                        if let Ok(p) = port_str.parse::<u16>() {
                            *port = Some(p);
                        } else {
                            bail!("Invalid SSH port number: '{}'", port_str);
                        }
                    } else if def_port != 22 {
                        *port = Some(def_port);
                    }
                }
            }

            if !check_ssh_auth(&endpoint).unwrap_or(false) {
                println!(
                    "  [!] Non-interactive SSH login did not succeed for {}.",
                    endpoint.display_label()
                );
                let current_user = match &endpoint {
                    RemoteEndpoint::Ssh { user, .. } => user.clone().unwrap_or_default(),
                    _ => String::new(),
                };
                let user_prompt = if current_user.is_empty() {
                    "  Enter SSH username (or press Enter to use default SSH user): ".to_string()
                } else {
                    format!("  Enter SSH username [default: {}]: ", current_user)
                };
                let entered_user = prompt_line(&user_prompt)?;
                let user_override = if entered_user.is_empty() {
                    None
                } else {
                    Some(entered_user)
                };

                println!("  [*] Starting interactive SSH authentication (please enter your password/OTP if prompted)...");
                authenticate_ssh_interactive(&mut endpoint, user_override)?;
                if !check_ssh_auth(&endpoint)? {
                    bail!(
                        "SSH verification failed after interactive login to {}",
                        endpoint.display_label()
                    );
                }
                println!("  [OK] Interactive SSH ControlMaster session established.");
            } else {
                println!("  [OK] SSH authentication verified.");
            }
        }

        // Offer to persist configuration in ~/.config/.agy-sync
        if let RemoteEndpoint::Ssh {
            ref host,
            ref port,
            ref user,
            ..
        } = endpoint
        {
            let changed = saved_cfg.default_host.as_deref() != Some(host.as_str())
                || saved_cfg.default_port != *port
                || saved_cfg.default_user != *user;
            if changed {
                let save_it = prompt_yes_no(
                    "  Save these remote connection defaults to ~/.config/.agy-sync? [Y/n]: ",
                    true,
                )?;
                if save_it {
                    saved_cfg.default_host = Some(host.clone());
                    saved_cfg.default_port = *port;
                    saved_cfg.default_user = user.clone();
                    saved_cfg.save(home_str)?;
                    println!("  [OK] Saved configuration to ~/.config/.agy-sync.");
                }
            }
        }
    }

    // Step 4: Final Summary & Confirmation
    println!("--------------------------------------------------------------------------------");
    println!(" Synchronization Plan Summary:");
    println!("  • Project Path      : {}", norm_project.display());
    println!("  • Remote Endpoint   : {}", endpoint.display_label());
    println!(
        "  • Sync Mode         : {:?}{}",
        base_config.mode,
        if base_config.dry_run {
            " (DRY-RUN)"
        } else {
            ""
        }
    );
    let sel_names: Vec<String> = selected_harnesses.iter().map(|k| k.short_name()).collect();
    let ign_names: Vec<String> = ignored_harnesses.iter().map(|k| k.short_name()).collect();
    println!(
        "  • Harnesses to Sync : {}",
        if sel_names.is_empty() {
            "(Workspace folder only)".to_string()
        } else {
            sel_names.join(", ")
        }
    );
    if !ign_names.is_empty() {
        println!("  • Harnesses Ignored : {}", ign_names.join(", "));
    }
    println!("--------------------------------------------------------------------------------");

    let confirm = prompt_yes_no(" Proceed with synchronization now? [y/N]: ", false)?;
    if !confirm {
        println!(" Synchronization cancelled by user.");
        return Ok(());
    }

    println!();
    if base_config.mode == SyncMode::Inspect {
        base_config.mode = SyncMode::Sync;
    }
    base_config.selected_harnesses = selected_harnesses;
    base_config.endpoint = Some(endpoint);

    let report = run_engine(&base_config)?;
    print_report(&report, &base_config);

    Ok(())
}
