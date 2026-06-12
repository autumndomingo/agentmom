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

#[tokio::test]
async fn fake_workers_create_assigned_workspace_without_shared_sqlite() -> Result<()> {
    let api_state = tempfile::tempdir()?;
    let api_addr = free_addr()?;
    let api_url = format!("http://{api_addr}");
    let _api = spawn_api(api_state.path(), &api_addr)?;
    wait_ready(&api_url).await?;

    let node_a = spawn_worker("node-a", &api_url)?;
    let node_b = spawn_worker("node-b", &api_url)?;
    wait_for_node(api_state.path(), "node-a").await?;
    wait_for_node(api_state.path(), "node-b").await?;

    let client = reqwest::Client::new();
    let job = client
        .post(format!("{api_url}/api/workspaces"))
        .json(&json!({
            "name": "alice",
            "node_id": "node-b",
            "backup_interval": 0
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    let job_id = job
        .pointer("/job/id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("create workspace response missing job id: {job}"))?;
    wait_for_job_status(&api_url, job_id, "succeeded").await?;
    wait_for_workspace_status(&api_url, "alice", "running").await?;

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
    let api_state = tempfile::tempdir()?;
    let api_addr = free_addr()?;
    let api_url = format!("http://{api_addr}");
    let _api = spawn_api(api_state.path(), &api_addr)?;
    wait_ready(&api_url).await?;
    let _node_a = spawn_worker("node-a", &api_url)?;
    wait_for_node(api_state.path(), "node-a").await?;

    let client = reqwest::Client::new();
    client
        .post(format!("{api_url}/api/workspaces"))
        .json(&json!({
            "name": "owned",
            "node_id": "node-a",
            "backup_interval": 0
        }))
        .send()
        .await?
        .error_for_status()?;
    wait_for_workspace_node(&api_url, "owned", "node-a").await?;

    let response = client
        .post(format!("{api_url}/worker/workspaces/owned/state"))
        .json(&json!({
            "node_id": "node-b",
            "status": "running"
        }))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    Ok(())
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
    let state = tempfile::tempdir()?;
    let msb_home = tempfile::tempdir()?;
    let bind = free_addr()?;
    let child = Command::new(MOM_BIN)
        .args(["worker", "--interval", "1"])
        .env("MOM_RUNTIME", "fake")
        .env("MOM_NODE_ID", node)
        .env("MOM_STATE_DIR", state.path())
        .env("MSB_HOME", msb_home.path())
        .env("MOM_API_URL", api_url)
        .env("MOM_WORKER_BIND", &bind)
        .env("MOM_WORKER_URL", format!("http://{bind}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn worker {node}"))?;
    Ok(TestNode {
        _state: state,
        msb_home,
        _process: ChildGuard { child },
    })
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
