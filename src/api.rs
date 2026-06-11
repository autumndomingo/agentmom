use super::*;

pub(crate) async fn api(args: ApiArgs) -> Result<()> {
    ensure_fleet_schema()?;
    let (notifier, _) = broadcast::channel(1024);
    let state = ApiState { notifier };
    let app = Router::new()
        .route("/health/live", get(api_health_live))
        .route("/health/ready", get(api_health_ready))
        .route("/metrics", get(api_metrics))
        .route("/api/jobs", post(api_create_job))
        .route("/api/jobs/{id}", get(api_get_job))
        .route(
            "/api/workspaces",
            get(api_list_workspaces).post(api_create_workspace),
        )
        .route("/api/workspaces/{name}/events", get(api_workspace_events))
        .route("/worker/register", post(api_worker_register))
        .route("/worker/heartbeat", post(api_worker_register))
        .route("/worker/claim", post(api_worker_claim))
        .route("/worker/jobs/{id}/events", post(api_worker_job_event))
        .route("/worker/jobs/{id}/complete", post(api_worker_job_complete))
        .route("/worker/events", get(api_worker_events))
        .merge(ui::api_routes())
        .with_state(Arc::new(state));
    let app = ui::serve_assets(app);
    let addr: SocketAddr = args
        .bind
        .parse()
        .with_context(|| format!("parse API bind address {}", args.bind))?;
    log_record("info", "api_start", None, "Agent Mom API starting");
    println!("Agent Mom API listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn api_health_live() -> Result<Json<HealthResponse>, ApiError> {
    Ok(Json(HealthResponse {
        ok: true,
        node: node_id()?,
        db: fleet_state_dir()?.join("fleet.db").display().to_string(),
    }))
}

async fn api_health_ready() -> Result<Json<HealthResponse>, ApiError> {
    ensure_fleet_schema()?;
    Ok(Json(HealthResponse {
        ok: true,
        node: node_id()?,
        db: fleet_state_dir()?.join("fleet.db").display().to_string(),
    }))
}

async fn api_metrics() -> Result<String, ApiError> {
    let workspaces = workspace_all()?.len();
    let jobs = job_counts()?;
    let backups = backup_count()?;
    Ok(format!(
        "# HELP agentmom_workspaces Total workspaces in the Agent Mom database\n\
         # TYPE agentmom_workspaces gauge\n\
         agentmom_workspaces {workspaces}\n\
         # HELP agentmom_backups_total Backup artifact records\n\
         # TYPE agentmom_backups_total gauge\n\
         agentmom_backups_total {backups}\n\
         # HELP agentmom_jobs Jobs by status\n\
         # TYPE agentmom_jobs gauge\n{}",
        jobs.into_iter()
            .map(|(status, count)| format!(
                "agentmom_jobs{{status=\"{}\"}} {}",
                escape_metric_label(&status),
                count
            ))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

async fn api_create_job(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CreateJobRequest>,
) -> Result<Json<JobResponse>, ApiError> {
    let job = create_job(request)?;
    let _ = state.notifier.send("job_available".to_string());
    Ok(Json(JobResponse { job }))
}

async fn api_get_job(AxumPath(id): AxumPath<String>) -> Result<Json<JobResponse>, ApiError> {
    Ok(Json(JobResponse { job: job_get(&id)? }))
}

async fn api_list_workspaces() -> Result<Json<Vec<WorkspaceRecord>>, ApiError> {
    Ok(Json(workspace_all()?))
}

async fn api_create_workspace(
    State(state): State<Arc<ApiState>>,
    Json(request): Json<CreateWorkspaceRequest>,
) -> Result<Json<JobResponse>, ApiError> {
    let name = sanitize_workspace_name(&request.name)?;
    let assigned_node = request.node_id.clone().unwrap_or(node_id()?);
    let user_id = request.user.clone().unwrap_or_else(|| name.clone());
    let memory = u32::try_from(request.memory).context("memory must fit in u32 MiB")?;
    workspace_upsert_pending(
        &name,
        &user_id,
        &format!("mom-{name}"),
        &format!("mom-{name}-workspace"),
        Some(&assigned_node),
        request.cpus,
        memory,
        request.volume_quota,
        request.idle_timeout,
        request.backup_interval,
    )?;
    let job = create_job(CreateJobRequest {
        workspace_name: name,
        kind: "create".to_string(),
        node_id: Some(assigned_node),
        payload: json!({
            "user": request.user,
            "cpus": request.cpus,
            "memory": request.memory,
            "volume_quota": request.volume_quota,
            "idle_timeout": request.idle_timeout,
            "backup_interval": request.backup_interval
        }),
    })?;
    let _ = state.notifier.send("job_available".to_string());
    Ok(Json(JobResponse { job }))
}

async fn api_workspace_events(
    AxumPath(name): AxumPath<String>,
) -> Result<Json<Vec<WorkspaceEvent>>, ApiError> {
    Ok(Json(workspace_events_since(&name, 0)?))
}

async fn api_worker_register(
    headers: HeaderMap,
    Json(request): Json<RegisterNodeRequest>,
) -> Result<Json<Value>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    register_node(
        &request.node_id,
        &request.capacity,
        request.worker_url.as_deref(),
    )?;
    Ok(Json(json!({ "ok": true })))
}

async fn api_worker_claim(
    headers: HeaderMap,
    Json(request): Json<ClaimJobRequest>,
) -> Result<Json<Option<JobRecord>>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    register_node(
        &request.node_id,
        &request.capacity,
        request.worker_url.as_deref(),
    )?;
    if !request.pressure.capacity_ok {
        return Ok(Json(None));
    }
    Ok(Json(claim_job(&request.node_id)?))
}

async fn api_worker_job_event(
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<JobEventRequest>,
) -> Result<Json<Value>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    let job = job_get(&id)?;
    record_workspace_event(
        &job.workspace_name,
        &request.event_type,
        &request.status,
        &request.message,
        json!({
            "job_id": id,
            "worker_node_id": request.node_id,
            "metadata": request.metadata
        }),
    )?;
    Ok(Json(json!({ "ok": true })))
}

async fn api_worker_job_complete(
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(request): Json<CompleteJobRequest>,
) -> Result<Json<JobResponse>, ApiError> {
    require_worker_token(&headers).map_err(ApiError::Unauthorized)?;
    let job = complete_job(&id, &request.node_id, &request.status, request.output)?;
    Ok(Json(JobResponse { job }))
}

async fn api_worker_events(
    State(state): State<Arc<ApiState>>,
    Query(query): Query<WorkerEventsQuery>,
) -> Sse<impl tokio_stream::Stream<Item = std::result::Result<SseEvent, Infallible>>> {
    let node_id = query.node_id;
    let stream = BroadcastStream::new(state.notifier.subscribe()).filter_map(move |message| {
        let node_id = node_id.clone();
        match message {
            Ok(kind) => Some(Ok(SseEvent::default()
                .event(kind)
                .data(json!({ "node_id": node_id }).to_string()))),
            Err(_) => None,
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}
