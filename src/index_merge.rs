use crate::discovery::find_summary_pb_files;
use crate::protobuf::{rewrite_protobuf_strings, ProtobufSummariesMap};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

/// Full row from `conversation_summaries` in `conversation_summaries.db`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryRow {
    pub conversation_id: String,
    pub title: String,
    pub preview: String,
    pub step_count: i64,
    pub created_time: String,
    pub last_modified_time: String,
    pub workspace_uris: String,
    pub status: String,
    pub agent_name: String,
    pub parent_conversation_id: String,
    pub project_id: String,
    pub last_user_input_time: String,
    pub raw_summary: Vec<u8>,
}

/// FTS chunk row from `conversation_search_chunks` / `conversation_search_fts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchChunkRow {
    pub conversation_id: String,
    pub chunk_type: String,
    pub step_index: i64,
    pub content_text: String,
}

/// Portable manifest containing all global SQLite and Protobuf index entries for a specific set
/// of project conversations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectIndexManifest {
    pub summaries: Vec<SummaryRow>,
    pub search_chunks: Vec<SearchChunkRow>,
    /// Raw `TrajectorySummary` protobuf entries keyed by `conversation_id` for `*_summaries_proto.pb`
    pub protobuf_entries: BTreeMap<String, Vec<u8>>,
    /// Filenames of `*_summaries_proto.pb` present in the source harness directory
    pub protobuf_filenames: Vec<String>,
}

/// Statistics returned after merging a `ProjectIndexManifest` into a target harness directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MergeStats {
    pub summaries_upserted: usize,
    pub summaries_skipped_older: usize,
    pub fts_chunks_updated: usize,
    pub protobuf_entries_updated: usize,
}

/// Ensures the schema of `conversation_summaries.db` exists on the given SQLite connection.
pub fn ensure_summary_db_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS conversation_summaries (
            conversation_id TEXT PRIMARY KEY,
            title TEXT NOT NULL DEFAULT '',
            preview TEXT NOT NULL DEFAULT '',
            step_count INTEGER NOT NULL DEFAULT 0,
            created_time TEXT NOT NULL DEFAULT '',
            last_modified_time TEXT NOT NULL DEFAULT '',
            workspace_uris TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT '',
            agent_name TEXT NOT NULL DEFAULT '',
            parent_conversation_id TEXT NOT NULL DEFAULT '',
            project_id TEXT NOT NULL DEFAULT '',
            last_user_input_time TEXT NOT NULL DEFAULT '',
            raw_summary BLOB
        );
        CREATE INDEX IF NOT EXISTS idx_summaries_modified ON conversation_summaries(last_modified_time DESC);
        CREATE INDEX IF NOT EXISTS idx_summaries_parent ON conversation_summaries(parent_conversation_id);
        CREATE INDEX IF NOT EXISTS idx_summaries_project ON conversation_summaries(project_id);
        CREATE TABLE IF NOT EXISTS conversation_search_chunks (
            chunk_id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation_id TEXT NOT NULL,
            chunk_type TEXT NOT NULL,
            step_index INTEGER NOT NULL DEFAULT -1,
            content_text TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_search_chunks_conv ON conversation_search_chunks(conversation_id);
        CREATE VIRTUAL TABLE IF NOT EXISTS conversation_search_fts USING fts5(
            content_text,
            content='conversation_search_chunks',
            content_rowid='chunk_id',
            tokenize='unicode61'
        );
        CREATE TRIGGER IF NOT EXISTS conversation_search_chunks_ai AFTER INSERT ON conversation_search_chunks BEGIN
            INSERT INTO conversation_search_fts(rowid, content_text) VALUES (new.chunk_id, new.content_text);
        END;
        CREATE TRIGGER IF NOT EXISTS conversation_search_chunks_ad AFTER DELETE ON conversation_search_chunks BEGIN
            INSERT INTO conversation_search_fts(conversation_search_fts, rowid, content_text) VALUES('delete', old.chunk_id, old.content_text);
        END;
        CREATE TRIGGER IF NOT EXISTS conversation_search_chunks_au AFTER UPDATE ON conversation_search_chunks BEGIN
            INSERT INTO conversation_search_fts(conversation_search_fts, rowid, content_text) VALUES('delete', old.chunk_id, old.content_text);
            INSERT INTO conversation_search_fts(rowid, content_text) VALUES (new.chunk_id, new.content_text);
        END;",
    )
    .context("Failed to initialize conversation_summaries.db schema")?;
    Ok(())
}

/// Exports the SQLite summary rows, FTS search chunks, and `*_summaries_proto.pb` entries
/// for `conversation_ids` from `harness_dir`, optionally remapping `$HOME` paths (`from_home` -> `to_home`).
pub fn export_project_manifest(
    harness_dir: &Path,
    conversation_ids: &BTreeSet<String>,
    remap_homes: Option<(&str, &str)>,
) -> Result<ProjectIndexManifest> {
    let mut manifest = ProjectIndexManifest::default();
    if conversation_ids.is_empty() {
        return Ok(manifest);
    }

    let db_path = harness_dir.join("conversation_summaries.db");
    if db_path.exists() {
        let conn = Connection::open(&db_path)
            .with_context(|| format!("Failed to open {}", db_path.display()))?;
        let _ = conn.busy_timeout(std::time::Duration::from_millis(3000));

        let mut sum_stmt = conn.prepare(
            "SELECT conversation_id, title, preview, step_count, created_time, last_modified_time,
                    workspace_uris, status, agent_name, parent_conversation_id, project_id,
                    last_user_input_time, raw_summary
             FROM conversation_summaries
             WHERE conversation_id = ?1",
        )?;

        let has_chunks_table: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='conversation_search_chunks'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;

        for cid in conversation_ids {
            let row_opt = sum_stmt
                .query_row(params![cid], |row| {
                    Ok(SummaryRow {
                        conversation_id: row.get(0)?,
                        title: row.get(1)?,
                        preview: row.get(2)?,
                        step_count: row.get(3)?,
                        created_time: row.get(4)?,
                        last_modified_time: row.get(5)?,
                        workspace_uris: row.get(6)?,
                        status: row.get(7)?,
                        agent_name: row.get(8)?,
                        parent_conversation_id: row.get(9)?,
                        project_id: row.get(10)?,
                        last_user_input_time: row.get(11)?,
                        raw_summary: row.get::<_, Option<Vec<u8>>>(12)?.unwrap_or_default(),
                    })
                })
                .optional()?;

            if let Some(mut s_row) = row_opt {
                if let Some((from_h, to_h)) = remap_homes {
                    if !from_h.is_empty() && from_h != to_h {
                        s_row.workspace_uris = s_row.workspace_uris.replace(from_h, to_h);
                        s_row.preview = s_row.preview.replace(from_h, to_h);
                        if !s_row.raw_summary.is_empty() {
                            if let Ok(rewritten) =
                                rewrite_protobuf_strings(&s_row.raw_summary, from_h, to_h)
                            {
                                s_row.raw_summary = rewritten;
                            }
                        }
                    }
                }
                manifest.summaries.push(s_row);
            }

            if has_chunks_table {
                let mut chunk_stmt = conn.prepare(
                    "SELECT conversation_id, chunk_type, step_index, content_text
                     FROM conversation_search_chunks
                     WHERE conversation_id = ?1
                     ORDER BY chunk_id ASC",
                )?;
                let chunks = chunk_stmt.query_map(params![cid], |row| {
                    Ok(SearchChunkRow {
                        conversation_id: row.get(0)?,
                        chunk_type: row.get(1)?,
                        step_index: row.get(2)?,
                        content_text: row.get(3)?,
                    })
                })?;
                for c in chunks.flatten() {
                    let mut c_row = c;
                    if let Some((from_h, to_h)) = remap_homes {
                        if !from_h.is_empty() && from_h != to_h {
                            c_row.content_text = c_row.content_text.replace(from_h, to_h);
                        }
                    }
                    manifest.search_chunks.push(c_row);
                }
            }
        }
    }

    // Export from any `*_summaries_proto.pb` files present in `harness_dir`
    for pb_path in find_summary_pb_files(harness_dir) {
        if let Some(fname) = pb_path.file_name().and_then(|n| n.to_str()) {
            if !manifest.protobuf_filenames.contains(&fname.to_string()) {
                manifest.protobuf_filenames.push(fname.to_string());
            }
        }
        if let Ok(pb_map) = ProtobufSummariesMap::load_from_path(&pb_path) {
            for cid in conversation_ids {
                if let Some(raw_bytes) = pb_map.get(cid) {
                    let entry_bytes = if let Some((from_h, to_h)) = remap_homes {
                        rewrite_protobuf_strings(raw_bytes, from_h, to_h)
                            .unwrap_or_else(|_| raw_bytes.to_vec())
                    } else {
                        raw_bytes.to_vec()
                    };
                    manifest.protobuf_entries.insert(cid.clone(), entry_bytes);
                }
            }
        }
    }

    // Also ensure any summary row with non-empty `raw_summary` populates `protobuf_entries` if missing
    for s_row in &manifest.summaries {
        if !s_row.raw_summary.is_empty()
            && !manifest.protobuf_entries.contains_key(&s_row.conversation_id)
        {
            manifest
                .protobuf_entries
                .insert(s_row.conversation_id.clone(), s_row.raw_summary.clone());
        }
    }

    Ok(manifest)
}

/// Creates a lightweight rolling backup of `conversation_summaries.db` and `*_summaries_proto.pb`
/// inside `<harness_dir>/.sync_backups/latest/` prior to mutating the global index.
pub fn backup_global_indexes(harness_dir: &Path) -> Result<()> {
    let backup_dir = harness_dir.join(".sync_backups").join("latest");
    fs::create_dir_all(&backup_dir)?;

    let db_path = harness_dir.join("conversation_summaries.db");
    if db_path.exists() {
        fs::copy(&db_path, backup_dir.join("conversation_summaries.db"))?;
    }
    for pb_path in find_summary_pb_files(harness_dir) {
        if let Some(fname) = pb_path.file_name() {
            fs::copy(&pb_path, backup_dir.join(fname))?;
        }
    }
    Ok(())
}

/// Surgically merges `manifest` into `harness_dir`'s `conversation_summaries.db` and `*_summaries_proto.pb`
/// without disturbing any other project's sessions!
///
/// If `force` is false (default for bidirectional `sync`), a conversation is only updated when the
/// incoming `step_count` is strictly greater than existing, or `step_count` is equal and
/// `last_modified_time` is newer or equal.
pub fn merge_project_manifest(
    harness_dir: &Path,
    manifest: &ProjectIndexManifest,
    force: bool,
) -> Result<MergeStats> {
    let mut stats = MergeStats::default();
    if manifest.summaries.is_empty() && manifest.protobuf_entries.is_empty() {
        return Ok(stats);
    }

    fs::create_dir_all(harness_dir)?;
    backup_global_indexes(harness_dir)?;

    let db_path = harness_dir.join("conversation_summaries.db");
    let mut conn = Connection::open(&db_path)
        .with_context(|| format!("Failed to open {}", db_path.display()))?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(5000));
    ensure_summary_db_schema(&conn)?;

    let mut updated_cids = BTreeSet::new();

    {
        let tx = conn.transaction()?;

        // Group incoming search chunks by conversation_id
        let mut chunks_by_cid: BTreeMap<&str, Vec<&SearchChunkRow>> = BTreeMap::new();
        for c in &manifest.search_chunks {
            chunks_by_cid
                .entry(c.conversation_id.as_str())
                .or_default()
                .push(c);
        }

        for s in &manifest.summaries {
            let existing: Option<(i64, String)> = tx
                .query_row(
                    "SELECT step_count, last_modified_time FROM conversation_summaries WHERE conversation_id = ?1",
                    params![&s.conversation_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;

            let should_upsert = match existing {
                None => true,
                Some((ex_steps, ex_time)) => {
                    force
                        || s.step_count > ex_steps
                        || (s.step_count == ex_steps && s.last_modified_time >= ex_time)
                }
            };

            if !should_upsert {
                stats.summaries_skipped_older += 1;
                continue;
            }

            tx.execute(
                "INSERT INTO conversation_summaries (
                    conversation_id, title, preview, step_count, created_time, last_modified_time,
                    workspace_uris, status, agent_name, parent_conversation_id, project_id,
                    last_user_input_time, raw_summary
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                ON CONFLICT(conversation_id) DO UPDATE SET
                    title = excluded.title,
                    preview = excluded.preview,
                    step_count = excluded.step_count,
                    created_time = excluded.created_time,
                    last_modified_time = excluded.last_modified_time,
                    workspace_uris = excluded.workspace_uris,
                    status = excluded.status,
                    agent_name = excluded.agent_name,
                    parent_conversation_id = excluded.parent_conversation_id,
                    project_id = excluded.project_id,
                    last_user_input_time = excluded.last_user_input_time,
                    raw_summary = excluded.raw_summary",
                params![
                    &s.conversation_id,
                    &s.title,
                    &s.preview,
                    s.step_count,
                    &s.created_time,
                    &s.last_modified_time,
                    &s.workspace_uris,
                    &s.status,
                    &s.agent_name,
                    &s.parent_conversation_id,
                    &s.project_id,
                    &s.last_user_input_time,
                    &s.raw_summary,
                ],
            )?;

            stats.summaries_upserted += 1;
            updated_cids.insert(s.conversation_id.clone());

            if let Some(inc_chunks) = chunks_by_cid.get(s.conversation_id.as_str()) {
                tx.execute(
                    "DELETE FROM conversation_search_chunks WHERE conversation_id = ?1",
                    params![&s.conversation_id],
                )?;
                let mut ins_chunk = tx.prepare(
                    "INSERT INTO conversation_search_chunks (conversation_id, chunk_type, step_index, content_text)
                     VALUES (?1, ?2, ?3, ?4)",
                )?;
                for c in inc_chunks {
                    ins_chunk.execute(params![
                        &c.conversation_id,
                        &c.chunk_type,
                        c.step_index,
                        &c.content_text
                    ])?;
                    stats.fts_chunks_updated += 1;
                }
            }
        }

        tx.commit()?;
    }

    // Flush WAL so external readers see the newly merged rows immediately
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");

    // Update `*_summaries_proto.pb` files in `harness_dir`
    if !manifest.protobuf_entries.is_empty() {
        let mut target_pb_paths = find_summary_pb_files(harness_dir);
        if target_pb_paths.is_empty() {
            if !manifest.protobuf_filenames.is_empty() {
                for fname in &manifest.protobuf_filenames {
                    target_pb_paths.push(harness_dir.join(fname));
                }
            } else {
                target_pb_paths.push(harness_dir.join("agyhub_summaries_proto.pb"));
            }
        }

        for pb_path in target_pb_paths {
            let mut pb_map = ProtobufSummariesMap::load_from_path(&pb_path)?;
            for (cid, raw_bytes) in &manifest.protobuf_entries {
                if force || updated_cids.contains(cid) || pb_map.get(cid).is_none() {
                    pb_map.upsert(cid.clone(), raw_bytes.clone());
                    stats.protobuf_entries_updated += 1;
                }
            }
            pb_map.save_to_path(&pb_path)?;
        }
    }

    Ok(stats)
}

/// Remaps `$HOME` paths (`from_home` -> `to_home`) inside per-conversation artifacts on disk:
/// - `conversations/<id>.db` -> `trajectory_metadata_blob` (and `steps` if `deep_remap` is enabled)
/// - `implicit/<id>.pb` -> length-prefixed Protobuf string rewriting
/// - `brain/<id>/.system_generated/logs/transcript*.jsonl` -> UTF-8 path replacement (if `deep_remap` is enabled)
pub fn remap_conversation_artifacts(
    harness_dir: &Path,
    conversation_ids: &BTreeSet<String>,
    from_home: &str,
    to_home: &str,
    deep_remap: bool,
) -> Result<()> {
    let from_h = from_home.trim_end_matches('/');
    let to_h = to_home.trim_end_matches('/');
    if from_h.is_empty() || to_h.is_empty() || from_h == to_h {
        return Ok(());
    }

    for cid in conversation_ids {
        // 1. Remap conversations/<id>.db
        let conv_db_path = harness_dir.join("conversations").join(format!("{}.db", cid));
        if conv_db_path.exists() {
            remap_single_conversation_db(&conv_db_path, from_h, to_h, deep_remap)?;
        }

        // 2. Remap implicit/<id>.pb
        let implicit_path = harness_dir.join("implicit").join(format!("{}.pb", cid));
        if implicit_path.exists() {
            if let Ok(bytes) = fs::read(&implicit_path) {
                if let Ok(rewritten) = rewrite_protobuf_strings(&bytes, from_h, to_h) {
                    if rewritten != bytes {
                        fs::write(&implicit_path, rewritten)?;
                    }
                }
            }
        }

        // 3. Optional deep remap for transcript.jsonl / transcript_full.jsonl
        if deep_remap {
            let logs_dir = harness_dir
                .join("brain")
                .join(cid)
                .join(".system_generated")
                .join("logs");
            for name in ["transcript.jsonl", "transcript_full.jsonl"] {
                let p = logs_dir.join(name);
                if p.exists() {
                    if let Ok(text) = fs::read_to_string(&p) {
                        if text.contains(from_h) {
                            let updated = text.replace(from_h, to_h);
                            fs::write(&p, updated)?;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn remap_single_conversation_db(
    db_path: &Path,
    from_h: &str,
    to_h: &str,
    deep_remap: bool,
) -> Result<()> {
    let mut conn = Connection::open(db_path)
        .with_context(|| format!("Failed to open conversation DB {}", db_path.display()))?;
    let _ = conn.busy_timeout(std::time::Duration::from_millis(3000));

    let has_meta: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='trajectory_metadata_blob'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap_or(0)
        > 0;

    if has_meta {
        let tx = conn.transaction()?;
        let rows: Vec<(String, Vec<u8>)> = {
            let mut stmt = tx.prepare("SELECT id, data FROM trajectory_metadata_blob")?;
            let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            mapped.flatten().collect()
        };

        for (id, blob) in rows {
            if let Ok(rewritten) = rewrite_protobuf_strings(&blob, from_h, to_h) {
                if rewritten != blob {
                    tx.execute(
                        "UPDATE trajectory_metadata_blob SET data = ?1 WHERE id = ?2",
                        params![rewritten, id],
                    )?;
                }
            }
        }
        tx.commit()?;
    }

    if deep_remap {
        let has_steps: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='steps'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if has_steps {
            let tx = conn.transaction()?;
            let rows: Vec<(i64, Vec<u8>, Vec<u8>)> = {
                let mut stmt = tx.prepare("SELECT idx, metadata, step_payload FROM steps")?;
                let mapped = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                mapped.flatten().collect()
            };
            for (idx, meta, payload) in rows {
                let new_meta = rewrite_protobuf_strings(&meta, from_h, to_h).unwrap_or(meta.clone());
                let new_payload =
                    rewrite_protobuf_strings(&payload, from_h, to_h).unwrap_or(payload.clone());
                if new_meta != meta || new_payload != payload {
                    tx.execute(
                        "UPDATE steps SET metadata = ?1, step_payload = ?2 WHERE idx = ?3",
                        params![new_meta, new_payload, idx],
                    )?;
                }
            }
            tx.commit()?;
        }
    }

    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protobuf::encode_varint;
    use tempfile::TempDir;

    fn make_dummy_proto_with_uri(uri: &str) -> Vec<u8> {
        let mut out = Vec::new();
        encode_varint(0x0a, &mut out);
        encode_varint(uri.len() as u64, &mut out);
        out.extend_from_slice(uri.as_bytes());
        out
    }

    #[test]
    fn test_surgical_export_remap_and_merge_preserves_other_projects() -> Result<()> {
        let tmp = TempDir::new()?;
        let src_dir = tmp.path().join("src_harness");
        let dst_dir = tmp.path().join("dst_harness");
        fs::create_dir_all(&src_dir)?;
        fs::create_dir_all(&dst_dir)?;

        let cid_proj = "11111111-1111-1111-1111-111111111111";
        let cid_other = "99999999-9999-9999-9999-999999999999";

        // 1. Populate source DB & PB with cid_proj (using /Users/devuser)
        let src_conn = Connection::open(src_dir.join("conversation_summaries.db"))?;
        ensure_summary_db_schema(&src_conn)?;
        let raw_mac = make_dummy_proto_with_uri("file:///Users/devuser/proj");
        src_conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, preview, step_count, last_modified_time, workspace_uris, raw_summary)
             VALUES (?1, 'My Project', ' working in /Users/devuser/proj ', 25, '2026-10-10T02:00:00Z', '[\"file:///Users/devuser/proj\"]', ?2)",
            params![cid_proj, &raw_mac],
        )?;
        src_conn.execute(
            "INSERT INTO conversation_search_chunks (conversation_id, chunk_type, step_index, content_text)
             VALUES (?1, 'step', 1, 'Edited /Users/devuser/proj/main.rs')",
            params![cid_proj],
        )?;
        drop(src_conn);

        let mut src_pb = ProtobufSummariesMap::new();
        src_pb.upsert(cid_proj.to_string(), raw_mac.clone());
        src_pb.save_to_path(&src_dir.join("agyhub_summaries_proto.pb"))?;

        // 2. Populate destination DB & PB with an existing unrelated session cid_other AND an older version of cid_proj
        let dst_conn = Connection::open(dst_dir.join("conversation_summaries.db"))?;
        ensure_summary_db_schema(&dst_conn)?;
        let raw_other = make_dummy_proto_with_uri("file:///home/remote/devuser/other");
        dst_conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, preview, step_count, last_modified_time, workspace_uris, raw_summary)
             VALUES (?1, 'Other Unrelated', 'preview', 100, '2026-10-10T01:00:00Z', '[\"file:///home/remote/devuser/other\"]', ?2)",
            params![cid_other, &raw_other],
        )?;
        dst_conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, preview, step_count, last_modified_time, workspace_uris, raw_summary)
             VALUES (?1, 'My Project Old', 'old', 10, '2026-10-10T01:00:00Z', '[\"file:///home/remote/devuser/proj\"]', ?2)",
            params![cid_proj, &raw_other],
        )?;
        drop(dst_conn);

        let mut dst_pb = ProtobufSummariesMap::new();
        dst_pb.upsert(cid_other.to_string(), raw_other.clone());
        dst_pb.save_to_path(&dst_dir.join("agyhub_summaries_proto.pb"))?;

        // 3. Export cid_proj from src with remapping (/Users/devuser -> /home/remote/devuser)
        let mut set = BTreeSet::new();
        set.insert(cid_proj.to_string());
        let manifest = export_project_manifest(
            &src_dir,
            &set,
            Some(("/Users/devuser", "/home/remote/devuser")),
        )?;

        // 4. Merge into dst
        let stats = merge_project_manifest(&dst_dir, &manifest, false)?;
        assert_eq!(stats.summaries_upserted, 1);
        assert_eq!(stats.fts_chunks_updated, 1);
        assert_eq!(stats.protobuf_entries_updated, 1);

        // 5. Verify dst DB has BOTH cid_other (untouched!) and updated cid_proj (remapped!)
        let verify_conn = Connection::open(dst_dir.join("conversation_summaries.db"))?;
        let (steps, ws, raw_merged): (i64, String, Vec<u8>) = verify_conn.query_row(
            "SELECT step_count, workspace_uris, raw_summary FROM conversation_summaries WHERE conversation_id = ?1",
            params![cid_proj],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!(steps, 25);
        assert_eq!(ws, "[\"file:///home/remote/devuser/proj\"]");
        assert_eq!(
            raw_merged,
            make_dummy_proto_with_uri("file:///home/remote/devuser/proj")
        );

        let other_steps: i64 = verify_conn.query_row(
            "SELECT step_count FROM conversation_summaries WHERE conversation_id = ?1",
            params![cid_other],
            |r| r.get(0),
        )?;
        assert_eq!(other_steps, 100);

        // Verify FTS search works on the remapped path!
        let fts_matches: i64 = verify_conn.query_row(
            "SELECT COUNT(*) FROM conversation_search_fts WHERE conversation_search_fts MATCH 'remote'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(fts_matches, 1);

        // Verify Protobuf map has both entries
        let merged_pb =
            ProtobufSummariesMap::load_from_path(&dst_dir.join("agyhub_summaries_proto.pb"))?;
        assert_eq!(merged_pb.len(), 2);

        Ok(())
    }
}
