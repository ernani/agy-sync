use crate::config::{detect_port_in_ssh_config, AgySyncConfig, SyncStateLedger};
use crate::discovery::{discover_project_sessions, normalize_project_path};
use crate::engine::{format_bytes, print_report, run_engine, EngineConfig, SyncMode};
use crate::harnesses::{
    compare_workspace_equality, discover_all_projects_in_harness, discover_harnesses,
    migrate_project_between_local_harnesses, HarnessKind, PlacementMode,
};
use crate::index_merge::purge_sessions_from_harness;
use crate::transport::{authenticate_ssh_interactive, check_ssh_auth, RemoteEndpoint};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

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

    println!();
    let run_mig = prompt_yes_no(
        " Would you also like to run the Cross-Harness Project Migration & Coexistence Wizard now? [y/N]: ",
        false,
    )?;
    if run_mig {
        run_interactive_migration_wizard(&base_config, None, None)?;
    }

    Ok(())
}

fn describe_session_comparison(
    a_label: &str,
    b_label: &str,
    a_sessions: &BTreeMap<String, crate::discovery::DiscoveredSession>,
    b_sessions: &BTreeMap<String, crate::discovery::DiscoveredSession>,
) -> String {
    let mut all_ids: BTreeSet<&String> = a_sessions.keys().collect();
    all_ids.extend(b_sessions.keys());
    if all_ids.is_empty() {
        return "No sessions in either harness".to_string();
    }
    let mut equal = 0usize;
    let mut a_ahead = 0usize;
    let mut b_ahead = 0usize;
    let mut only_a = 0usize;
    let mut only_b = 0usize;

    for id in all_ids {
        match (a_sessions.get(id), b_sessions.get(id)) {
            (Some(a), Some(b)) => {
                if a.step_count > b.step_count {
                    a_ahead += 1;
                } else if b.step_count > a.step_count {
                    b_ahead += 1;
                } else {
                    equal += 1;
                }
            }
            (Some(_), None) => only_a += 1,
            (None, Some(_)) => only_b += 1,
            (None, None) => {}
        }
    }

    if a_ahead == 0 && b_ahead == 0 && only_a == 0 && only_b == 0 {
        format!("EQUAL ({} session(s) identical in both)", equal)
    } else {
        format!(
            "equal={}, {} ahead={}, {} ahead={}, only in {}={}, only in {}={}",
            equal, a_label, a_ahead, b_label, b_ahead, a_label, only_a, b_label, only_b
        )
    }
}

fn prompt_placement_mode(a_label: &str, b_label: &str) -> Result<PlacementMode> {
    println!("      Select placement mode:");
    println!(
        "        [b] Coexist in BOTH ('{}' + '{}') [default]",
        a_label, b_label
    );
    println!(
        "        [1] Exist ONLY in '{}' (consolidate into '{}' & remove from '{}')",
        a_label, a_label, b_label
    );
    println!(
        "        [2] Exist ONLY in '{}' (consolidate into '{}' & remove from '{}')",
        b_label, b_label, a_label
    );
    let choice = prompt_line("      -> Choice [b/1/2, default: b]: ")?;
    match choice.to_ascii_lowercase().as_str() {
        "1" => Ok(PlacementMode::OnlyInSource),
        "2" => Ok(PlacementMode::OnlyInTarget),
        _ => Ok(PlacementMode::CoexistInBoth),
    }
}

/// Runs the two-pass step-by-step interactive wizard to migrate or bridge projects between two local
/// session harnesses (`Pass 1: Harness A -> Harness B`, `Pass 2: Harness B -> Harness A` counterpart),
/// followed by `Pass 3: Deleted / Ghost Project & Session Cleanup`.
pub fn run_interactive_migration_wizard(
    config: &EngineConfig,
    from_harness_override: Option<String>,
    to_harness_override: Option<String>,
) -> Result<()> {
    let gemini_root = config.local_gemini_root();
    let available: Vec<HarnessKind> = HarnessKind::discover_available_kinds(&gemini_root)
        .into_iter()
        .filter(|k| k.session_subdir().is_some())
        .collect();

    if available.len() < 2 {
        bail!(
            "At least 2 session-based harnesses are required under {} to run cross-harness migration.",
            gemini_root.display()
        );
    }

    println!("================================================================================");
    println!(
        " Agy Interactive Cross-Harness Project Migration & Coexistence Wizard{}",
        if config.dry_run { " (DRY-RUN)" } else { "" }
    );
    println!("================================================================================");
    println!(" Available Session Harnesses in {}:", gemini_root.display());
    for (idx, k) in available.iter().enumerate() {
        println!(
            "  [{}] {:<18} ({})",
            idx + 1,
            k.cli_id(),
            k.display_path()
        );
    }
    println!("--------------------------------------------------------------------------------");

    let default_a_idx = 0usize;
    let default_b_idx = available
        .iter()
        .position(|k| *k == HarnessKind::AgyCli)
        .filter(|&i| i != default_a_idx)
        .unwrap_or(1);

    let harness_a = if let Some(ref raw_a) = from_harness_override {
        HarnessKind::from_cli_id(raw_a)
            .ok_or_else(|| anyhow::anyhow!("Invalid --from harness '{}'", raw_a))?
    } else {
        let input = prompt_line(&format!(
            " Select Primary Harness A (number or name) [default: {} ({})]: ",
            default_a_idx + 1,
            available[default_a_idx].cli_id()
        ))?;
        if input.is_empty() {
            available[default_a_idx].clone()
        } else if let Ok(n) = input.parse::<usize>() {
            if n >= 1 && n <= available.len() {
                available[n - 1].clone()
            } else {
                bail!("Invalid harness index {}", n);
            }
        } else {
            HarnessKind::from_cli_id(&input)
                .ok_or_else(|| anyhow::anyhow!("Unknown harness '{}'", input))?
        }
    };

    let harness_b = if let Some(ref raw_b) = to_harness_override {
        HarnessKind::from_cli_id(raw_b)
            .ok_or_else(|| anyhow::anyhow!("Invalid --to harness '{}'", raw_b))?
    } else {
        let fallback_b = if available[default_b_idx] == harness_a {
            available.iter().find(|k| **k != harness_a).unwrap().clone()
        } else {
            available[default_b_idx].clone()
        };
        let input = prompt_line(&format!(
            " Select Counterpart Harness B (number or name) [default: {}]: ",
            fallback_b.cli_id()
        ))?;
        if input.is_empty() {
            fallback_b
        } else if let Ok(n) = input.parse::<usize>() {
            if n >= 1 && n <= available.len() {
                available[n - 1].clone()
            } else {
                bail!("Invalid harness index {}", n);
            }
        } else {
            HarnessKind::from_cli_id(&input)
                .ok_or_else(|| anyhow::anyhow!("Unknown harness '{}'", input))?
        }
    };

    if harness_a == harness_b {
        bail!("Harness A and Harness B must be distinct harnesses.");
    }

    let dir_a = config.local_dir_for_harness(&harness_a);
    let dir_b = config.local_dir_for_harness(&harness_b);
    let label_a = harness_a.cli_id().to_string();
    let label_b = harness_b.cli_id().to_string();

    let projects_a = discover_all_projects_in_harness(&dir_a, &gemini_root, &config.local_home)?;
    let projects_b = discover_all_projects_in_harness(&dir_b, &gemini_root, &config.local_home)?;

    // Build basename lookup for counterpart matching across different parent folders
    let mut b_by_basename: BTreeMap<String, PathBuf> = BTreeMap::new();
    for pb in &projects_b {
        if let Some(name) = pb.file_name().and_then(|n| n.to_str()) {
            b_by_basename.insert(name.to_string(), pb.clone());
        }
    }
    let mut a_by_basename: BTreeMap<String, PathBuf> = BTreeMap::new();
    for pa in &projects_a {
        if let Some(name) = pa.file_name().and_then(|n| n.to_str()) {
            a_by_basename.insert(name.to_string(), pa.clone());
        }
    }

    let mut handled_b_paths = BTreeSet::new();
    let mut ledger = SyncStateLedger::load(&gemini_root);

    // =========================================================================
    // PASS 1: Harness A -> Harness B
    // =========================================================================
    println!("================================================================================");
    println!(
        " PASS 1/3: Bringing Projects from '{}' -> '{}' ({} project(s) found)",
        label_a,
        label_b,
        projects_a.len()
    );
    println!("================================================================================");

    for (idx, path_a) in projects_a.iter().enumerate() {
        let proj_name = path_a
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project");
        let disc_a = discover_project_sessions(path_a, &dir_a)?;
        if disc_a.sessions.is_empty() {
            continue;
        }

        let counterpart_b_path = if projects_b.contains(path_a) {
            Some(path_a.clone())
        } else {
            b_by_basename.get(proj_name).cloned()
        };

        let disc_b_opt = if let Some(ref pb) = counterpart_b_path {
            handled_b_paths.insert(pb.clone());
            Some(discover_project_sessions(pb, &dir_b)?)
        } else {
            let direct_b = discover_project_sessions(path_a, &dir_b)?;
            if !direct_b.sessions.is_empty() {
                Some(direct_b)
            } else {
                None
            }
        };

        let total_steps_a: i64 = disc_a.sessions.values().map(|s| s.step_count).sum();
        println!(
            "\n [{}/{}] Project: {} ({})",
            idx + 1,
            projects_a.len(),
            proj_name,
            path_a.display()
        );
        println!(
            "    • In '{}': {} session(s), {} total steps ({})",
            label_a,
            disc_a.sessions.len(),
            total_steps_a,
            format_bytes(disc_a.total_bytes)
        );
        if let Some(latest) = disc_a.latest_session() {
            if !latest.title.is_empty() || !latest.last_prompt_snippet.is_empty() {
                println!(
                    "    • Latest in '{}': {} | \"{}\"",
                    label_a,
                    if latest.title.is_empty() {
                        "(Untitled)"
                    } else {
                        &latest.title
                    },
                    latest.last_prompt_snippet
                );
            }
        }

        let target_proj_path = if let Some(ref disc_b) = disc_b_opt {
            let pb = counterpart_b_path.as_ref().unwrap_or(path_a);
            let ws_eq = compare_workspace_equality(path_a, pb);
            let sess_eq =
                describe_session_comparison(&label_a, &label_b, &disc_a.sessions, &disc_b.sessions);
            println!("    • [SIMILAR / COEXISTING PROJECT DETECTED in '{}']", label_b);
            println!("      - Workspace Equality: {}", ws_eq.summary_line());
            println!("      - Session Equality  : {}", sess_eq);
            pb.clone()
        } else {
            println!("    • In '{}': not present yet (0 sessions)", label_b);
            path_a.clone()
        };

        let proceed = prompt_yes_no(
            &format!(
                "    -> Should project '{}' go from '{}' to '{}'? [y/N]: ",
                proj_name, label_a, label_b
            ),
            false,
        )?;
        if !proceed {
            continue;
        }

        let placement = prompt_placement_mode(&label_a, &label_b)?;
        let chosen_dst_path = if path_a != &target_proj_path {
            println!("      Two workspace paths exist for '{}':", proj_name);
            println!("        [1] {} (from '{}')", path_a.display(), label_a);
            println!(
                "        [2] {} (from '{}')",
                target_proj_path.display(),
                label_b
            );
            let p_choice = prompt_line("      -> Choose canonical workspace path [1/2, default: 1]: ")?;
            if p_choice.trim() == "2" {
                target_proj_path
            } else {
                path_a.clone()
            }
        } else {
            path_a.clone()
        };

        let (stats, purged) = migrate_project_between_local_harnesses(
            &gemini_root,
            &dir_a,
            &dir_b,
            path_a,
            &chosen_dst_path,
            placement,
            config.deep_remap,
            config.dry_run,
        )?;
        println!(
            "      [OK] Applied {:?}: upserted={} (FTS={}, Protobuf={}), purged={}",
            placement,
            stats.summaries_upserted,
            stats.fts_chunks_updated,
            stats.protobuf_entries_updated,
            purged
        );

        if !config.dry_run {
            for (cid, s) in &disc_a.sessions {
                if placement != PlacementMode::OnlyInTarget {
                    ledger.record_session(
                        &chosen_dst_path,
                        "",
                        &label_a,
                        cid,
                        &s.title,
                        s.step_count,
                        &s.parent_conversation_id,
                        true,
                        false,
                    );
                }
                if placement != PlacementMode::OnlyInSource {
                    ledger.record_session(
                        &chosen_dst_path,
                        "",
                        &label_b,
                        cid,
                        &s.title,
                        s.step_count,
                        &s.parent_conversation_id,
                        true,
                        false,
                    );
                }
            }
            let _ = ledger.save(&gemini_root);
        }
    }

    // =========================================================================
    // PASS 2: Counterpart Harness B -> Harness A
    // =========================================================================
    let remaining_b: Vec<PathBuf> = projects_b
        .into_iter()
        .filter(|p| !handled_b_paths.contains(p))
        .collect();

    println!("\n================================================================================");
    println!(
        " PASS 2/3 (Counterpart): Bringing Projects from '{}' -> '{}' ({} remaining project(s))",
        label_b,
        label_a,
        remaining_b.len()
    );
    println!("================================================================================");

    for (idx, path_b) in remaining_b.iter().enumerate() {
        let proj_name = path_b
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project");
        let disc_b = discover_project_sessions(path_b, &dir_b)?;
        if disc_b.sessions.is_empty() {
            continue;
        }

        let counterpart_a_path = if projects_a.contains(path_b) {
            Some(path_b.clone())
        } else {
            a_by_basename.get(proj_name).cloned()
        };

        let total_steps_b: i64 = disc_b.sessions.values().map(|s| s.step_count).sum();
        println!(
            "\n [{}/{}] Project: {} ({})",
            idx + 1,
            remaining_b.len(),
            proj_name,
            path_b.display()
        );
        println!(
            "    • In '{}': {} session(s), {} total steps ({})",
            label_b,
            disc_b.sessions.len(),
            total_steps_b,
            format_bytes(disc_b.total_bytes)
        );
        if let Some(latest) = disc_b.latest_session() {
            if !latest.title.is_empty() || !latest.last_prompt_snippet.is_empty() {
                println!(
                    "    • Latest in '{}': {} | \"{}\"",
                    label_b,
                    if latest.title.is_empty() {
                        "(Untitled)"
                    } else {
                        &latest.title
                    },
                    latest.last_prompt_snippet
                );
            }
        }

        let target_proj_path = if let Some(ref pa) = counterpart_a_path {
            let disc_a = discover_project_sessions(pa, &dir_a)?;
            let ws_eq = compare_workspace_equality(path_b, pa);
            let sess_eq =
                describe_session_comparison(&label_b, &label_a, &disc_b.sessions, &disc_a.sessions);
            println!("    • [SIMILAR / COEXISTING PROJECT DETECTED in '{}']", label_a);
            println!("      - Workspace Equality: {}", ws_eq.summary_line());
            println!("      - Session Equality  : {}", sess_eq);
            pa.clone()
        } else {
            println!("    • In '{}': not present yet (0 sessions)", label_a);
            path_b.clone()
        };

        let proceed = prompt_yes_no(
            &format!(
                "    -> Should project '{}' go from '{}' to '{}'? [y/N]: ",
                proj_name, label_b, label_a
            ),
            false,
        )?;
        if !proceed {
            continue;
        }

        let placement = prompt_placement_mode(&label_b, &label_a)?;
        let chosen_dst_path = if path_b != &target_proj_path {
            println!("      Two workspace paths exist for '{}':", proj_name);
            println!("        [1] {} (from '{}')", path_b.display(), label_b);
            println!(
                "        [2] {} (from '{}')",
                target_proj_path.display(),
                label_a
            );
            let p_choice = prompt_line("      -> Choose canonical workspace path [1/2, default: 2]: ")?;
            if p_choice.trim() == "1" {
                path_b.clone()
            } else {
                target_proj_path
            }
        } else {
            path_b.clone()
        };

        let (stats, purged) = migrate_project_between_local_harnesses(
            &gemini_root,
            &dir_b,
            &dir_a,
            path_b,
            &chosen_dst_path,
            placement,
            config.deep_remap,
            config.dry_run,
        )?;
        println!(
            "      [OK] Applied {:?}: upserted={} (FTS={}, Protobuf={}), purged={}",
            placement,
            stats.summaries_upserted,
            stats.fts_chunks_updated,
            stats.protobuf_entries_updated,
            purged
        );

        if !config.dry_run {
            for (cid, s) in &disc_b.sessions {
                if placement != PlacementMode::OnlyInTarget {
                    ledger.record_session(
                        &chosen_dst_path,
                        "",
                        &label_b,
                        cid,
                        &s.title,
                        s.step_count,
                        &s.parent_conversation_id,
                        true,
                        false,
                    );
                }
                if placement != PlacementMode::OnlyInSource {
                    ledger.record_session(
                        &chosen_dst_path,
                        "",
                        &label_a,
                        cid,
                        &s.title,
                        s.step_count,
                        &s.parent_conversation_id,
                        true,
                        false,
                    );
                }
            }
            let _ = ledger.save(&gemini_root);
        }
    }

    // =========================================================================
    // PASS 3: Deleted / Ghost Project & Conversation Cleanup
    // =========================================================================
    println!("\n================================================================================");
    println!(" PASS 3/3: Deleted / Ghost Project & Conversation Reconciliation");
    println!("================================================================================");

    let mut all_discovered_paths = projects_a.clone();
    all_discovered_paths.extend(remaining_b);

    let mut ghost_candidates = Vec::new();
    for p in &all_discovered_paths {
        if !p.exists() {
            ghost_candidates.push(p.clone());
        }
    }

    if ghost_candidates.is_empty() {
        println!("  [OK] All discovered project workspace folders exist on disk.");
    } else {
        println!(
            "  Found {} deleted workspace folder(s) that still have leftover sessions:",
            ghost_candidates.len()
        );
        for gp in ghost_candidates {
            let da = discover_project_sessions(&gp, &dir_a)?;
            let db = discover_project_sessions(&gp, &dir_b)?;
            let count_a = da.sessions.len();
            let count_b = db.sessions.len();
            if count_a == 0 && count_b == 0 {
                continue;
            }
            println!(
                "\n  • Deleted Workspace: {} ('{}': {} session(s), '{}': {} session(s))",
                gp.display(),
                label_a,
                count_a,
                label_b,
                count_b
            );
            let del = prompt_yes_no(
                "    -> Delete these leftover sessions from local harnesses? [y/N]: ",
                false,
            )?;
            if del {
                if count_a > 0 {
                    let cids: BTreeSet<String> = da.sessions.keys().cloned().collect();
                    let n = purge_sessions_from_harness(&dir_a, &cids, config.dry_run)?;
                    println!("      [OK] Purged {} session(s) from '{}'.", n, label_a);
                }
                if count_b > 0 {
                    let cids: BTreeSet<String> = db.sessions.keys().cloned().collect();
                    let n = purge_sessions_from_harness(&dir_b, &cids, config.dry_run)?;
                    println!("      [OK] Purged {} session(s) from '{}'.", n, label_b);
                }
                if !config.dry_run {
                    ledger.remove_project(&gp);
                    let _ = ledger.save(&gemini_root);
                }
            }
        }
    }

    println!("================================================================================");
    println!(" Cross-Harness Migration & Coexistence Wizard Complete.");
    println!("================================================================================");
    Ok(())
}
