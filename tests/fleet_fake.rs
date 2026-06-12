use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::StatusCode;
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

const MOM_BIN: &str = env!("CARGO_BIN_EXE_mom");
const WORKER_TOKEN: &str = "test-worker-token";

struct ChildGuard {
    child: Child,
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
    api_url: String,
    _api: ChildGuard,
}

#[tokio::test]
async fn fake_worker_start_stop_backup_jobs_update_central_state() -> Result<()> {
    let fleet = TestFleet::start().await?;
    let node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let create = create_workspace(&fleet.api_url, "alice", "node-a", 0).await?;
    wait_for_job_status(&fleet.api_url, &create, "succeeded").await?;
    wait_for_workspace_status(&fleet.api_url, "alice", "running").await?;

    let stop = create_job(&fleet.api_url, "alice", "stop").await?;
    wait_for_job_status(&fleet.api_url, &stop, "succeeded").await?;
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
async fn fake_workers_create_assigned_workspace_without_shared_sqlite() -> Result<()> {
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
    let fleet = TestFleet::start().await?;
    let node = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    reqwest::Client::new()
        .post(format!("{}/api/vms", fleet.api_url))
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
    let fleet = TestFleet::start().await?;
    insert_node(fleet.api_state.path(), "stale-node", now_epoch()? - 3600)?;
    let node = spawn_worker("fresh-node", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "fresh-node").await?;

    reqwest::Client::new()
        .post(format!("{}/api/vms", fleet.api_url))
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
async fn offline_node_is_not_reenabled_by_stale_heartbeat() -> Result<()> {
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
async fn service_open_routes_to_assigned_worker_url() -> Result<()> {
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

    let result = reqwest::Client::new()
        .post(format!("{}/api/vms/svc/opencode", fleet.api_url))
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
            api_url,
            _api: api,
        })
    }
}

fn spawn_api(state_dir: &Path, bind: &str, envs: &[(&str, &str)]) -> Result<ChildGuard> {
    let mut command = Command::new(MOM_BIN);
    command
        .args(["api", "--bind", bind])
        .env("MOM_RUNTIME", "fake")
        .env("MOM_STATE_DIR", state_dir)
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
