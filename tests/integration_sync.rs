use agy_sync::discovery::{normalize_project_path, path_to_file_uri};
use agy_sync::engine::{run_engine, EngineConfig, SessionAction, SyncMode};
use agy_sync::harnesses::HarnessKind;
use agy_sync::index_merge::ensure_summary_db_schema;
use agy_sync::protobuf::{encode_varint, ProtobufSummariesMap};
use agy_sync::transport::RemoteEndpoint;
use rusqlite::{params, Connection};
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn create_proto_blob_with_uri(uri: &str) -> Vec<u8> {
    let mut out = Vec::new();
    encode_varint(0x0a, &mut out);
    encode_varint(uri.len() as u64, &mut out);
    out.extend_from_slice(uri.as_bytes());
    out
}

fn create_conversation_db(db_path: &Path, uri: &str) {
    if let Some(p) = db_path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    let conn = Connection::open(db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE trajectory_metadata_blob (id text PRIMARY KEY, data blob);
         CREATE TABLE steps (idx integer PRIMARY KEY, metadata blob, step_payload blob);",
    )
    .unwrap();
    let blob = create_proto_blob_with_uri(uri);
    conn.execute(
        "INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', ?1)",
        params![blob],
    )
    .unwrap();
}

fn read_trajectory_metadata_blob(db_path: &Path) -> Vec<u8> {
    let conn = Connection::open(db_path).unwrap();
    conn.query_row(
        "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn test_bidirectional_sync_mac_and_remote_end_to_end() {
    let tmp = TempDir::new().unwrap();
    let mac_home_dir = tmp.path().join("Users/alice");
    let remote_home_dir = tmp.path().join("home/remote/alice");

    fs::create_dir_all(&mac_home_dir).unwrap();
    fs::create_dir_all(&remote_home_dir).unwrap();

    let mac_home = normalize_project_path(&mac_home_dir)
        .to_string_lossy()
        .to_string();
    let remote_home = normalize_project_path(&remote_home_dir)
        .to_string_lossy()
        .to_string();

    let mac_proj = Path::new(&mac_home).join("Documents/projects/super-app");
    let remote_proj = Path::new(&remote_home).join("Documents/projects/super-app");
    fs::create_dir_all(&mac_proj).unwrap();
    fs::create_dir_all(&remote_proj).unwrap();

    // Workspace files on each side
    fs::write(mac_proj.join("mac_feature.rs"), b"pub fn mac() {}").unwrap();
    fs::write(remote_proj.join("remote_feature.rs"), b"pub fn remote() {}").unwrap();

    let mac_agy = Path::new(&mac_home).join(".gemini/antigravity");
    let remote_agy = Path::new(&remote_home).join(".gemini/antigravity");
    fs::create_dir_all(&mac_agy).unwrap();
    fs::create_dir_all(&remote_agy).unwrap();

    let mac_proj_uri = path_to_file_uri(&mac_proj);
    let remote_proj_uri = path_to_file_uri(&remote_proj);

    let s1 = "11111111-1111-1111-1111-111111111111"; // Newer on Mac (20 steps)
    let s1_sub = "11111111-2222-3333-4444-555555555555"; // Subagent of S1 on Mac (6 steps)
    let s2 = "22222222-2222-2222-2222-222222222222"; // Older on Mac (5 steps), Newer on Remote (18 steps)
    let s3 = "33333333-3333-3333-3333-333333333333"; // Only on Remote (12 steps)
    let mac_other = "aaaaaaaa-0000-0000-0000-000000000000"; // Unrelated Mac project
    let remote_other = "bbbbbbbb-0000-0000-0000-000000000000"; // Unrelated Remote project

    // Populate Mac conversation_summaries.db & agyhub_summaries_proto.pb
    {
        let conn = Connection::open(mac_agy.join("conversation_summaries.db")).unwrap();
        ensure_summary_db_schema(&conn).unwrap();
        let blob_mac = create_proto_blob_with_uri(&mac_proj_uri);
        let blob_other = create_proto_blob_with_uri(&format!("file://{}/other", mac_home));

        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, step_count, last_modified_time, workspace_uris, parent_conversation_id, last_user_input_time, raw_summary)
             VALUES (?1, 'S1 Root', 20, '2026-10-10 01:00:00Z', ?2, '', '2026-10-10 01:00:00Z', ?3)",
            params![s1, format!("[\"{}\"]", mac_proj_uri), blob_mac],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, step_count, last_modified_time, workspace_uris, parent_conversation_id, last_user_input_time, raw_summary)
             VALUES (?1, 'S1 Subagent', 6, '2026-10-10 01:01:00Z', ?2, ?3, '2026-10-10 01:01:00Z', ?4)",
            params![s1_sub, format!("[\"{}\"]", mac_proj_uri), s1, blob_mac],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, step_count, last_modified_time, workspace_uris, parent_conversation_id, last_user_input_time, raw_summary)
             VALUES (?1, 'S2 Old', 5, '2026-10-09 10:00:00Z', ?2, '', '2026-10-09 10:00:00Z', ?3)",
            params![s2, format!("[\"{}\"]", mac_proj_uri), blob_mac],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, step_count, last_modified_time, workspace_uris, parent_conversation_id, last_user_input_time, raw_summary)
             VALUES (?1, 'Mac Other', 30, '2026-10-09 10:00:00Z', ?2, '', '2026-10-09 10:00:00Z', ?3)",
            params![mac_other, format!("[\"file://{}/other\"]", mac_home), blob_other],
        )
        .unwrap();

        let mut pb = ProtobufSummariesMap::new();
        pb.upsert(s1.to_string(), blob_mac.clone());
        pb.upsert(s1_sub.to_string(), blob_mac.clone());
        pb.upsert(s2.to_string(), blob_mac.clone());
        pb.upsert(mac_other.to_string(), blob_other);
        pb.save_to_path(&mac_agy.join("agyhub_summaries_proto.pb"))
            .unwrap();

        create_conversation_db(
            &mac_agy.join(format!("conversations/{}.db", s1)),
            &mac_proj_uri,
        );
        create_conversation_db(
            &mac_agy.join(format!("conversations/{}.db", s1_sub)),
            &mac_proj_uri,
        );
        create_conversation_db(
            &mac_agy.join(format!("conversations/{}.db", s2)),
            &mac_proj_uri,
        );
        fs::create_dir_all(mac_agy.join(format!("brain/{}/scratch", s1))).unwrap();
        fs::write(
            mac_agy.join(format!("brain/{}/plan.md", s1)),
            b"# S1 Plan on Mac",
        )
        .unwrap();
        fs::create_dir_all(mac_agy.join("annotations")).unwrap();
        fs::write(
            mac_agy.join(format!("annotations/{}.pbtxt", s1)),
            b"title:\"S1 Root\"",
        )
        .unwrap();
    }

    // Populate Remote conversation_summaries.db & agyhub_summaries_proto.pb
    {
        let conn = Connection::open(remote_agy.join("conversation_summaries.db")).unwrap();
        ensure_summary_db_schema(&conn).unwrap();
        let blob_rt = create_proto_blob_with_uri(&remote_proj_uri);
        let blob_other = create_proto_blob_with_uri(&format!("file://{}/rt_other", remote_home));

        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, step_count, last_modified_time, workspace_uris, parent_conversation_id, last_user_input_time, raw_summary)
             VALUES (?1, 'S2 Updated on Remote', 18, '2026-10-10 02:00:00Z', ?2, '', '2026-10-10 02:00:00Z', ?3)",
            params![s2, format!("[\"{}\"]", remote_proj_uri), blob_rt],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, step_count, last_modified_time, workspace_uris, parent_conversation_id, last_user_input_time, raw_summary)
             VALUES (?1, 'S3 Remote Exclusive', 12, '2026-10-10 02:30:00Z', ?2, '', '2026-10-10 02:30:00Z', ?3)",
            params![s3, format!("[\"{}\"]", remote_proj_uri), blob_rt],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, step_count, last_modified_time, workspace_uris, parent_conversation_id, last_user_input_time, raw_summary)
             VALUES (?1, 'Remote Other', 50, '2026-10-10 02:30:00Z', ?2, '', '2026-10-10 02:30:00Z', ?3)",
            params![remote_other, format!("[\"file://{}/rt_other\"]", remote_home), blob_other],
        )
        .unwrap();

        let mut pb = ProtobufSummariesMap::new();
        pb.upsert(s2.to_string(), blob_rt.clone());
        pb.upsert(s3.to_string(), blob_rt.clone());
        pb.upsert(remote_other.to_string(), blob_other);
        pb.save_to_path(&remote_agy.join("agyhub_summaries_proto.pb"))
            .unwrap();

        create_conversation_db(
            &remote_agy.join(format!("conversations/{}.db", s2)),
            &remote_proj_uri,
        );
        create_conversation_db(
            &remote_agy.join(format!("conversations/{}.db", s3)),
            &remote_proj_uri,
        );
        fs::create_dir_all(remote_agy.join(format!("brain/{}", s2))).unwrap();
        fs::write(
            remote_agy.join(format!("brain/{}/walkthrough.md", s2)),
            b"# S2 Walkthrough from Remote",
        )
        .unwrap();
        fs::create_dir_all(remote_agy.join(format!("brain/{}", s3))).unwrap();
        fs::write(
            remote_agy.join(format!("brain/{}/spec.md", s3)),
            b"# S3 Spec from Remote",
        )
        .unwrap();
    }

    // Run bidirectional Sync Engine
    let config = EngineConfig {
        mode: SyncMode::Sync,
        project_path: mac_proj.clone(),
        local_home: mac_home.clone(),
        local_gemini_dir: Path::new(&mac_home).join(".gemini"),
        selected_harnesses: vec![HarnessKind::Agy20],
        endpoint: Some(RemoteEndpoint::LocalRoot {
            root_home: Path::new(&remote_home).to_path_buf(),
        }),
        no_remap: false,
        deep_remap: false,
        dry_run: false,
        delete: false,
        include_build_dirs: false,
        extra_excludes: vec![],
    };

    let report = run_engine(&config).unwrap();

    // Verify reconciliation decisions
    assert_eq!(
        report.session_actions.get(s1),
        Some(&SessionAction::PushToRemote)
    );
    assert_eq!(
        report.session_actions.get(s1_sub),
        Some(&SessionAction::PushToRemote)
    );
    assert_eq!(
        report.session_actions.get(s2),
        Some(&SessionAction::PullToLocal)
    );
    assert_eq!(
        report.session_actions.get(s3),
        Some(&SessionAction::PullToLocal)
    );

    // Verify step delta details in harness report
    let hr = &report.harness_reports[0];
    let s2_pulled = hr
        .pulled_sessions
        .iter()
        .find(|d| d.conversation_id == s2)
        .unwrap();
    assert_eq!(s2_pulled.before_steps, 5);
    assert_eq!(s2_pulled.after_steps, 18);

    // 1. Verify workspace files synced bidirectionally
    assert!(mac_proj.join("remote_feature.rs").exists());
    assert!(remote_proj.join("mac_feature.rs").exists());

    // 2. Verify brain & annotation files synced
    assert!(remote_agy.join(format!("brain/{}/plan.md", s1)).exists());
    assert!(remote_agy
        .join(format!("annotations/{}.pbtxt", s1))
        .exists());
    assert!(mac_agy
        .join(format!("brain/{}/walkthrough.md", s2))
        .exists());
    assert!(mac_agy.join(format!("brain/{}/spec.md", s3)).exists());

    // 3. Verify per-conversation SQLite trajectory_metadata_blob was remapped on both sides!
    assert_eq!(
        read_trajectory_metadata_blob(&remote_agy.join(format!("conversations/{}.db", s1))),
        create_proto_blob_with_uri(&remote_proj_uri)
    );
    assert_eq!(
        read_trajectory_metadata_blob(&mac_agy.join(format!("conversations/{}.db", s3))),
        create_proto_blob_with_uri(&mac_proj_uri)
    );

    // 4. Verify global conversation_summaries.db and agyhub_summaries_proto.pb on both sides
    let mac_conn = Connection::open(mac_agy.join("conversation_summaries.db")).unwrap();
    let (s2_mac_steps, s2_mac_ws): (i64, String) = mac_conn
        .query_row(
            "SELECT step_count, workspace_uris FROM conversation_summaries WHERE conversation_id = ?1",
            params![s2],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(s2_mac_steps, 18);
    assert_eq!(s2_mac_ws, format!("[\"{}\"]", mac_proj_uri));

    // Unrelated Mac session still intact
    let other_mac_count: i64 = mac_conn
        .query_row(
            "SELECT COUNT(*) FROM conversation_summaries WHERE conversation_id = ?1",
            params![mac_other],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(other_mac_count, 1);

    let rt_conn = Connection::open(remote_agy.join("conversation_summaries.db")).unwrap();
    let (s1_rt_steps, s1_rt_ws): (i64, String) = rt_conn
        .query_row(
            "SELECT step_count, workspace_uris FROM conversation_summaries WHERE conversation_id = ?1",
            params![s1],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(s1_rt_steps, 20);
    assert_eq!(s1_rt_ws, format!("[\"{}\"]", remote_proj_uri));

    // Unrelated Remote session still intact
    let other_rt_count: i64 = rt_conn
        .query_row(
            "SELECT COUNT(*) FROM conversation_summaries WHERE conversation_id = ?1",
            params![remote_other],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(other_rt_count, 1);

    // 5. Run a second consecutive sync (InSyncDeltaCheck) and verify per-conversation DB blobs
    //    do NOT ping-pong or clobber local $HOME URIs with remote $HOME URIs!
    let report2 = run_engine(&config).unwrap();
    assert_eq!(
        report2.session_actions.get(s1),
        Some(&SessionAction::InSyncDeltaCheck)
    );
    assert_eq!(
        read_trajectory_metadata_blob(&mac_agy.join(format!("conversations/{}.db", s1))),
        create_proto_blob_with_uri(&mac_proj_uri)
    );
    assert_eq!(
        read_trajectory_metadata_blob(&remote_agy.join(format!("conversations/{}.db", s1))),
        create_proto_blob_with_uri(&remote_proj_uri)
    );

    // 6. Verify running sync directly on $HOME is rejected with an error
    let mut bad_home_cfg = config.clone();
    bad_home_cfg.project_path = Path::new(&mac_home).to_path_buf();
    assert!(run_engine(&bad_home_cfg).is_err());
}

#[test]
fn test_multi_harness_sync_agy20_plugins_and_legacy_cli() {
    let tmp = TempDir::new().unwrap();
    let mac_home_dir = tmp.path().join("Users/bob");
    let remote_home_dir = tmp.path().join("home/remote/bob");

    fs::create_dir_all(&mac_home_dir).unwrap();
    fs::create_dir_all(&remote_home_dir).unwrap();

    let mac_home = normalize_project_path(&mac_home_dir)
        .to_string_lossy()
        .to_string();
    let remote_home = normalize_project_path(&remote_home_dir)
        .to_string_lossy()
        .to_string();

    let mac_proj = Path::new(&mac_home).join("Documents/projects/vscode-app");
    let remote_proj = Path::new(&remote_home).join("Documents/projects/vscode-app");
    fs::create_dir_all(&mac_proj).unwrap();
    fs::create_dir_all(&remote_proj).unwrap();
    fs::write(mac_proj.join("index.ts"), b"console.log('hello');").unwrap();

    let mac_gemini = Path::new(&mac_home).join(".gemini");
    let remote_gemini = Path::new(&remote_home).join(".gemini");
    let mac_agy20 = mac_gemini.join("antigravity");
    let remote_agy20 = remote_gemini.join("antigravity");
    fs::create_dir_all(&mac_agy20).unwrap();
    fs::create_dir_all(&remote_agy20).unwrap();

    let mac_proj_uri = format!("file://{}", mac_proj.display());
    let proj_uuid = "4215a0cc-24dd-463a-b752-559132119263";
    let conv_vscode = "77777777-8888-9999-aaaa-bbbbbbbbbbbb";

    // 1. Register project in ~/.gemini/config/projects/<proj_uuid>.json on Mac
    let cfg_dir = mac_gemini.join("config/projects");
    fs::create_dir_all(&cfg_dir).unwrap();
    let cfg_json = serde_json::json!({
        "id": proj_uuid,
        "name": "vscode-app",
        "projectResources": {
            "resources": [
                { "folderUri": mac_proj_uri }
            ]
        }
    });
    fs::write(
        cfg_dir.join(format!("{}.json", proj_uuid)),
        serde_json::to_string_pretty(&cfg_json).unwrap(),
    )
    .unwrap();

    // 2. Create a session in ~/.gemini/antigravity where workspace_uris is empty ("[]")
    //    and correlation relies on project_id == proj_uuid, plus agyhub_summaries_proto.pb!
    {
        let conn = Connection::open(mac_agy20.join("conversation_summaries.db")).unwrap();
        ensure_summary_db_schema(&conn).unwrap();
        let blob_mac = create_proto_blob_with_uri(&mac_proj_uri);
        conn.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, step_count, last_modified_time, workspace_uris, project_id, last_user_input_time, raw_summary)
             VALUES (?1, 'VS Code Plugin Session', 14, '2026-10-10 03:00:00Z', '[]', ?2, '2026-10-10 03:00:00Z', ?3)",
            params![conv_vscode, proj_uuid, blob_mac],
        )
        .unwrap();

        let mut pb = ProtobufSummariesMap::new();
        pb.upsert(conv_vscode.to_string(), blob_mac);
        pb.save_to_path(&mac_agy20.join("agyhub_summaries_proto.pb"))
            .unwrap();

        create_conversation_db(
            &mac_agy20.join(format!("conversations/{}.db", conv_vscode)),
            &mac_proj_uri,
        );
    }

    // 3. Create legacy gemini-cli state on Remote to pull into Mac
    {
        let r_projects_json = serde_json::json!({
            "projects": {
                remote_proj.to_string_lossy().to_string(): "vscode-app-slug"
            }
        });
        fs::write(
            remote_gemini.join("projects.json"),
            serde_json::to_string_pretty(&r_projects_json).unwrap(),
        )
        .unwrap();
        let r_hist = remote_gemini.join("history/vscode-app-slug");
        fs::create_dir_all(&r_hist).unwrap();
        fs::write(
            r_hist.join(".project_root"),
            remote_proj.to_string_lossy().as_bytes(),
        )
        .unwrap();
    }

    let config = EngineConfig {
        mode: SyncMode::Sync,
        project_path: mac_proj.clone(),
        local_home: mac_home.clone(),
        local_gemini_dir: mac_gemini.clone(),
        selected_harnesses: vec![
            HarnessKind::Agy20,
            HarnessKind::AgyIdePlugins,
            HarnessKind::GeminiCliLegacy,
        ],
        endpoint: Some(RemoteEndpoint::LocalRoot {
            root_home: Path::new(&remote_home).to_path_buf(),
        }),
        no_remap: false,
        deep_remap: false,
        dry_run: false,
        delete: false,
        include_build_dirs: false,
        extra_excludes: vec![],
    };

    let report = run_engine(&config).unwrap();
    assert!(report.pushed_cids.contains(conv_vscode));

    // Verify Agy 2.0 conversation_summaries.db and agyhub_summaries_proto.pb on Remote
    let r_conn = Connection::open(remote_agy20.join("conversation_summaries.db")).unwrap();
    let r_steps: i64 = r_conn
        .query_row(
            "SELECT step_count FROM conversation_summaries WHERE conversation_id = ?1",
            params![conv_vscode],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(r_steps, 14);
    assert!(remote_agy20.join("agyhub_summaries_proto.pb").exists());

    // Verify config/projects/<proj_uuid>.json was synced and remapped on Remote
    let r_cfg_content =
        fs::read_to_string(remote_gemini.join(format!("config/projects/{}.json", proj_uuid)))
            .unwrap();
    assert!(r_cfg_content.contains(&remote_home));
    assert!(!r_cfg_content.contains(&mac_home));

    // Verify legacy gemini-cli history and projects.json were pulled to Mac and remapped
    let m_pr = fs::read_to_string(mac_gemini.join("history/vscode-app-slug/.project_root")).unwrap();
    assert!(m_pr.contains(&mac_home));
    assert!(!m_pr.contains(&remote_home));
}
