use std::{
    env,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{StatusCode, header};
use serde_json::{Value, json};

#[tokio::test]
#[ignore = "requires just dev-utm plus AGENTMOM_UTM_E2E=1"]
async fn utm_user_workspace_hermes_inference_e2e() -> Result<()> {
    if env::var("AGENTMOM_UTM_E2E").ok().as_deref() != Some("1") {
        bail!("set AGENTMOM_UTM_E2E=1 to run the mutating UTM e2e test");
    }

    let e2e = UtmE2e::from_env()?;
    e2e.wait_ready().await?;

    let admin = e2e.login_admin().await?;
    let invite = e2e.create_invite(&admin).await?;
    let user_email = format!("utm-e2e-{}@example.com", now_millis()?);
    let user = e2e.login(&user_email, Some(&invite)).await?;
    assert_eq!(
        user.value.pointer("/user/role").and_then(Value::as_str),
        Some("user")
    );
    assert!(
        user.value
            .pointer("/user/code")
            .and_then(Value::as_str)
            .is_some_and(|code| !code.is_empty()),
        "new user login should return a reusable user code: {}",
        user.value
    );

    let workspace = e2e
        .setup_user_workspace(&user, "Agent Mom UTM E2E", "Hermes E2E")
        .await?;
    let result = async {
        e2e.wait_workspace_status(&user, &workspace, "running")
            .await?;

        let proxy = e2e
            .execute(
                &user,
                &workspace,
                r#"
python3 - <<'PY'
import urllib.request

req = urllib.request.Request(
    "https://openrouter.ai/api/v1/models",
    headers={"User-Agent": "agentmom-utm-e2e"},
)
with urllib.request.urlopen(req, timeout=45) as response:
    if response.status != 200:
        raise SystemExit(f"unexpected status {response.status}")
    response.read(2048)
print("proxy smoke ok")
PY
"#,
            )
            .await?;
        assert!(
            proxy.contains("proxy smoke ok"),
            "proxy smoke should succeed, got {proxy:?}"
        );

        let help = e2e
            .hermes(&user, &workspace, json!({ "args": ["--help"] }))
            .await?;
        let help_output = job_output_text(&help);
        assert!(
            help_output.contains("Hermes") || help_output.to_lowercase().contains("usage"),
            "Hermes help should print usage text, got {help_output:?}"
        );

        let dashboard_url = e2e.open_hermes_dashboard(&user, &workspace).await?;
        assert!(
            dashboard_url.starts_with("http://") || dashboard_url.starts_with("https://"),
            "Hermes dashboard open should return a URL, got {dashboard_url:?}"
        );

        let model = env::var("AGENTMOM_UTM_HERMES_MODEL")
            .unwrap_or_else(|_| "openai/gpt-4o-mini".to_string());
        let inference = e2e
            .hermes(
                &user,
                &workspace,
                json!({
                    "args": [
                        "-z",
                        "Reply with exactly: OK",
                        "-m",
                        model,
                        "--provider",
                        "openrouter"
                    ]
                }),
            )
            .await?;
        let inference_output = job_output_text(&inference);
        assert!(
            inference_output.contains("OK"),
            "Hermes inference should contain OK, got {inference_output:?}"
        );

        let failed = e2e
            .create_job(
                &user,
                &workspace,
                "execute",
                json!({ "command": ["sh", "-lc", "echo expected-failure >&2; exit 7"] }),
            )
            .await?;
        let failed = e2e.wait_job_status(&user, &failed, "failed").await?;
        let failed_output = failed
            .pointer("/job/output_json")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            failed_output.contains("\"ok\":false") || failed_output.contains("\"code\":7"),
            "failed guest command should be persisted as failed with command output, got {failed}"
        );

        Ok(())
    }
    .await;

    e2e.finish_with_cleanup(&admin, &workspace, user.user_id(), result)
        .await
}

struct UtmE2e {
    client: reqwest::Client,
    api_url: String,
}

struct Session {
    cookie: String,
    value: Value,
}

impl Session {
    fn user_id(&self) -> Option<i64> {
        self.value.pointer("/user/id").and_then(Value::as_i64)
    }
}

impl UtmE2e {
    fn from_env() -> Result<Self> {
        let api_url = env::var("AGENTMOM_UTM_API_URL")
            .or_else(|_| env::var("MOM_API_URL"))
            .unwrap_or_else(|_| "http://127.0.0.1:8787".to_string())
            .trim_end_matches('/')
            .to_string();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(900))
            .build()?;
        Ok(Self { client, api_url })
    }

    async fn wait_ready(&self) -> Result<()> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while tokio::time::Instant::now() < deadline {
            if let Ok(response) = self
                .client
                .get(format!("{}/health/ready", self.api_url))
                .send()
                .await
                && response.status().is_success()
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        bail!("timed out waiting for UTM dev API at {}", self.api_url)
    }

    async fn login_admin(&self) -> Result<Session> {
        let email = env::var("AGENTMOM_UTM_ADMIN_EMAIL")
            .unwrap_or_else(|_| "admin@example.com".to_string());
        let code = env::var("AGENTMOM_UTM_ADMIN_USER_CODE").ok();
        let session = self.login(&email, code.as_deref()).await.with_context(|| {
            format!(
                "admin login failed for {email}; for an existing dev DB set AGENTMOM_UTM_ADMIN_USER_CODE"
            )
        })?;
        if session.value.pointer("/user/role").and_then(Value::as_str) != Some("admin") {
            bail!(
                "UTM e2e admin login did not return an admin user: {}",
                session.value
            );
        }
        Ok(session)
    }

    async fn login(&self, email: &str, access_code: Option<&str>) -> Result<Session> {
        let mut body = json!({ "email": email });
        if let Some(access_code) = access_code {
            body["access_code"] = json!(access_code);
        }
        let response = self
            .client
            .post(format!("{}/api/auth/login", self.api_url))
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("login failed with {status}: {body}");
        }
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .ok_or_else(|| anyhow!("login response did not set a session cookie"))?
            .to_str()
            .context("login Set-Cookie header is not valid UTF-8")?
            .to_string();
        let value = response.json::<Value>().await?;
        Ok(Session { cookie, value })
    }

    async fn create_invite(&self, admin: &Session) -> Result<String> {
        let response = self
            .request(
                admin,
                self.client
                    .post(format!("{}/api/admin/invites", self.api_url)),
            )
            .json(&json!({
                "label": format!("UTM e2e {}", now_millis()?),
                "max_uses": 1
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        response
            .get("code")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| anyhow!("invite response missing code: {response}"))
    }

    async fn setup_user_workspace(
        &self,
        user: &Session,
        full_name: &str,
        agent_name: &str,
    ) -> Result<String> {
        let response = self
            .request(
                user,
                self.client.post(format!("{}/api/me/setup", self.api_url)),
            )
            .json(&json!({
                "full_name": full_name,
                "agent_name": agent_name
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        response
            .pointer("/workspace/name")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| anyhow!("setup response missing workspace name: {response}"))
    }

    async fn wait_workspace_status(
        &self,
        session: &Session,
        workspace: &str,
        status: &str,
    ) -> Result<Value> {
        let label = format!("workspace {workspace} status {status}");
        self.wait_until(&label, || async {
            let workspaces = self
                .request(
                    session,
                    self.client.get(format!("{}/api/workspaces", self.api_url)),
                )
                .send()
                .await?
                .error_for_status()?
                .json::<Vec<Value>>()
                .await?;
            Ok(workspaces.into_iter().find(|candidate| {
                candidate.get("name").and_then(Value::as_str) == Some(workspace)
                    && candidate.get("status").and_then(Value::as_str) == Some(status)
            }))
        })
        .await
    }

    async fn execute(&self, session: &Session, workspace: &str, script: &str) -> Result<String> {
        let job = self
            .create_job(
                session,
                workspace,
                "execute",
                json!({ "command": ["sh", "-lc", script] }),
            )
            .await?;
        let job = self.wait_job_status(session, &job, "succeeded").await?;
        Ok(job_output_text(&job))
    }

    async fn hermes(&self, session: &Session, workspace: &str, payload: Value) -> Result<Value> {
        let job = self
            .create_job(session, workspace, "hermes", payload)
            .await?;
        self.wait_job_status(session, &job, "succeeded").await
    }

    async fn open_hermes_dashboard(&self, session: &Session, workspace: &str) -> Result<String> {
        let response = self
            .request(
                session,
                self.client.post(format!(
                    "{}/api/workspaces/{workspace}/hermes-ui",
                    self.api_url
                )),
            )
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
            .ok_or_else(|| anyhow!("Hermes dashboard response missing URL: {response}"))
    }

    async fn create_job(
        &self,
        session: &Session,
        workspace: &str,
        kind: &str,
        payload: Value,
    ) -> Result<String> {
        let response = self
            .request(
                session,
                self.client.post(format!("{}/api/jobs", self.api_url)),
            )
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
            .ok_or_else(|| anyhow!("create job response missing id: {response}"))
    }

    async fn wait_job_status(
        &self,
        session: &Session,
        job_id: &str,
        status: &str,
    ) -> Result<Value> {
        let label = format!("job {job_id} status {status}");
        self.wait_until(&label, || async {
            let job = self
                .request(
                    session,
                    self.client
                        .get(format!("{}/api/jobs/{job_id}", self.api_url)),
                )
                .send()
                .await?
                .error_for_status()?
                .json::<Value>()
                .await?;
            let current = job.pointer("/job/status").and_then(Value::as_str);
            if current == Some(status) {
                return Ok(Some(job));
            }
            if matches!(current, Some("failed" | "canceled")) && status != "failed" {
                bail!("job {job_id} reached terminal non-success state: {job}");
            }
            Ok(None)
        })
        .await
    }

    async fn finish_with_cleanup<T>(
        &self,
        admin: &Session,
        workspace: &str,
        user_id: Option<i64>,
        result: Result<T>,
    ) -> Result<T> {
        let cleanup = self.cleanup(admin, workspace, user_id).await;
        match (result, cleanup) {
            (Ok(value), Ok(())) => Ok(value),
            (Ok(_), Err(error)) => Err(error.context("UTM e2e cleanup failed")),
            (Err(error), Ok(())) => Err(error),
            (Err(error), Err(cleanup_error)) => {
                Err(error.context(format!("UTM e2e cleanup also failed: {cleanup_error:#}")))
            }
        }
    }

    async fn cleanup(&self, admin: &Session, workspace: &str, user_id: Option<i64>) -> Result<()> {
        if !workspace.is_empty()
            && let Ok(remove) = self
                .create_job(
                    admin,
                    workspace,
                    "remove",
                    json!({ "remove_workspace_dir": true }),
                )
                .await
        {
            let _ = self.wait_job_status(admin, &remove, "succeeded").await;
        }
        if let Some(user_id) = user_id {
            let response = self
                .request(
                    admin,
                    self.client
                        .delete(format!("{}/api/admin/users/{user_id}", self.api_url)),
                )
                .send()
                .await?;
            if !matches!(response.status(), StatusCode::OK | StatusCode::NOT_FOUND) {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                bail!("delete e2e user failed with {status}: {body}");
            }
        }
        Ok(())
    }

    async fn wait_until<F, Fut>(&self, label: &str, mut check: F) -> Result<Value>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<Option<Value>>>,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1200);
        while tokio::time::Instant::now() < deadline {
            if let Some(value) = check().await? {
                return Ok(value);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        bail!("timed out waiting for {label}")
    }

    fn request(
        &self,
        session: &Session,
        request: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        request.header(header::COOKIE, session.cookie.clone())
    }
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
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return error.to_string();
    }
    value.to_string()
}

fn now_millis() -> Result<u128> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}
