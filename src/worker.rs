use super::*;

#[derive(Clone)]
struct WorkerState {
    api: WorkerApi,
    acp: acp::HermesAcpState,
    services: service::ServiceState,
}

#[derive(Clone)]
struct WorkerApi {
    client: reqwest::Client,
    api_url: String,
    node: String,
    runtime: RuntimeMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuntimeMode {
    Microvm,
    Fake,
}

impl RuntimeMode {
    fn from_env() -> Self {
        if env::var("MOM_RUNTIME").is_ok_and(|value| value == "fake") {
            Self::Fake
        } else {
            Self::Microvm
        }
    }

    fn is_fake(self) -> bool {
        matches!(self, Self::Fake)
    }
}

#[derive(Debug, Deserialize)]
struct OpenServiceRequest {
    workspace_name: String,
    vm_name: String,
}

pub(crate) async fn worker(args: WorkerArgs) -> Result<()> {
    let node = node_id()?;
    let client = worker_api_client()?;
    let sse_client = worker_sse_client()?;
    let api_url = args.api_url.trim_end_matches('/').to_string();
    let runtime = RuntimeMode::from_env();
    let worker_api = WorkerApi {
        client: client.clone(),
        api_url: api_url.clone(),
        node: node.clone(),
        runtime,
    };
    let worker_url = args
        .worker_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", args.bind));
    let addr: SocketAddr = args
        .bind
        .parse()
        .with_context(|| format!("parse worker bind address {}", args.bind))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind worker HTTP {addr}"))?;
    let state = Arc::new(WorkerState {
        api: worker_api.clone(),
        acp: acp::HermesAcpState::default(),
        services: service::ServiceState::default(),
    });
    let worker_http = tokio::spawn(run_worker_http(listener, state));
    register_worker(&client, &api_url, &node, &worker_url).await?;
    let (wake_tx, mut wake_rx) = mpsc::channel::<()>(32);
    let sse_url = api_url.clone();
    let sse_node = node.clone();
    let sse_task = tokio::spawn(async move {
        worker_sse_loop(sse_client, sse_url, sse_node, wake_tx).await;
    });

    log_record("info", "worker_start", None, "Agent Mom worker starting");
    let mut shutdown = Box::pin(shutdown_signal());
    loop {
        let mut claimed = false;
        match worker_claim_once(&worker_api, &worker_url).await {
            Ok(value) => claimed = value,
            Err(error) => {
                log_record(
                    "error",
                    "worker_claim_failed",
                    None,
                    &format!("worker claim failed: {error:#}"),
                );
                eprintln!("worker claim failed: {error:#}");
            }
        }
        if let Err(error) = worker_reconcile_once(&worker_api).await {
            log_record(
                "error",
                "worker_reconcile_cycle_failed",
                None,
                &format!("worker reconcile cycle failed: {error:#}"),
            );
            eprintln!("worker reconcile cycle failed: {error:#}");
        }
        if args.once {
            sse_task.abort();
            worker_http.abort();
            return Ok(());
        }
        if claimed {
            continue;
        }
        tokio::select! {
            _ = &mut shutdown => {
                log_record("info", "worker_shutdown", None, "Agent Mom worker shutting down");
                sse_task.abort();
                worker_http.abort();
                return Ok(());
            },
            _ = wake_rx.recv() => {},
            _ = tokio::time::sleep(Duration::from_secs(args.interval)) => {},
        }
    }
}

fn worker_api_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .context("build worker API HTTP client")
}

fn worker_sse_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .context("build worker SSE HTTP client")
}

async fn run_worker_http(listener: tokio::net::TcpListener, state: Arc<WorkerState>) -> Result<()> {
    let app = Router::new()
        .route("/worker/health", get(worker_health))
        .route("/worker/hermes-acp/ws", get(worker_hermes_acp_ws))
        .route("/worker/services/hermes/open", post(worker_open_hermes))
        .with_state(state);
    let addr = listener
        .local_addr()
        .context("read worker HTTP listener address")?;
    log_record(
        "info",
        "worker_http_start",
        None,
        &format!("Agent Mom worker HTTP listening on http://{addr}"),
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn worker_health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn worker_open_hermes(
    State(state): State<Arc<WorkerState>>,
    headers: HeaderMap,
    Json(request): Json<OpenServiceRequest>,
) -> Result<Json<Value>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    let workspace = state
        .api
        .workspace(&request.workspace_name)
        .await
        .map_err(ApiError::Anyhow)?;
    if workspace.node_id.as_deref() != Some(&state.api.node) {
        return Err(ApiError::Unauthorized(anyhow!(
            "workspace {} is not assigned to worker {}",
            workspace.name,
            state.api.node
        )));
    }
    if request.vm_name != workspace.vm_name {
        return Err(ApiError::Unauthorized(anyhow!(
            "service-open vm does not match workspace assignment"
        )));
    }
    if state.api.runtime.is_fake() {
        let url = fake_open_hermes(&workspace.name)
            .await
            .map_err(ApiError::Anyhow)?;
        return Ok(Json(json!({ "url": url })));
    }
    let url = service::open_hermes_dashboard(&state.services, &workspace.name, &workspace.vm_name)
        .await?;
    Ok(Json(json!({ "url": url })))
}

async fn worker_hermes_acp_ws(
    State(state): State<Arc<WorkerState>>,
    headers: HeaderMap,
    Query(query): Query<acp::WorkerAcpWsQuery>,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Result<Response, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    let workspace = state
        .api
        .workspace(&query.workspace_name)
        .await
        .map_err(ApiError::Anyhow)?;
    if workspace.node_id.as_deref() != Some(&state.api.node) {
        return Err(ApiError::Unauthorized(anyhow!(
            "workspace {} is not assigned to worker {}",
            workspace.name,
            state.api.node
        )));
    }
    if query.vm_name != workspace.vm_name {
        return Err(ApiError::Unauthorized(anyhow!(
            "Hermes ACP vm does not match workspace assignment"
        )));
    }
    if state.api.runtime.is_fake() {
        let workspace_name = workspace.name.clone();
        return Ok(ws
            .on_upgrade(move |socket| acp::fake_worker_socket(workspace_name, socket))
            .into_response());
    }

    let vm = workspace_running_vm_local(&state.api, &workspace)
        .await
        .map_err(ApiError::Anyhow)?;
    let acp = state.acp.clone();
    let workspace_name = workspace.name.clone();
    let vm_name = workspace.vm_name.clone();
    Ok(ws
        .on_upgrade(move |socket| {
            acp::bridge_worker_socket(acp, workspace_name, vm_name, vm, socket)
        })
        .into_response())
}

async fn register_worker(
    client: &reqwest::Client,
    api_url: &str,
    node: &str,
    worker_url: &str,
) -> Result<()> {
    client
        .post(format!("{api_url}/worker/register"))
        .with_worker_token()
        .json(&json!({
            "node_id": node,
            "capacity": node_capacity(),
            "worker_url": worker_url
        }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn worker_sse_loop(
    client: reqwest::Client,
    api_url: String,
    node: String,
    wake_tx: mpsc::Sender<()>,
) {
    loop {
        let url = format!("{api_url}/worker/events?node_id={}", url_component(&node));
        let result = async {
            let response = client
                .get(url)
                .with_worker_token()
                .send()
                .await?
                .error_for_status()?;
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                chunk?;
                let _ = wake_tx.send(()).await;
            }
            Ok::<(), reqwest::Error>(())
        }
        .await;
        if let Err(error) = result {
            eprintln!("worker SSE disconnected: {error:#}");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn worker_claim_once(api: &WorkerApi, worker_url: &str) -> Result<bool> {
    let records = api.workspaces().await?;
    let pressure = node_pressure(&records).await?;
    let response = api
        .client
        .post(format!("{}/worker/claim", api.api_url))
        .with_worker_token()
        .json(&json!({
            "node_id": api.node,
            "capacity": node_capacity(),
            "pressure": pressure,
            "worker_url": worker_url
        }))
        .send()
        .await?
        .error_for_status()?
        .json::<Option<JobRecord>>()
        .await?;
    let Some(job) = response else {
        return Ok(false);
    };
    run_claimed_job(api, job).await?;
    Ok(true)
}

async fn run_claimed_job(api: &WorkerApi, job: JobRecord) -> Result<()> {
    worker_job_event(
        api,
        WorkerJobEvent {
            job_id: &job.id,
            event_type: "job_running",
            status: "running",
            message: "worker started job",
            metadata: json!({ "kind": job.kind }),
        },
    )
    .await?;
    let heartbeat = spawn_job_heartbeat(api.clone(), job.id.clone(), job.kind.clone());
    let result = execute_job(api, &job).await;
    heartbeat.abort();
    match result {
        Ok(output) => {
            api.client
                .post(format!("{}/worker/jobs/{}/complete", api.api_url, job.id))
                .with_worker_token()
                .json(&json!({
                    "node_id": api.node,
                    "status": "succeeded",
                    "output": output
                }))
                .send()
                .await?
                .error_for_status()?;
            Ok(())
        }
        Err(error) => {
            let message = format!("{error:#}");
            api.client
                .post(format!("{}/worker/jobs/{}/complete", api.api_url, job.id))
                .with_worker_token()
                .json(&json!({
                    "node_id": api.node,
                    "status": "failed",
                    "output": { "error": message }
                }))
                .send()
                .await?
                .error_for_status()?;
            log_record("error", "job_failed", Some(&job.workspace_name), &message);
            Ok(())
        }
    }
}

fn spawn_job_heartbeat(
    api: WorkerApi,
    job_id: String,
    kind: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if let Err(error) = worker_job_event(
                &api,
                WorkerJobEvent {
                    job_id: &job_id,
                    event_type: "job_running",
                    status: "running",
                    message: "worker job heartbeat",
                    metadata: json!({ "kind": kind }),
                },
            )
            .await
            {
                log_record(
                    "error",
                    "worker_job_heartbeat_failed",
                    None,
                    &format!("worker job heartbeat failed for {job_id}: {error:#}"),
                );
            }
        }
    })
}

struct WorkerJobEvent<'a> {
    job_id: &'a str,
    event_type: &'a str,
    status: &'a str,
    message: &'a str,
    metadata: Value,
}

async fn worker_job_event(api: &WorkerApi, event: WorkerJobEvent<'_>) -> Result<()> {
    api.client
        .post(format!(
            "{}/worker/jobs/{}/events",
            api.api_url, event.job_id
        ))
        .with_worker_token()
        .json(&json!({
            "node_id": api.node,
            "event_type": event.event_type,
            "status": event.status,
            "message": event.message,
            "metadata": event.metadata
        }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

async fn execute_job(api: &WorkerApi, job: &JobRecord) -> Result<Value> {
    let payload: Value = serde_json::from_str(&job.payload_json)?;
    match job.kind.as_str() {
        "create" => {
            let workspace = api.workspace(&job.workspace_name).await?;
            create_workspace_local(api, &workspace, &payload).await?;
            Ok(json!({ "created": true }))
        }
        "start" | "warm" => {
            let workspace = api.workspace(&job.workspace_name).await?;
            api.update_workspace(
                &workspace.name,
                Some("starting"),
                Some("running"),
                true,
                false,
            )
            .await?;
            ensure_workspace_running_local(api, &workspace).await?;
            Ok(json!({ "started": true }))
        }
        "stop" => {
            let workspace = api.workspace(&job.workspace_name).await?;
            stop_workspace_local(api, &workspace).await?;
            Ok(json!({ "stopped": true }))
        }
        "remove" => {
            let workspace = api.workspace(&job.workspace_name).await?;
            let remove_workspace_dir = payload
                .get("remove_workspace_dir")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            remove_workspace_local(api, &workspace, remove_workspace_dir).await?;
            Ok(json!({ "removed": true, "workspace_dir_removed": remove_workspace_dir }))
        }
        "backup" => {
            let workspace = api.workspace(&job.workspace_name).await?;
            let leave_stopped = payload
                .get("leave_stopped")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            backup_workspace_local(api, &workspace, leave_stopped).await?;
            Ok(json!({ "backed_up": true }))
        }
        "restore" => {
            let workspace = api.workspace(&job.workspace_name).await?;
            restore_workspace_local(api, &workspace, &payload).await?;
            Ok(json!({ "restored": true }))
        }
        "execute" => {
            let command = payload
                .get("command")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("execute job payload requires command array"))?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(ToString::to_string)
                        .ok_or_else(|| anyhow!("command entries must be strings"))
                })
                .collect::<Result<Vec<_>>>()?;
            let workspace = api.workspace(&job.workspace_name).await?;
            api.update_workspace(&workspace.name, None, None, true, false)
                .await?;
            let vm = workspace_running_vm_local(api, &workspace).await?;
            let output = capture_guest_command(&vm, command).await?;
            Ok(output)
        }
        "hermes" => {
            let empty_args = Vec::new();
            let args = payload
                .get("args")
                .and_then(Value::as_array)
                .unwrap_or(&empty_args)
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(ToString::to_string)
                        .ok_or_else(|| anyhow!("args entries must be strings"))
                })
                .collect::<Result<Vec<_>>>()?;
            let workspace = api.workspace(&job.workspace_name).await?;
            api.update_workspace(&workspace.name, None, None, true, false)
                .await?;
            let vm = workspace_running_vm_local(api, &workspace).await?;
            let mut command = vec!["hermes".to_string()];
            command.extend(args);
            let output = capture_guest_command(&vm, command).await?;
            Ok(output)
        }
        other => bail!("unknown job kind: {other}"),
    }
}

async fn worker_reconcile_once(api: &WorkerApi) -> Result<()> {
    let records = api.workspaces().await?;
    let now = now_epoch()?;
    for record in records {
        if record.node_id.as_deref() != Some(api.node.as_str()) {
            continue;
        }
        if let Err(error) = worker_reconcile_workspace(api, &record, now).await {
            log_record(
                "error",
                "workspace_reconcile_failed",
                Some(&record.name),
                "workspace reconciliation failed",
            );
            if let Err(report_error) = api
                .update_workspace(&record.name, Some("error"), None, false, false)
                .await
            {
                eprintln!(
                    "failed to report reconcile status for {}: {report_error:#}",
                    record.name
                );
            }
            if let Err(report_error) = api
                .event(
                    &record.name,
                    "workspace_reconcile_failed",
                    "failed",
                    &format!("{error:#}"),
                    json!({ "vm": record.vm_name, "workspace_dir": record.workspace_dir_name }),
                )
                .await
            {
                eprintln!(
                    "failed to report reconcile event for {}: {report_error:#}",
                    record.name
                );
            }
            eprintln!("reconcile {} failed: {error:#}", record.name);
        }
    }
    Ok(())
}

async fn worker_reconcile_workspace(
    api: &WorkerApi,
    record: &WorkspaceRecord,
    now: i64,
) -> Result<()> {
    if record.desired_state == "running" && record.status != "idle-stopped" {
        ensure_workspace_running_local(api, record).await?;
        if record.idle_timeout_secs > 0
            && now.saturating_sub(record.last_used_at) >= record.idle_timeout_secs as i64
        {
            log_record(
                "info",
                "workspace_idle_stop",
                Some(&record.name),
                "workspace idle timeout reached",
            );
            if let Ok(handle) = get_vm(&record.vm_name).await
                && handle.status().is_running()
            {
                handle.stop_with_timeout(Duration::from_secs(10)).await?;
            }
            api.update_workspace(
                &record.name,
                Some("idle-stopped"),
                Some("stopped"),
                false,
                false,
            )
            .await?;
            api.event(
                &record.name,
                "workspace_idle_stopped",
                "succeeded",
                "workspace stopped after idle timeout",
                json!({ "idle_seconds": now.saturating_sub(record.last_used_at) }),
            )
            .await?;
        }
    }

    if backup_due(record, now)
        && let Err(error) = backup_workspace_local(api, record, false).await
    {
        log_record(
            "error",
            "workspace_backup_failed",
            Some(&record.name),
            "workspace backup failed",
        );
        api.event(
            &record.name,
            "workspace_backup_failed",
            "failed",
            &format!("{error:#}"),
            json!({}),
        )
        .await?;
        eprintln!("backup {} failed: {error:#}", record.name);
    }
    Ok(())
}

impl WorkerApi {
    async fn workspaces(&self) -> Result<Vec<WorkspaceRecord>> {
        self.client
            .get(format!(
                "{}/worker/workspaces?node_id={}",
                self.api_url,
                url_component(&self.node)
            ))
            .with_worker_token()
            .send()
            .await?
            .error_for_status()?
            .json::<Vec<WorkspaceRecord>>()
            .await
            .map_err(Into::into)
    }

    async fn workspace(&self, name: &str) -> Result<WorkspaceRecord> {
        self.workspaces()
            .await?
            .into_iter()
            .find(|workspace| workspace.name == name)
            .ok_or_else(|| anyhow!("workspace not found in API: {name}"))
    }

    async fn update_workspace(
        &self,
        name: &str,
        status: Option<&str>,
        desired_state: Option<&str>,
        touch: bool,
        mark_backup: bool,
    ) -> Result<()> {
        self.client
            .post(format!("{}/worker/workspaces/{name}/state", self.api_url))
            .with_worker_token()
            .json(&json!({
                "node_id": self.node,
                "status": status,
                "desired_state": desired_state,
                "touch": touch,
                "mark_backup": mark_backup
            }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn event(
        &self,
        name: &str,
        event_type: &str,
        status: &str,
        message: &str,
        metadata: Value,
    ) -> Result<()> {
        self.client
            .post(format!("{}/worker/workspaces/{name}/events", self.api_url))
            .with_worker_token()
            .json(&json!({
                "node_id": self.node,
                "event_type": event_type,
                "status": status,
                "message": message,
                "metadata": metadata
            }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn record_backup(&self, name: &str, artifact: &BackupArtifact) -> Result<String> {
        let response = self
            .client
            .post(format!("{}/worker/workspaces/{name}/backups", self.api_url))
            .with_worker_token()
            .json(&json!({
                "node_id": self.node,
                "kind": artifact.kind,
                "location": artifact.location,
                "status": "succeeded",
                "size_bytes": artifact.size_bytes
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        response
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| anyhow!("backup artifact response did not include id"))
    }
}

async fn create_workspace_local(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
    payload: &Value,
) -> Result<()> {
    if api.runtime.is_fake() {
        return fake_create_workspace(api, workspace, payload).await;
    }
    api.event(
        &workspace.name,
        "workspace_create_started",
        "running",
        "workspace create requested",
        json!({
            "vm": workspace.vm_name,
            "workspace_dir": workspace.workspace_dir_name,
            "cpus": workspace.cpus,
            "memory_mib": workspace.memory_mib,
            "workspace_quota_mib": workspace.workspace_quota_mib
        }),
    )
    .await?;
    if let Err(error) = create_workspace_vm(workspace, false).await {
        let workspace_dir_path = workspace_dir_path(&workspace.workspace_dir_name)?;
        if workspace_dir_path.exists() {
            api.event(
                &workspace.name,
                "workspace_create_retry_recreate_started",
                "running",
                "workspace directory exists after create failure; recreating vm around it",
                json!({
                    "vm": workspace.vm_name,
                    "workspace_dir": workspace.workspace_dir_name,
                    "error": format!("{error:#}")
                }),
            )
            .await?;
            create_workspace_vm(workspace, true).await.with_context(|| {
                format!(
                    "initial create failed with {error:#}; retry around existing workspace directory also failed"
                )
            })?;
        } else {
            api.update_workspace(&workspace.name, Some("create-failed"), None, false, false)
                .await?;
            api.event(
                &workspace.name,
                "workspace_create_failed",
                "failed",
                &format!("{error:#}"),
                json!({ "vm": workspace.vm_name, "workspace_dir": workspace.workspace_dir_name }),
            )
            .await?;
            return Err(error);
        }
    }
    api.update_workspace(&workspace.name, Some("stopped"), None, false, false)
        .await?;
    api.event(
        &workspace.name,
        "workspace_created",
        "succeeded",
        "workspace VM created and stopped with persistent workspace directory",
        json!({
            "vm": workspace.vm_name,
            "workspace_dir": workspace.workspace_dir_name,
            "user": payload.get("user")
        }),
    )
    .await?;
    Ok(())
}

async fn stop_workspace_local(api: &WorkerApi, workspace: &WorkspaceRecord) -> Result<()> {
    if api.runtime.is_fake() {
        return fake_stop_workspace(api, workspace).await;
    }
    api.update_workspace(&workspace.name, None, Some("stopped"), false, false)
        .await?;
    if let Ok(handle) = get_vm(&workspace.vm_name).await
        && handle.status().is_running()
    {
        handle.stop_with_timeout(Duration::from_secs(10)).await?;
    }
    api.update_workspace(&workspace.name, Some("stopped"), None, false, false)
        .await?;
    api.event(
        &workspace.name,
        "workspace_stopped",
        "succeeded",
        "workspace stopped",
        json!({ "vm": workspace.vm_name }),
    )
    .await?;
    Ok(())
}

async fn remove_workspace_local(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
    remove_workspace_dir: bool,
) -> Result<()> {
    if api.runtime.is_fake() {
        return fake_remove_workspace(api, workspace, remove_workspace_dir).await;
    }
    api.update_workspace(
        &workspace.name,
        Some("removing"),
        Some("removed"),
        false,
        false,
    )
    .await?;
    remove_vm(&workspace.vm_name)
        .await
        .with_context(|| format!("remove VM {}", workspace.vm_name))?;
    if remove_workspace_dir {
        runtime::remove_workspace_dir(&workspace.workspace_dir_name)
            .await
            .with_context(|| format!("remove workspace dir {}", workspace.workspace_dir_name))?;
    }
    api.update_workspace(
        &workspace.name,
        Some("removed"),
        Some("removed"),
        false,
        false,
    )
    .await?;
    api.event(
        &workspace.name,
        "workspace_removed",
        "succeeded",
        "workspace vm removed",
        json!({ "workspace_dir_removed": remove_workspace_dir }),
    )
    .await
}

async fn ensure_workspace_running_local(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
) -> Result<()> {
    if api.runtime.is_fake() {
        return fake_start_workspace(api, workspace).await;
    }
    match get_vm(&workspace.vm_name).await {
        Ok(handle) if handle.status().is_running() => {
            api.update_workspace(&workspace.name, Some("running"), None, false, false)
                .await
        }
        Ok(handle) => {
            api.event(
                &workspace.name,
                "vm_starting",
                "running",
                "starting workspace vm",
                json!({ "vm": workspace.vm_name }),
            )
            .await?;
            handle.start().await?;
            api.update_workspace(&workspace.name, Some("running"), None, false, false)
                .await?;
            api.event(
                &workspace.name,
                "vm_started",
                "succeeded",
                "workspace vm started",
                json!({ "vm": workspace.vm_name }),
            )
            .await
        }
        Err(error) => {
            api.event(
                &workspace.name,
                "vm_recreate_started",
                "running",
                "workspace vm missing; recreating it around existing workspace directory",
                json!({ "vm": workspace.vm_name, "workspace_dir": workspace.workspace_dir_name }),
            )
            .await?;
            create_workspace_vm(workspace, true)
                .await
                .with_context(|| {
                    format!(
                        "workspace {} has no vm {}; failed to recreate it after: {error:#}",
                        workspace.name, workspace.vm_name
                    )
                })?;
            api.event(
                &workspace.name,
                "vm_recreated",
                "succeeded",
                "workspace vm recreated around existing workspace directory",
                json!({ "vm": workspace.vm_name, "workspace_dir": workspace.workspace_dir_name }),
            )
            .await?;
            let handle = get_vm(&workspace.vm_name)
                .await
                .with_context(|| format!("get recreated VM '{}'", workspace.vm_name))?;
            handle.start().await?;
            api.update_workspace(&workspace.name, Some("running"), None, false, false)
                .await?;
            api.event(
                &workspace.name,
                "vm_started",
                "succeeded",
                "workspace vm started",
                json!({ "vm": workspace.vm_name }),
            )
            .await
        }
    }
}

async fn create_workspace_vm(workspace: &WorkspaceRecord, replace: bool) -> Result<()> {
    let request = WorkspaceVmRequest {
        name: workspace.vm_name.clone(),
        replace,
        cpus: workspace.cpus,
        memory_mib: u64::from(workspace.memory_mib),
        workspace_name: workspace.name.clone(),
        workspace_dir_name: workspace.workspace_dir_name.clone(),
        workspace_quota_mib: workspace.workspace_quota_mib,
    };
    create_vm(request).await
}

async fn workspace_running_vm_local(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
) -> Result<GuestVm> {
    if api.runtime.is_fake() {
        bail!("MOM_RUNTIME=fake does not support guest command execution yet");
    }
    match get_vm(&workspace.vm_name).await {
        Ok(handle) => match handle.status() {
            VmStatus::Running | VmStatus::Draining => handle
                .connect_with_timeout(Duration::from_secs(30))
                .await
                .with_context(|| format!("connect to running vm '{}'", workspace.vm_name)),
            VmStatus::Stopped | VmStatus::Crashed | VmStatus::Paused | VmStatus::Unknown => {
                api.event(
                    &workspace.name,
                    "vm_starting",
                    "running",
                    "starting workspace vm",
                    json!({ "vm": workspace.vm_name }),
                )
                .await?;
                let vm = handle
                    .start()
                    .await
                    .with_context(|| format!("start vm '{}'", workspace.vm_name))?;
                api.update_workspace(&workspace.name, Some("running"), None, false, false)
                    .await?;
                api.event(
                    &workspace.name,
                    "vm_started",
                    "succeeded",
                    "workspace vm started",
                    json!({ "vm": workspace.vm_name }),
                )
                .await?;
                Ok(vm)
            }
            VmStatus::Missing => Err(anyhow!(
                "workspace {} has no VM {}; recreate it",
                workspace.name,
                workspace.vm_name
            )),
        },
        Err(error) => Err(error).with_context(|| {
            format!(
                "workspace {} has no vm {}; recreate it",
                workspace.name, workspace.vm_name
            )
        }),
    }
}

async fn backup_workspace_local(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
    leave_stopped: bool,
) -> Result<()> {
    if api.runtime.is_fake() {
        let _ = leave_stopped;
        return fake_backup_workspace(api, workspace).await;
    }
    let was_running = match get_vm(&workspace.vm_name).await {
        Ok(handle) => {
            let running = handle.status().is_running();
            if running {
                api.event(
                    &workspace.name,
                    "backup_stop_started",
                    "running",
                    "stopping workspace before backup",
                    json!({ "vm": workspace.vm_name }),
                )
                .await?;
                handle.stop_with_timeout(Duration::from_secs(20)).await?;
                api.update_workspace(&workspace.name, Some("backup-stopped"), None, false, false)
                    .await?;
            }
            running
        }
        Err(_) => false,
    };
    let workspace_dir_path = workspace_dir_path(&workspace.workspace_dir_name)?;
    if !workspace_dir_path.exists() {
        bail!(
            "workspace directory {} does not exist at {}",
            workspace.workspace_dir_name,
            workspace_dir_path.display()
        );
    }
    api.event(
        &workspace.name,
        "workspace_backup_started",
        "running",
        "workspace directory backup started",
        json!({ "workspace_dir": workspace.workspace_dir_name }),
    )
    .await?;
    let artifact = backup::run_restic_backup(workspace, &workspace_dir_path).await?;
    let backup_id = api.record_backup(&workspace.name, &artifact).await?;
    api.update_workspace(&workspace.name, None, None, false, true)
        .await?;
    api.event(
        &workspace.name,
        "workspace_backup_succeeded",
        "succeeded",
        "workspace directory backup completed",
        json!({
            "workspace_dir": workspace.workspace_dir_name,
            "backup_id": backup_id,
            "kind": artifact.kind,
            "location": artifact.location
        }),
    )
    .await?;
    if was_running && !leave_stopped && workspace.desired_state == "running" {
        ensure_workspace_running_local(api, workspace).await?;
    }
    Ok(())
}

async fn restore_workspace_local(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
    payload: &Value,
) -> Result<()> {
    let backup_id = payload
        .get("backup_id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("restore job payload requires backup_id"))?;
    let backup_location = payload
        .get("backup_location")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("restore job payload requires backup_location"))?;
    let backup_workspace_name = payload
        .get("backup_workspace_name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("restore job payload requires backup_workspace_name"))?;
    if backup_workspace_name != workspace.name {
        bail!(
            "restore backup belongs to workspace {}, not {}",
            backup_workspace_name,
            workspace.name
        );
    }
    let desired_state = payload
        .get("desired_state")
        .and_then(Value::as_str)
        .unwrap_or(&workspace.desired_state);
    if api.runtime.is_fake() {
        return fake_restore_workspace(api, workspace, backup_id, backup_location).await;
    }
    if let Ok(handle) = get_vm(&workspace.vm_name).await
        && handle.status().is_running()
    {
        handle.stop_with_timeout(Duration::from_secs(20)).await?;
    }
    api.update_workspace(&workspace.name, Some("restoring"), None, false, false)
        .await?;
    api.event(
        &workspace.name,
        "workspace_restore_started",
        "running",
        "workspace directory restore started",
        json!({ "backup_id": backup_id, "location": backup_location }),
    )
    .await?;
    let workspace_dir_path = workspace_dir_path(&workspace.workspace_dir_name)?;
    backup::run_restic_restore(backup_id, backup_location, &workspace_dir_path).await?;
    api.update_workspace(
        &workspace.name,
        Some("restored"),
        Some(desired_state),
        false,
        false,
    )
    .await?;
    api.event(
        &workspace.name,
        "workspace_restored",
        "succeeded",
        "workspace directory restored from backup",
        json!({
            "backup_id": backup_id,
            "location": backup_location,
            "desired_state": desired_state
        }),
    )
    .await?;
    if desired_state == "running" {
        ensure_workspace_running_local(api, workspace).await?;
    } else if get_vm(&workspace.vm_name).await.is_err() {
        create_workspace_vm(workspace, true).await?;
        api.update_workspace(
            &workspace.name,
            Some("stopped"),
            Some("stopped"),
            false,
            false,
        )
        .await?;
    } else {
        api.update_workspace(
            &workspace.name,
            Some("stopped"),
            Some("stopped"),
            false,
            false,
        )
        .await?;
    }
    Ok(())
}

fn fake_workspace_dir(workspace: &WorkspaceRecord) -> Result<PathBuf> {
    Ok(runtime_home()?.join("fake").join(&workspace.name))
}

async fn fake_create_workspace(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
    payload: &Value,
) -> Result<()> {
    let dir = fake_workspace_dir(workspace)?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(
        dir.join("metadata.json"),
        serde_json::to_vec_pretty(&json!({
            "workspace": workspace.name,
            "vm": workspace.vm_name,
            "workspace_dir": workspace.workspace_dir_name,
            "payload": payload
        }))?,
    )?;
    api.update_workspace(&workspace.name, Some("stopped"), None, false, false)
        .await?;
    api.event(
        &workspace.name,
        "workspace_created",
        "succeeded",
        "fake workspace created",
        json!({ "runtime": "fake" }),
    )
    .await
}

async fn fake_start_workspace(api: &WorkerApi, workspace: &WorkspaceRecord) -> Result<()> {
    let dir = fake_workspace_dir(workspace)?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(dir.join("state"), b"running")?;
    api.update_workspace(&workspace.name, Some("running"), None, false, false)
        .await?;
    api.event(
        &workspace.name,
        "vm_started",
        "succeeded",
        "fake workspace started",
        json!({ "runtime": "fake" }),
    )
    .await
}

async fn fake_stop_workspace(api: &WorkerApi, workspace: &WorkspaceRecord) -> Result<()> {
    let dir = fake_workspace_dir(workspace)?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(dir.join("state"), b"stopped")?;
    api.update_workspace(
        &workspace.name,
        Some("stopped"),
        Some("stopped"),
        false,
        false,
    )
    .await?;
    api.event(
        &workspace.name,
        "workspace_stopped",
        "succeeded",
        "fake workspace stopped",
        json!({ "runtime": "fake" }),
    )
    .await
}

async fn fake_remove_workspace(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
    remove_workspace_dir: bool,
) -> Result<()> {
    let dir = fake_workspace_dir(workspace)?;
    let _ = fs::remove_dir_all(&dir);
    api.update_workspace(
        &workspace.name,
        Some("removed"),
        Some("removed"),
        false,
        false,
    )
    .await?;
    api.event(
        &workspace.name,
        "workspace_removed",
        "succeeded",
        "fake workspace removed",
        json!({ "runtime": "fake", "workspace_dir_removed": remove_workspace_dir }),
    )
    .await
}

async fn fake_backup_workspace(api: &WorkerApi, workspace: &WorkspaceRecord) -> Result<()> {
    let dir = fake_workspace_dir(workspace)?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let snapshot = format!("fake-{}-{}", workspace.name, now_epoch()?);
    fs::write(dir.join("last-backup"), snapshot.as_bytes())?;
    let artifact = BackupArtifact {
        kind: "restic".to_string(),
        location: format!("fake-restic#{snapshot}"),
        size_bytes: Some(0),
    };
    let backup_id = api.record_backup(&workspace.name, &artifact).await?;
    api.update_workspace(&workspace.name, None, None, false, true)
        .await?;
    api.event(
        &workspace.name,
        "workspace_backup_succeeded",
        "succeeded",
        "fake workspace backup completed",
        json!({ "runtime": "fake", "backup_id": backup_id }),
    )
    .await
}

async fn fake_restore_workspace(
    api: &WorkerApi,
    workspace: &WorkspaceRecord,
    backup_id: &str,
    backup_location: &str,
) -> Result<()> {
    let dir = fake_workspace_dir(workspace)?;
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(dir.join("restored-from"), backup_location.as_bytes())?;
    fs::write(dir.join("state"), b"running")?;
    api.update_workspace(
        &workspace.name,
        Some("running"),
        Some("running"),
        false,
        false,
    )
    .await?;
    api.event(
        &workspace.name,
        "workspace_restored",
        "succeeded",
        "fake workspace restored from backup",
        json!({ "runtime": "fake", "backup_id": backup_id, "location": backup_location }),
    )
    .await
}

async fn fake_open_hermes(workspace_name: &str) -> Result<String> {
    let dir = runtime_home()?.join("fake").join(workspace_name);
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    fs::write(dir.join("service-hermes"), b"opened")?;
    let base = env::var("MOM_FAKE_SERVICE_BASE_URL")
        .unwrap_or_else(|_| "http://fake.agentmom.local".to_string());
    Ok(format!(
        "{}/{}/hermes",
        base.trim_end_matches('/'),
        workspace_name
    ))
}
