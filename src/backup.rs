use super::*;

pub(crate) async fn backup_workspace(
    workspace: &WorkspaceRecord,
    leave_stopped: bool,
) -> Result<()> {
    let was_running = match Sandbox::get(&workspace.sandbox_name).await {
        Ok(handle) => {
            let running = handle.status() == SandboxStatus::Running
                || handle.status() == SandboxStatus::Draining;
            if running {
                record_workspace_event(
                    &workspace.name,
                    "backup_stop_started",
                    "running",
                    "stopping workspace before backup",
                    json!({ "sandbox": workspace.sandbox_name }),
                )?;
                println!("stopping {} before backup", workspace.sandbox_name);
                handle.stop_with_timeout(Duration::from_secs(20)).await?;
                workspace_mark_status(&workspace.name, "backup-stopped")?;
            }
            running
        }
        Err(_) => false,
    };

    let volume_path = microsandbox_volume_path(&workspace.volume_name)?;
    if !volume_path.exists() {
        bail!(
            "workspace volume {} does not exist at {}",
            workspace.volume_name,
            volume_path.display()
        );
    }
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
        "workspace volume backup started",
        json!({ "volume": workspace.volume_name }),
    )?;
    let artifact = run_restic_backup(workspace, &volume_path).await?;
    let backup_id = record_backup_artifact(workspace, &artifact, "succeeded")?;
    workspace_mark_backup(&workspace.name)?;
    record_workspace_event(
        &workspace.name,
        "workspace_backup_succeeded",
        "succeeded",
        "workspace volume backup completed",
        json!({
            "volume": workspace.volume_name,
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

pub(crate) async fn run_restic_backup(
    workspace: &WorkspaceRecord,
    volume_path: &Path,
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
        .arg(volume_path)
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
    workspace_stop(name).await?;
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
    let volume_path = microsandbox_volume_path(&workspace.volume_name)?;
    run_restic_restore(&backup.id, &backup.location, &volume_path).await?;
    workspace_mark_status(name, "restored")?;
    record_workspace_event(
        name,
        "workspace_restored",
        "succeeded",
        "workspace volume restored from backup",
        json!({ "backup_id": backup.id, "location": backup.location }),
    )?;
    println!("restored workspace {name} from {}", backup.id);
    Ok(())
}

pub(crate) async fn run_restic_restore(
    backup_id: &str,
    backup_location: &str,
    volume_path: &Path,
) -> Result<()> {
    if env::var_os("RESTIC_REPOSITORY").is_none() {
        bail!("RESTIC_REPOSITORY must be set before workspace restore can run");
    }
    if !command_exists("restic").await {
        bail!("restic must be installed before workspace restore can run");
    }
    let snapshot = backup_location
        .rsplit_once('#')
        .map(|(_, snapshot)| snapshot)
        .filter(|snapshot| !snapshot.is_empty())
        .ok_or_else(|| anyhow!("backup {backup_id} is missing restic snapshot id"))?;
    if volume_path.exists() {
        fs::remove_dir_all(volume_path)
            .with_context(|| format!("remove existing volume path {}", volume_path.display()))?;
    }
    let parent = volume_path
        .parent()
        .ok_or_else(|| anyhow!("volume path has no parent: {}", volume_path.display()))?;
    fs::create_dir_all(parent)?;
    let status = TokioCommand::new("restic")
        .arg("restore")
        .arg(snapshot)
        .arg("--target")
        .arg(parent)
        .stdin(Stdio::null())
        .status()
        .await?;
    if !status.success() {
        bail!("restic restore exited with {status}");
    }
    Ok(())
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
