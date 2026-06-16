use super::*;

pub(crate) async fn backup_workspace(
    workspace: &WorkspaceRecord,
    leave_stopped: bool,
) -> Result<()> {
    let workspace_dir_path = workspace_dir_path(&workspace.workspace_dir_name)?;
    if !workspace_assigned_to_local(workspace)? {
        return backup_workspace_via_worker(workspace, leave_stopped, Some(&workspace_dir_path))
            .await;
    }
    if !workspace_dir_path.exists() {
        return backup_workspace_via_worker(workspace, leave_stopped, Some(&workspace_dir_path))
            .await;
    }

    let was_running = match get_vm(&workspace.vm_name).await {
        Ok(handle) => {
            let status = handle.status();
            let running = status.is_running();
            if status.is_started() {
                record_workspace_event(
                    &workspace.name,
                    "backup_stop_started",
                    "running",
                    "stopping workspace before backup",
                    json!({ "vm": workspace.vm_name }),
                )?;
                println!("stopping {} before backup", workspace.vm_name);
                handle.stop_with_timeout(Duration::from_secs(20)).await?;
                workspace_mark_status(&workspace.name, "backup-stopped")?;
            }
            running
        }
        Err(_) => false,
    };

    log_record(
        "info",
        "workspace_backup_started",
        Some(&workspace.name),
        "workspace backup started",
    );
    record_workspace_event(
        &workspace.name,
        "workspace_backup_started",
        "running",
        "workspace directory backup started",
        json!({ "workspace_dir": workspace.workspace_dir_name }),
    )?;
    let artifact = run_restic_backup(workspace, &workspace_dir_path).await?;
    let backup_id = record_backup_artifact(workspace, &artifact, "succeeded")?;
    workspace_mark_backup(&workspace.name)?;
    record_workspace_event(
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
    )?;

    if was_running && !leave_stopped && workspace.desired_state == "running" {
        workspace_ensure_running(workspace).await?;
    }
    Ok(())
}

async fn backup_workspace_via_worker(
    workspace: &WorkspaceRecord,
    leave_stopped: bool,
    local_workspace_dir_path: Option<&Path>,
) -> Result<()> {
    let Some(node_id) = workspace.node_id.as_deref() else {
        bail!(
            "workspace directory {} does not exist at {}, and workspace {} is not assigned to a worker node",
            workspace.workspace_dir_name,
            local_workspace_dir_path
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "-".to_string()),
            workspace.name
        );
    };
    require_claimable_node(node_id).with_context(|| {
        format!(
            "workspace {} is assigned to node {node_id}, but that node is not accepting jobs",
            workspace.workspace_dir_name,
        )
    })?;
    let job = create_job(CreateJobRequest {
        workspace_name: workspace.name.clone(),
        node_id: Some(node_id.to_string()),
        kind: "backup".to_string(),
        payload: json!({ "leave_stopped": leave_stopped }),
    })?;
    println!(
        "queued backup job {} for workspace {} on node {}",
        job.id, workspace.name, node_id
    );
    wait_for_worker_job("backup", &job.id).await?;
    Ok(())
}

pub(crate) async fn wait_for_worker_job(kind: &str, job_id: &str) -> Result<JobRecord> {
    let deadline = now_epoch()?.saturating_add(900);
    loop {
        let job = job_get(job_id)?;
        match job.status.as_str() {
            "succeeded" => {
                println!("{kind} job {job_id} succeeded");
                return Ok(job);
            }
            "failed" | "canceled" => {
                bail!(
                    "{kind} job {job_id} ended with status {}: {}",
                    job.status,
                    job.output_json
                        .as_deref()
                        .unwrap_or("no job output was recorded")
                );
            }
            _ => {
                if now_epoch()? >= deadline {
                    bail!(
                        "timed out waiting for {kind} job {job_id}; current status is {}",
                        job.status
                    );
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

pub(crate) async fn run_restic_backup(
    workspace: &WorkspaceRecord,
    workspace_dir_path: &Path,
) -> Result<BackupArtifact> {
    if env::var_os("RESTIC_REPOSITORY").is_none() {
        bail!("RESTIC_REPOSITORY must be set before workspace backups can run");
    }
    if !command_exists("restic").await {
        bail!("restic must be installed before workspace backups can run");
    }

    println!("running restic backup for {}", workspace.name);
    let output = TokioCommand::new("restic")
        .arg("backup")
        .arg("--json")
        .arg(workspace_dir_path)
        .arg("--tag")
        .arg("agentmom")
        .arg("--tag")
        .arg(&workspace.name)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        bail!(
            "restic backup exited with {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let snapshot_id = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|value| {
            (value.get("message_type").and_then(Value::as_str) == Some("summary"))
                .then(|| value.get("snapshot_id").and_then(Value::as_str))
                .flatten()
                .map(str::to_string)
        })
        .ok_or_else(|| anyhow!("restic backup did not report a snapshot_id"))?;
    let repository = env::var("RESTIC_REPOSITORY").unwrap_or_else(|_| "restic".to_string());
    Ok(BackupArtifact {
        kind: "restic".to_string(),
        location: format!("{repository}#{snapshot_id}"),
        size_bytes: None,
    })
}

pub(crate) fn workspace_backups(name: &str) -> Result<()> {
    let backups = backup_records_for_workspace(name)?;
    println!(
        "{:<28} {:<12} {:<10} {:<12} LOCATION",
        "ID", "KIND", "STATUS", "CREATED"
    );
    for backup in backups {
        println!(
            "{:<28} {:<12} {:<10} {:<12} {}",
            backup.id, backup.kind, backup.status, backup.created_at, backup.location
        );
    }
    Ok(())
}

pub(crate) async fn workspace_restore(name: &str, backup_id: Option<&str>) -> Result<()> {
    let workspace = workspace_get(name)?;
    let backup = match backup_id {
        Some(id) => backup_record_get(id)?,
        None => latest_restic_backup(name)?,
    };
    if backup.kind != "restic" {
        bail!(
            "restore supports restic artifacts only; backup {} is {}",
            backup.id,
            backup.kind
        );
    }
    if backup.workspace_name != workspace.name {
        bail!(
            "backup {} belongs to workspace {}, not {}",
            backup.id,
            backup.workspace_name,
            workspace.name
        );
    }
    if !workspace_assigned_to_local(&workspace)? {
        return restore_workspace_via_worker(&workspace, &backup).await;
    }
    workspace_stop(name).await?;
    let workspace_dir_path = workspace_dir_path(&workspace.workspace_dir_name)?;
    run_restic_restore(&backup.id, &backup.location, &workspace_dir_path).await?;
    workspace_mark_status(name, "restored")?;
    record_workspace_event(
        name,
        "workspace_restored",
        "succeeded",
        "workspace directory restored from backup",
        json!({ "backup_id": backup.id, "location": backup.location }),
    )?;
    println!("restored workspace {name} from {}", backup.id);
    Ok(())
}

async fn restore_workspace_via_worker(
    workspace: &WorkspaceRecord,
    backup: &BackupRecord,
) -> Result<()> {
    let Some(node_id) = workspace.node_id.as_deref() else {
        bail!(
            "workspace {} is not assigned to a worker node; cannot queue restore",
            workspace.name
        );
    };
    require_claimable_node(node_id).with_context(|| {
        format!(
            "workspace {} is assigned to node {node_id}, but that node is not accepting jobs",
            workspace.name
        )
    })?;
    let job = create_job(CreateJobRequest {
        workspace_name: workspace.name.clone(),
        node_id: Some(node_id.to_string()),
        kind: "restore".to_string(),
        payload: json!({
            "backup_id": backup.id,
            "backup_location": backup.location,
            "backup_workspace_name": backup.workspace_name,
            "desired_state": workspace.desired_state
        }),
    })?;
    println!(
        "queued restore job {} for workspace {} on node {}",
        job.id, workspace.name, node_id
    );
    wait_for_worker_job("restore", &job.id).await?;
    Ok(())
}

fn workspace_assigned_to_local(workspace: &WorkspaceRecord) -> Result<bool> {
    let Some(assigned_node) = workspace.node_id.as_deref() else {
        return Ok(true);
    };
    Ok(assigned_node == node_id()?)
}

pub(crate) async fn run_restic_restore(
    backup_id: &str,
    backup_location: &str,
    workspace_dir_path: &Path,
) -> Result<()> {
    validate_backup_id_path_component(backup_id)?;
    if env::var_os("RESTIC_REPOSITORY").is_none() {
        bail!("RESTIC_REPOSITORY must be set before workspace restore can run");
    }
    if !command_exists("restic").await {
        bail!("restic must be installed before workspace restore can run");
    }
    let (repository, snapshot) = backup_location
        .rsplit_once('#')
        .filter(|(repository, snapshot)| !repository.is_empty() && !snapshot.is_empty())
        .ok_or_else(|| anyhow!("backup {backup_id} is missing restic snapshot id"))?;
    let ambient_repository = env::var("RESTIC_REPOSITORY")
        .context("RESTIC_REPOSITORY must be set before workspace restore can run")?;
    if ambient_repository != repository {
        bail!(
            "backup {backup_id} was recorded from restic repository {repository}, but RESTIC_REPOSITORY is {ambient_repository}"
        );
    }
    let parent = workspace_dir_path.parent().ok_or_else(|| {
        anyhow!(
            "workspace directory has no parent: {}",
            workspace_dir_path.display()
        )
    })?;
    fs::create_dir_all(parent)?;
    let workspace_dir_name = workspace_dir_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            anyhow!(
                "workspace directory path has no final component: {}",
                workspace_dir_path.display()
            )
        })?;
    let restore_tmp = parent.join(format!(".restore-{backup_id}"));
    let previous_workspace_dir = parent.join(format!(".pre-restore-{backup_id}"));
    recover_interrupted_restore(
        workspace_dir_path,
        &restore_tmp,
        &previous_workspace_dir,
        workspace_dir_name,
    )?;
    if restore_tmp.exists() {
        fs::remove_dir_all(&restore_tmp)
            .with_context(|| format!("remove stale restore dir {}", restore_tmp.display()))?;
    }
    fs::create_dir_all(&restore_tmp)
        .with_context(|| format!("create restore dir {}", restore_tmp.display()))?;
    let status = TokioCommand::new("restic")
        .arg("restore")
        .arg("-r")
        .arg(repository)
        .arg(snapshot)
        .arg("--target")
        .arg(&restore_tmp)
        .stdin(Stdio::null())
        .status()
        .await?;
    if !status.success() {
        bail!("restic restore exited with {status}");
    }
    let restored_workspace_dir =
        restored_workspace_dir_path(&restore_tmp, workspace_dir_path, workspace_dir_name)?;
    if previous_workspace_dir.exists() {
        fs::remove_dir_all(&previous_workspace_dir).with_context(|| {
            format!(
                "remove stale previous restore dir {}",
                previous_workspace_dir.display()
            )
        })?;
    }
    commit_restored_workspace_dir(
        workspace_dir_path,
        &restored_workspace_dir,
        &previous_workspace_dir,
    )?;
    fs::remove_dir_all(&restore_tmp)
        .with_context(|| format!("remove restore dir {}", restore_tmp.display()))?;
    Ok(())
}

fn recover_interrupted_restore(
    workspace_dir_path: &Path,
    restore_tmp: &Path,
    previous_workspace_dir: &Path,
    workspace_dir_name: &str,
) -> Result<()> {
    if workspace_dir_path.exists() {
        return Ok(());
    }
    if restore_tmp.exists()
        && let Ok(restored_workspace_dir) =
            restored_workspace_dir_path(restore_tmp, workspace_dir_path, workspace_dir_name)
        && restored_workspace_dir.exists()
    {
        fs::rename(&restored_workspace_dir, workspace_dir_path).with_context(|| {
            format!(
                "complete interrupted restore by moving {} to {}",
                restored_workspace_dir.display(),
                workspace_dir_path.display()
            )
        })?;
        if previous_workspace_dir.exists() {
            fs::remove_dir_all(previous_workspace_dir).with_context(|| {
                format!(
                    "remove previous workspace dir {} after interrupted restore recovery",
                    previous_workspace_dir.display()
                )
            })?;
        }
        return Ok(());
    }
    if previous_workspace_dir.exists() {
        fs::rename(previous_workspace_dir, workspace_dir_path).with_context(|| {
            format!(
                "recover interrupted restore by moving {} back to {}",
                previous_workspace_dir.display(),
                workspace_dir_path.display()
            )
        })?;
    }
    Ok(())
}

fn commit_restored_workspace_dir(
    workspace_dir_path: &Path,
    restored_workspace_dir: &Path,
    previous_workspace_dir: &Path,
) -> Result<()> {
    if workspace_dir_path.exists() {
        fs::rename(workspace_dir_path, previous_workspace_dir).with_context(|| {
            format!(
                "move current workspace directory {} aside to {}",
                workspace_dir_path.display(),
                previous_workspace_dir.display()
            )
        })?;
    }
    fs::rename(restored_workspace_dir, workspace_dir_path).with_context(|| {
        if previous_workspace_dir.exists() && !workspace_dir_path.exists() {
            let _ = fs::rename(previous_workspace_dir, workspace_dir_path);
        }
        format!(
            "move restored workspace directory {} to {}",
            restored_workspace_dir.display(),
            workspace_dir_path.display()
        )
    })?;
    if previous_workspace_dir.exists() {
        fs::remove_dir_all(previous_workspace_dir).with_context(|| {
            format!(
                "remove previous restored workspace directory {}",
                previous_workspace_dir.display()
            )
        })?;
    }
    Ok(())
}

fn validate_backup_id_path_component(backup_id: &str) -> Result<()> {
    if backup_id.is_empty()
        || !backup_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("backup id is not safe for restore temp paths: {backup_id:?}");
    }
    Ok(())
}

fn restored_workspace_dir_path(
    root: &Path,
    original_path: &Path,
    workspace_dir_name: &str,
) -> Result<PathBuf> {
    if let Ok(relative) = original_path.strip_prefix("/") {
        let path = root.join(relative);
        if path.exists() {
            return Ok(path);
        }
    }
    if !original_path.is_absolute() {
        let path = root.join(original_path);
        if path.exists() {
            return Ok(path);
        }
    }
    find_dir_named(root, workspace_dir_name)?.ok_or_else(|| {
        anyhow!(
            "restic restore did not contain workspace directory {} under {}",
            workspace_dir_name,
            root.display()
        )
    })
}

fn find_dir_named(root: &Path, name: &str) -> Result<Option<PathBuf>> {
    for entry in fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path.file_name().and_then(|part| part.to_str()) == Some(name) {
            return Ok(Some(path));
        }
        if let Some(found) = find_dir_named(&path, name)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

async fn command_exists(name: &str) -> bool {
    TokioCommand::new("sh")
        .arg("-c")
        .arg(format!("command -v {} >/dev/null 2>&1", shell_quote(name)))
        .stdin(Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_restore_with_restored_dir_finishes_commit() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let workspace = dir.path().join("workspace");
        let previous = dir.path().join(".pre-restore-bak_test");
        let restore_tmp = dir.path().join(".restore-bak_test");
        let restored = restore_tmp.join("workspace");
        fs::create_dir_all(&previous)?;
        fs::write(previous.join("marker"), b"old")?;
        fs::create_dir_all(&restored)?;
        fs::write(restored.join("marker"), b"new")?;

        recover_interrupted_restore(&workspace, &restore_tmp, &previous, "workspace")?;

        assert_eq!(fs::read_to_string(workspace.join("marker"))?, "new");
        assert!(!previous.exists());
        Ok(())
    }

    #[test]
    fn interrupted_restore_without_restored_dir_rolls_back_previous() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let workspace = dir.path().join("workspace");
        let previous = dir.path().join(".pre-restore-bak_test");
        let restore_tmp = dir.path().join(".restore-bak_test");
        fs::create_dir_all(&previous)?;
        fs::write(previous.join("marker"), b"old")?;

        recover_interrupted_restore(&workspace, &restore_tmp, &previous, "workspace")?;

        assert_eq!(fs::read_to_string(workspace.join("marker"))?, "old");
        assert!(!previous.exists());
        Ok(())
    }

    #[test]
    fn commit_restored_workspace_rolls_back_if_final_rename_fails() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let workspace = dir.path().join("workspace");
        let previous = dir.path().join(".pre-restore-bak_test");
        let missing_restored = dir.path().join("missing-restored");
        fs::create_dir_all(&workspace)?;
        fs::write(workspace.join("marker"), b"old")?;

        let result = commit_restored_workspace_dir(&workspace, &missing_restored, &previous);

        assert!(result.is_err());
        assert_eq!(fs::read_to_string(workspace.join("marker"))?, "old");
        assert!(!previous.exists());
        Ok(())
    }
}
