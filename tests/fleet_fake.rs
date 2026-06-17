use std::{
    collections::HashMap,
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message as WsMessage,
        client::IntoClientRequest,
        http::{HeaderValue, header::COOKIE},
    },
};

const MOM_BIN: &str = env!("CARGO_BIN_EXE_mom");
const WORKER_TOKEN: &str = "test-worker-token";
static FLEET_TEST_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
static ADMIN_COOKIES: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

fn test_capacity() -> Value {
    json!({
        "cpus": 8,
        "memory_mib": 32768,
        "max_active_workspaces": 24,
        "disk_reserve_mib": 1024
    })
}

async fn fleet_test_guard() -> OwnedSemaphorePermit {
    FLEET_TEST_SEMAPHORE
        .get_or_init(|| Arc::new(Semaphore::new(1)))
        .clone()
        .acquire_owned()
        .await
        .expect("fleet test semaphore should not close")
}

struct ChildGuard {
    child: Child,
}

impl ChildGuard {
    fn stop(&mut self) -> Result<()> {
        let _ = self.child.kill();
        self.child.wait().context("wait for child process")?;
        Ok(())
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct TestNode {
    _state: TempDir,
    runtime_home: TempDir,
    worker_url: String,
    _process: ChildGuard,
}

struct TestFleet {
    api_state: TempDir,
    api_addr: String,
    api_url: String,
    api: ChildGuard,
}

impl Drop for TestFleet {
    fn drop(&mut self) {
        if let Some(cache) = ADMIN_COOKIES.get() {
            let _ = cache
                .lock()
                .map(|mut cookies| cookies.remove(&self.api_url));
        }
    }
}

#[tokio::test]
async fn first_signup_becomes_admin_and_users_login_with_passwords() -> Result<()> {
    let fleet = TestFleet::start().await?;
    let client = reqwest::Client::new();

    let first_response = client
        .post(format!("{}/api/auth/signup", fleet.api_url))
        .json(&json!({
            "full_name": "Admin User",
            "email": "admin@example.com",
            "password": "correct horse battery staple"
        }))
        .send()
        .await?
        .error_for_status()?;
    let admin_cookie = session_cookie_from_response(&first_response)?;
    let first = first_response.json::<Value>().await?;
    assert_eq!(first["user"]["role"], "admin");
    assert!(first["user"].get("code").is_none());

    let wrong_password = client
        .post(format!("{}/api/auth/login", fleet.api_url))
        .json(&json!({
            "email": "admin@example.com",
            "password": "wrong password"
        }))
        .send()
        .await?;
    assert_eq!(wrong_password.status(), StatusCode::UNAUTHORIZED);

    client
        .post(format!("{}/api/auth/login", fleet.api_url))
        .json(&json!({
            "email": "admin@example.com",
            "password": "correct horse battery staple"
        }))
        .send()
        .await?
        .error_for_status()?;

    let invite = client
        .post(format!("{}/api/admin/invites", fleet.api_url))
        .header(reqwest::header::COOKIE, admin_cookie)
        .json(&json!({ "label": "Test invite" }))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    let invite_code = invite["code"]
        .as_str()
        .ok_or_else(|| anyhow!("invite response did not return code"))?;
    assert_eq!(invite_code.len(), 8);

    let participant = client
        .post(format!("{}/api/auth/signup", fleet.api_url))
        .json(&json!({
            "full_name": "Participant User",
            "email": "participant@example.com",
            "code": invite_code,
            "password": "participant password"
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    assert_eq!(participant["user"]["role"], "user");
    assert!(participant["user"].get("code").is_none());

    let invite_code_as_password = client
        .post(format!("{}/api/auth/login", fleet.api_url))
        .json(&json!({
            "email": "participant@example.com",
            "password": invite_code
        }))
        .send()
        .await?;
    assert_eq!(invite_code_as_password.status(), StatusCode::UNAUTHORIZED);

    client
        .post(format!("{}/api/auth/login", fleet.api_url))
        .json(&json!({
            "email": "participant@example.com",
            "password": "participant password"
        }))
        .send()
        .await?
        .error_for_status()?;

    Ok(())
}

#[tokio::test]
async fn cli_generates_user_and_admin_invites_for_signup() -> Result<()> {
    let fleet = TestFleet::start().await?;
    let client = reqwest::Client::new();

    client
        .post(format!("{}/api/auth/signup", fleet.api_url))
        .json(&json!({
            "full_name": "Bootstrap Admin",
            "email": "bootstrap-admin@example.com",
            "password": "correct horse battery staple"
        }))
        .send()
        .await?
        .error_for_status()?;

    let user_code = cli_invite_code(fleet.api_state.path(), "user", "CLI user invite")?;
    let user = client
        .post(format!("{}/api/auth/signup", fleet.api_url))
        .json(&json!({
            "full_name": "CLI User",
            "email": "cli-user@example.com",
            "code": user_code,
            "password": "participant password"
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    assert_eq!(user["user"]["role"], "user");

    let admin_code = cli_invite_code(fleet.api_state.path(), "admin", "CLI admin invite")?;
    let admin = client
        .post(format!("{}/api/auth/signup", fleet.api_url))
        .json(&json!({
            "full_name": "CLI Admin",
            "email": "cli-admin@example.com",
            "code": admin_code,
            "password": "participant password"
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    assert_eq!(admin["user"]["role"], "admin");

    Ok(())
}

#[test]
fn current_schema_without_vm_version_is_repaired() -> Result<()> {
    let api_state = tempfile::tempdir()?;
    let db = Connection::open(api_state.path().join("fleet.db"))?;
    db.execute_batch(
        r#"
CREATE TABLE schema_version (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE workspaces (
    name TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL UNIQUE,
    slug TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    user_id TEXT NOT NULL,
    owner_user_id INTEGER,
    agent_name TEXT,
    vm_name TEXT NOT NULL UNIQUE,
    workspace_dir_name TEXT NOT NULL UNIQUE,
    node_id TEXT,
    desired_state TEXT NOT NULL,
    cpus INTEGER NOT NULL,
    memory_mib INTEGER NOT NULL,
    workspace_quota_mib INTEGER NOT NULL,
    status TEXT NOT NULL,
    idle_timeout_secs INTEGER NOT NULL,
    backup_interval_secs INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    last_backup_at INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE workspace_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_name TEXT NOT NULL,
    node_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    status TEXT NOT NULL,
    message TEXT NOT NULL,
    metadata_json TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

INSERT INTO schema_version (id, version, updated_at)
VALUES (1, 5, 1000);

INSERT INTO workspaces (
    name, workspace_id, slug, display_name, user_id, owner_user_id, agent_name,
    vm_name, workspace_dir_name, node_id, desired_state, cpus, memory_mib,
    workspace_quota_mib, status, idle_timeout_secs, backup_interval_secs,
    last_used_at, last_backup_at, created_at, updated_at
) VALUES (
    'legacy-workspace', 'ws_legacy_workspace', 'legacy-workspace',
    'Legacy Workspace', 'legacy-user', NULL, 'codex',
    'mom-legacy-workspace', 'mom-legacy-workspace-dir', 'mom-1', 'running',
    1, 2048, 10240, 'stopped', 1800, 0, 1000, NULL, 1000, 1000
);

INSERT INTO workspace_events (
    workspace_name, node_id, event_type, status, message, metadata_json, created_at
) VALUES (
    'legacy-workspace', 'mom-1', 'workspace_created', 'succeeded',
    'legacy workspace created', '{"version":"0.1.0-legacy"}', 1001
);
"#,
    )?;
    drop(db);

    run_mom(api_state.path(), &["node", "list"])?;

    let db = Connection::open(api_state.path().join("fleet.db"))?;
    let has_vm_version: i64 = db.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('workspaces') WHERE name = 'vm_version'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(has_vm_version, 1);
    let vm_version: String = db.query_row(
        "SELECT vm_version FROM workspaces WHERE name = 'legacy-workspace'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(vm_version, "0.1.0-legacy");

    Ok(())
}

#[tokio::test]
async fn worker_retries_registration_until_api_is_ready() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let api_state = tempfile::tempdir()?;
    let api_addr = free_addr()?;
    let api_url = format!("http://{api_addr}");
    let node = spawn_worker("retry-node", &api_url)?;

    wait_until("worker HTTP starts before API", || async {
        reqwest::get(format!("{}/worker/health", node.worker_url))
            .await
            .ok()
            .filter(|response| response.status().is_success())
            .is_some()
    })
    .await?;

    let _api = spawn_api(api_state.path(), &api_addr, &[])?;
    wait_ready(&api_url).await?;
    wait_for_node(api_state.path(), "retry-node").await?;

    Ok(())
}

#[tokio::test]
async fn admin_infra_overview_returns_fleet_snapshot() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let _node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let create = create_workspace(&fleet.api_url, "infra-check", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &create, "succeeded").await?;

    let cookie = admin_cookie(&fleet.api_url).await?;
    let overview = reqwest::Client::new()
        .get(format!("{}/api/admin/infra", fleet.api_url))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;

    assert_eq!(overview["app_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        overview.pointer("/nodes/0/node_id").and_then(Value::as_str),
        Some("node-a")
    );
    let workspace_counts = overview
        .get("workspace_status_counts")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("overview missing workspace counts: {overview}"))?;
    let workspace_total = workspace_counts
        .values()
        .filter_map(Value::as_i64)
        .sum::<i64>();
    assert_eq!(workspace_total, 1);
    assert!(
        overview
            .get("jobs")
            .and_then(Value::as_array)
            .is_some_and(|jobs| jobs.iter().any(|job| {
                job.get("workspace_name").and_then(Value::as_str) == Some("infra-check")
                    && job.get("kind").and_then(Value::as_str) == Some("create")
            })),
        "overview should include recent create job: {overview}"
    );

    Ok(())
}

#[tokio::test]
async fn fake_worker_start_stop_backup_jobs_update_central_state() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let create = create_workspace(&fleet.api_url, "alice", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &create, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "alice", "running").await?;
    assert_eq!(
        workspace(&fleet.api_url, "alice")
            .await?
            .get("vm_version")
            .and_then(Value::as_str),
        Some(env!("CARGO_PKG_VERSION"))
    );
    let cookie = admin_cookie(&fleet.api_url).await?;

    reqwest::Client::new()
        .post(format!("{}/api/workspaces/alice/stop", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?
        .error_for_status()?;
    wait_for_workspace_status(&fleet.api_url, "alice", "stopped").await?;
    assert_eq!(
        std::fs::read_to_string(node.runtime_home.path().join("fake/alice/state"))?,
        "stopped"
    );

    let start = create_job(&fleet.api_url, "alice", "start").await?;
    wait_for_job_status(&fleet.api_url, &start, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "alice", "running").await?;
    assert_eq!(
        std::fs::read_to_string(node.runtime_home.path().join("fake/alice/state"))?,
        "running"
    );

    let pause = create_job(&fleet.api_url, "alice", "pause").await?;
    wait_for_job_status(&fleet.api_url, &pause, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "alice", "paused").await?;
    assert_eq!(
        std::fs::read_to_string(node.runtime_home.path().join("fake/alice/state"))?,
        "paused"
    );

    let suspend = create_job(&fleet.api_url, "alice", "suspend").await?;
    wait_for_job_status(&fleet.api_url, &suspend, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "alice", "suspended").await?;
    assert_eq!(
        std::fs::read_to_string(node.runtime_home.path().join("fake/alice/state"))?,
        "suspended"
    );

    let resume = create_job(&fleet.api_url, "alice", "resume").await?;
    wait_for_job_status(&fleet.api_url, &resume, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "alice", "running").await?;
    assert_eq!(
        std::fs::read_to_string(node.runtime_home.path().join("fake/alice/state"))?,
        "running"
    );

    reqwest::Client::new()
        .post(format!("{}/api/workspaces/alice/pause", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?
        .error_for_status()?;
    wait_for_workspace_status(&fleet.api_url, "alice", "paused").await?;

    reqwest::Client::new()
        .post(format!("{}/api/workspaces/alice/resume", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?
        .error_for_status()?;
    wait_for_workspace_status(&fleet.api_url, "alice", "running").await?;

    reqwest::Client::new()
        .post(format!("{}/api/workspaces/alice/suspend", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?
        .error_for_status()?;
    wait_for_workspace_status(&fleet.api_url, "alice", "suspended").await?;

    reqwest::Client::new()
        .post(format!("{}/api/workspaces/alice/upgrade", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?
        .error_for_status()?;
    wait_for_workspace_status(&fleet.api_url, "alice", "stopped").await?;
    assert_eq!(
        workspace(&fleet.api_url, "alice")
            .await?
            .get("vm_version")
            .and_then(Value::as_str),
        Some(env!("CARGO_PKG_VERSION"))
    );

    let backup = create_job(&fleet.api_url, "alice", "backup").await?;
    wait_for_job_status(&fleet.api_url, &backup, "succeeded").await?;
    wait_for_backup_count(fleet.api_state.path(), "alice", 1).await?;

    Ok(())
}

#[tokio::test]
async fn restore_job_payload_is_canonicalized_from_backup_catalog() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let create = create_workspace(&fleet.api_url, "restore-canonical", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &create, "succeeded").await?;

    let backup = create_job(&fleet.api_url, "restore-canonical", "backup").await?;
    wait_for_job_status(&fleet.api_url, &backup, "succeeded").await?;
    let (backup_id, backup_location) =
        latest_backup_record(fleet.api_state.path(), "restore-canonical")?;

    let response = create_job_value_with_payload(
        &fleet.api_url,
        "restore-canonical",
        "restore",
        json!({
            "backup_id": backup_id,
            "backup_location": "fake-restic#tampered",
            "backup_workspace_name": "other-workspace",
            "desired_state": "stopped"
        }),
    )
    .await?;
    let job_id = response
        .pointer("/job/id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("restore job response missing id: {response}"))?;
    let payload_json = response
        .pointer("/job/payload_json")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("restore job response missing payload_json: {response}"))?;
    let payload: Value = serde_json::from_str(payload_json)?;
    assert_eq!(payload["backup_location"], backup_location);
    assert_eq!(payload["backup_workspace_name"], "restore-canonical");

    wait_for_job_status(&fleet.api_url, job_id, "succeeded").await?;
    assert_eq!(
        std::fs::read_to_string(
            node.runtime_home
                .path()
                .join("fake/restore-canonical/restored-from")
        )?,
        backup_location
    );
    assert_eq!(
        std::fs::read_to_string(
            node.runtime_home
                .path()
                .join("fake/restore-canonical/state")
        )?,
        "stopped"
    );

    Ok(())
}

#[tokio::test]
async fn browser_workspace_routes_require_session_cookie() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let client = reqwest::Client::new();

    let list = client
        .get(format!("{}/api/workspaces", fleet.api_url))
        .send()
        .await?;
    assert_eq!(list.status(), StatusCode::UNAUTHORIZED);

    let create = client
        .post(format!("{}/api/workspaces", fleet.api_url))
        .json(&json!({ "name": "unauthenticated" }))
        .send()
        .await?;
    assert_eq!(create.status(), StatusCode::UNAUTHORIZED);

    let cookie = admin_cookie(&fleet.api_url).await?;
    client
        .get(format!("{}/api/workspaces", fleet.api_url))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await?
        .error_for_status()?;

    Ok(())
}

#[tokio::test]
async fn workspace_backup_cli_queues_remote_worker_job_when_workspace_dir_is_not_local()
-> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let _node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let create = create_workspace(&fleet.api_url, "remote-backup", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &create, "succeeded").await?;

    let local_runtime = tempfile::tempdir()?;
    let local_workspaces = tempfile::tempdir()?;
    let stale_dir = local_workspaces.path().join("mom-remote-backup-workspace");
    fs::create_dir_all(&stale_dir)?;
    fs::write(stale_dir.join("stale-local-marker"), b"wrong copy")?;
    run_mom_with_env(
        fleet.api_state.path(),
        &["workspace", "backup", "remote-backup", "--leave-stopped"],
        &[
            (
                "MOM_MICROVM_STATE_DIR",
                local_runtime.path().to_str().unwrap_or(""),
            ),
            (
                "MOM_MICROVM_WORKSPACE_DIR",
                local_workspaces.path().to_str().unwrap_or(""),
            ),
            ("MOM_NODE_ID", "control"),
        ],
    )?;
    wait_for_backup_count(fleet.api_state.path(), "remote-backup", 1).await?;

    Ok(())
}

#[tokio::test]
async fn workspace_backup_restore_cli_rejects_stale_remote_worker() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    insert_node(fleet.api_state.path(), "stale-node", now_epoch()? - 3600)?;
    insert_workspace(fleet.api_state.path(), "stale-remote", "stale-node")?;
    insert_backup_record(fleet.api_state.path(), "stale-remote", "stale-node")?;

    let backup_status = run_mom_status(
        fleet.api_state.path(),
        &["workspace", "backup", "stale-remote", "--leave-stopped"],
    )?;
    assert!(
        !backup_status.success(),
        "CLI backup should fail before queueing work to a stale node"
    );

    let restore_status = run_mom_status(
        fleet.api_state.path(),
        &["workspace", "restore", "stale-remote"],
    )?;
    assert!(
        !restore_status.success(),
        "CLI restore should fail before queueing work to a stale node"
    );
    assert_eq!(
        queued_job_count(fleet.api_state.path(), "stale-remote")?,
        0,
        "stale-node CLI paths must not leave queued work behind"
    );

    Ok(())
}

#[tokio::test]
async fn workspace_lifecycle_cli_rejects_stale_remote_worker_before_queueing() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    insert_node(fleet.api_state.path(), "stale-node", now_epoch()? - 3600)?;
    insert_workspace_with_state(
        fleet.api_state.path(),
        "stale-lifecycle",
        "stale-node",
        "stopped",
        "stopped",
    )?;

    let start_status = run_mom_status(
        fleet.api_state.path(),
        &["workspace", "start", "stale-lifecycle"],
    )?;
    assert!(
        !start_status.success(),
        "CLI start should fail before queueing work to a stale node"
    );
    assert_eq!(
        workspace_desired_state(fleet.api_state.path(), "stale-lifecycle")?,
        "stopped",
        "failed remote start must not change desired state"
    );

    let stop_status = run_mom_status(
        fleet.api_state.path(),
        &["workspace", "stop", "stale-lifecycle"],
    )?;
    assert!(
        !stop_status.success(),
        "CLI stop should fail before queueing work to a stale node"
    );

    let remove_status = run_mom_status(
        fleet.api_state.path(),
        &["workspace", "rm", "stale-lifecycle", "--force"],
    )?;
    assert!(
        !remove_status.success(),
        "CLI remove should fail before queueing work to a stale node"
    );
    assert_eq!(
        queued_job_count(fleet.api_state.path(), "stale-lifecycle")?,
        0,
        "stale-node CLI lifecycle paths must not leave queued work behind"
    );

    Ok(())
}

#[tokio::test]
async fn workspace_inspect_labels_remote_runtime_as_not_checked_locally() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let _node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let create = create_workspace(&fleet.api_url, "remote-inspect", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &create, "succeeded").await?;

    let local_runtime = tempfile::tempdir()?;
    let output = run_mom_output_with_env(
        fleet.api_state.path(),
        &["workspace", "inspect", "remote-inspect"],
        &[
            ("MOM_NODE_ID", "control"),
            (
                "MOM_MICROVM_STATE_DIR",
                local_runtime.path().to_str().unwrap_or(""),
            ),
        ],
    )?;
    assert!(
        output.contains("Inspecting node: control"),
        "inspect output should identify the local inspecting node: {output}"
    );
    assert!(
        output.contains("VM status: not checked locally; assigned to node-a"),
        "inspect output should not report remote VM as missing: {output}"
    );

    Ok(())
}

#[tokio::test]
async fn fake_workers_create_assigned_workspace_without_shared_sqlite() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;

    let node_a = spawn_worker("node-a", &fleet.api_url)?;
    let node_b = spawn_worker("node-b", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;
    wait_for_node(fleet.api_state.path(), "node-b").await?;

    let job_id = create_workspace(&fleet.api_url, "alice", "node-b", 0).await?;
    wait_for_job_status(&fleet.api_url, &job_id, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "alice", "running").await?;

    assert!(
        !node_a.runtime_home.path().join("fake/alice").exists(),
        "node-a should not create node-b's workspace"
    );
    assert!(
        node_b.runtime_home.path().join("fake/alice").exists(),
        "node-b should create its assigned workspace"
    );
    assert_no_local_fleet_db(&node_a)?;
    assert_no_local_fleet_db(&node_b)?;

    Ok(())
}

#[tokio::test]
async fn ui_create_selects_registered_worker_node() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;
    let cookie = admin_cookie(&fleet.api_url).await?;

    reqwest::Client::new()
        .post(format!("{}/api/workspaces", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&json!({ "name": "ui-created" }))
        .send()
        .await?
        .error_for_status()?;

    wait_for_workspace_node(&fleet.api_url, "ui-created", "node-a").await?;
    wait_for_workspace_status(&fleet.api_url, "ui-created", "running").await?;
    assert!(
        node.runtime_home.path().join("fake/ui-created").exists(),
        "UI-created workspace should be created on the registered worker"
    );

    Ok(())
}

#[tokio::test]
async fn unpinned_jobs_are_pinned_to_workspace_owner() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let node_a = spawn_worker("node-a", &fleet.api_url)?;
    let node_b = spawn_worker("node-b", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;
    wait_for_node(fleet.api_state.path(), "node-b").await?;

    let create = create_workspace(&fleet.api_url, "owner", "node-b", 0).await?;
    wait_for_job_status(&fleet.api_url, &create, "succeeded").await?;

    let job = create_job_value(&fleet.api_url, "owner", "stop").await?;
    let job_id = job
        .pointer("/job/id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("create job response missing job id: {job}"))?;
    assert_eq!(
        job.pointer("/job/node_id").and_then(Value::as_str),
        Some("node-b")
    );
    wait_for_job_status(&fleet.api_url, job_id, "succeeded").await?;
    assert!(
        !node_a.runtime_home.path().join("fake/owner").exists(),
        "node-a should not claim node-b's unpinned job"
    );
    assert_eq!(
        std::fs::read_to_string(node_b.runtime_home.path().join("fake/owner/state"))?,
        "stopped"
    );

    Ok(())
}

#[tokio::test]
async fn worker_state_updates_require_assigned_node() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let _node_a = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let client = reqwest::Client::new();
    create_workspace(&fleet.api_url, "owned", "node-a", 0).await?;
    wait_for_workspace_node(&fleet.api_url, "owned", "node-a").await?;

    let response = client
        .post(format!("{}/worker/workspaces/owned/state", fleet.api_url))
        .bearer_auth(WORKER_TOKEN)
        .json(&json!({
            "node_id": "node-b",
            "status": "running"
        }))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    Ok(())
}

#[tokio::test]
async fn worker_events_requires_worker_token() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;

    let response = reqwest::Client::new()
        .get(format!("{}/worker/events?node_id=node-a", fleet.api_url))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    Ok(())
}

#[tokio::test]
async fn worker_workspace_list_requires_token_and_is_node_scoped() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let _node_a = spawn_worker("node-a", &fleet.api_url)?;
    let _node_b = spawn_worker("node-b", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;
    wait_for_node(fleet.api_state.path(), "node-b").await?;

    let create_a = create_workspace(&fleet.api_url, "owned-a", "node-a", 0).await?;
    let create_b = create_workspace(&fleet.api_url, "owned-b", "node-b", 0).await?;
    wait_for_job_status(&fleet.api_url, &create_a, "succeeded").await?;
    wait_for_job_status(&fleet.api_url, &create_b, "succeeded").await?;

    let client = reqwest::Client::new();
    let unauthorized = client
        .get(format!(
            "{}/worker/workspaces?node_id=node-a",
            fleet.api_url
        ))
        .send()
        .await?;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let workspaces = client
        .get(format!(
            "{}/worker/workspaces?node_id=node-a",
            fleet.api_url
        ))
        .bearer_auth(WORKER_TOKEN)
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<Value>>()
        .await?;
    let names = workspaces
        .iter()
        .filter_map(|workspace| workspace.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["owned-a"]);

    Ok(())
}

#[tokio::test]
async fn recover_host_reassigns_and_restores_latest_backup_on_target_node() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let node_a = spawn_worker("node-a", &fleet.api_url)?;
    let node_b = spawn_worker("node-b", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;
    wait_for_node(fleet.api_state.path(), "node-b").await?;

    let create = create_workspace(&fleet.api_url, "recover-me", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &create, "succeeded").await?;
    let backup = create_job(&fleet.api_url, "recover-me", "backup").await?;
    wait_for_job_status(&fleet.api_url, &backup, "succeeded").await?;
    wait_for_backup_count(fleet.api_state.path(), "recover-me", 1).await?;
    let stale_job = insert_running_job(fleet.api_state.path(), "recover-me", "node-a")?;

    run_recover_host(fleet.api_state.path(), "node-a", "node-b")?;

    assert_eq!(job_status(&fleet.api_url, &stale_job).await?, "failed");
    wait_for_workspace_node(&fleet.api_url, "recover-me", "node-b").await?;
    wait_for_workspace_status(&fleet.api_url, "recover-me", "running").await?;
    assert!(
        node_a.runtime_home.path().join("fake/recover-me").exists(),
        "source worker's local fake workspace_dir remains as lost-host residue"
    );
    let restored_from = node_b
        .runtime_home
        .path()
        .join("fake/recover-me/restored-from");
    wait_until("workspace restored on node-b", || {
        let restored_from = restored_from.clone();
        async move { restored_from.exists() }
    })
    .await?;

    Ok(())
}

#[tokio::test]
async fn stopped_recovery_restores_on_full_target_without_active_capacity() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let target = spawn_worker_with_envs(
        "target-node",
        &fleet.api_url,
        &[("MOM_CAPACITY_ACTIVE_WORKSPACES", "1")],
    )?;
    wait_for_node(fleet.api_state.path(), "target-node").await?;

    let occupying = create_workspace(&fleet.api_url, "target-running", "target-node", 0).await?;
    wait_for_job_status(&fleet.api_url, &occupying, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "target-running", "running").await?;

    let now = now_epoch()?;
    insert_node(fleet.api_state.path(), "source-node", now)?;
    insert_workspace_with_state(
        fleet.api_state.path(),
        "recover-stopped",
        "source-node",
        "stopped",
        "stopped",
    )?;
    insert_backup_record(fleet.api_state.path(), "recover-stopped", "source-node")?;

    run_recover_host(fleet.api_state.path(), "source-node", "target-node")?;

    wait_for_workspace_node(&fleet.api_url, "recover-stopped", "target-node").await?;
    wait_for_workspace_status(&fleet.api_url, "recover-stopped", "stopped").await?;
    let restored_from = target
        .runtime_home
        .path()
        .join("fake/recover-stopped/restored-from");
    wait_until("stopped workspace restored on full target", || {
        let restored_from = restored_from.clone();
        async move { restored_from.exists() }
    })
    .await?;

    Ok(())
}

#[tokio::test]
async fn recover_host_rejects_batches_that_exceed_target_capacity() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let now = now_epoch()?;
    insert_node(fleet.api_state.path(), "source-node", now)?;
    insert_node_with_capacity(fleet.api_state.path(), "target-node", now, 1)?;
    insert_workspace(fleet.api_state.path(), "recover-a", "source-node")?;
    insert_workspace(fleet.api_state.path(), "recover-b", "source-node")?;
    insert_backup_record(fleet.api_state.path(), "recover-a", "source-node")?;
    insert_backup_record(fleet.api_state.path(), "recover-b", "source-node")?;

    let status = run_mom_status(
        fleet.api_state.path(),
        &[
            "fleet",
            "recover-host",
            "--from",
            "source-node",
            "--to",
            "target-node",
        ],
    )?;
    assert!(
        !status.success(),
        "recovery should reject a batch that exceeds target active capacity"
    );
    assert_eq!(
        workspace_count_for_node(fleet.api_state.path(), "target-node")?,
        0,
        "failed recovery must not move any workspace to the target"
    );
    assert_eq!(queued_job_count(fleet.api_state.path(), "recover-a")?, 0);
    assert_eq!(queued_job_count(fleet.api_state.path(), "recover-b")?, 0);
    assert_eq!(
        node_status(fleet.api_state.path(), "source-node")?,
        "ready",
        "failed recovery must not mark the source offline"
    );

    Ok(())
}

#[tokio::test]
async fn reconcile_removed_workspace_preserves_directory_cleanup_intent() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    insert_workspace_with_state(
        fleet.api_state.path(),
        "remove-dir",
        "node-a",
        "removed",
        "removing-dir",
    )?;
    let source_dir = node
        .runtime_home
        .path()
        .join("workspaces/mom-remove-dir-workspace");
    fs::create_dir_all(&source_dir)?;
    fs::write(source_dir.join("marker"), b"delete me")?;

    wait_for_workspace_status(&fleet.api_url, "remove-dir", "removed").await?;
    wait_until("interrupted remove workspace dir cleaned up", || {
        let source_dir = source_dir.clone();
        async move { !source_dir.exists() }
    })
    .await?;

    Ok(())
}

#[tokio::test]
async fn worker_service_open_rejects_spoofed_vm_identity() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let job_id = create_workspace(&fleet.api_url, "guard", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &job_id, "succeeded").await?;

    let response = reqwest::Client::new()
        .post(format!("{}/worker/services/hermes/open", node.worker_url))
        .bearer_auth(WORKER_TOKEN)
        .json(&json!({
            "workspace_name": "guard",
            "vm_name": "mom-other"
        }))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        !node
            .runtime_home
            .path()
            .join("fake/guard/service-hermes")
            .exists(),
        "worker should not open a service for a mismatched vm"
    );

    Ok(())
}

#[tokio::test]
async fn create_selects_fresh_worker_over_stale_node() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    insert_node(fleet.api_state.path(), "stale-node", now_epoch()? - 3600)?;
    let node = spawn_worker("fresh-node", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "fresh-node").await?;
    let cookie = admin_cookie(&fleet.api_url).await?;

    reqwest::Client::new()
        .post(format!("{}/api/workspaces", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&json!({ "name": "fresh" }))
        .send()
        .await?
        .error_for_status()?;

    wait_for_workspace_node(&fleet.api_url, "fresh", "fresh-node").await?;
    wait_for_workspace_status(&fleet.api_url, "fresh", "running").await?;
    assert!(
        node.runtime_home.path().join("fake/fresh").exists(),
        "workspace should be placed on the fresh worker"
    );

    Ok(())
}

#[tokio::test]
async fn concurrent_create_reserves_the_last_node_slot() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    insert_node_with_capacity(fleet.api_state.path(), "node-a", now_epoch()?, 1)?;
    let cookie = admin_cookie(&fleet.api_url).await?;
    let client = reqwest::Client::new();

    let create_a = client
        .post(format!("{}/api/workspaces", fleet.api_url))
        .header(reqwest::header::COOKIE, cookie.clone())
        .json(&json!({
            "name": "slot-a",
            "node_id": "node-a"
        }))
        .send();
    let create_b = client
        .post(format!("{}/api/workspaces", fleet.api_url))
        .header(reqwest::header::COOKIE, cookie)
        .json(&json!({
            "name": "slot-b",
            "node_id": "node-a"
        }))
        .send();

    let (response_a, response_b) = tokio::join!(create_a, create_b);
    let statuses = [response_a?.status(), response_b?.status()];
    assert_eq!(
        statuses.iter().filter(|status| status.is_success()).count(),
        1,
        "only one concurrent create should reserve the final active-workspace slot"
    );
    assert_eq!(
        workspace_count_for_node(fleet.api_state.path(), "node-a")?,
        1
    );

    Ok(())
}

#[tokio::test]
async fn duplicate_create_does_not_move_existing_workspace() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let node_a = spawn_worker("node-a", &fleet.api_url)?;
    let node_b = spawn_worker("node-b", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;
    wait_for_node(fleet.api_state.path(), "node-b").await?;

    let create = create_workspace(&fleet.api_url, "dupe", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &create, "succeeded").await?;
    let cookie = admin_cookie(&fleet.api_url).await?;
    let response = reqwest::Client::new()
        .post(format!("{}/api/workspaces", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&json!({
            "name": "dupe",
            "node_id": "node-b"
        }))
        .send()
        .await?;
    assert!(!response.status().is_success());
    wait_for_workspace_node(&fleet.api_url, "dupe", "node-a").await?;
    assert!(node_a.runtime_home.path().join("fake/dupe").exists());
    assert!(
        !node_b.runtime_home.path().join("fake/dupe").exists(),
        "duplicate create should not move the workspace to node-b"
    );

    Ok(())
}

#[tokio::test]
async fn explicit_create_rejects_full_node() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    insert_node_with_capacity(fleet.api_state.path(), "full-node", now_epoch()?, 1)?;
    insert_workspace(fleet.api_state.path(), "existing", "full-node")?;
    let cookie = admin_cookie(&fleet.api_url).await?;

    let response = reqwest::Client::new()
        .post(format!("{}/api/workspaces", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&json!({
            "name": "overflow",
            "node_id": "full-node"
        }))
        .send()
        .await?;
    assert!(!response.status().is_success());

    Ok(())
}

#[tokio::test]
async fn explicit_create_rejects_node_without_memory_capacity() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    insert_node_with_resources(
        fleet.api_state.path(),
        "small-node",
        now_epoch()?,
        8,
        1024,
        48,
    )?;
    let cookie = admin_cookie(&fleet.api_url).await?;

    let response = reqwest::Client::new()
        .post(format!("{}/api/workspaces", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&json!({
            "name": "too-large",
            "node_id": "small-node",
            "memory": 2048
        }))
        .send()
        .await?;
    assert!(!response.status().is_success());

    Ok(())
}

#[tokio::test]
async fn idle_stopped_workspace_does_not_count_against_capacity() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    insert_node_with_capacity(fleet.api_state.path(), "node-a", now_epoch()?, 1)?;
    insert_workspace_with_state(
        fleet.api_state.path(),
        "idle",
        "node-a",
        "stopped",
        "idle-stopped",
    )?;
    let cookie = admin_cookie(&fleet.api_url).await?;

    let response = reqwest::Client::new()
        .post(format!("{}/api/workspaces", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&json!({
            "name": "replacement",
            "node_id": "node-a"
        }))
        .send()
        .await?;
    assert!(
        response.status().is_success(),
        "idle-stopped workspace should not consume active placement capacity: {}",
        response.text().await.unwrap_or_default()
    );

    Ok(())
}

#[tokio::test]
async fn worker_reconcile_ignores_unassigned_and_idle_stopped_workspaces() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    insert_unassigned_workspace(fleet.api_state.path(), "legacy")?;
    insert_workspace_with_state(
        fleet.api_state.path(),
        "cold",
        "node-a",
        "running",
        "idle-stopped",
    )?;

    let node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        !node.runtime_home.path().join("fake/legacy").exists(),
        "worker should not reconcile unassigned legacy workspaces"
    );
    assert!(
        !node.runtime_home.path().join("fake/cold").exists(),
        "idle-stopped workspace should remain cold until a job wakes it"
    );

    let start = create_job(&fleet.api_url, "cold", "start").await?;
    wait_for_job_status(&fleet.api_url, &start, "succeeded").await?;
    assert_eq!(
        std::fs::read_to_string(node.runtime_home.path().join("fake/cold/state"))?,
        "running"
    );

    Ok(())
}

#[tokio::test]
async fn offline_node_is_not_reenabled_by_stale_heartbeat() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let client = reqwest::Client::new();
    insert_node_with_status(
        fleet.api_state.path(),
        "lost-node",
        now_epoch()?,
        24,
        "offline",
    )?;

    client
        .post(format!("{}/worker/register", fleet.api_url))
        .bearer_auth(WORKER_TOKEN)
        .json(&json!({
            "node_id": "lost-node",
            "capacity": {
                "cpus": 8,
                "memory_mib": 32768,
                "max_active_workspaces": 24,
                "disk_reserve_mib": 1024
            },
            "worker_url": "http://100.64.0.42:9090"
        }))
        .send()
        .await?
        .error_for_status()?;

    assert_eq!(node_status(fleet.api_state.path(), "lost-node")?, "offline");
    let cookie = admin_cookie(&fleet.api_url).await?;
    let response = client
        .post(format!("{}/api/workspaces", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&json!({
            "name": "should-not-place",
            "node_id": "lost-node"
        }))
        .send()
        .await?;
    assert!(!response.status().is_success());

    let response = client
        .post(format!("{}/worker/claim", fleet.api_url))
        .bearer_auth(WORKER_TOKEN)
        .json(&json!({
            "node_id": "lost-node",
            "capacity": {
                "cpus": 8,
                "memory_mib": 32768,
                "max_active_workspaces": 24,
                "disk_reserve_mib": 1024
            },
            "pressure": {
                "managed_vms": 0,
                "running_vms": 0,
                "active_workspaces": 0,
                "allocated_memory_mib": 0,
                "disk_available_mib": 65536,
                "disk_ok": true,
                "capacity_ok": true
            },
            "worker_url": "http://100.64.0.42:9090"
        }))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let response = client
        .get(format!(
            "{}/worker/workspaces?node_id=lost-node",
            fleet.api_url
        ))
        .bearer_auth(WORKER_TOKEN)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    Ok(())
}

#[tokio::test]
async fn node_lifecycle_controls_placement_and_claims() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let create = create_workspace(&fleet.api_url, "lifecycle", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &create, "succeeded").await?;

    run_mom(fleet.api_state.path(), &["node", "cordon", "node-a"])?;
    assert_eq!(node_status(fleet.api_state.path(), "node-a")?, "cordoned");
    let cookie = admin_cookie(&fleet.api_url).await?;
    let response = reqwest::Client::new()
        .post(format!("{}/api/workspaces", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&json!({
            "name": "blocked-on-cordon",
            "node_id": "node-a"
        }))
        .send()
        .await?;
    assert!(
        !response.status().is_success(),
        "cordoned node should not receive new placements"
    );

    let stop = create_job(&fleet.api_url, "lifecycle", "stop").await?;
    wait_for_job_status(&fleet.api_url, &stop, "succeeded").await?;
    assert_eq!(
        std::fs::read_to_string(node.runtime_home.path().join("fake/lifecycle/state"))?,
        "stopped",
        "cordoned node should still claim assigned workspace jobs"
    );

    run_mom(fleet.api_state.path(), &["node", "drain", "node-a"])?;
    assert_eq!(node_status(fleet.api_state.path(), "node-a")?, "draining");
    let response = reqwest::Client::new()
        .post(format!("{}/api/jobs", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&json!({
            "workspace_name": "lifecycle",
            "kind": "start"
        }))
        .send()
        .await?;
    assert!(
        !response.status().is_success(),
        "draining node should reject newly queued assigned jobs"
    );

    run_mom(fleet.api_state.path(), &["node", "uncordon", "node-a"])?;
    assert_eq!(node_status(fleet.api_state.path(), "node-a")?, "ready");
    wait_for_node_ready_fresh(fleet.api_state.path(), "node-a").await?;
    let start = create_job(&fleet.api_url, "lifecycle", "start").await?;
    wait_for_job_status(&fleet.api_url, &start, "succeeded").await?;

    Ok(())
}

#[tokio::test]
async fn worker_job_completion_is_idempotent_for_retried_terminal_results() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let client = reqwest::Client::new();
    insert_node(fleet.api_state.path(), "node-a", now_epoch()?)?;
    insert_workspace(fleet.api_state.path(), "retry-complete", "node-a")?;
    let job_id = create_job(&fleet.api_url, "retry-complete", "start").await?;

    let claimed = client
        .post(format!("{}/worker/claim", fleet.api_url))
        .bearer_auth(WORKER_TOKEN)
        .json(&json!({
            "node_id": "node-a",
            "capacity": test_capacity(),
            "pressure": {
                "managed_vms": 0,
                "running_vms": 0,
                "active_workspaces": 0,
                "allocated_memory_mib": 0,
                "disk_available_mib": 65536,
                "disk_ok": true,
                "capacity_ok": true
            },
            "worker_url": "http://100.64.0.42:9090"
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Option<Value>>()
        .await?
        .ok_or_else(|| anyhow!("worker did not claim queued job"))?;
    assert_eq!(
        claimed.pointer("/id").and_then(Value::as_str),
        Some(job_id.as_str())
    );

    let completion = json!({
        "node_id": "node-a",
        "status": "succeeded",
        "output": { "started": true }
    });
    for _ in 0..2 {
        let response = client
            .post(format!("{}/worker/jobs/{job_id}/complete", fleet.api_url))
            .bearer_auth(WORKER_TOKEN)
            .json(&completion)
            .send()
            .await?;
        assert!(
            response.status().is_success(),
            "completion retry failed with {}: {}",
            response.status(),
            response.text().await.unwrap_or_default()
        );
    }

    assert_eq!(job_status(&fleet.api_url, &job_id).await?, "succeeded");
    Ok(())
}

#[tokio::test]
async fn worker_claim_skips_workspace_with_active_job() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let client = reqwest::Client::new();
    insert_node(fleet.api_state.path(), "node-a", now_epoch()?)?;
    insert_workspace(fleet.api_state.path(), "claim-serial", "node-a")?;
    let first_job = create_job(&fleet.api_url, "claim-serial", "start").await?;
    let second_job = create_job(&fleet.api_url, "claim-serial", "stop").await?;

    let first_claim = claim_worker_job(&client, &fleet.api_url, "node-a").await?;
    assert_eq!(
        first_claim
            .as_ref()
            .and_then(|job| job.get("id"))
            .and_then(Value::as_str),
        Some(first_job.as_str())
    );

    let blocked_claim = claim_worker_job(&client, &fleet.api_url, "node-a").await?;
    assert!(
        blocked_claim.is_none(),
        "second job for same workspace must wait while first job is active"
    );

    client
        .post(format!(
            "{}/worker/jobs/{}/complete",
            fleet.api_url, first_job
        ))
        .bearer_auth(WORKER_TOKEN)
        .json(&json!({
            "node_id": "node-a",
            "status": "succeeded",
            "output": { "ok": true }
        }))
        .send()
        .await?
        .error_for_status()?;

    let second_claim = claim_worker_job(&client, &fleet.api_url, "node-a").await?;
    assert_eq!(
        second_claim
            .as_ref()
            .and_then(|job| job.get("id"))
            .and_then(Value::as_str),
        Some(second_job.as_str())
    );

    Ok(())
}

#[tokio::test]
async fn pressured_worker_still_claims_capacity_relieving_jobs() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let client = reqwest::Client::new();
    insert_node(fleet.api_state.path(), "node-a", now_epoch()?)?;
    insert_workspace_with_state(
        fleet.api_state.path(),
        "blocked-start",
        "node-a",
        "stopped",
        "stopped",
    )?;
    insert_workspace(fleet.api_state.path(), "free-capacity", "node-a")?;
    let start_job = create_job(&fleet.api_url, "blocked-start", "start").await?;
    let stop_job = create_job(&fleet.api_url, "free-capacity", "stop").await?;

    let claim = claim_worker_job_with_capacity_ok(&client, &fleet.api_url, "node-a", false)
        .await?
        .ok_or_else(|| anyhow!("pressured worker did not claim a capacity-relieving job"))?;
    assert_eq!(
        claim.get("id").and_then(Value::as_str),
        Some(stop_job.as_str())
    );
    assert_eq!(
        job_status(&fleet.api_url, &start_job).await?,
        "queued",
        "capacity-sensitive jobs should stay queued while the worker reports pressure"
    );

    Ok(())
}

#[tokio::test]
async fn worker_claim_skips_start_that_would_exceed_memory_capacity() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let client = reqwest::Client::new();
    insert_node_with_resources(fleet.api_state.path(), "node-a", now_epoch()?, 8, 3072, 48)?;
    insert_workspace_with_state(
        fleet.api_state.path(),
        "running-big",
        "node-a",
        "running",
        "running",
    )?;
    insert_workspace_with_memory(
        fleet.api_state.path(),
        "stopped-big",
        "node-a",
        "stopped",
        "stopped",
        2048,
    )?;

    let start_job = create_job(&fleet.api_url, "stopped-big", "start").await?;
    let claim = claim_worker_job_with_capacity(
        &client,
        &fleet.api_url,
        "node-a",
        true,
        json!({
            "cpus": 8,
            "memory_mib": 3072,
            "max_active_workspaces": 48,
            "disk_reserve_mib": 1024
        }),
    )
    .await?;

    assert!(
        claim.is_none(),
        "worker should not claim start job that would exceed memory capacity"
    );
    assert_eq!(
        job_status(&fleet.api_url, &start_job).await?,
        "queued",
        "capacity-sensitive job should remain queued"
    );

    Ok(())
}

#[tokio::test]
async fn worker_job_events_refresh_node_freshness_without_changing_status() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let client = reqwest::Client::new();
    insert_node(fleet.api_state.path(), "node-a", now_epoch()? - 3600)?;
    insert_workspace(fleet.api_state.path(), "heartbeat", "node-a")?;
    let running_job = insert_running_job(fleet.api_state.path(), "heartbeat", "node-a")?;

    client
        .post(format!(
            "{}/worker/jobs/{}/events",
            fleet.api_url, running_job
        ))
        .bearer_auth(WORKER_TOKEN)
        .json(&json!({
            "node_id": "node-a",
            "event_type": "job_heartbeat",
            "status": "running",
            "message": "test heartbeat",
            "metadata": {}
        }))
        .send()
        .await?
        .error_for_status()?;

    assert!(node_ready_fresh(fleet.api_state.path(), "node-a")?);
    run_mom(fleet.api_state.path(), &["node", "cordon", "node-a"])?;
    client
        .post(format!(
            "{}/worker/jobs/{}/events",
            fleet.api_url, running_job
        ))
        .bearer_auth(WORKER_TOKEN)
        .json(&json!({
            "node_id": "node-a",
            "event_type": "job_heartbeat",
            "status": "running",
            "message": "test heartbeat",
            "metadata": {}
        }))
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(
        node_status(fleet.api_state.path(), "node-a")?,
        "cordoned",
        "freshness updates must not change node lifecycle status"
    );

    Ok(())
}

#[tokio::test]
async fn catalog_backup_and_monitor_check_cover_deployed_catalog() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let _node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    run_mom(fleet.api_state.path(), &["db", "status"])?;
    run_mom(
        fleet.api_state.path(),
        &[
            "monitor",
            "check",
            "--api-url",
            &fleet.api_url,
            "--min-ready-nodes",
            "1",
        ],
    )?;

    let backup_dir = tempfile::tempdir()?;
    let backup_path = backup_dir.path().join("fleet-backup.db");
    run_mom(
        fleet.api_state.path(),
        &[
            "db",
            "backup",
            "--output",
            backup_path.to_str().ok_or_else(|| anyhow!("utf-8 path"))?,
        ],
    )?;
    assert!(backup_path.exists());
    assert!(
        run_mom_status(
            fleet.api_state.path(),
            &[
                "db",
                "backup",
                "--output",
                backup_path.to_str().ok_or_else(|| anyhow!("utf-8 path"))?,
            ],
        )?
        .code()
        .is_some_and(|code| code != 0),
        "catalog backup should refuse to overwrite an existing file"
    );

    run_mom(fleet.api_state.path(), &["node", "drain", "node-a"])?;
    assert!(
        run_mom_status(
            fleet.api_state.path(),
            &["monitor", "check", "--min-ready-nodes", "1"]
        )?
        .code()
        .is_some_and(|code| code != 0),
        "monitor check should fail when no fresh ready nodes exist"
    );

    Ok(())
}

#[tokio::test]
async fn worker_register_rejects_unspecified_worker_url() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let response = reqwest::Client::new()
        .post(format!("{}/worker/register", fleet.api_url))
        .bearer_auth(WORKER_TOKEN)
        .json(&json!({
            "node_id": "bad-node",
            "capacity": {
                "cpus": 8,
                "memory_mib": 32768,
                "max_active_workspaces": 24,
                "disk_reserve_mib": 1024
            },
            "worker_url": "http://0.0.0.0:9090"
        }))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    Ok(())
}

#[tokio::test]
async fn worker_tokens_are_bound_to_node_identity() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let token_dir = tempfile::tempdir()?;
    let token_a_path = token_dir.path().join("node-a-token");
    let token_b_path = token_dir.path().join("node-b-token");
    fs::write(&token_a_path, "token-a")?;
    fs::write(&token_b_path, "token-b")?;
    let token_files = format!(
        "node-a={},node-b={}",
        token_a_path.display(),
        token_b_path.display()
    );
    let fleet = TestFleet::start_with_api_env(&[("MOM_WORKER_TOKEN_FILES", &token_files)]).await?;
    let client = reqwest::Client::new();

    client
        .post(format!("{}/worker/register", fleet.api_url))
        .bearer_auth("token-a")
        .json(&json!({
            "node_id": "node-a",
            "capacity": test_capacity(),
            "worker_url": "http://100.64.0.42:9090"
        }))
        .send()
        .await?
        .error_for_status()?;

    let spoofed_register = client
        .post(format!("{}/worker/register", fleet.api_url))
        .bearer_auth("token-a")
        .json(&json!({
            "node_id": "node-b",
            "capacity": test_capacity(),
            "worker_url": "http://100.64.0.43:9090"
        }))
        .send()
        .await?;
    assert_eq!(spoofed_register.status(), StatusCode::UNAUTHORIZED);

    let spoofed_query = client
        .get(format!(
            "{}/worker/workspaces?node_id=node-a",
            fleet.api_url
        ))
        .bearer_auth("token-b")
        .send()
        .await?;
    assert_eq!(spoofed_query.status(), StatusCode::UNAUTHORIZED);

    client
        .post(format!("{}/worker/register", fleet.api_url))
        .bearer_auth("token-b")
        .json(&json!({
            "node_id": "node-b",
            "capacity": test_capacity(),
            "worker_url": "http://100.64.0.43:9090"
        }))
        .send()
        .await?
        .error_for_status()?;

    Ok(())
}

#[tokio::test]
async fn worker_register_rejects_urls_outside_allowlist() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet =
        TestFleet::start_with_api_env(&[("MOM_WORKER_URL_ALLOWLIST", "http://100.64.0.42:9090")])
            .await?;
    let client = reqwest::Client::new();

    client
        .post(format!("{}/worker/register", fleet.api_url))
        .bearer_auth(WORKER_TOKEN)
        .json(&json!({
            "node_id": "allowed-node",
            "capacity": {
                "cpus": 8,
                "memory_mib": 32768,
                "max_active_workspaces": 24,
                "disk_reserve_mib": 1024
            },
            "worker_url": "http://100.64.0.42:9090"
        }))
        .send()
        .await?
        .error_for_status()?;

    let response = client
        .post(format!("{}/worker/register", fleet.api_url))
        .bearer_auth(WORKER_TOKEN)
        .json(&json!({
            "node_id": "poison-node",
            "capacity": {
                "cpus": 8,
                "memory_mib": 32768,
                "max_active_workspaces": 24,
                "disk_reserve_mib": 1024
            },
            "worker_url": "http://100.64.0.43:9090"
        }))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    Ok(())
}

#[tokio::test]
async fn sse_wakes_worker_without_waiting_for_poll_interval() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let _node = spawn_worker_with_options("node-a", &fleet.api_url, "30", None)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let started = Instant::now();
    let job_id = create_workspace(&fleet.api_url, "wake", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &job_id, "succeeded").await?;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "job should complete via SSE wakeup, not 30s polling interval"
    );

    Ok(())
}

#[tokio::test]
async fn worker_survives_transient_api_outage_and_claims_after_recovery() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let mut fleet = TestFleet::start().await?;
    let _node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    fleet.stop_api()?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    fleet.start_api(&[]).await?;

    let job_id = create_workspace(&fleet.api_url, "api-recovered", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &job_id, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "api-recovered", "running").await?;

    Ok(())
}

#[tokio::test]
async fn service_open_routes_to_assigned_worker_url() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let node_a =
        spawn_worker_with_options("node-a", &fleet.api_url, "1", Some("http://node-a.fake"))?;
    let node_b =
        spawn_worker_with_options("node-b", &fleet.api_url, "1", Some("http://node-b.fake"))?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;
    wait_for_node(fleet.api_state.path(), "node-b").await?;

    let job_id = create_workspace(&fleet.api_url, "svc", "node-b", 0).await?;
    wait_for_job_status(&fleet.api_url, &job_id, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "svc", "running").await?;
    let cookie = admin_cookie(&fleet.api_url).await?;

    let result = reqwest::Client::new()
        .post(format!("{}/api/workspaces/svc/hermes-ui", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    let stdout = result.get("stdout").and_then(Value::as_str).unwrap_or("");
    assert!(
        stdout.contains("http://node-b.fake/svc/hermes"),
        "service URL should come from assigned node-b, got {stdout:?}"
    );
    assert!(
        !node_a
            .runtime_home
            .path()
            .join("fake/svc/service-hermes")
            .exists(),
        "node-a should not open node-b's service"
    );
    assert!(
        node_b
            .runtime_home
            .path()
            .join("fake/svc/service-hermes")
            .exists(),
        "node-b should open its assigned service"
    );

    Ok(())
}

#[tokio::test]
async fn preview_register_routes_to_assigned_worker_and_lists() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let node_a =
        spawn_worker_with_options("node-a", &fleet.api_url, "1", Some("http://node-a.fake"))?;
    let node_b =
        spawn_worker_with_options("node-b", &fleet.api_url, "1", Some("http://node-b.fake"))?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;
    wait_for_node(fleet.api_state.path(), "node-b").await?;

    let job_id = create_workspace(&fleet.api_url, "preview-svc", "node-b", 0).await?;
    wait_for_job_status(&fleet.api_url, &job_id, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "preview-svc", "running").await?;
    let cookie = admin_cookie(&fleet.api_url).await?;

    let preview = reqwest::Client::new()
        .post(format!(
            "{}/api/workspaces/preview-svc/previews",
            fleet.api_url
        ))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&json!({
            "name": "Vite Dev",
            "port": 5173,
            "path": "app"
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    assert_eq!(preview["name"], "vite-dev");
    assert_eq!(
        preview["url"],
        "http://node-b.fake/preview-svc/previews/vite-dev-5173/app"
    );
    assert!(
        !node_a
            .runtime_home
            .path()
            .join("fake/preview-svc/service-preview-vite-dev")
            .exists(),
        "node-a should not open node-b's preview"
    );
    assert!(
        node_b
            .runtime_home
            .path()
            .join("fake/preview-svc/service-preview-vite-dev")
            .exists(),
        "node-b should open its assigned preview"
    );

    let previews = reqwest::Client::new()
        .get(format!(
            "{}/api/workspaces/preview-svc/previews",
            fleet.api_url
        ))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<Value>>()
        .await?;
    assert_eq!(previews.len(), 1);
    assert_eq!(previews[0]["name"], "vite-dev");

    let cli_list = run_mom_output_with_env(
        fleet.api_state.path(),
        &["workspace", "preview", "list", "preview-svc"],
        &[],
    )?;
    assert!(cli_list.contains("vite-dev"));
    assert!(cli_list.contains("http://node-b.fake/preview-svc/previews/vite-dev-5173/app"));

    let removed = reqwest::Client::new()
        .delete(format!(
            "{}/api/workspaces/preview-svc/previews/vite-dev",
            fleet.api_url
        ))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    assert_eq!(removed["removed"], true);

    let previews = reqwest::Client::new()
        .get(format!(
            "{}/api/workspaces/preview-svc/previews",
            fleet.api_url
        ))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<Value>>()
        .await?;
    assert!(previews.is_empty());

    Ok(())
}

#[tokio::test]
async fn hermes_chat_websocket_routes_to_assigned_worker() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let _node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let job_id = create_workspace(&fleet.api_url, "chat", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &job_id, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "chat", "running").await?;
    let cookie = admin_cookie(&fleet.api_url).await?;

    let mut request = format!(
        "{}/api/workspaces/chat/chat/ws",
        fleet.api_url.replacen("http://", "ws://", 1)
    )
    .into_client_request()?;
    request
        .headers_mut()
        .insert(COOKIE, HeaderValue::from_str(&cookie)?);
    let (mut socket, _) = connect_async(request).await?;

    let status = loop {
        let message = socket
            .next()
            .await
            .ok_or_else(|| anyhow!("chat websocket closed before status"))??;
        let message = ws_text_json(message)?;
        if message["method"] == "mom/timing" {
            continue;
        }
        break message;
    };
    assert_eq!(status["method"], "mom/status");
    assert_eq!(status["params"]["state"], "connected");
    assert_eq!(status["params"]["workspace"], "chat");

    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} });
    socket
        .send(WsMessage::Text(ping.to_string().into()))
        .await?;
    let echo = socket
        .next()
        .await
        .ok_or_else(|| anyhow!("chat websocket closed before echo"))??;
    assert_eq!(ws_text_json(echo)?, ping);

    Ok(())
}

#[tokio::test]
async fn hermes_tui_routes_use_direct_hermes_dashboard_paths() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let _node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let job_id = create_workspace(&fleet.api_url, "tui", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &job_id, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "tui", "running").await?;
    let cookie = admin_cookie(&fleet.api_url).await?;

    let sessions = reqwest::Client::new()
        .get(format!("{}/api/workspaces/tui/tui/sessions", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    let session_id = sessions["sessions"][0]["id"]
        .as_str()
        .ok_or_else(|| anyhow!("missing fake Hermes session id"))?;
    assert_eq!(session_id, "tui-fake-session");

    let mut request = format!(
        "{}/api/workspaces/tui/tui/pty?resume={}",
        fleet.api_url.replacen("http://", "ws://", 1),
        session_id
    )
    .into_client_request()?;
    request
        .headers_mut()
        .insert(COOKIE, HeaderValue::from_str(&cookie)?);
    let (mut socket, _) = connect_async(request).await?;

    let banner = socket
        .next()
        .await
        .ok_or_else(|| anyhow!("TUI websocket closed before banner"))??;
    let banner = ws_text(banner)?;
    assert!(banner.contains("Hermes TUI fake runtime"));
    assert!(banner.contains("session: tui-fake-session"));

    socket.send(WsMessage::Text("hello".into())).await?;
    let echo = socket
        .next()
        .await
        .ok_or_else(|| anyhow!("TUI websocket closed before echo"))??;
    assert_eq!(ws_text(echo)?, "hello");

    Ok(())
}

#[tokio::test]
async fn tls_ask_allows_only_registered_service_tunnel_hostnames() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let _node = spawn_worker_with_options(
        "mom-1",
        &fleet.api_url,
        "1",
        Some("https://mom-1-45887.agentmom.xyz"),
    )?;
    wait_for_node(fleet.api_state.path(), "mom-1").await?;

    let client = reqwest::Client::new();
    let before = client
        .get(format!(
            "{}/api/tls-ask?domain=mom-1-45887.agentmom.xyz",
            fleet.api_url
        ))
        .send()
        .await?;
    assert_eq!(before.status(), reqwest::StatusCode::FORBIDDEN);

    let job_id = create_workspace(&fleet.api_url, "tls-svc", "mom-1", 0).await?;
    wait_for_job_status(&fleet.api_url, &job_id, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "tls-svc", "running").await?;
    let cookie = admin_cookie(&fleet.api_url).await?;

    client
        .post(format!(
            "{}/api/workspaces/tls-svc/hermes-ui",
            fleet.api_url
        ))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?
        .error_for_status()?;

    let after = client
        .get(format!(
            "{}/api/tls-ask?domain=mom-1-45887.agentmom.xyz",
            fleet.api_url
        ))
        .send()
        .await?;
    assert_eq!(after.status(), reqwest::StatusCode::OK);

    let unknown = client
        .get(format!(
            "{}/api/tls-ask?domain=mom-1-45888.agentmom.xyz",
            fleet.api_url
        ))
        .send()
        .await?;
    assert_eq!(unknown.status(), reqwest::StatusCode::FORBIDDEN);

    Ok(())
}

impl TestFleet {
    async fn start() -> Result<Self> {
        Self::start_with_api_env(&[]).await
    }

    async fn start_with_api_env(envs: &[(&str, &str)]) -> Result<Self> {
        let api_state = tempfile::tempdir()?;
        let api_addr = free_addr()?;
        let api_url = format!("http://{api_addr}");
        let api = spawn_api(api_state.path(), &api_addr, envs)?;
        wait_ready(&api_url).await?;
        Ok(Self {
            api_state,
            api_addr,
            api_url,
            api,
        })
    }

    fn stop_api(&mut self) -> Result<()> {
        self.api.stop()
    }

    async fn start_api(&mut self, envs: &[(&str, &str)]) -> Result<()> {
        self.api = spawn_api(self.api_state.path(), &self.api_addr, envs)?;
        wait_ready(&self.api_url).await
    }
}

fn spawn_api(state_dir: &Path, bind: &str, envs: &[(&str, &str)]) -> Result<ChildGuard> {
    let config_path = state_dir.join("config.json");
    fs::write(
        &config_path,
        serde_json::to_string_pretty(&json!({
            "schema_version": 1,
            "auth": {
                "secret": "test-auth-secret"
            }
        }))?,
    )
    .context("write test Agent Mom config")?;
    let mut command = Command::new(MOM_BIN);
    command
        .args(["api", "--bind", bind])
        .env("MOM_RUNTIME", "fake")
        .env("MOM_STATE_DIR", state_dir)
        .env("MOM_CONFIG", &config_path)
        .env("MOM_UI_DIST", state_dir.join("missing-ui"))
        .env("MOM_WORKER_TOKEN", WORKER_TOKEN)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in envs {
        command.env(key, value);
    }
    let child = command.spawn().context("spawn mom api")?;
    Ok(ChildGuard { child })
}

fn cli_invite_code(state_dir: &Path, role: &str, label: &str) -> Result<String> {
    let output = Command::new(MOM_BIN)
        .args(["auth", "invite", role, "--label", label])
        .env("MOM_STATE_DIR", state_dir)
        .output()
        .context("run mom auth invite")?;
    if !output.status.success() {
        bail!(
            "mom auth invite failed with {}: stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let code = String::from_utf8(output.stdout)
        .context("invite code output was not valid UTF-8")?
        .trim()
        .to_string();
    if code.len() != 8 {
        bail!("unexpected invite code output: {code}");
    }
    Ok(code)
}

fn spawn_worker(node: &str, api_url: &str) -> Result<TestNode> {
    spawn_worker_with_options(node, api_url, "1", None)
}

fn spawn_worker_with_options(
    node: &str,
    api_url: &str,
    interval: &str,
    fake_service_base_url: Option<&str>,
) -> Result<TestNode> {
    spawn_worker_with_envs_and_options(node, api_url, interval, fake_service_base_url, &[])
}

fn spawn_worker_with_envs(node: &str, api_url: &str, envs: &[(&str, &str)]) -> Result<TestNode> {
    spawn_worker_with_envs_and_options(node, api_url, "1", None, envs)
}

fn spawn_worker_with_envs_and_options(
    node: &str,
    api_url: &str,
    interval: &str,
    fake_service_base_url: Option<&str>,
    envs: &[(&str, &str)],
) -> Result<TestNode> {
    let state = tempfile::tempdir()?;
    let runtime_home = tempfile::tempdir()?;
    let bind = free_addr()?;
    let mut command = Command::new(MOM_BIN);
    command
        .args(["worker", "--interval", interval])
        .env("MOM_RUNTIME", "fake")
        .env("MOM_NODE_ID", node)
        .env("MOM_STATE_DIR", state.path())
        .env("MOM_MICROVM_STATE_DIR", runtime_home.path())
        .env("MOM_API_URL", api_url)
        .env("MOM_WORKER_BIND", &bind)
        .env("MOM_WORKER_URL", format!("http://{bind}"))
        .env("MOM_WORKER_TOKEN", WORKER_TOKEN)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(base_url) = fake_service_base_url {
        command.env("MOM_FAKE_SERVICE_BASE_URL", base_url);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    let child = command
        .spawn()
        .with_context(|| format!("spawn worker {node}"))?;
    Ok(TestNode {
        _state: state,
        runtime_home,
        worker_url: format!("http://{bind}"),
        _process: ChildGuard { child },
    })
}

fn run_recover_host(api_state: &Path, from: &str, to: &str) -> Result<()> {
    let status = run_mom_status(
        api_state,
        &["fleet", "recover-host", "--from", from, "--to", to],
    )
    .context("run fleet recover-host")?;
    if !status.success() {
        bail!("fleet recover-host exited with {status}");
    }
    Ok(())
}

fn run_mom(api_state: &Path, args: &[&str]) -> Result<()> {
    let status =
        run_mom_status(api_state, args).with_context(|| format!("run mom {}", args.join(" ")))?;
    if !status.success() {
        bail!("mom {} exited with {status}", args.join(" "));
    }
    Ok(())
}

fn run_mom_with_env(api_state: &Path, args: &[&str], envs: &[(&str, &str)]) -> Result<()> {
    let output = run_mom_output_with_env(api_state, args, envs)
        .with_context(|| format!("run mom {}", args.join(" ")))?;
    let _ = output;
    Ok(())
}

fn run_mom_output_with_env(
    api_state: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
) -> Result<String> {
    let mut command = Command::new(MOM_BIN);
    command
        .args(args)
        .env("MOM_STATE_DIR", api_state)
        .env("MOM_WORKER_TOKEN", WORKER_TOKEN)
        .stdin(Stdio::null());
    for (key, value) in envs {
        command.env(key, value);
    }
    let output = command
        .output()
        .with_context(|| format!("run mom {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "mom {} exited with {}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn run_mom_status(api_state: &Path, args: &[&str]) -> Result<std::process::ExitStatus> {
    Command::new(MOM_BIN)
        .args(args)
        .env("MOM_STATE_DIR", api_state)
        .env("MOM_WORKER_TOKEN", WORKER_TOKEN)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("run mom {}", args.join(" ")))
}

async fn create_workspace(
    api_url: &str,
    name: &str,
    node: &str,
    backup_interval: u64,
) -> Result<String> {
    let cookie = admin_cookie(api_url).await?;
    let response = reqwest::Client::new()
        .post(format!("{api_url}/api/workspaces"))
        .header(reqwest::header::COOKIE, cookie)
        .json(&json!({
            "name": name,
            "node_id": node,
            "backup_interval": backup_interval
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    response
        .pointer("/job/id")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("create workspace response missing job id: {response}"))
}

async fn create_job(api_url: &str, workspace: &str, kind: &str) -> Result<String> {
    let response = create_job_value(api_url, workspace, kind).await?;
    response
        .pointer("/job/id")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("create job response missing job id: {response}"))
}

async fn create_job_value(api_url: &str, workspace: &str, kind: &str) -> Result<Value> {
    create_job_value_with_payload(api_url, workspace, kind, json!({})).await
}

async fn create_job_value_with_payload(
    api_url: &str,
    workspace: &str,
    kind: &str,
    payload: Value,
) -> Result<Value> {
    let cookie = admin_cookie(api_url).await?;
    Ok(reqwest::Client::new()
        .post(format!("{api_url}/api/jobs"))
        .header(reqwest::header::COOKIE, cookie)
        .json(&json!({
            "workspace_name": workspace,
            "kind": kind,
            "payload": payload
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?)
}

async fn claim_worker_job(
    client: &reqwest::Client,
    api_url: &str,
    node: &str,
) -> Result<Option<Value>> {
    claim_worker_job_with_capacity_ok(client, api_url, node, true).await
}

async fn claim_worker_job_with_capacity_ok(
    client: &reqwest::Client,
    api_url: &str,
    node: &str,
    capacity_ok: bool,
) -> Result<Option<Value>> {
    claim_worker_job_with_capacity(client, api_url, node, capacity_ok, test_capacity()).await
}

async fn claim_worker_job_with_capacity(
    client: &reqwest::Client,
    api_url: &str,
    node: &str,
    capacity_ok: bool,
    capacity: Value,
) -> Result<Option<Value>> {
    Ok(client
        .post(format!("{api_url}/worker/claim"))
        .bearer_auth(WORKER_TOKEN)
        .json(&json!({
            "node_id": node,
            "capacity": capacity,
            "pressure": {
                "managed_vms": 0,
                "running_vms": 0,
                "active_workspaces": 0,
                "allocated_memory_mib": 0,
                "disk_available_mib": 65536,
                "disk_ok": true,
                "capacity_ok": capacity_ok
            },
            "worker_url": "http://100.64.0.42:9090"
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Option<Value>>()
        .await?)
}

async fn wait_ready(api_url: &str) -> Result<()> {
    wait_until("api ready", || async {
        reqwest::get(format!("{api_url}/health/ready"))
            .await
            .ok()
            .filter(|response| response.status().is_success())
            .is_some()
    })
    .await
}

async fn wait_for_node(api_state: &Path, node: &str) -> Result<()> {
    wait_until(&format!("node {node} registered"), || async {
        node_exists(api_state, node).unwrap_or(false)
    })
    .await
}

async fn wait_for_node_ready_fresh(api_state: &Path, node: &str) -> Result<()> {
    wait_until(&format!("node {node} fresh ready"), || async {
        node_ready_fresh(api_state, node).unwrap_or(false)
    })
    .await
}

fn node_exists(api_state: &Path, node: &str) -> Result<bool> {
    let db_path = api_state.join("fleet.db");
    if !db_path.exists() {
        return Ok(false);
    }
    let db = Connection::open(db_path)?;
    let count: i64 = db.query_row(
        "SELECT COUNT(*) FROM nodes WHERE node_id = ?1",
        [node],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn node_ready_fresh(api_state: &Path, node: &str) -> Result<bool> {
    let db_path = api_state.join("fleet.db");
    if !db_path.exists() {
        return Ok(false);
    }
    let db = Connection::open(db_path)?;
    let now = now_epoch()?;
    Ok(db
        .query_row(
            "SELECT last_seen_at FROM nodes WHERE node_id = ?1 AND status = 'ready'",
            [node],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some_and(|last_seen| now.saturating_sub(last_seen) <= 60))
}

fn insert_node(api_state: &Path, node: &str, last_seen_at: i64) -> Result<()> {
    insert_node_with_capacity(api_state, node, last_seen_at, 48)
}

fn insert_node_with_capacity(
    api_state: &Path,
    node: &str,
    last_seen_at: i64,
    max_active_workspaces: i64,
) -> Result<()> {
    insert_node_with_status(
        api_state,
        node,
        last_seen_at,
        max_active_workspaces,
        "ready",
    )
}

fn insert_node_with_resources(
    api_state: &Path,
    node: &str,
    last_seen_at: i64,
    cpus: i64,
    memory_mib: i64,
    max_active_workspaces: i64,
) -> Result<()> {
    insert_node_with_resources_and_status(
        api_state,
        node,
        last_seen_at,
        cpus,
        memory_mib,
        max_active_workspaces,
        "ready",
    )
}

fn insert_node_with_status(
    api_state: &Path,
    node: &str,
    last_seen_at: i64,
    max_active_workspaces: i64,
    status: &str,
) -> Result<()> {
    insert_node_with_resources_and_status(
        api_state,
        node,
        last_seen_at,
        16,
        65536,
        max_active_workspaces,
        status,
    )
}

fn insert_node_with_resources_and_status(
    api_state: &Path,
    node: &str,
    last_seen_at: i64,
    cpus: i64,
    memory_mib: i64,
    max_active_workspaces: i64,
    status: &str,
) -> Result<()> {
    let db = Connection::open(api_state.join("fleet.db"))?;
    db.execute(
        r#"
	INSERT INTO nodes (
	    node_id, worker_url, cpus, memory_mib, max_active_workspaces, disk_reserve_mib,
	    last_seen_at, status
) VALUES (?1, ?2, ?3, ?4, ?5, 1024, ?6, ?7)
	"#,
        (
            node,
            format!("http://{node}.invalid:9090"),
            cpus,
            memory_mib,
            max_active_workspaces,
            last_seen_at,
            status,
        ),
    )?;
    Ok(())
}

fn insert_workspace(api_state: &Path, name: &str, node: &str) -> Result<()> {
    insert_workspace_with_state(api_state, name, node, "running", "running")
}

fn insert_workspace_with_state(
    api_state: &Path,
    name: &str,
    node: &str,
    desired_state: &str,
    status: &str,
) -> Result<()> {
    insert_workspace_with_memory(api_state, name, node, desired_state, status, 2048)
}

fn insert_workspace_with_memory(
    api_state: &Path,
    name: &str,
    node: &str,
    desired_state: &str,
    status: &str,
    memory_mib: i64,
) -> Result<()> {
    let now = now_epoch()?;
    let workspace_id = test_workspace_id(name);
    let db = Connection::open(api_state.join("fleet.db"))?;
    db.execute(
        r#"
	INSERT INTO workspaces (
		    name, workspace_id, slug, display_name, user_id, vm_version, vm_name, workspace_dir_name, node_id, desired_state, cpus, memory_mib,
		    workspace_quota_mib, status, idle_timeout_secs, backup_interval_secs,
		    last_used_at, last_backup_at, created_at, updated_at
	) VALUES (?1, ?2, ?1, ?1, ?1, ?3, ?4, ?5, ?6, ?7, 1, ?8, 10240, ?9, 1800, 0, ?10, NULL, ?10, ?10)
		"#,
        (
            name,
            workspace_id,
            env!("CARGO_PKG_VERSION"),
            format!("mom-{name}"),
            format!("mom-{name}-workspace"),
            node,
            desired_state,
            memory_mib,
            status,
            now,
        ),
    )?;
    Ok(())
}

fn insert_running_job(api_state: &Path, workspace: &str, node: &str) -> Result<String> {
    let now = now_epoch()?;
    let job_id = format!("job-{workspace}-running");
    let db = Connection::open(api_state.join("fleet.db"))?;
    db.execute(
        r#"
INSERT INTO jobs (
    id, workspace_name, node_id, kind, status, payload_json, output_json,
    claimed_by, claimed_at, created_at, updated_at
) VALUES (?1, ?2, ?3, 'stop', 'running', '{}', NULL, ?3, ?4, ?4, ?4)
"#,
        (&job_id, workspace, node, now),
    )?;
    Ok(job_id)
}

fn insert_backup_record(api_state: &Path, workspace: &str, node: &str) -> Result<String> {
    let now = now_epoch()?;
    let backup_id = format!("bak-{workspace}-test");
    let db = Connection::open(api_state.join("fleet.db"))?;
    db.execute(
        r#"
INSERT INTO workspace_backups (
    id, workspace_name, node_id, kind, location, status, size_bytes, created_at
) VALUES (?1, ?2, ?3, 'restic', ?4, 'succeeded', 0, ?5)
"#,
        (
            &backup_id,
            workspace,
            node,
            format!("fake-restic#{backup_id}"),
            now,
        ),
    )?;
    Ok(backup_id)
}

fn insert_unassigned_workspace(api_state: &Path, name: &str) -> Result<()> {
    let now = now_epoch()?;
    let workspace_id = test_workspace_id(name);
    let db = Connection::open(api_state.join("fleet.db"))?;
    db.execute(
        r#"
INSERT INTO workspaces (
    name, workspace_id, slug, display_name, user_id, vm_version, vm_name, workspace_dir_name, node_id, desired_state, cpus, memory_mib,
    workspace_quota_mib, status, idle_timeout_secs, backup_interval_secs,
    last_used_at, last_backup_at, created_at, updated_at
) VALUES (?1, ?2, ?1, ?1, ?1, ?3, ?4, ?5, NULL, 'running', 1, 2048, 10240, 'running', 1800, 0, ?6, NULL, ?6, ?6)
"#,
        (
            name,
            workspace_id,
            env!("CARGO_PKG_VERSION"),
            format!("mom-{name}"),
            format!("mom-{name}-workspace"),
            now,
        ),
    )?;
    Ok(())
}

fn test_workspace_id(name: &str) -> String {
    format!("ws_test_{}", name.replace('-', "_"))
}

fn workspace_count_for_node(api_state: &Path, node: &str) -> Result<i64> {
    let db = Connection::open(api_state.join("fleet.db"))?;
    Ok(db.query_row(
        "SELECT COUNT(*) FROM workspaces WHERE node_id = ?1 AND status != 'removed'",
        [node],
        |row| row.get(0),
    )?)
}

fn queued_job_count(api_state: &Path, workspace: &str) -> Result<i64> {
    let db = Connection::open(api_state.join("fleet.db"))?;
    Ok(db.query_row(
        "SELECT COUNT(*) FROM jobs WHERE workspace_name = ?1 AND status = 'queued'",
        [workspace],
        |row| row.get(0),
    )?)
}

fn workspace_desired_state(api_state: &Path, workspace: &str) -> Result<String> {
    let db = Connection::open(api_state.join("fleet.db"))?;
    Ok(db.query_row(
        "SELECT desired_state FROM workspaces WHERE name = ?1",
        [workspace],
        |row| row.get(0),
    )?)
}

fn node_status(api_state: &Path, node: &str) -> Result<String> {
    let db = Connection::open(api_state.join("fleet.db"))?;
    Ok(db.query_row(
        "SELECT status FROM nodes WHERE node_id = ?1",
        [node],
        |row| row.get(0),
    )?)
}

fn now_epoch() -> Result<i64> {
    Ok(i64::try_from(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    )?)
}

async fn wait_for_backup_count(api_state: &Path, workspace: &str, expected: i64) -> Result<()> {
    wait_until(
        &format!("workspace {workspace} backup count {expected}"),
        || async { backup_count(api_state, workspace).unwrap_or(0) == expected },
    )
    .await
}

fn backup_count(api_state: &Path, workspace: &str) -> Result<i64> {
    let db_path = api_state.join("fleet.db");
    if !db_path.exists() {
        return Ok(0);
    }
    let db = Connection::open(db_path)?;
    let count: i64 = db.query_row(
        "SELECT COUNT(*) FROM workspace_backups WHERE workspace_name = ?1",
        [workspace],
        |row| row.get(0),
    )?;
    Ok(count)
}

fn latest_backup_record(api_state: &Path, workspace: &str) -> Result<(String, String)> {
    let db = Connection::open(api_state.join("fleet.db"))?;
    Ok(db.query_row(
        r#"
SELECT id, location
FROM workspace_backups
WHERE workspace_name = ?1
ORDER BY created_at DESC
LIMIT 1
"#,
        [workspace],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

async fn wait_for_job_status(api_url: &str, job_id: &str, status: &str) -> Result<()> {
    wait_until(&format!("job {job_id} status {status}"), || async {
        job_status(api_url, job_id)
            .await
            .ok()
            .as_deref()
            .is_some_and(|actual| actual == status)
    })
    .await
}

async fn job_status(api_url: &str, job_id: &str) -> Result<String> {
    let cookie = admin_cookie(api_url).await?;
    let value = reqwest::Client::new()
        .get(format!("{api_url}/api/jobs/{job_id}"))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    value
        .pointer("/job/status")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| anyhow!("job response missing status: {value}"))
}

async fn wait_for_workspace_status(api_url: &str, name: &str, status: &str) -> Result<()> {
    wait_until(&format!("workspace {name} status {status}"), || async {
        workspace(api_url, name)
            .await
            .ok()
            .and_then(|value| {
                value
                    .get("status")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .as_deref()
            == Some(status)
    })
    .await
}

async fn wait_for_workspace_node(api_url: &str, name: &str, node: &str) -> Result<()> {
    wait_until(&format!("workspace {name} node {node}"), || async {
        workspace(api_url, name)
            .await
            .ok()
            .and_then(|value| {
                value
                    .get("node_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .as_deref()
            == Some(node)
    })
    .await
}

async fn workspace(api_url: &str, name: &str) -> Result<Value> {
    let cookie = admin_cookie(api_url).await?;
    let values = reqwest::Client::new()
        .get(format!("{api_url}/api/workspaces"))
        .header(reqwest::header::COOKIE, cookie)
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<Value>>()
        .await?;
    values
        .into_iter()
        .find(|value| value.get("name").and_then(Value::as_str) == Some(name))
        .ok_or_else(|| anyhow!("workspace not found: {name}"))
}

async fn admin_cookie(api_url: &str) -> Result<String> {
    if let Some(cookie) = ADMIN_COOKIES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("admin cookie cache should not be poisoned")
        .get(api_url)
        .cloned()
    {
        return Ok(cookie);
    }

    let client = reqwest::Client::new();
    let cookie = admin_login(&client, api_url).await?;
    ADMIN_COOKIES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("admin cookie cache should not be poisoned")
        .insert(api_url.to_string(), cookie.clone());
    Ok(cookie)
}

async fn admin_login(client: &reqwest::Client, api_url: &str) -> Result<String> {
    const ADMIN_PASSWORD: &str = "agentmom test admin password";

    let login_response = client
        .post(format!("{api_url}/api/auth/login"))
        .json(&json!({
            "email": "admin@example.com",
            "password": ADMIN_PASSWORD
        }))
        .send()
        .await?;
    if login_response.status().is_success() {
        return session_cookie_from_response(&login_response);
    }
    if login_response.status() != StatusCode::UNAUTHORIZED {
        let status = login_response.status();
        let body = login_response.text().await.unwrap_or_default();
        bail!("admin login failed with {status}: {body}");
    }

    let signup_response = client
        .post(format!("{api_url}/api/auth/signup"))
        .json(&json!({
            "full_name": "Admin User",
            "email": "admin@example.com",
            "password": ADMIN_PASSWORD
        }))
        .send()
        .await?;
    let status = signup_response.status();
    if !status.is_success() {
        let body = signup_response.text().await.unwrap_or_default();
        bail!("admin signup failed with {status}: {body}");
    }
    session_cookie_from_response(&signup_response)
}

fn session_cookie_from_response(response: &reqwest::Response) -> Result<String> {
    let cookie = response
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .ok_or_else(|| anyhow!("login response did not set a session cookie"))?;
    Ok(cookie.to_string())
}

fn ws_text_json(message: WsMessage) -> Result<Value> {
    match message {
        WsMessage::Text(text) => serde_json::from_str(&text).context("parse websocket JSON text"),
        other => bail!("expected websocket text message, got {other:?}"),
    }
}

fn ws_text(message: WsMessage) -> Result<String> {
    match message {
        WsMessage::Text(text) => Ok(text.to_string()),
        other => bail!("expected websocket text message, got {other:?}"),
    }
}

async fn wait_until<F, Fut>(label: &str, mut check: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if check().await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("timed out waiting for {label}")
}

fn free_addr() -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.to_string())
}

fn assert_no_local_fleet_db(node: &TestNode) -> Result<()> {
    let path: PathBuf = node._state.path().join("fleet.db");
    if path.exists() {
        bail!("worker unexpectedly created local {}", path.display());
    }
    Ok(())
}
