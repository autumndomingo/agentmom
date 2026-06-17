use std::collections::HashMap;

use super::*;
use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng},
};
use axum::{
    http::header::{COOKIE, SET_COOKIE},
    routing::delete,
};
use hmac::{Hmac, Mac};
use rand::Rng;

type HmacSha256 = Hmac<Sha256>;

const SESSION_COOKIE: &str = "agentmom_session";

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuthUser {
    pub(crate) id: i64,
    pub(crate) email: String,
    pub(crate) full_name: String,
    pub(crate) role: String,
    pub(crate) invite_id: Option<i64>,
    pub(crate) last_seen_at: Option<i64>,
}

#[derive(Debug, Serialize)]
struct InviteRecord {
    id: i64,
    label: String,
    description: String,
    code: String,
    role: String,
    max_uses: Option<i64>,
    used_count: i64,
    active: bool,
    created_by_user_id: Option<i64>,
    created_at: i64,
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct SignupRequest {
    full_name: String,
    email: String,
    #[serde(default)]
    code: Option<String>,
    password: String,
}

#[derive(Debug, Deserialize)]
struct SetupRequest {
    full_name: String,
    agent_name: String,
}

#[derive(Debug, Deserialize)]
struct CreateInviteRequest {
    label: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    max_uses: Option<i64>,
}

#[derive(Debug, Serialize)]
struct LoginResponse {
    ok: bool,
    user: AuthUser,
    workspace: Option<WorkspaceRecord>,
}

#[derive(Debug, Serialize)]
struct MeResponse {
    user: AuthUser,
    workspace: Option<WorkspaceRecord>,
}

#[derive(Debug, Serialize)]
struct InviteCreatedResponse {
    invite: InviteRecord,
    code: String,
}

#[derive(Debug, Serialize)]
struct InviteDetailResponse {
    invite: InviteRecord,
    users: Vec<AuthUser>,
    workspaces: Vec<WorkspaceRecord>,
}

#[derive(Debug, Serialize)]
struct UsersResponse {
    users: Vec<AuthUser>,
}

#[derive(Debug, Serialize)]
struct AdminInfraResponse {
    generated_at: i64,
    app_version: String,
    users: Vec<AdminInfraUser>,
    nodes: Vec<AdminInfraNode>,
    jobs: Vec<JobRecord>,
    workspace_status_counts: HashMap<String, i64>,
    node_status_counts: HashMap<String, i64>,
    job_status_counts: HashMap<String, i64>,
}

#[derive(Debug, Serialize)]
struct AdminInfraUser {
    #[serde(flatten)]
    user: AuthUser,
    workspace: Option<AdminInfraWorkspace>,
}

#[derive(Debug, Serialize)]
struct AdminInfraWorkspace {
    #[serde(flatten)]
    workspace: WorkspaceRecord,
}

#[derive(Debug, Serialize)]
struct AdminInfraNode {
    #[serde(flatten)]
    node: NodeRecord,
    stale: bool,
    active_workspaces: usize,
    desired_running_workspaces: usize,
    allocated_cpus: u32,
    allocated_memory_mib: u64,
}

#[derive(Debug, Serialize)]
struct InvitesResponse {
    invites: Vec<InviteRecord>,
}

#[derive(Debug, Serialize)]
struct OkResponse {
    ok: bool,
}

#[derive(Debug, Serialize)]
struct AuthErrorBody {
    error: String,
}

pub(crate) fn api_routes() -> Router<Arc<ApiState>> {
    Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/signup", post(signup))
        .route("/api/auth/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/me/setup", post(setup_me))
        .route("/api/admin/users", get(admin_users))
        .route("/api/admin/infra", get(admin_infra))
        .route("/api/admin/users/{id}", delete(admin_delete_user))
        .route(
            "/api/admin/invites",
            get(admin_invites).post(admin_create_invite),
        )
        .route("/api/admin/invites/{id}", get(admin_invite_detail))
        .route(
            "/api/admin/invites/{id}/disable",
            post(admin_disable_invite),
        )
}

pub(crate) fn seed_admin_from_config() -> Result<()> {
    ensure_fleet_schema()?;
    let config = load_mom_config()?;
    config.validate_for_api()?;
    Ok(())
}

pub(crate) fn current_user(headers: &HeaderMap) -> Result<AuthUser, AuthError> {
    let token = cookie_value(headers, SESSION_COOKIE).ok_or(AuthError::Unauthorized)?;
    let token_hash = session_token_hash(token)?;
    let now = now_epoch().map_err(AuthError::from)?;
    let db = fleet_db().map_err(AuthError::from)?;
    let user = db
        .query_row(
            r#"
SELECT users.id, users.email, users.full_name, users.role, users.invite_id, users.last_seen_at
FROM sessions
JOIN users ON users.id = sessions.user_id
WHERE sessions.token_hash = ?1
"#,
            params![token_hash],
            user_from_row,
        )
        .optional()
        .map_err(AuthError::from)?
        .ok_or(AuthError::Unauthorized)?;
    db.execute(
        "UPDATE sessions SET last_seen_at = ?2 WHERE token_hash = ?1",
        params![token_hash, now],
    )
    .map_err(AuthError::from)?;
    db.execute(
        "UPDATE users SET last_seen_at = ?2, updated_at = ?2 WHERE id = ?1",
        params![user.id, now],
    )
    .map_err(AuthError::from)?;
    Ok(AuthUser {
        last_seen_at: Some(now),
        ..user
    })
}

pub(crate) fn require_admin(headers: &HeaderMap) -> Result<AuthUser, AuthError> {
    let user = current_user(headers)?;
    if user.role == "admin" {
        Ok(user)
    } else {
        Err(AuthError::Forbidden)
    }
}

pub(crate) fn authorize_workspace(
    headers: &HeaderMap,
    name: &str,
) -> Result<WorkspaceRecord, AuthError> {
    let user = current_user(headers)?;
    let workspace = workspace_get(name).map_err(AuthError::from)?;
    if user.role == "admin" || workspace.owner_user_id == Some(user.id) {
        Ok(workspace)
    } else {
        Err(AuthError::Forbidden)
    }
}

pub(crate) fn visible_workspaces(headers: &HeaderMap) -> Result<Vec<WorkspaceRecord>, AuthError> {
    let user = current_user(headers)?;
    if user.role == "admin" {
        return workspace_all().map_err(AuthError::from);
    }
    Ok(workspace_for_user(user.id)
        .map_err(AuthError::from)?
        .into_iter()
        .collect())
}

async fn login(Json(request): Json<LoginRequest>) -> Result<Response, AuthError> {
    let (user, token) = authenticate_session(&request.email, &request.password)?;
    let workspace = workspace_for_user(user.id)?;
    let cookie = session_cookie(&token);
    Ok((
        [(SET_COOKIE, cookie)],
        Json(LoginResponse {
            ok: true,
            user,
            workspace,
        }),
    )
        .into_response())
}

async fn signup(Json(request): Json<SignupRequest>) -> Result<Response, AuthError> {
    let (user, token) = create_user_session(
        &request.full_name,
        &request.email,
        request.code.as_deref(),
        &request.password,
    )?;
    let workspace = workspace_for_user(user.id)?;
    let cookie = session_cookie(&token);
    Ok((
        [(SET_COOKIE, cookie)],
        Json(LoginResponse {
            ok: true,
            user,
            workspace,
        }),
    )
        .into_response())
}

async fn logout(headers: HeaderMap) -> Result<Response, AuthError> {
    if let Some(token) = cookie_value(&headers, SESSION_COOKIE) {
        let token_hash = session_token_hash(token)?;
        let db = fleet_db()?;
        db.execute(
            "DELETE FROM sessions WHERE token_hash = ?1",
            params![token_hash],
        )?;
    }
    Ok((
        [(SET_COOKIE, clear_session_cookie())],
        Json(OkResponse { ok: true }),
    )
        .into_response())
}

async fn me(headers: HeaderMap) -> Result<Json<MeResponse>, AuthError> {
    let user = current_user(&headers)?;
    let workspace = workspace_for_user(user.id)?;
    Ok(Json(MeResponse { user, workspace }))
}

async fn setup_me(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<SetupRequest>,
) -> Result<Json<MeResponse>, AuthError> {
    let user = current_user(&headers)?;
    let full_name = request.full_name.trim();
    let agent_name = request.agent_name.trim();
    if full_name.is_empty() || agent_name.is_empty() {
        return Err(AuthError::BadRequest(
            "name and agent name are required".to_string(),
        ));
    }
    let db = fleet_db()?;
    let now = now_epoch()?;
    db.execute(
        "UPDATE users SET full_name = ?2, updated_at = ?3 WHERE id = ?1",
        params![user.id, full_name, now],
    )?;

    let workspace = match workspace_for_user(user.id)? {
        Some(existing) => existing,
        None => create_owned_workspace(&state, &user, agent_name)?,
    };
    let refreshed = user_get(user.id)?;
    Ok(Json(MeResponse {
        user: refreshed,
        workspace: Some(workspace),
    }))
}

async fn admin_users(headers: HeaderMap) -> Result<Json<UsersResponse>, AuthError> {
    require_admin(&headers)?;
    Ok(Json(UsersResponse { users: user_all()? }))
}

async fn admin_infra(headers: HeaderMap) -> Result<Json<AdminInfraResponse>, AuthError> {
    require_admin(&headers)?;
    let now = now_epoch()?;
    let stale_cutoff = now.saturating_sub(
        i64::try_from(env_u64("MOM_NODE_STALE_SECS", 60))
            .context("MOM_NODE_STALE_SECS is too large")?,
    );
    let users = user_all()?;
    let workspaces = workspace_all()?;
    let nodes = node_all()?;

    let mut workspaces_by_owner = workspaces
        .iter()
        .filter_map(|workspace| workspace.owner_user_id.map(|owner| (owner, workspace)))
        .collect::<HashMap<_, _>>();

    let users = users
        .into_iter()
        .map(|user| {
            let workspace =
                workspaces_by_owner
                    .remove(&user.id)
                    .map(|workspace| AdminInfraWorkspace {
                        workspace: workspace.clone(),
                    });
            AdminInfraUser { user, workspace }
        })
        .collect();

    let nodes = nodes
        .into_iter()
        .map(|node| {
            let assigned = workspaces
                .iter()
                .filter(|workspace| {
                    workspace.node_id.as_deref() == Some(node.node_id.as_str())
                        && workspace.status != "removed"
                })
                .collect::<Vec<_>>();
            let active_workspaces = assigned
                .iter()
                .filter(|workspace| {
                    matches!(
                        workspace.status.as_str(),
                        "running" | "starting" | "restoring"
                    )
                })
                .count();
            let desired_running_workspaces = assigned
                .iter()
                .filter(|workspace| workspace.desired_state == "running")
                .count();
            let allocated_cpus = assigned
                .iter()
                .map(|workspace| u32::from(workspace.cpus))
                .sum();
            let allocated_memory_mib = assigned
                .iter()
                .map(|workspace| u64::from(workspace.memory_mib))
                .sum();
            AdminInfraNode {
                stale: node.last_seen_at < stale_cutoff,
                active_workspaces,
                desired_running_workspaces,
                allocated_cpus,
                allocated_memory_mib,
                node,
            }
        })
        .collect();

    Ok(Json(AdminInfraResponse {
        generated_at: now,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        users,
        nodes,
        jobs: recent_jobs(25)?,
        workspace_status_counts: counts_map(workspace_status_counts()?),
        node_status_counts: counts_map(node_status_counts()?),
        job_status_counts: counts_map(job_counts()?),
    }))
}

async fn admin_invites(headers: HeaderMap) -> Result<Json<InvitesResponse>, AuthError> {
    require_admin(&headers)?;
    Ok(Json(InvitesResponse {
        invites: invite_all()?,
    }))
}

async fn admin_create_invite(
    headers: HeaderMap,
    Json(request): Json<CreateInviteRequest>,
) -> Result<Json<InviteCreatedResponse>, AuthError> {
    let admin = require_admin(&headers)?;
    let label = request.label.trim();
    if label.is_empty() {
        return Err(AuthError::BadRequest(
            "invite label is required".to_string(),
        ));
    }
    if request.max_uses.is_some_and(|value| value < 1) {
        return Err(AuthError::BadRequest(
            "max uses must be positive".to_string(),
        ));
    }
    let code = generate_access_code();
    let code = normalize_access_code(&code);
    let now = now_epoch()?;
    let db = fleet_db()?;
    db.execute(
        r#"
INSERT INTO invites (
    label, description, code, role, max_uses, used_count,
    active, created_by_user_id, created_at
) VALUES (?1, ?2, ?3, 'user', ?4, 0, 1, ?5, ?6)
"#,
        params![
            label,
            request.description.trim(),
            code,
            request.max_uses,
            admin.id,
            now,
        ],
    )?;
    let id = db.last_insert_rowid();
    Ok(Json(InviteCreatedResponse {
        invite: invite_get(id)?,
        code,
    }))
}

async fn admin_invite_detail(
    headers: HeaderMap,
    AxumPath(id): AxumPath<i64>,
) -> Result<Json<InviteDetailResponse>, AuthError> {
    require_admin(&headers)?;
    let invite = invite_get(id)?;
    let users = users_for_invite(id)?;
    let workspaces = workspaces_for_invite(id)?;
    Ok(Json(InviteDetailResponse {
        invite,
        users,
        workspaces,
    }))
}

async fn admin_disable_invite(
    headers: HeaderMap,
    AxumPath(id): AxumPath<i64>,
) -> Result<Json<InviteRecord>, AuthError> {
    require_admin(&headers)?;
    let db = fleet_db()?;
    let changed = db.execute("UPDATE invites SET active = 0 WHERE id = ?1", params![id])?;
    if changed == 0 {
        return Err(AuthError::NotFound);
    }
    Ok(Json(invite_get(id)?))
}

async fn admin_delete_user(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<i64>,
) -> Result<Json<OkResponse>, AuthError> {
    let admin = require_admin(&headers)?;
    if admin.id == id {
        return Err(AuthError::BadRequest(
            "refusing to delete your own user".to_string(),
        ));
    }
    delete_user_and_workspace(&state, id)?;
    Ok(Json(OkResponse { ok: true }))
}

fn authenticate_session(email: &str, password: &str) -> Result<(AuthUser, String), AuthError> {
    let email = normalize_email(email)?;
    let now = now_epoch()?;
    let mut db = fleet_db()?;
    let tx = db.transaction()?;
    let password_hash = tx
        .query_row(
            "SELECT password_hash FROM users WHERE email = ?1",
            params![email],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(password_hash) = password_hash else {
        return Err(AuthError::Unauthorized);
    };
    if !verify_password(password, &password_hash)? {
        return Err(AuthError::Unauthorized);
    }
    let user = tx.query_row(
        "SELECT id, email, full_name, role, invite_id, last_seen_at FROM users WHERE email = ?1",
        params![email],
        user_from_row,
    )?;
    tx.execute(
        "UPDATE users SET last_seen_at = ?2, updated_at = ?2 WHERE id = ?1",
        params![user.id, now],
    )?;
    let token = create_session_in_tx(&tx, user.id)?;
    tx.commit()?;
    Ok((
        AuthUser {
            last_seen_at: Some(now),
            ..user
        },
        token,
    ))
}

fn create_user_session(
    full_name: &str,
    email: &str,
    invite_code: Option<&str>,
    password: &str,
) -> Result<(AuthUser, String), AuthError> {
    let full_name = normalize_full_name(full_name)?;
    let email = normalize_email(email)?;
    validate_password(password)?;
    let password_hash = hash_password(password)?;
    let code = invite_code
        .map(normalize_access_code)
        .filter(|value| !value.is_empty());
    let now = now_epoch()?;
    let mut db = fleet_db()?;
    let tx = db.transaction()?;
    let email_exists: bool = tx.query_row(
        "SELECT COUNT(*) > 0 FROM users WHERE email = ?1",
        params![email],
        |row| row.get(0),
    )?;
    if email_exists {
        return Err(AuthError::BadRequest(
            "email is already registered".to_string(),
        ));
    }
    let (role, invite_id) = if user_count(&tx)? == 0 {
        ("admin".to_string(), None)
    } else {
        let code = code.ok_or(AuthError::InvalidSignupCode)?;
        let invite = tx
            .query_row(
                r#"
SELECT id, label, description, code, role, max_uses, used_count, active, created_by_user_id, created_at
FROM invites
WHERE code = ?1 AND active = 1
"#,
                params![code],
                invite_from_row,
            )
            .optional()?
            .ok_or(AuthError::InvalidSignupCode)?;
        if invite.max_uses.is_some_and(|max| invite.used_count >= max) {
            return Err(AuthError::InvalidSignupCode);
        }
        tx.execute(
            "UPDATE invites SET used_count = used_count + 1 WHERE id = ?1",
            params![invite.id],
        )?;
        (invite.role, Some(invite.id))
    };
    let user_id = insert_user_with_password(
        &tx,
        &email,
        &password_hash,
        &full_name,
        &role,
        invite_id,
        now,
    )?;
    if let Some(invite_id) = invite_id {
        tx.execute(
            "INSERT INTO invite_redemptions (invite_id, user_id, redeemed_at) VALUES (?1, ?2, ?3)",
            params![invite_id, user_id, now],
        )?;
    }
    let user = tx.query_row(
        "SELECT id, email, full_name, role, invite_id, last_seen_at FROM users WHERE email = ?1",
        params![email],
        user_from_row,
    )?;
    let token = create_session_in_tx(&tx, user.id)?;
    tx.commit()?;
    Ok((user, token))
}

fn user_count(tx: &rusqlite::Transaction<'_>) -> Result<i64, AuthError> {
    Ok(tx.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?)
}

fn insert_user_with_password(
    tx: &rusqlite::Transaction<'_>,
    email: &str,
    password_hash: &str,
    full_name: &str,
    role: &str,
    invite_id: Option<i64>,
    now: i64,
) -> Result<i64, AuthError> {
    tx.execute(
        r#"
INSERT INTO users (email, password_hash, full_name, role, invite_id, created_at, updated_at, last_seen_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?6)
"#,
        params![email, password_hash, full_name, role, invite_id, now],
    )?;
    Ok(tx.last_insert_rowid())
}

fn create_session_in_tx(tx: &rusqlite::Transaction<'_>, user_id: i64) -> Result<String, AuthError> {
    let token = generate_session_token();
    let now = now_epoch()?;
    tx.execute(
        "INSERT INTO sessions (user_id, token_hash, created_at, last_seen_at) VALUES (?1, ?2, ?3, ?3)",
        params![user_id, session_token_hash(&token)?, now],
    )?;
    Ok(token)
}

fn create_owned_workspace(
    state: &ApiState,
    user: &AuthUser,
    agent_name: &str,
) -> Result<WorkspaceRecord, AuthError> {
    let display_name = agent_name.trim().to_string();
    let name = workspace_slug_from_name(&format!("{} {}", user.email, agent_name))?;
    let vm_name = format!("mom-{name}");
    let workspace_dir_name = format!("mom-{name}-workspace");
    let node_id = workspace_upsert_pending_on_ready_node(
        WorkspaceUpsert {
            name: &name,
            display_name: &display_name,
            user_id: &user.email,
            owner_user_id: Some(user.id),
            agent_name: Some(agent_name),
            vm_name: &vm_name,
            workspace_dir_name: &workspace_dir_name,
            assigned_node_id: None,
            cpus: default_workspace_cpus(),
            memory_mib: u32::try_from(default_workspace_memory())
                .context("default workspace memory too large")?,
            workspace_quota_mib: default_workspace_quota(),
            idle_timeout_secs: default_workspace_idle_timeout(),
            backup_interval_secs: default_workspace_backup_interval(),
        },
        None,
    )
    .map_err(|error| {
        if error.to_string().contains("no ready worker nodes") {
            AuthError::Unavailable("no ready worker nodes are registered".to_string())
        } else {
            AuthError::Anyhow(error)
        }
    })?;
    create_job(CreateJobRequest {
        workspace_name: name.clone(),
        kind: "create".to_string(),
        node_id: Some(node_id),
        payload: json!({
            "user": user.email,
            "cpus": default_workspace_cpus(),
            "memory": default_workspace_memory(),
            "workspace_quota": default_workspace_quota(),
            "idle_timeout": default_workspace_idle_timeout(),
            "backup_interval": default_workspace_backup_interval()
        }),
    })?;
    let _ = state.notifier.send("job_available".to_string());
    workspace_get(&name).map_err(AuthError::from)
}

fn workspace_for_user(user_id: i64) -> Result<Option<WorkspaceRecord>> {
    let db = fleet_db()?;
    db.query_row(
        r#"
SELECT workspace_id, name, slug, display_name, user_id, vm_name, workspace_dir_name, desired_state, cpus, memory_mib,
       node_id, status, workspace_quota_mib, idle_timeout_secs, backup_interval_secs, last_used_at, last_backup_at,
       owner_user_id, agent_name, vm_version
FROM workspaces
WHERE owner_user_id = ?1
ORDER BY name
LIMIT 1
"#,
        params![user_id],
        workspace_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn delete_user_and_workspace(state: &ApiState, user_id: i64) -> Result<(), AuthError> {
    let db = fleet_db()?;
    let workspace: Option<WorkspaceRecord> = db
        .query_row(
            r#"
SELECT workspace_id, name, slug, display_name, user_id, vm_name, workspace_dir_name, desired_state, cpus, memory_mib,
       node_id, status, workspace_quota_mib, idle_timeout_secs, backup_interval_secs, last_used_at, last_backup_at,
       owner_user_id, agent_name, vm_version
FROM workspaces
WHERE owner_user_id = ?1
"#,
            params![user_id],
            workspace_from_row,
        )
        .optional()?;
    if let Some(workspace) = workspace {
        match workspace.node_id.clone() {
            Some(node_id) => {
                workspace_set_desired(&workspace.name, "removed")?;
                workspace_mark_status(&workspace.name, "removing")?;
                create_job(CreateJobRequest {
                    workspace_name: workspace.name.clone(),
                    kind: "remove".to_string(),
                    node_id: Some(node_id),
                    payload: json!({ "remove_workspace_dir": true }),
                })?;
                let _ = state.notifier.send("job_available".to_string());
            }
            None => {
                db.execute(
                    "UPDATE workspaces SET desired_state = 'removed', status = 'removed', updated_at = ?2 WHERE name = ?1",
                    params![workspace.name, now_epoch()?],
                )?;
            }
        }
    }
    db.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])?;
    let changed = db.execute("DELETE FROM users WHERE id = ?1", params![user_id])?;
    if changed == 0 {
        return Err(AuthError::NotFound);
    }
    Ok(())
}

fn user_get(id: i64) -> Result<AuthUser, AuthError> {
    let db = fleet_db()?;
    db.query_row(
        "SELECT id, email, full_name, role, invite_id, last_seen_at FROM users WHERE id = ?1",
        params![id],
        user_from_row,
    )
    .optional()?
    .ok_or(AuthError::NotFound)
}

fn user_all() -> Result<Vec<AuthUser>, AuthError> {
    let db = fleet_db()?;
    let mut stmt = db.prepare(
        "SELECT id, email, full_name, role, invite_id, last_seen_at FROM users ORDER BY role, email",
    )?;
    Ok(stmt
        .query_map([], user_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn counts_map(counts: Vec<(String, i64)>) -> HashMap<String, i64> {
    counts.into_iter().collect()
}

fn users_for_invite(invite_id: i64) -> Result<Vec<AuthUser>, AuthError> {
    let db = fleet_db()?;
    let mut stmt = db.prepare(
        "SELECT id, email, full_name, role, invite_id, last_seen_at FROM users WHERE invite_id = ?1 ORDER BY email",
    )?;
    Ok(stmt
        .query_map(params![invite_id], user_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn workspaces_for_invite(invite_id: i64) -> Result<Vec<WorkspaceRecord>, AuthError> {
    let db = fleet_db()?;
    let mut stmt = db.prepare(
        r#"
SELECT workspace_id, name, slug, display_name, user_id, vm_name, workspace_dir_name, desired_state, cpus, memory_mib,
       node_id, status, workspace_quota_mib, idle_timeout_secs, backup_interval_secs, last_used_at, last_backup_at,
       owner_user_id, agent_name, vm_version
FROM workspaces
JOIN users ON users.id = workspaces.owner_user_id
WHERE users.invite_id = ?1
ORDER BY workspaces.name
"#,
    )?;
    Ok(stmt
        .query_map(params![invite_id], workspace_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn invite_all() -> Result<Vec<InviteRecord>, AuthError> {
    let db = fleet_db()?;
    let mut stmt = db.prepare(
        r#"
SELECT id, label, description, code, role, max_uses, used_count, active, created_by_user_id, created_at
FROM invites
ORDER BY created_at DESC, id DESC
"#,
    )?;
    Ok(stmt
        .query_map([], invite_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn invite_get(id: i64) -> Result<InviteRecord, AuthError> {
    let db = fleet_db()?;
    db.query_row(
        r#"
SELECT id, label, description, code, role, max_uses, used_count, active, created_by_user_id, created_at
FROM invites
WHERE id = ?1
"#,
        params![id],
        invite_from_row,
    )
    .optional()?
    .ok_or(AuthError::NotFound)
}

fn user_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuthUser> {
    Ok(AuthUser {
        id: row.get(0)?,
        email: row.get(1)?,
        full_name: row.get(2)?,
        role: row.get(3)?,
        invite_id: row.get(4)?,
        last_seen_at: row.get(5)?,
    })
}

fn invite_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InviteRecord> {
    let active: i64 = row.get(7)?;
    Ok(InviteRecord {
        id: row.get(0)?,
        label: row.get(1)?,
        description: row.get(2)?,
        code: row.get(3)?,
        role: row.get(4)?,
        max_uses: row.get(5)?,
        used_count: row.get(6)?,
        active: active != 0,
        created_by_user_id: row.get(8)?,
        created_at: row.get(9)?,
    })
}

fn normalize_email(email: &str) -> Result<String, AuthError> {
    let email = email.trim().to_ascii_lowercase();
    if !is_valid_email(&email) {
        return Err(AuthError::BadRequest("valid email is required".to_string()));
    }
    Ok(email)
}

fn normalize_full_name(full_name: &str) -> Result<String, AuthError> {
    let full_name = full_name.trim();
    if full_name.is_empty() {
        return Err(AuthError::BadRequest("name is required".to_string()));
    }
    Ok(full_name.to_string())
}

fn is_valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.contains('@')
        && !email.chars().any(char::is_whitespace)
}

fn session_token_hash(token: &str) -> Result<String, AuthError> {
    hmac_hex(token.trim().as_bytes())
}

fn validate_password(password: &str) -> Result<(), AuthError> {
    if password.len() < 8 {
        return Err(AuthError::BadRequest(
            "password must be at least 8 characters".to_string(),
        ));
    }
    Ok(())
}

fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|error| AuthError::Anyhow(anyhow!("hash password: {error}")))?
        .to_string())
}

fn verify_password(password: &str, hash: &str) -> Result<bool, AuthError> {
    let parsed = PasswordHash::new(hash)
        .map_err(|error| AuthError::Anyhow(anyhow!("parse password hash: {error}")))?;
    Ok(Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok())
}

fn hmac_hex(bytes: &[u8]) -> Result<String, AuthError> {
    let config = load_mom_config().map_err(AuthError::from)?;
    let secret = config.auth_secret().map_err(AuthError::from)?;
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| AuthError::Config("auth.secret is invalid".to_string()))?;
    mac.update(bytes);
    Ok(format!("{:x}", mac.finalize().into_bytes()))
}

fn normalize_access_code(code: &str) -> String {
    code.trim()
        .chars()
        .map(|ch| match ch {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
            _ => ch.to_ascii_uppercase(),
        })
        .collect()
}

fn generate_access_code() -> String {
    random_code_part(8)
}

fn generate_session_token() -> String {
    format!(
        "session_{}_{}",
        now_epoch().unwrap_or_default(),
        random_code_part(48)
    )
}

fn random_code_part(length: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    (0..length)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find_map(|(cookie_name, value)| (cookie_name == name).then_some(value))
}

fn session_cookie(token: &str) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax{}",
        secure_cookie_suffix()
    )
}

fn clear_session_cookie() -> String {
    format!(
        "{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax{}; Max-Age=0",
        secure_cookie_suffix()
    )
}

fn secure_cookie_suffix() -> &'static str {
    if env_flag_enabled("MOM_SESSION_COOKIE_SECURE") {
        "; Secure"
    } else {
        ""
    }
}

fn env_flag_enabled(name: &str) -> bool {
    env::var(name)
        .ok()
        .is_some_and(|value| bool_flag_enabled(&value))
}

fn bool_flag_enabled(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[derive(Debug)]
pub(crate) enum AuthError {
    Unauthorized,
    InvalidSignupCode,
    Forbidden,
    NotFound,
    Unavailable(String),
    BadRequest(String),
    Config(String),
    Anyhow(anyhow::Error),
}

impl From<anyhow::Error> for AuthError {
    fn from(error: anyhow::Error) -> Self {
        Self::Anyhow(error)
    }
}

impl From<rusqlite::Error> for AuthError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Anyhow(error.into())
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            AuthError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "Invalid email or password.".to_string(),
            ),
            AuthError::InvalidSignupCode => {
                (StatusCode::UNAUTHORIZED, "Invalid signup code.".to_string())
            }
            AuthError::Forbidden => (
                StatusCode::FORBIDDEN,
                "Admin access is required.".to_string(),
            ),
            AuthError::NotFound => (StatusCode::NOT_FOUND, "Not found.".to_string()),
            AuthError::Unavailable(error) => (StatusCode::SERVICE_UNAVAILABLE, error),
            AuthError::BadRequest(error) => (StatusCode::BAD_REQUEST, error),
            AuthError::Config(error) => (StatusCode::INTERNAL_SERVER_ERROR, error),
            AuthError::Anyhow(error) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
        };
        (status, Json(AuthErrorBody { error })).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_flag_enabled_accepts_explicit_true_values() {
        assert!(bool_flag_enabled("1"));
        assert!(bool_flag_enabled("true"));
        assert!(bool_flag_enabled(" YES "));
        assert!(bool_flag_enabled("on"));
        assert!(!bool_flag_enabled("0"));
        assert!(!bool_flag_enabled("false"));
        assert!(!bool_flag_enabled(""));
    }
}
