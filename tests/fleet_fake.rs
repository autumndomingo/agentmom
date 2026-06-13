use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::StatusCode;
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MOM_BIN: &str = env!("CARGO_BIN_EXE_mom");
const WORKER_TOKEN: &str = "test-worker-token";
static FLEET_TEST_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

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
    msb_home: TempDir,
    worker_url: String,
    _process: ChildGuard,
}

struct TestFleet {
    api_state: TempDir,
    api_addr: String,
    api_url: String,
    api: ChildGuard,
}

#[tokio::test]
async fn first_user_claims_admin_and_existing_users_need_their_own_code() -> Result<()> {
    let fleet = TestFleet::start().await?;
    let client = reqwest::Client::new();

    let first_response = client
        .post(format!("{}/api/auth/login", fleet.api_url))
        .json(&json!({ "email": "admin@example.com" }))
        .send()
        .await?
        .error_for_status()?;
    let admin_cookie = session_cookie_from_response(&first_response)?;
    let first = first_response.json::<Value>().await?;
    let admin_code = first["user"]["code"]
        .as_str()
        .ok_or_else(|| anyhow!("first login did not return user code"))?
        .to_string();
    assert_eq!(first["user"]["role"], "admin");
    assert!(admin_code.starts_with("AM-"));

    let missing_code = client
        .post(format!("{}/api/auth/login", fleet.api_url))
        .json(&json!({ "email": "admin@example.com" }))
        .send()
        .await?;
    assert_eq!(missing_code.status(), StatusCode::UNAUTHORIZED);

    client
        .post(format!("{}/api/auth/login", fleet.api_url))
        .json(&json!({ "email": "admin@example.com", "access_code": admin_code }))
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

    let participant = client
        .post(format!("{}/api/auth/login", fleet.api_url))
        .json(&json!({
            "email": "participant@example.com",
            "access_code": invite_code
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    let participant_code = participant["user"]["code"]
        .as_str()
        .ok_or_else(|| anyhow!("participant login did not return user code"))?;
    assert_ne!(participant_code, invite_code);

    let invite_reuse_as_login = client
        .post(format!("{}/api/auth/login", fleet.api_url))
        .json(&json!({
            "email": "participant@example.com",
            "access_code": invite_code
        }))
        .send()
        .await?;
    assert_eq!(invite_reuse_as_login.status(), StatusCode::UNAUTHORIZED);

    client
        .post(format!("{}/api/auth/login", fleet.api_url))
        .json(&json!({
            "email": "participant@example.com",
            "access_code": participant_code
        }))
        .send()
        .await?
        .error_for_status()?;

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

    reqwest::Client::new()
        .post(format!("{}/api/workspaces/alice/stop", fleet.api_url))
        .send()
        .await?
        .error_for_status()?;
    wait_for_workspace_status(&fleet.api_url, "alice", "stopped").await?;
    assert_eq!(
        std::fs::read_to_string(node.msb_home.path().join("fake/alice/state"))?,
        "stopped"
    );

    let start = create_job(&fleet.api_url, "alice", "start").await?;
    wait_for_job_status(&fleet.api_url, &start, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "alice", "running").await?;
    assert_eq!(
        std::fs::read_to_string(node.msb_home.path().join("fake/alice/state"))?,
        "running"
    );

    let backup = create_job(&fleet.api_url, "alice", "backup").await?;
    wait_for_job_status(&fleet.api_url, &backup, "succeeded").await?;
    wait_for_backup_count(fleet.api_state.path(), "alice", 1).await?;

    Ok(())
}

#[tokio::test]
async fn workspace_backup_cli_queues_remote_worker_job_when_volume_is_not_local() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let _node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let create = create_workspace(&fleet.api_url, "remote-backup", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &create, "succeeded").await?;

    let local_msb = tempfile::tempdir()?;
    run_mom_with_env(
        fleet.api_state.path(),
        &["workspace", "backup", "remote-backup", "--leave-stopped"],
        &[("MSB_HOME", local_msb.path().to_str().unwrap_or(""))],
    )?;
    wait_for_backup_count(fleet.api_state.path(), "remote-backup", 1).await?;

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

    let local_msb = tempfile::tempdir()?;
    let output = run_mom_output_with_env(
        fleet.api_state.path(),
        &["workspace", "inspect", "remote-inspect"],
        &[
            ("MOM_NODE_ID", "control"),
            ("MSB_HOME", local_msb.path().to_str().unwrap_or("")),
        ],
    )?;
    assert!(
        output.contains("Inspecting node: control"),
        "inspect output should identify the local inspecting node: {output}"
    );
    assert!(
        output.contains("Sandbox status: not checked locally; assigned to node-a"),
        "inspect output should not report remote sandbox as missing: {output}"
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
        !node_a.msb_home.path().join("fake/alice").exists(),
        "node-a should not create node-b's workspace"
    );
    assert!(
        node_b.msb_home.path().join("fake/alice").exists(),
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
        .post(format!("{}/api/vms", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&json!({ "name": "ui-created" }))
        .send()
        .await?
        .error_for_status()?;

    wait_for_workspace_node(&fleet.api_url, "ui-created", "node-a").await?;
    wait_for_workspace_status(&fleet.api_url, "ui-created", "running").await?;
    assert!(
        node.msb_home.path().join("fake/ui-created").exists(),
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
        !node_a.msb_home.path().join("fake/owner").exists(),
        "node-a should not claim node-b's unpinned job"
    );
    assert_eq!(
        std::fs::read_to_string(node_b.msb_home.path().join("fake/owner/state"))?,
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

    run_recover_host(fleet.api_state.path(), "node-a", "node-b")?;

    wait_for_workspace_node(&fleet.api_url, "recover-me", "node-b").await?;
    wait_for_workspace_status(&fleet.api_url, "recover-me", "running").await?;
    assert!(
        node_a.msb_home.path().join("fake/recover-me").exists(),
        "source worker's local fake volume remains as lost-host residue"
    );
    let restored_from = node_b.msb_home.path().join("fake/recover-me/restored-from");
    wait_until("workspace restored on node-b", || {
        let restored_from = restored_from.clone();
        async move { restored_from.exists() }
    })
    .await?;

    Ok(())
}

#[tokio::test]
async fn worker_service_open_rejects_spoofed_sandbox_identity() -> Result<()> {
    let _guard = fleet_test_guard().await;
    let fleet = TestFleet::start().await?;
    let node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let job_id = create_workspace(&fleet.api_url, "guard", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &job_id, "succeeded").await?;

    let response = reqwest::Client::new()
        .post(format!("{}/worker/services/opencode/open", node.worker_url))
        .bearer_auth(WORKER_TOKEN)
        .json(&json!({
            "workspace_name": "guard",
            "sandbox_name": "mom-other"
        }))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        !node
            .msb_home
            .path()
            .join("fake/guard/service-opencode")
            .exists(),
        "worker should not open a service for a mismatched sandbox"
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
        .post(format!("{}/api/vms", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .json(&json!({ "name": "fresh" }))
        .send()
        .await?
        .error_for_status()?;

    wait_for_workspace_node(&fleet.api_url, "fresh", "fresh-node").await?;
    wait_for_workspace_status(&fleet.api_url, "fresh", "running").await?;
    assert!(
        node.msb_home.path().join("fake/fresh").exists(),
        "workspace should be placed on the fresh worker"
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
    let response = reqwest::Client::new()
        .post(format!("{}/api/workspaces", fleet.api_url))
        .json(&json!({
            "name": "dupe",
            "node_id": "node-b"
        }))
        .send()
        .await?;
    assert!(!response.status().is_success());
    wait_for_workspace_node(&fleet.api_url, "dupe", "node-a").await?;
    assert!(node_a.msb_home.path().join("fake/dupe").exists());
    assert!(
        !node_b.msb_home.path().join("fake/dupe").exists(),
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

    let response = reqwest::Client::new()
        .post(format!("{}/api/workspaces", fleet.api_url))
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

    let response = reqwest::Client::new()
        .post(format!("{}/api/workspaces", fleet.api_url))
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
        !node.msb_home.path().join("fake/legacy").exists(),
        "worker should not reconcile unassigned legacy workspaces"
    );
    assert!(
        !node.msb_home.path().join("fake/cold").exists(),
        "idle-stopped workspace should remain cold until a job wakes it"
    );

    let start = create_job(&fleet.api_url, "cold", "start").await?;
    wait_for_job_status(&fleet.api_url, &start, "succeeded").await?;
    assert_eq!(
        std::fs::read_to_string(node.msb_home.path().join("fake/cold/state"))?,
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
    let response = client
        .post(format!("{}/api/workspaces", fleet.api_url))
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
                "managed_sandboxes": 0,
                "running_sandboxes": 0,
                "active_workspaces": 0,
                "allocated_memory_mib": 0,
                "disk_available_mib": 65536,
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
    let response = reqwest::Client::new()
        .post(format!("{}/api/workspaces", fleet.api_url))
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
        std::fs::read_to_string(node.msb_home.path().join("fake/lifecycle/state"))?,
        "stopped",
        "cordoned node should still claim assigned workspace jobs"
    );

    run_mom(fleet.api_state.path(), &["node", "drain", "node-a"])?;
    assert_eq!(node_status(fleet.api_state.path(), "node-a")?, "draining");
    let start = create_job(&fleet.api_url, "lifecycle", "start").await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(job_status(&fleet.api_url, &start).await?, "queued");

    run_mom(fleet.api_state.path(), &["node", "uncordon", "node-a"])?;
    assert_eq!(node_status(fleet.api_state.path(), "node-a")?, "ready");
    wait_for_job_status(&fleet.api_url, &start, "succeeded").await?;

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
        .post(format!("{}/api/workspaces/svc/opencode", fleet.api_url))
        .header(reqwest::header::COOKIE, &cookie)
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    let stdout = result.get("stdout").and_then(Value::as_str).unwrap_or("");
    assert!(
        stdout.contains("http://node-b.fake/svc/opencode"),
        "service URL should come from assigned node-b, got {stdout:?}"
    );
    assert!(
        !node_a
            .msb_home
            .path()
            .join("fake/svc/service-opencode")
            .exists(),
        "node-a should not open node-b's service"
    );
    assert!(
        node_b
            .msb_home
            .path()
            .join("fake/svc/service-opencode")
            .exists(),
        "node-b should open its assigned service"
    );

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
        .post(format!("{}/api/vms/tls-svc/hermes-ui", fleet.api_url))
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

fn spawn_worker(node: &str, api_url: &str) -> Result<TestNode> {
    spawn_worker_with_options(node, api_url, "1", None)
}

fn spawn_worker_with_options(
    node: &str,
    api_url: &str,
    interval: &str,
    fake_service_base_url: Option<&str>,
) -> Result<TestNode> {
    let state = tempfile::tempdir()?;
    let msb_home = tempfile::tempdir()?;
    let bind = free_addr()?;
    let mut command = Command::new(MOM_BIN);
    command
        .args(["worker", "--interval", interval])
        .env("MOM_RUNTIME", "fake")
        .env("MOM_NODE_ID", node)
        .env("MOM_STATE_DIR", state.path())
        .env("MSB_HOME", msb_home.path())
        .env("MOM_API_URL", api_url)
        .env("MOM_WORKER_BIND", &bind)
        .env("MOM_WORKER_URL", format!("http://{bind}"))
        .env("MOM_WORKER_TOKEN", WORKER_TOKEN)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(base_url) = fake_service_base_url {
        command.env("MOM_FAKE_SERVICE_BASE_URL", base_url);
    }
    let child = command
        .spawn()
        .with_context(|| format!("spawn worker {node}"))?;
    Ok(TestNode {
        _state: state,
        msb_home,
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
    let response = reqwest::Client::new()
        .post(format!("{api_url}/api/workspaces"))
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
    Ok(reqwest::Client::new()
        .post(format!("{api_url}/api/jobs"))
        .json(&json!({
            "workspace_name": workspace,
            "kind": kind
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
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

fn insert_node_with_status(
    api_state: &Path,
    node: &str,
    last_seen_at: i64,
    max_active_workspaces: i64,
    status: &str,
) -> Result<()> {
    let db = Connection::open(api_state.join("fleet.db"))?;
    db.execute(
        r#"
INSERT INTO nodes (
    node_id, worker_url, cpus, memory_mib, max_active_workspaces, disk_reserve_mib,
    last_seen_at, status
) VALUES (?1, ?2, 16, 65536, ?3, 1024, ?4, ?5)
"#,
        (
            node,
            format!("http://{node}.invalid:9090"),
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
    let now = now_epoch()?;
    let db = Connection::open(api_state.join("fleet.db"))?;
    db.execute(
        r#"
INSERT INTO workspaces (
    name, user_id, sandbox_name, volume_name, node_id, desired_state, cpus, memory_mib,
    volume_quota_mib, status, idle_timeout_secs, backup_interval_secs,
    last_used_at, last_backup_at, created_at, updated_at
) VALUES (?1, ?1, ?2, ?3, ?4, ?5, 1, 2048, 10240, ?6, 1800, 0, ?7, NULL, ?7, ?7)
"#,
        (
            name,
            format!("mom-{name}"),
            format!("mom-{name}-workspace"),
            node,
            desired_state,
            status,
            now,
        ),
    )?;
    Ok(())
}

fn insert_unassigned_workspace(api_state: &Path, name: &str) -> Result<()> {
    let now = now_epoch()?;
    let db = Connection::open(api_state.join("fleet.db"))?;
    db.execute(
        r#"
INSERT INTO workspaces (
    name, user_id, sandbox_name, volume_name, node_id, desired_state, cpus, memory_mib,
    volume_quota_mib, status, idle_timeout_secs, backup_interval_secs,
    last_used_at, last_backup_at, created_at, updated_at
) VALUES (?1, ?1, ?2, ?3, NULL, 'running', 1, 2048, 10240, 'running', 1800, 0, ?4, NULL, ?4, ?4)
"#,
        (
            name,
            format!("mom-{name}"),
            format!("mom-{name}-workspace"),
            now,
        ),
    )?;
    Ok(())
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
    let value = reqwest::get(format!("{api_url}/api/jobs/{job_id}"))
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
    let values = reqwest::get(format!("{api_url}/api/workspaces"))
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
    let response = reqwest::Client::new()
        .post(format!("{api_url}/api/auth/login"))
        .json(&json!({
            "email": "admin@example.com"
        }))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!("admin login failed with {status}: {body}");
    }
    session_cookie_from_response(&response)
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
