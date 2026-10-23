# `agy-sync` — Multi-Harness Project & Session Synchronization Engine (`~/.gemini`)

`agy-sync` is a standalone, zero-dependency Rust CLI engine that identifies a project workspace folder (or an entire parent folder of multiple projects) and discovers every correlated conversation, recursive subagent, SQLite database, FTS5 search index, Protobuf summary index, `brain/` artifact, `scratch/` directory, `annotations/`, `implicit/` context, `browser_recordings/`, and `knowledge/` item across all Agy & Gemini harnesses stored in `~/.gemini/`.

Built as **one unified codebase for any multi-machine developer workflow**—whether syncing between a macOS laptop and a Linux desktop, a cloud VM, a GPU devbox, or a remote SSH workstation—it synchronizes both the project workspace files and the correlated `~/.gemini/` harness sessions **byte-for-byte with delta compression (`rsync -avz`)** and **surgical SQLite + Protobuf index merging** (including automatic cross-OS `$HOME` path remapping between macOS `/Users/<user>` and Linux `/home/<user>` or custom `$HOME` roots).

---

## Supported Harnesses in `~/.gemini/`

| CLI ID | Harness Name | Storage Path in `~/.gemini/` | Description |
| :--- | :--- | :--- | :--- |
| `agy-2.0` | **agy (2.0 / vscode plugin)** | `~/.gemini/antigravity/` | Unified Agy 2.0 Web UI & VS Code Extension (`agy --hub --app_data_dir=antigravity`) sessions (`conversation_summaries.db`, `agyhub_summaries_proto.pb`, `conversations/*.db`, `brain/<id>/`) |
| `agy-cli` | **agy-cli** | `~/.gemini/antigravity-cli/` | Antigravity CLI sessions (`conversation_summaries.db`, `*_summaries_proto.pb`, `history.jsonl`, `conversations/*.db`, `brain/<id>/`) |
| `agy-ide` | **agy (ide)** | `~/.gemini/antigravity-ide/` | Antigravity standalone IDE sessions (`agyhub_summaries_proto.pb`, `conversations/*.db`, `brain/<id>/`) |
| `agy-plugins` | **agy (ide plugins & project config)** | `~/.gemini/config/projects/*.json` | Workspace registration & `project_id` mappings used by the VS Code plugin and Agy 2.0 |
| `gemini-cli` | **gemini-cli (legacy)** | `~/.gemini/{projects.json,history,tmp}` | Legacy Gemini CLI project registry, chat logs, and shell history |
| `<subdir>` | **Dynamic & Experimental Harnesses** | `~/.gemini/<subdir>/` | Zero-config auto-discovery for any additional or next-gen agentic session store under `~/.gemini/` sharing the `conversation_summaries.db` + `*_summaries_proto.pb` + `brain/<id>/` architecture *(including certain high-velocity, personal-watercraft-named harnesses — if you know, you know 😉)* |

---

## Architecture & Under-the-Hood Highlights

- **Schema-Less Recursive Protobuf Wire-Format Rewriter**:
  - Directly parses and rewrites `$HOME` path prefixes (`/Users/<user>` $\leftrightarrow$ Linux `$HOME`) inside compiled Protobuf blobs (`*_summaries_proto.pb`, `raw_summary`, `trajectory_metadata_blob`, `implicit/*.pb`) by traversing nested length-delimited messages and recalculating all enclosing *varint* byte lengths bottom-up—without requiring `.proto` schema definitions.
- **Surgical Multi-Index Merging & Deep Self-Healing**:
  - Upserts only the target project's conversations into `conversation_summaries.db`, `conversations_fts` (FTS5 full-text search chunks), and `*_summaries_proto.pb` (with passive SQLite WAL checkpointing, rolling backups under `.sync_backups/latest/`, and automatic schema migration for legacy `conversation_summaries.db` tables) without disturbing any other project on either workstation.
  - Automatically deep-scans per-conversation `conversations/<id>.db` (`trajectory_metadata_blob`) and `implicit/<id>.pb` files to self-heal any unremapped foreign `$HOME` paths.
- **Full Recursive Subagent & Artifact Graph Discovery**:
  - Traces root conversations via `workspace_uris` (including cross-OS relative workspace path matching), recursively discovers child subagent/swarm sessions at any depth via `parent_conversation_id` and `brain/<id>/.system_generated/messages/*.json`, and correlates orphan `brain/<id>/` directories and `knowledge/*/metadata.json` entries.
- **Multi-Project Container Mode**:
  - Pointing `agy-sync` at a parent directory containing multiple project repositories (e.g., `~/Documents/projects`) automatically detects the union of child projects across Local and Remote and iterates `[1/N]..[N/N]` project-by-project.
- **High-Scale & Hardware-Key Ergonomics**:
  - Automatically elevates macOS soft file-descriptor limits (`RLIMIT_NOFILE` from `256` $\rightarrow$ `65536`), batches `rsync --files-from` transfers into chunks of 64 paths, excludes OS-specific build directories and symlinks (`target`, `node_modules`, `.venv`, `__pycache__`), and maintains a 4-hour SSH `ControlMaster` socket (`ControlPersist=4h`) so hardware security keys (FIDO2/WebAuthn) only require a single touch.

---

## Interactive Mode (`-i` / `--interactive`)

Run `agy-sync -i` inside any project directory to launch the interactive step-by-step wizard (in `en-US`):

```bash
agy-sync -i
```

1. **Multi-Harness Activity Scan**: Detects the current project path, checks which harnesses in `~/.gemini/` have interactions with it, and displays the session count, disk size, latest event timestamp, and a snippet of the **last user prompt** for each active harness.
2. **Per-Harness Confirmation (`[y/N]`)**: Asks one by one which detected harnesses to include (`y`) and which to ignore (`n`).
3. **Remote Host & SSH Port Resolution**: Prompts for the remote hostname (pre-filled from `$HOME/.config/.agy-sync` if configured) and automatically detects custom SSH ports from `host:port` or `~/.ssh/config`.
4. **SSH Authentication & Interactive Fallback**: Verifies non-interactive SSH connectivity first (`BatchMode=yes`). If already authenticated via SSH key or `ControlMaster`, proceeds immediately without prompting. Otherwise, establishes an interactive SSH `ControlMaster` socket (`ControlPersist=4h`) so password/OTP/FIDO2 authentication is completed once and reused across all `rsync` and `ssh` transfers.
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

# 3. Inspect or sync an entire parent folder containing multiple projects ([1/N]..[N/N])
agy-sync -p ~/Documents/projects inspect --probe-remote
agy-sync -p ~/Documents/projects sync

# 4. Preview bidirectional sync (Dry-Run)
agy-sync --dry-run sync

# 5. Run full bidirectional synchronization (Local <-> Remote)
agy-sync sync

# 6. Synchronize only specific harnesses
agy-sync --harness agy-2.0,agy-cli sync

# 7. Unidirectional push (Local -> Remote) or pull (Remote -> Local)
agy-sync push
agy-sync pull
```
