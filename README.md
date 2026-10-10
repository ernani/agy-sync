# `agy-sync` — Multi-Harness Project & Session Synchronization Engine (`~/.gemini`)

`agy-sync` is a standalone Rust CLI tool that identifies a project workspace folder and discovers every correlated conversation, subagent, SQLite database, Protobuf summary index, `brain/` artifact, `scratch/` directory, `annotations/`, `implicit/` context, `browser_recordings/`, and `knowledge/` item across all Agy & Gemini harnesses stored in `~/.gemini/`.

It synchronizes both the project workspace folder and the selected `~/.gemini/` harness sessions **byte-for-byte with delta compression (`rsync -avz`)** and **surgical SQLite + Protobuf index merging** (including automatic `$HOME` path remapping between macOS `/Users/<user>` and Linux `/home/<user>` workstations).

---

## Supported Harnesses in `~/.gemini/`

| CLI ID | Harness Name | Storage Path in `~/.gemini/` | Description |
| :--- | :--- | :--- | :--- |
| `agy-2.0` | **agy (2.0 / vscode plugin)** | `~/.gemini/antigravity/` | Unified Agy 2.0 Web UI & VS Code Extension (`agy --hub --app_data_dir=antigravity`) sessions (`conversation_summaries.db`, `agyhub_summaries_proto.pb`, `conversations/*.db`, `brain/<id>/`) |
| `agy-cli` | **agy-cli** | `~/.gemini/antigravity-cli/` | Antigravity CLI sessions (`conversation_summaries.db`, `*_summaries_proto.pb`, `history.jsonl`, `conversations/*.db`, `brain/<id>/`) |
| `agy-ide` | **agy (ide)** | `~/.gemini/antigravity-ide/` | Antigravity standalone IDE sessions (`agyhub_summaries_proto.pb`, `conversations/*.db`, `brain/<id>/`) |
| `agy-plugins` | **agy (ide plugins & project config)** | `~/.gemini/config/projects/*.json` | Workspace registration & `project_id` mappings used by the VS Code plugin and Agy 2.0 |
| `gemini-cli` | **gemini-cli (legacy)** | `~/.gemini/{projects.json,history,tmp}` | Legacy Gemini CLI project registry, chat logs, and shell history |
| `<subdir>` | **Dynamic Session Stores** | `~/.gemini/<subdir>/` | Automatically discovers and syncs any additional session store subdirectory present under `~/.gemini/` |

---

## Interactive Mode (`-i` / `--interactive`)

Run `agy-sync -i` inside any project directory to launch the interactive step-by-step wizard (in `en-US`):

```bash
agy-sync -i
```

1. **Multi-Harness Activity Scan**: Detects the current project path, checks which harnesses in `~/.gemini/` have interactions with it, and displays the session count, disk size, latest event timestamp, and a snippet of the **last user prompt** for each active harness.
2. **Per-Harness Confirmation (`[y/N]`)**: Asks one by one which detected harnesses to include (`y`) and which to ignore (`n`).
3. **Remote Host & SSH Port Resolution**: Prompts for the remote hostname (pre-filled from `$HOME/.config/.agy-sync` if configured) and automatically detects custom SSH ports from `host:port` or `~/.ssh/config`.
4. **SSH Authentication & Interactive Fallback**: Verifies non-interactive SSH connectivity first (`BatchMode=yes`). If already authenticated via SSH key or `ControlMaster`, proceeds immediately without prompting. Otherwise, prompts for the SSH port/username and establishes an interactive SSH `ControlMaster` socket (`/tmp/agy-sync-ssh-<pid>.sock`) so password/OTP authentication is entered once and reused across all `rsync` and `ssh` transfers.
5. **Post-Sync Bidirectional Report**: Displays a detailed summary of all **Outbound (`Local -> Remote`)** and **Inbound (`Remote -> Local`)** conversations and step-count deltas (`before_steps -> after_steps`).

---

## Persistent Configuration (`$HOME/.config/.agy-sync`)

No hostnames or usernames are hardcoded in `agy-sync`. You can view or persist your default remote workstation settings in `$HOME/.config/.agy-sync` (`0600` permissions):

```bash
# Save default remote host (and optional port/user)
agy-sync config --set-remote my-workstation.example.com
agy-sync config --set-port 22 --set-user myuser

# View current configuration
agy-sync config --show
```

---

## Non-Interactive CLI Usage

```bash
# 1. Inspect correlated sessions across active harnesses locally
agy-sync inspect

# 2. Inspect all harnesses and compare against the remote workstation over SSH
agy-sync --harness all inspect --probe-remote

# 3. Preview bidirectional sync (Dry-Run)
agy-sync --dry-run sync

# 4. Run full bidirectional synchronization (Local <-> Remote)
agy-sync sync

# 5. Synchronize only specific harnesses
agy-sync --harness agy-2.0,agy-cli sync

# 6. Unidirectional push (Local -> Remote) or pull (Remote -> Local)
agy-sync push
agy-sync pull
```
