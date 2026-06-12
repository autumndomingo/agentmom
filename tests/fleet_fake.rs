use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::StatusCode;
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::TempDir;

const MOM_BIN: &str = env!("CARGO_BIN_EXE_mom");

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
async fn worker_state_updates_require_assigned_node() -> Result<()> {
    let fleet = TestFleet::start().await?;
    let _node_a = spawn_worker("node-a", &fleet.api_url)?;
    wait_for_node(fleet.api_state.path(), "node-a").await?;

    let client = reqwest::Client::new();
    create_workspace(&fleet.api_url, "owned", "node-a", 0).await?;
    wait_for_workspace_node(&fleet.api_url, "owned", "node-a").await?;

    let response = client
        .post(format!("{}/worker/workspaces/owned/state", fleet.api_url))
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
        let api_state = tempfile::tempdir()?;
        let api_addr = free_addr()?;
        let api_url = format!("http://{api_addr}");
        let api = spawn_api(api_state.path(), &api_addr)?;
        wait_ready(&api_url).await?;
        Ok(Self {
            api_state,
            api_url,
            _api: api,
        })
    }
}

fn spawn_api(state_dir: &Path, bind: &str) -> Result<ChildGuard> {
    let child = Command::new(MOM_BIN)
        .args(["api", "--bind", bind])
        .env("MOM_STATE_DIR", state_dir)
        .env("MOM_UI_DIST", state_dir.join("missing-ui"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn mom api")?;
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
        _process: ChildGuard { child },
    })
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
    let response = reqwest::Client::new()
        .post(format!("{api_url}/api/jobs"))
        .json(&json!({
            "workspace_name": workspace,
            "kind": kind
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
        .ok_or_else(|| anyhow!("create job response missing job id: {response}"))
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
        let Ok(response) = reqwest::get(format!("{api_url}/api/jobs/{job_id}")).await else {
            return false;
        };
        let Ok(value) = response.json::<Value>().await else {
            return false;
        };
        value.pointer("/job/status").and_then(Value::as_str) == Some(status)
    })
    .await
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
