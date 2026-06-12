use std::{
    env,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{RequestBuilder, StatusCode};
use serde_json::{Value, json};

#[derive(Clone)]
struct RealFleet {
    client: reqwest::Client,
    api_url: String,
    worker_token: String,
    basic_auth: Option<(String, String)>,
    node_a: String,
    node_b: Option<String>,
}

#[tokio::test]
#[ignore = "requires AGENTMOM_REAL_API_URL and AGENTMOM_REAL_WORKER_TOKEN"]
async fn real_api_health_metrics_and_worker_sse_auth() -> Result<()> {
    let fleet = RealFleet::from_env()?;

    let ready = fleet
        .request(fleet.client.get(format!("{}/health/ready", fleet.api_url)))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    assert_eq!(ready.get("ok").and_then(Value::as_bool), Some(true));

    let metrics = fleet
        .request(fleet.client.get(format!("{}/metrics", fleet.api_url)))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    assert!(metrics.contains("agentmom_workspaces"));
    assert!(metrics.contains("agentmom_jobs"));

    let unauthenticated = fleet
        .request(fleet.client.get(format!(
            "{}/worker/events?node_id={}",
            fleet.api_url, fleet.node_a
        )))
        .send()
        .await?;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let authenticated = fleet
        .request(
            fleet
                .client
                .get(format!(
                    "{}/worker/events?node_id={}",
                    fleet.api_url, fleet.node_a
                ))
                .bearer_auth(&fleet.worker_token),
        )
        .send()
        .await?;
    assert!(
        authenticated.status().is_success(),
        "worker SSE with token should return success, got {}",
        authenticated.status()
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires AGENTMOM_REAL_ALLOW_CREATE=1 and real worker capacity"]
async fn real_create_workspace_and_run_marker_job() -> Result<()> {
    let fleet = RealFleet::from_env()?;
    fleet.require_create_enabled()?;
    let workspace = unique_workspace("real");
    let marker = format!("agentmom-marker-{}", now_millis()?);

    let create = fleet
        .create_workspace(&workspace, Some(&fleet.node_a), 0)
        .await?;
    fleet.wait_for_job_status(&create, "succeeded").await?;
    fleet
        .wait_for_workspace_node(&workspace, &fleet.node_a)
        .await?;

    let execute = fleet
        .create_job(
            &workspace,
            "execute",
            json!({
                "command": [
                    "sh",
                    "-lc",
                    format!("printf '%s\\n' {marker:?} > /workspace/agentmom-real-marker && cat /workspace/agentmom-real-marker")
                ]
            }),
        )
        .await?;
    let job = fleet.wait_for_job_status(&execute, "succeeded").await?;
    let output = job_output_text(&job);
    assert!(
        output.contains(&marker),
        "execute output should contain marker {marker:?}, got {output:?}"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires AGENTMOM_REAL_ALLOW_CREATE=1 and AGENTMOM_REAL_ALLOW_BACKUP=1"]
async fn real_backup_smoke_records_artifact() -> Result<()> {
    let fleet = RealFleet::from_env()?;
    fleet.require_create_enabled()?;
    if env::var("AGENTMOM_REAL_ALLOW_BACKUP").ok().as_deref() != Some("1") {
        bail!("set AGENTMOM_REAL_ALLOW_BACKUP=1 to run restic backup smoke");
    }
    let workspace = unique_workspace("backup");

    let create = fleet
        .create_workspace(&workspace, Some(&fleet.node_a), 0)
        .await?;
    fleet.wait_for_job_status(&create, "succeeded").await?;

    let backup = fleet.create_job(&workspace, "backup", json!({})).await?;
    let job = fleet.wait_for_job_status(&backup, "succeeded").await?;
    let output = job_output_text(&job);
    assert!(
        output.contains("backed_up") || output.is_empty(),
        "backup job should succeed, got output {output:?}"
    );

    let events = fleet.workspace_events(&workspace).await?;
    assert!(
        events.iter().any(|event| {
            event
                .get("event_type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "workspace_backup_succeeded")
        }),
        "workspace events should include workspace_backup_succeeded: {events:?}"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires AGENTMOM_REAL_ALLOW_CREATE=1 and two worker nodes"]
async fn real_two_node_explicit_placement_smoke() -> Result<()> {
    let fleet = RealFleet::from_env()?;
    fleet.require_create_enabled()?;
    let node_b = fleet
        .node_b
        .as_deref()
        .ok_or_else(|| anyhow!("set AGENTMOM_REAL_NODE_B to run two-node placement smoke"))?;
    let workspace = unique_workspace("nodeb");

    let create = fleet.create_workspace(&workspace, Some(node_b), 0).await?;
    fleet.wait_for_job_status(&create, "succeeded").await?;
    fleet.wait_for_workspace_node(&workspace, node_b).await?;

    let stop = fleet.create_job(&workspace, "stop", json!({})).await?;
    fleet.wait_for_job_status(&stop, "succeeded").await?;
    fleet
        .wait_for_workspace_status(&workspace, "stopped")
        .await?;

    Ok(())
}

#[tokio::test]
#[ignore = "requires AGENTMOM_REAL_API_URL; does not create workspaces"]
async fn real_unknown_explicit_node_is_rejected() -> Result<()> {
    let fleet = RealFleet::from_env()?;
    let workspace = unique_workspace("badnode");
    let response = fleet
        .request(
            fleet
                .client
                .post(format!("{}/api/workspaces", fleet.api_url))
                .json(&json!({
                    "name": workspace,
                    "node_id": format!("missing-node-{}", now_millis()?)
                })),
        )
        .send()
        .await?;
    assert!(
        !response.status().is_success(),
        "unknown node create should fail, got {}",
        response.status()
    );
    Ok(())
}

impl RealFleet {
    fn from_env() -> Result<Self> {
        let api_url = env::var("AGENTMOM_REAL_API_URL")
            .context("AGENTMOM_REAL_API_URL is required")?
            .trim_end_matches('/')
            .to_string();
        let worker_token = env::var("AGENTMOM_REAL_WORKER_TOKEN")
            .context("AGENTMOM_REAL_WORKER_TOKEN is required")?;
        let node_a = env::var("AGENTMOM_REAL_NODE_A").unwrap_or_else(|_| "pika-build".to_string());
        let node_b = env::var("AGENTMOM_REAL_NODE_B").ok();
        let basic_auth = env::var("AGENTMOM_REAL_BASIC_AUTH")
            .ok()
            .map(|value| {
                value
                    .split_once(':')
                    .map(|(user, password)| (user.to_string(), password.to_string()))
                    .ok_or_else(|| anyhow!("AGENTMOM_REAL_BASIC_AUTH must be user:password"))
            })
            .transpose()?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            api_url,
            worker_token,
            basic_auth,
            node_a,
            node_b,
        })
    }

    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.basic_auth {
            Some((user, password)) => request.basic_auth(user, Some(password)),
            None => request,
        }
    }

    fn require_create_enabled(&self) -> Result<()> {
        if env::var("AGENTMOM_REAL_ALLOW_CREATE").ok().as_deref() == Some("1") {
            Ok(())
        } else {
            bail!("set AGENTMOM_REAL_ALLOW_CREATE=1 to run workspace-creating real-host tests")
        }
    }

    async fn create_workspace(
        &self,
        name: &str,
        node_id: Option<&str>,
        backup_interval: u64,
    ) -> Result<String> {
        let mut body = json!({
            "name": name,
            "backup_interval": backup_interval
        });
        if let Some(node_id) = node_id {
            body["node_id"] = json!(node_id);
        }
        let response = self
            .request(
                self.client
                    .post(format!("{}/api/workspaces", self.api_url))
                    .json(&body),
            )
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

    async fn create_job(&self, workspace: &str, kind: &str, payload: Value) -> Result<String> {
        let response = self
            .request(
                self.client
                    .post(format!("{}/api/jobs", self.api_url))
                    .json(&json!({
                        "workspace_name": workspace,
                        "kind": kind,
                        "payload": payload
                    })),
            )
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

    async fn wait_for_job_status(&self, job_id: &str, status: &str) -> Result<Value> {
        let label = format!("job {job_id} status {status}");
        self.wait_until(&label, || async {
            let job = self.job(job_id).await?;
            if job.pointer("/job/status").and_then(Value::as_str) == Some(status) {
                return Ok(Some(job));
            }
            if matches!(
                job.pointer("/job/status").and_then(Value::as_str),
                Some("failed" | "canceled")
            ) {
                bail!("job {job_id} reached terminal non-success state: {job}");
            }
            Ok(None)
        })
        .await
    }

    async fn wait_for_workspace_node(&self, name: &str, node: &str) -> Result<Value> {
        let label = format!("workspace {name} node {node}");
        self.wait_until(&label, || async {
            let workspace = self.workspace(name).await?;
            Ok(
                (workspace.get("node_id").and_then(Value::as_str) == Some(node))
                    .then_some(workspace),
            )
        })
        .await
    }

    async fn wait_for_workspace_status(&self, name: &str, status: &str) -> Result<Value> {
        let label = format!("workspace {name} status {status}");
        self.wait_until(&label, || async {
            let workspace = self.workspace(name).await?;
            Ok(
                (workspace.get("status").and_then(Value::as_str) == Some(status))
                    .then_some(workspace),
            )
        })
        .await
    }

    async fn wait_until<F, Fut>(&self, label: &str, mut check: F) -> Result<Value>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<Option<Value>>>,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
        while tokio::time::Instant::now() < deadline {
            if let Some(value) = check().await? {
                return Ok(value);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        bail!("timed out waiting for {label}")
    }

    async fn job(&self, job_id: &str) -> Result<Value> {
        self.request(
            self.client
                .get(format!("{}/api/jobs/{job_id}", self.api_url)),
        )
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await
        .map_err(Into::into)
    }

    async fn workspace(&self, name: &str) -> Result<Value> {
        let values = self
            .request(self.client.get(format!("{}/api/workspaces", self.api_url)))
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

    async fn workspace_events(&self, name: &str) -> Result<Vec<Value>> {
        self.request(
            self.client
                .get(format!("{}/api/workspaces/{name}/events", self.api_url)),
        )
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<Value>>()
        .await
        .map_err(Into::into)
    }
}

fn unique_workspace(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        now_millis().unwrap_or_default()
    )
}

fn now_millis() -> Result<u128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}

fn job_output_text(job: &Value) -> String {
    let Some(raw) = job.pointer("/job/output_json").and_then(Value::as_str) else {
        return String::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return raw.to_string();
    };
    if let Some(stdout) = value.get("stdout").and_then(Value::as_str) {
        let stderr = value.get("stderr").and_then(Value::as_str).unwrap_or("");
        return [stdout, stderr]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
    }
    value.to_string()
}
