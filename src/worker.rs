use super::*;

#[derive(Clone)]
struct WorkerState {
    services: service::ServiceState,
}

#[derive(Debug, Deserialize)]
struct OpenServiceRequest {
    workspace_name: String,
    sandbox_name: String,
}

pub(crate) async fn worker(args: WorkerArgs) -> Result<()> {
    ensure_fleet_schema()?;
    let node = node_id()?;
    let client = reqwest::Client::new();
    let api_url = args.api_url.trim_end_matches('/').to_string();
    let worker_url = args
        .worker_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", args.bind));
    let state = Arc::new(WorkerState {
        services: service::ServiceState::default(),
    });
    let worker_http = tokio::spawn(run_worker_http(args.bind.clone(), state));
    register_worker(&client, &api_url, &node, &worker_url).await?;
    let (wake_tx, mut wake_rx) = mpsc::channel::<()>(32);
    let sse_client = client.clone();
    let sse_url = api_url.clone();
    let sse_node = node.clone();
    tokio::spawn(async move {
        worker_sse_loop(sse_client, sse_url, sse_node, wake_tx).await;
    });

    log_record("info", "worker_start", None, "Agent Mom worker starting");
    loop {
        if worker_claim_once(&client, &api_url, &node, &worker_url).await? && args.once {
            return Ok(());
        }
        if args.once {
            worker_http.abort();
            return Ok(());
        }
        tokio::select! {
            _ = wake_rx.recv() => {},
            _ = tokio::time::sleep(Duration::from_secs(args.interval)) => {},
        }
    }
}

async fn run_worker_http(bind: String, state: Arc<WorkerState>) -> Result<()> {
    let app = Router::new()
        .route("/worker/health", get(worker_health))
        .route("/worker/services/{service}/open", post(worker_open_service))
        .with_state(state);
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("parse worker bind address {bind}"))?;
    log_record(
        "info",
        "worker_http_start",
        None,
        &format!("Agent Mom worker HTTP listening on http://{addr}"),
    );
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind worker HTTP {addr}"))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn worker_health() -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn worker_open_service(
    State(state): State<Arc<WorkerState>>,
    AxumPath(service): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<OpenServiceRequest>,
) -> Result<Json<Value>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    let url = service::open_workspace_service(
        &state.services,
        &request.workspace_name,
        &request.sandbox_name,
        &service,
    )
    .await?;
    Ok(Json(json!({ "url": url })))
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
            let response = client.get(url).send().await?.error_for_status()?;
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

async fn worker_claim_once(
    client: &reqwest::Client,
    api_url: &str,
    node: &str,
    worker_url: &str,
) -> Result<bool> {
    let records = workspace_all()?;
    let pressure = node_pressure(&records).await?;
    let response = client
        .post(format!("{api_url}/worker/claim"))
        .with_worker_token()
        .json(&json!({
            "node_id": node,
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
    run_claimed_job(client, api_url, node, job).await?;
    Ok(true)
}

async fn run_claimed_job(
    client: &reqwest::Client,
    api_url: &str,
    node: &str,
    job: JobRecord,
) -> Result<()> {
    worker_job_event(
        client,
        api_url,
        node,
        &job.id,
        "job_running",
        "running",
        "worker started job",
        json!({ "kind": job.kind }),
    )
    .await?;
    let result = execute_job(&job).await;
    match result {
        Ok(output) => {
            client
                .post(format!("{api_url}/worker/jobs/{}/complete", job.id))
                .with_worker_token()
                .json(&json!({
                    "node_id": node,
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
            let _ = client
                .post(format!("{api_url}/worker/jobs/{}/complete", job.id))
                .with_worker_token()
                .json(&json!({
                    "node_id": node,
                    "status": "failed",
                    "output": { "error": message }
                }))
                .send()
                .await;
            Err(error)
        }
    }
}

async fn worker_job_event(
    client: &reqwest::Client,
    api_url: &str,
    node: &str,
    job_id: &str,
    event_type: &str,
    status: &str,
    message: &str,
    metadata: Value,
) -> Result<()> {
    client
        .post(format!("{api_url}/worker/jobs/{job_id}/events"))
        .with_worker_token()
        .json(&json!({
            "node_id": node,
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

async fn execute_job(job: &JobRecord) -> Result<Value> {
    let payload: Value = serde_json::from_str(&job.payload_json)?;
    match job.kind.as_str() {
        "create" => {
            let args = WorkspaceCreateArgs {
                name: job.workspace_name.clone(),
                user: payload
                    .get("user")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                replace: false,
                cpus: payload
                    .get("cpus")
                    .and_then(Value::as_u64)
                    .and_then(|value| u8::try_from(value).ok())
                    .unwrap_or_else(default_workspace_cpus),
                memory: payload
                    .get("memory")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(default_workspace_memory),
                volume_quota: payload
                    .get("volume_quota")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .unwrap_or_else(default_workspace_volume_quota),
                idle_timeout: payload
                    .get("idle_timeout")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(default_workspace_idle_timeout),
                backup_interval: payload
                    .get("backup_interval")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(default_workspace_backup_interval),
                rebuild_snapshot: false,
                no_snapshot: false,
            };
            workspace_create(args).await?;
            Ok(json!({ "created": true }))
        }
        "start" | "warm" => {
            let workspace = workspace_get(&job.workspace_name)?;
            workspace_touch(&workspace.name)?;
            workspace_set_desired(&workspace.name, "running")?;
            workspace_ensure_running(&workspace).await?;
            Ok(json!({ "started": true }))
        }
        "stop" => {
            workspace_stop(&job.workspace_name).await?;
            Ok(json!({ "stopped": true }))
        }
        "backup" => {
            let workspace = workspace_get(&job.workspace_name)?;
            backup::backup_workspace(&workspace, false).await?;
            Ok(json!({ "backed_up": true }))
        }
        "restore" => {
            let backup_id = payload.get("backup_id").and_then(Value::as_str);
            backup::workspace_restore(&job.workspace_name, backup_id).await?;
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
            let workspace = workspace_get(&job.workspace_name)?;
            workspace_touch(&workspace.name)?;
            let sandbox = workspace_running_sandbox(&workspace).await?;
            let output = capture_guest_command(&sandbox, command).await?;
            Ok(output)
        }
        "codex" => {
            let prompt = payload
                .get("prompt")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("codex job payload requires prompt"))?;
            let workspace = workspace_get(&job.workspace_name)?;
            workspace_touch(&workspace.name)?;
            let sandbox = workspace_running_sandbox(&workspace).await?;
            run_codex(&sandbox, prompt).await?;
            Ok(json!({ "ok": true }))
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
            let workspace = workspace_get(&job.workspace_name)?;
            workspace_touch(&workspace.name)?;
            let sandbox = workspace_running_sandbox(&workspace).await?;
            let mut command = vec!["hermes".to_string()];
            command.extend(args);
            let output = capture_guest_command(&sandbox, command).await?;
            Ok(output)
        }
        other => bail!("unknown job kind: {other}"),
    }
}
