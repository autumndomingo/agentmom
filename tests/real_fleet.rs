use std::{
    env,
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{RequestBuilder, StatusCode, header};
use serde_json::{Value, json};
use tokio::sync::OnceCell;

static ADMIN_SESSION: OnceCell<AdminSession> = OnceCell::const_new();

#[derive(Clone)]
struct RealFleet {
    client: reqwest::Client,
    api_url: String,
    worker_token: String,
    basic_auth: Option<(String, String)>,
    node_a: String,
}

#[derive(Clone)]
struct AdminSession {
    cookie: String,
}

#[tokio::test]
#[ignore = "requires AGENTMOM_REAL_API_URL and AGENTMOM_REAL_WORKER_TOKEN"]
async fn real_api_health_metrics_and_worker_auth() -> Result<()> {
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
    for expected in [
        "agentmom_workspaces",
        "agentmom_workspaces_by_status",
        "agentmom_jobs",
        "agentmom_nodes",
        "agentmom_nodes_stale",
        "agentmom_oldest_queued_job_age_seconds",
    ] {
        assert!(
            metrics.contains(expected),
            "metrics should contain {expected}, got:\n{metrics}"
        );
    }

    let unauthenticated = fleet
        .request(fleet.client.get(format!(
            "{}/worker/workspaces?node_id={}",
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
                    "{}/worker/workspaces?node_id={}",
                    fleet.api_url, fleet.node_a
                ))
                .bearer_auth(&fleet.worker_token),
        )
        .send()
        .await?;
    assert!(
        authenticated.status().is_success(),
        "worker feed with token should return success, got {}",
        authenticated.status()
    );

    Ok(())
}

#[tokio::test]
#[ignore = "requires AGENTMOM_REAL_ALLOW_CREATE=1 and admin credentials"]
async fn real_unknown_explicit_node_is_rejected() -> Result<()> {
    let fleet = RealFleet::from_env()?;
    fleet.require_create_enabled()?;
    let workspace = unique_workspace("badnode");
    let response = fleet
        .admin_request(
            fleet
                .client
                .post(format!("{}/api/workspaces", fleet.api_url)),
        )
        .await?
        .json(&json!({
            "name": workspace,
            "node_id": format!("missing-node-{}", now_millis()?)
        }))
        .send()
        .await?;
    assert!(
        !response.status().is_success(),
        "unknown node create should fail, got {}",
        response.status()
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

    let result = async {
        let create = fleet
            .create_workspace(&workspace, Some(&fleet.node_a), 0)
            .await?;
        fleet.wait_for_job_status(&create, "succeeded").await?;
        fleet
            .wait_for_workspace_node(&workspace, &fleet.node_a)
            .await?;

        let output = fleet
            .execute(
                &workspace,
                format!(
                    "printf '%s\\n' {marker:?} > /workspace/agentmom-real-marker && cat /workspace/agentmom-real-marker"
                ),
            )
            .await?;
        assert!(
            output.contains(&marker),
            "execute output should contain marker {marker:?}, got {output:?}"
        );
        let hermes_bins = fleet
            .execute(
                &workspace,
                "set -eu\ncommand -v hermes\ncommand -v hermes-acp\nhermes --help >/dev/null"
                    .to_string(),
            )
            .await?;
        assert!(
            hermes_bins.contains("hermes") && hermes_bins.contains("hermes-acp"),
            "Hermes binaries should be present in the guest, got {hermes_bins:?}"
        );
        let hermes_url = fleet.open_hermes_ui(&workspace).await?;
        assert!(
            hermes_url.starts_with("http://") || hermes_url.starts_with("https://"),
            "Hermes UI open should return a URL, got {hermes_url:?}"
        );
        Ok(())
    }
    .await;
    fleet.finish_with_cleanup(&workspace, result).await
}

#[tokio::test]
#[ignore = "requires AGENTMOM_REAL_ALLOW_CREATE=1 and AGENTMOM_REAL_ALLOW_BACKUP=1"]
async fn real_backup_restore_roundtrip_marker() -> Result<()> {
    let fleet = RealFleet::from_env()?;
    fleet.require_create_enabled()?;
    if env::var("AGENTMOM_REAL_ALLOW_BACKUP").ok().as_deref() != Some("1") {
        bail!("set AGENTMOM_REAL_ALLOW_BACKUP=1 to run restic backup/restore smoke");
    }
    let workspace = unique_workspace("backup");
    let marker = format!("agentmom-backup-marker-{}", now_millis()?);

    let result = async {
        let create = fleet
            .create_workspace(&workspace, Some(&fleet.node_a), 0)
            .await?;
        fleet.wait_for_job_status(&create, "succeeded").await?;

        fleet
            .execute(
                &workspace,
                format!("printf '%s\\n' {marker:?} > /workspace/agentmom-real-marker"),
            )
            .await?;

        let backup = fleet.create_job(&workspace, "backup", json!({})).await?;
        fleet.wait_for_job_status(&backup, "succeeded").await?;

        let events = fleet.workspace_events(&workspace).await?;
        let artifact = latest_backup_artifact(&events)?;

        fleet
            .execute(
                &workspace,
                "printf '%s\\n' overwritten > /workspace/agentmom-real-marker".to_string(),
            )
            .await?;

        let restore = fleet
            .create_job(
                &workspace,
                "restore",
                json!({
                    "backup_id": artifact.backup_id,
                    "backup_location": artifact.location,
                    "backup_workspace_name": workspace,
                    "desired_state": "running"
                }),
            )
            .await?;
        fleet.wait_for_job_status(&restore, "succeeded").await?;

        let restored = fleet
            .execute(
                &workspace,
                "cat /workspace/agentmom-real-marker".to_string(),
            )
            .await?;
        assert!(
            restored.contains(&marker),
            "restored marker should contain {marker:?}, got {restored:?}"
        );
        Ok(())
    }
    .await;
    fleet.finish_with_cleanup(&workspace, result).await
}

#[tokio::test]
#[ignore = "requires AGENTMOM_REAL_ALLOW_CATALOG_BACKUP=1 and AGENTMOM_REAL_API_SSH_HOST"]
async fn real_catalog_backup_and_restore_drill_over_ssh() -> Result<()> {
    if env::var("AGENTMOM_REAL_ALLOW_CATALOG_BACKUP")
        .ok()
        .as_deref()
        != Some("1")
    {
        bail!("set AGENTMOM_REAL_ALLOW_CATALOG_BACKUP=1 to run catalog backup drill");
    }
    let host =
        env::var("AGENTMOM_REAL_API_SSH_HOST").context("AGENTMOM_REAL_API_SSH_HOST is required")?;
    let state_dir = env::var("AGENTMOM_REAL_CATALOG_STATE_DIR")
        .unwrap_or_else(|_| "/var/lib/agentmom".to_string());
    let state_dir_q = shell_quote(&state_dir);
    let script = format!(
        r#"
set -eu
export MOM_STATE_DIR={state_dir_q}
mom_bin="$(command -v mom || true)"
if [ -z "$mom_bin" ]; then
  mom_bin="$(systemctl show -p ExecStart --value agentmom-api.service | sed -n 's/.*path=\([^ ;]*\/mom\).*/\1/p')"
fi
test -x "$mom_bin"
service_user="${{AGENTMOM_REAL_API_SERVICE_USER:-}}"
if [ -z "$service_user" ]; then
  service_user="$(systemctl show -p User --value agentmom-api.service || true)"
fi
if [ -n "$service_user" ]; then
  run_as_service() {{ sudo -u "$service_user" "$@"; }}
else
  run_as_service() {{ "$@"; }}
fi
run_as_service env MOM_STATE_DIR="$MOM_STATE_DIR" "$mom_bin" db status
sudo systemctl start agentmom-catalog-backup.service
latest="$(run_as_service sh -c 'ls -1t "$1"/catalog-backups/fleet-*.db | head -1' sh "$MOM_STATE_DIR")"
test -n "$latest"
tmpdir="$(run_as_service mktemp -d "$MOM_STATE_DIR"/catalog-restore-drill.XXXXXX)"
cleanup() {{ run_as_service rm -rf "$tmpdir"; }}
trap cleanup EXIT
run_as_service cp "$latest" "$tmpdir/fleet.db"
run_as_service env MOM_STATE_DIR="$tmpdir" "$mom_bin" db status
"#,
    );
    let output = Command::new("ssh")
        .arg(host)
        .arg(script)
        .output()
        .context("run remote catalog backup drill")?;
    assert!(
        output.status.success(),
        "catalog backup drill failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Schema version: 4"),
        "remote drill should report schema version 4, got {stdout:?}"
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
        let node_a = env::var("AGENTMOM_REAL_NODE_A").unwrap_or_else(|_| "mom-1".to_string());
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
            .timeout(Duration::from_secs(360))
            .build()?;
        Ok(Self {
            client,
            api_url,
            worker_token,
            basic_auth,
            node_a,
        })
    }

    fn request(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.basic_auth {
            Some((user, password)) => request.basic_auth(user, Some(password)),
            None => request,
        }
    }

    async fn admin_request(&self, request: RequestBuilder) -> Result<RequestBuilder> {
        let session = self.admin_session().await?;
        Ok(self
            .request(request)
            .header(header::COOKIE, session.cookie.clone()))
    }

    async fn admin_session(&self) -> Result<AdminSession> {
        ADMIN_SESSION
            .get_or_try_init(|| async { self.login_admin().await })
            .await
            .cloned()
    }

    async fn login_admin(&self) -> Result<AdminSession> {
        let email = env::var("AGENTMOM_REAL_ADMIN_EMAIL")
            .context("AGENTMOM_REAL_ADMIN_EMAIL is required; use the intended prod admin email")?;
        let password = env::var("AGENTMOM_REAL_ADMIN_PASSWORD")
            .context("AGENTMOM_REAL_ADMIN_PASSWORD is required")?;
        let response = self
            .request(
                self.client
                    .post(format!("{}/api/auth/login", self.api_url))
                    .json(&json!({ "email": email, "password": password })),
            )
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("admin login failed with {status}: {body}");
        }
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .ok_or_else(|| anyhow!("admin login response did not set a session cookie"))?
            .to_str()
            .context("admin login Set-Cookie header is not valid UTF-8")?
            .to_string();
        Ok(AdminSession { cookie })
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
            .admin_request(self.client.post(format!("{}/api/workspaces", self.api_url)))
            .await?
            .json(&body)
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
            .admin_request(self.client.post(format!("{}/api/jobs", self.api_url)))
            .await?
            .json(&json!({
                "workspace_name": workspace,
                "kind": kind,
                "payload": payload
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

    async fn execute(&self, workspace: &str, script: String) -> Result<String> {
        let execute = self
            .create_job(
                workspace,
                "execute",
                json!({
                    "command": ["sh", "-lc", script]
                }),
            )
            .await?;
        let job = self.wait_for_job_status(&execute, "succeeded").await?;
        Ok(job_output_text(&job))
    }

    async fn open_hermes_ui(&self, workspace: &str) -> Result<String> {
        let response = self
            .admin_request(self.client.post(format!(
                "{}/api/workspaces/{workspace}/hermes-ui",
                self.api_url
            )))
            .await?
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        response
            .get("stdout")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| anyhow!("Hermes UI response did not include stdout URL: {response}"))
    }

    async fn finish_with_cleanup<T>(&self, workspace: &str, result: Result<T>) -> Result<T> {
        let cleanup = self.cleanup_workspace(workspace).await;
        match (result, cleanup) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(error)) => Err(error.context("real fleet workspace cleanup failed")),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup_error)) => Err(error.context(format!(
                "real fleet workspace cleanup also failed: {cleanup_error:#}"
            ))),
        }
    }

    async fn cleanup_workspace(&self, workspace: &str) -> Result<()> {
        let _ = self.create_job(workspace, "stop", json!({})).await;
        let remove = self
            .create_job(workspace, "remove", json!({ "remove_workspace_dir": true }))
            .await?;
        self.wait_for_job_status(&remove, "succeeded").await?;
        let workspace = self.workspace(workspace).await?;
        if workspace.get("status").and_then(Value::as_str) != Some("removed") {
            bail!("workspace cleanup did not mark removed: {workspace}");
        }
        Ok(())
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

    async fn wait_until<F, Fut>(&self, label: &str, mut check: F) -> Result<Value>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<Option<Value>>>,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(900);
        while tokio::time::Instant::now() < deadline {
            if let Some(value) = check().await? {
                return Ok(value);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        bail!("timed out waiting for {label}")
    }

    async fn job(&self, job_id: &str) -> Result<Value> {
        self.admin_request(
            self.client
                .get(format!("{}/api/jobs/{job_id}", self.api_url)),
        )
        .await?
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await
        .map_err(Into::into)
    }

    async fn workspace(&self, name: &str) -> Result<Value> {
        let values = self
            .admin_request(self.client.get(format!("{}/api/workspaces", self.api_url)))
            .await?
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
        self.admin_request(
            self.client
                .get(format!("{}/api/workspaces/{name}/events", self.api_url)),
        )
        .await?
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<Value>>()
        .await
        .map_err(Into::into)
    }
}

struct BackupArtifact {
    backup_id: String,
    location: String,
}

fn latest_backup_artifact(events: &[Value]) -> Result<BackupArtifact> {
    events
        .iter()
        .rev()
        .find_map(|event| {
            (event.get("event_type").and_then(Value::as_str) == Some("workspace_backup_succeeded"))
                .then(|| event.get("metadata_json").and_then(Value::as_str))
                .flatten()
        })
        .map(|metadata| -> Result<BackupArtifact> {
            let metadata = serde_json::from_str::<Value>(metadata)
                .context("parse workspace_backup_succeeded metadata_json")?;
            let backup_id = metadata
                .get("backup_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("backup event missing backup_id: {metadata}"))?
                .to_string();
            let location = metadata
                .get("location")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("backup event missing location: {metadata}"))?
                .to_string();
            Ok(BackupArtifact {
                backup_id,
                location,
            })
        })
        .transpose()?
        .ok_or_else(|| anyhow!("workspace events did not include workspace_backup_succeeded"))
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
