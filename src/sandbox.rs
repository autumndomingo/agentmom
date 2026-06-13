use super::*;

fn workspace_network_policy() -> NetworkPolicy {
    NetworkPolicy::builder()
        .default_deny()
        .egress(|egress| egress.allow_public())
        .egress(|egress| egress.udp().port(53).allow_host())
        .egress(|egress| egress.tcp().port(53).allow_host())
        .egress(|egress| egress.tcp().port(1080).allow_host())
        .build()
        .expect("workspace network policy is valid")
}

pub(crate) async fn create_sandbox(
    args: CreateArgs,
    workspace: Option<WorkspaceMount>,
) -> Result<()> {
    println!("creating {} from {IMAGE}", args.name);
    let config = load_mom_config()?;
    let snapshot_name = config.snapshot_name()?.to_string();

    let memory = u32::try_from(args.memory).context("memory must fit in u32 MiB")?;
    if !args.no_snapshot {
        ensure_base_snapshot(&config, args.rebuild_snapshot).await?;
    }

    let mut builder = Sandbox::builder(&args.name)
        .cpus(args.cpus)
        .memory(memory)
        .entrypoint(["tail", "-f", "/dev/null"])
        .shell("/bin/sh")
        .network(|network| network.policy(workspace_network_policy()))
        .label(LABEL_MANAGED, "true")
        .label(LABEL_VERSION, env!("CARGO_PKG_VERSION"));

    if let Some(mount) = workspace {
        let workspace_name = mount.workspace_name.clone();
        let volume_quota_mib = mount.volume_quota_mib;
        builder = builder
            .label("mom.workspace", &workspace_name)
            .volume("/workspace", |m| {
                m.named_with(mount.volume_name, move |v| {
                    v.ensure_exists()
                        .quota(volume_quota_mib)
                        .label("mom.workspace", workspace_name)
                })
            });
    }

    if args.no_snapshot {
        builder = builder.image(IMAGE);
    } else {
        builder = builder.from_snapshot(&snapshot_name);
    }

    if args.replace {
        builder = builder.replace();
    }

    let sandbox = builder
        .create()
        .await
        .with_context(|| format!("create sandbox '{}'", args.name))?;

    if args.no_snapshot {
        provision_base(&sandbox, config.hermes_profile()).await?;
    } else {
        configure_guest_profile(&sandbox, config.hermes_profile()).await?;
    }
    apply_guest_auth_config(&sandbox, &config).await?;
    if args.no_snapshot {
        doctor(&sandbox).await?;
    }

    println!("stopping {} to persist filesystem changes", args.name);
    sandbox.stop().await?;
    println!("created {}", args.name);
    Ok(())
}

pub(crate) async fn apply_guest_auth_config(sandbox: &Sandbox, config: &MomConfig) -> Result<()> {
    println!("writing VM auth/config from host config");
    config.validate_for_guest_config()?;
    let hermes_home = format!("{GUEST_HERMES_HOME}/{}", config.hermes_profile());

    let fs = sandbox.fs();
    fs.mkdir("/workspace").await?;
    fs.mkdir(GUEST_HERMES_HOME).await?;
    fs.mkdir(&hermes_home).await?;
    fs.mkdir(&format!("{hermes_home}/home")).await?;
    fs.write(
        &format!("{hermes_home}/config.yaml"),
        hermes_config_yaml(config).as_bytes(),
    )
    .await?;
    fs.write(
        &format!("{hermes_home}/SOUL.md"),
        hermes_soul_md().as_bytes(),
    )
    .await?;
    if let Some(proxy_url) = config.credential_proxy_url() {
        fs.write(
            "/etc/profile.d/agentmom-proxy.sh",
            proxy_env_sh(proxy_url).as_bytes(),
        )
        .await?;
    }
    if let Some(ca_path) = &config.credentials.proxy_ca_path {
        let ca_path = resolve_required_file(ca_path, "credentials.proxy_ca_path")?;
        let ca = fs::read(&ca_path).with_context(|| format!("read {}", ca_path.display()))?;
        fs.mkdir("/usr/local/share/ca-certificates").await?;
        fs.write("/usr/local/share/ca-certificates/agentmom-proxy.crt", ca)
            .await?;
    }

    let hermes_home_q = shell_quote(&hermes_home);
    checked_shell(
        sandbox,
        &format!(
            r#"
set -eu
rm -f /root/.hermes/auth.json /root/.hermes-agent/*/auth.json
chmod 700 /root/.hermes-agent {hermes_home_q}
chmod 600 {hermes_home_q}/config.yaml {hermes_home_q}/SOUL.md
if [ -f /usr/local/share/ca-certificates/agentmom-proxy.crt ]; then update-ca-certificates || true; fi
ln -sfn {hermes_home_q} /root/.hermes
sync
"#
        ),
    )
    .await
}

pub(crate) async fn ensure_base_snapshot(config: &MomConfig, rebuild: bool) -> Result<()> {
    let snapshot_name = config.snapshot_name()?.to_string();
    if rebuild {
        println!("rebuilding base snapshot {snapshot_name}");
        let _ = Snapshot::remove(&snapshot_name, true).await;
        build_base_snapshot(config).await?;
        return doctor_base_snapshot(config).await;
    }

    match Snapshot::open(&snapshot_name).await {
        Ok(snapshot) => {
            println!(
                "using base snapshot {} ({})",
                snapshot_name,
                snapshot.digest()
            );
            Ok(())
        }
        Err(MicrosandboxError::SnapshotNotFound(_)) => bail!(
            "required base snapshot {} is missing; run `mom node ensure-base` before creating workspaces",
            snapshot_name
        ),
        Err(error) => Err(error).context("open base snapshot"),
    }
}

pub(crate) async fn ensure_base_snapshot_for_deploy(
    config: &MomConfig,
    rebuild: bool,
) -> Result<()> {
    let snapshot_name = config.snapshot_name()?.to_string();
    if rebuild {
        println!("rebuilding required base snapshot {snapshot_name}");
        let _ = Snapshot::remove(&snapshot_name, true).await;
        build_base_snapshot(config).await?;
    } else {
        match Snapshot::open(&snapshot_name).await {
            Ok(snapshot) => {
                println!(
                    "found required base snapshot {} ({})",
                    snapshot_name,
                    snapshot.digest()
                );
            }
            Err(MicrosandboxError::SnapshotNotFound(_)) => {
                println!(
                    "required base snapshot {} not found; building it",
                    snapshot_name
                );
                build_base_snapshot(config).await?;
            }
            Err(error) => return Err(error).context("open base snapshot"),
        }
    }

    doctor_base_snapshot(config).await
}

pub(crate) async fn build_base_snapshot(config: &MomConfig) -> Result<()> {
    let snapshot_name = config.snapshot_name()?.to_string();
    let hermes_profile_name = config.hermes_profile().to_string();

    if let Ok(handle) = Sandbox::get(BASE_BUILDER_NAME).await {
        if handle.status() == SandboxStatus::Running || handle.status() == SandboxStatus::Draining {
            handle.stop_with_timeout(Duration::from_secs(10)).await?;
        }
        Sandbox::remove(BASE_BUILDER_NAME).await?;
    }

    let builder = Sandbox::builder(BASE_BUILDER_NAME)
        .image(IMAGE)
        .replace()
        .entrypoint(["tail", "-f", "/dev/null"])
        .shell("/bin/sh")
        .network(|network| network.policy(workspace_network_policy()))
        .label(LABEL_MANAGED, "true")
        .label(LABEL_VERSION, env!("CARGO_PKG_VERSION"))
        .patch(move |patch| {
            let hermes_home = format!("{GUEST_HERMES_HOME}/{hermes_profile_name}");
            patch
                .mkdir("/workspace", Some(0o755))
                .mkdir(GUEST_HERMES_HOME, Some(0o700))
                .mkdir(&hermes_home, Some(0o700))
                .mkdir(format!("{hermes_home}/home"), Some(0o700))
        });

    let sandbox = builder
        .create()
        .await
        .with_context(|| format!("create base builder '{BASE_BUILDER_NAME}'"))?;
    provision_base(&sandbox, config.hermes_profile()).await?;
    doctor(&sandbox).await?;
    checked_shell(&sandbox, "sync").await?;

    println!("stopping {BASE_BUILDER_NAME} before snapshot");
    sandbox.stop().await?;

    let snapshot = Snapshot::builder(BASE_BUILDER_NAME)
        .destination(SnapshotDestination::Name(snapshot_name.clone()))
        .force()
        .create()
        .await
        .with_context(|| format!("create snapshot '{snapshot_name}'"))?;
    println!(
        "created base snapshot {} ({})",
        snapshot_name,
        snapshot.digest()
    );

    Sandbox::remove(BASE_BUILDER_NAME).await?;
    Ok(())
}

pub(crate) async fn doctor_base_snapshot(config: &MomConfig) -> Result<()> {
    let snapshot_name = config.snapshot_name()?.to_string();
    if let Ok(handle) = Sandbox::get(BASE_DOCTOR_NAME).await {
        if handle.status() == SandboxStatus::Running || handle.status() == SandboxStatus::Draining {
            handle.stop_with_timeout(Duration::from_secs(10)).await?;
        }
        Sandbox::remove(BASE_DOCTOR_NAME).await?;
    }

    println!("doctoring base snapshot {snapshot_name}");
    let sandbox = Sandbox::builder(BASE_DOCTOR_NAME)
        .from_snapshot(&snapshot_name)
        .replace()
        .entrypoint(["tail", "-f", "/dev/null"])
        .shell("/bin/sh")
        .network(|network| network.policy(workspace_network_policy()))
        .label(LABEL_MANAGED, "true")
        .label(LABEL_VERSION, env!("CARGO_PKG_VERSION"))
        .create()
        .await
        .with_context(|| {
            format!(
                "create base snapshot doctor sandbox '{}' from '{}'",
                BASE_DOCTOR_NAME, snapshot_name
            )
        })?;
    let result = doctor(&sandbox).await;
    let _ = sandbox.stop().await;
    let _ = Sandbox::remove(BASE_DOCTOR_NAME).await;
    result.with_context(|| format!("doctor base snapshot '{snapshot_name}'"))?;
    println!("base snapshot {snapshot_name} passed doctor");
    Ok(())
}

pub(crate) async fn provision_base(sandbox: &Sandbox, hermes_profile: &str) -> Result<()> {
    println!("installing Alpine packages, uv, and Hermes");
    checked_shell(
        sandbox,
        r#"
set -eu
apk add --no-cache \
  bash \
  build-base \
  ca-certificates \
  clang \
  compiler-rt \
  curl \
  git \
  libffi-dev \
  python3 \
  python3-dev
if ! command -v uv >/dev/null 2>&1; then
  curl -LsSf https://astral.sh/uv/install.sh | sh
fi
export PATH="/root/.local/bin:$PATH"
CC=clang UV_LINK_MODE=copy uv tool install --python 3.13 --force 'hermes-agent[all,messaging,acp]'
ln -sf /root/.local/bin/uv /usr/local/bin/uv
ln -sf /root/.local/bin/uvx /usr/local/bin/uvx
ln -sf /root/.local/bin/hermes /usr/local/bin/hermes
ln -sf /root/.local/bin/hermes-agent /usr/local/bin/hermes-agent
ln -sf /root/.local/bin/hermes-acp /usr/local/bin/hermes-acp
mkdir -p /workspace /root/.hermes-agent
"#,
    )
    .await?;
    configure_guest_profile(sandbox, hermes_profile).await
}

pub(crate) async fn configure_guest_profile(sandbox: &Sandbox, hermes_profile: &str) -> Result<()> {
    let hermes_home = format!("{GUEST_HERMES_HOME}/{hermes_profile}");
    let hermes_home_q = shell_quote(&hermes_home);
    checked_shell(
        sandbox,
        &format!(
            r#"
set -eu
mkdir -p /workspace /root/.hermes-agent {hermes_home_q}
ln -sfn {hermes_home_q} /root/.hermes
cat >/etc/profile.d/mom.sh <<'EOF'
export HERMES_HOME={hermes_home}
EOF
cat >/root/.profile <<'EOF'
export HERMES_HOME={hermes_home}
EOF
"#
        ),
    )
    .await
}

pub(crate) async fn proxy_smoke(sandbox: &Sandbox) -> Result<()> {
    checked_shell(
        sandbox,
        r#"
set -eu
if [ -f /root/.hermes/auth.json ]; then
  echo "raw auth files are present in the sandbox" >&2
  exit 1
fi
test -f /etc/profile.d/agentmom-proxy.sh
. /etc/profile.d/agentmom-proxy.sh
test -n "${HTTPS_PROXY:-}"
test "${OPENROUTER_API_KEY:-}" = "agentmom-proxy"
python3 - <<'PY'
import json
import ssl
import urllib.request

request = urllib.request.Request("https://openrouter.ai/api/v1/models")
with urllib.request.urlopen(request, timeout=20, context=ssl.create_default_context()) as response:
    payload = json.load(response)
if "data" not in payload:
    raise SystemExit("OpenRouter models response did not include data")
print("proxy smoke ok")
PY
"#,
    )
    .await
}

pub(crate) async fn doctor(sandbox: &Sandbox) -> Result<()> {
    checked_shell(
        sandbox,
        r#"
set -u
echo "== tools =="
uv --version
hermes --help >/tmp/mom-hermes-help.txt 2>&1 || true
head -20 /tmp/mom-hermes-help.txt
"#,
    )
    .await
}

pub(crate) async fn run_guest_command(sandbox: &Sandbox, command: Vec<String>) -> Result<()> {
    let output = capture_guest_command(sandbox, command).await?;
    print!("{}", output["stdout"].as_str().unwrap_or_default());
    eprint!("{}", output["stderr"].as_str().unwrap_or_default());
    if !output["ok"].as_bool().unwrap_or(false) {
        bail!(
            "guest command exited with {}",
            output["code"].as_i64().unwrap_or_default()
        );
    }
    Ok(())
}

pub(crate) async fn capture_guest_command(
    sandbox: &Sandbox,
    command: Vec<String>,
) -> Result<Value> {
    let (cmd, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("missing command"))?;
    let output = sandbox.exec(cmd, args.iter().cloned()).await?;
    let stdout = output.stdout()?;
    let stderr = output.stderr()?;
    let ok = output.status().success;
    let code = output.status().code;
    Ok(json!({
        "ok": ok,
        "code": code,
        "stdout": stdout,
        "stderr": stderr
    }))
}
