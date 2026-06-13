use super::*;

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
    let credential_mode = config.validate_for_guest_config()?;

    let (codex_auth, hermes_auth, opencode_auth) = if credential_mode.uses_guest_auth_files() {
        let codex_auth_path = resolve_required_file(
            &config.credentials.codex_auth_path,
            "credentials.codex_auth_path",
        )?;
        let codex_auth = fs::read(&codex_auth_path)
            .with_context(|| format!("read {}", codex_auth_path.display()))?;
        let hermes_auth = codex_auth_as_hermes_auth(&codex_auth_path)?;
        let opencode_auth_path = resolve_required_file(
            &config.credentials.opencode_auth_path,
            "credentials.opencode_auth_path",
        )?;
        let opencode_auth = opencode_auth_from_file(&opencode_auth_path)?;
        (Some(codex_auth), Some(hermes_auth), Some(opencode_auth))
    } else {
        (None, None, None)
    };
    let hermes_home = format!("{GUEST_HERMES_HOME}/{}", config.hermes_profile());

    let fs = sandbox.fs();
    fs.mkdir("/workspace").await?;
    fs.mkdir(GUEST_CODEX_HOME).await?;
    fs.mkdir(GUEST_HERMES_HOME).await?;
    fs.mkdir("/root/.local").await?;
    fs.mkdir("/root/.local/share").await?;
    fs.mkdir(GUEST_OPENCODE_DATA_HOME).await?;
    fs.mkdir("/root/.config").await?;
    fs.mkdir(GUEST_OPENCODE_CONFIG_HOME).await?;
    fs.mkdir(&hermes_home).await?;
    fs.mkdir(&format!("{hermes_home}/home")).await?;
    fs.write(
        &format!("{GUEST_CODEX_HOME}/config.toml"),
        codex_config_toml(config).as_bytes(),
    )
    .await?;
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
    if let Some(codex_auth) = codex_auth {
        fs.write(&format!("{GUEST_CODEX_HOME}/auth.json"), codex_auth)
            .await?;
    }
    if let Some(hermes_auth) = hermes_auth {
        fs.write(&format!("{hermes_home}/auth.json"), hermes_auth.as_bytes())
            .await?;
    }
    if let Some(opencode_auth) = opencode_auth {
        fs.write(
            &format!("{GUEST_OPENCODE_DATA_HOME}/auth.json"),
            opencode_auth.as_bytes(),
        )
        .await?;
    }
    fs.write(
        &format!("{GUEST_OPENCODE_CONFIG_HOME}/opencode.json"),
        opencode_config_json(config).as_bytes(),
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
    let auth_chmod = if credential_mode.uses_guest_auth_files() {
        format!(
            "/root/.codex/auth.json {hermes_home_q}/auth.json /root/.local/share/opencode/auth.json"
        )
    } else {
        String::new()
    };
    let remove_guest_auth = if credential_mode.uses_proxy() {
        "rm -f /root/.codex/auth.json /root/.hermes/auth.json /root/.hermes-agent/*/auth.json /root/.local/share/opencode/auth.json"
    } else {
        ":"
    };
    checked_shell(
        sandbox,
        &format!(
            r#"
set -eu
{remove_guest_auth}
chmod 700 /root/.codex /root/.hermes-agent /root/.local /root/.local/share /root/.local/share/opencode /root/.config /root/.config/opencode {hermes_home_q}
chmod 600 /root/.codex/config.toml {hermes_home_q}/config.yaml {hermes_home_q}/SOUL.md /root/.config/opencode/opencode.json {auth_chmod}
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
        .label(LABEL_MANAGED, "true")
        .label(LABEL_VERSION, env!("CARGO_PKG_VERSION"))
        .patch(move |patch| {
            let hermes_home = format!("{GUEST_HERMES_HOME}/{hermes_profile_name}");
            patch
                .mkdir("/workspace", Some(0o755))
                .mkdir(GUEST_CODEX_HOME, Some(0o700))
                .mkdir(GUEST_HERMES_HOME, Some(0o700))
                .mkdir("/root/.local", Some(0o700))
                .mkdir("/root/.local/share", Some(0o700))
                .mkdir(GUEST_OPENCODE_DATA_HOME, Some(0o700))
                .mkdir("/root/.config", Some(0o700))
                .mkdir(GUEST_OPENCODE_CONFIG_HOME, Some(0o700))
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
    println!("installing Alpine packages, uv, Codex, Hermes, and OpenCode");
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
  nodejs \
  npm \
  python3 \
  python3-dev
if ! command -v uv >/dev/null 2>&1; then
  curl -LsSf https://astral.sh/uv/install.sh | sh
fi
export PATH="/root/.local/bin:$PATH"
npm install -g @openai/codex
npm install -g opencode-ai
CC=clang UV_LINK_MODE=copy uv tool install --python 3.13 --force 'hermes-agent[all,messaging]'
ln -sf /root/.local/bin/uv /usr/local/bin/uv
ln -sf /root/.local/bin/uvx /usr/local/bin/uvx
ln -sf /root/.local/bin/hermes /usr/local/bin/hermes
ln -sf /root/.local/bin/hermes-agent /usr/local/bin/hermes-agent
ln -sf /root/.local/bin/hermes-acp /usr/local/bin/hermes-acp
mkdir -p /workspace /root/.codex /root/.hermes-agent /root/.local/share/opencode /root/.config/opencode
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
mkdir -p /workspace /root/.codex /root/.hermes-agent /root/.local/share/opencode /root/.config/opencode {hermes_home_q}
ln -sfn {hermes_home_q} /root/.hermes
cat >/etc/profile.d/mom.sh <<'EOF'
export HERMES_HOME={hermes_home}
export CODEX_HOME=/root/.codex
EOF
cat >/root/.profile <<'EOF'
export HERMES_HOME={hermes_home}
export CODEX_HOME=/root/.codex
EOF
"#
        ),
    )
    .await
}

pub(crate) async fn run_codex(sandbox: &Sandbox, prompt: &str) -> Result<()> {
    let config = load_mom_config()?;
    let credential_mode = config.credential_mode()?;
    if credential_mode == CredentialMode::OpenRouterProxy {
        bail!(
            "mom codex requires credentials.mode vm-auth-json; use Hermes/OpenRouter in openrouter-proxy mode"
        );
    }

    let prompt = shell_quote(prompt);
    let script = format!(
        r#"
set -eu
tmp="$(mktemp -d /root/mom-codex.XXXXXX)"
trap 'rm -rf "$tmp"' EXIT
if [ -f /root/.codex/auth.json ]; then
  cp /root/.codex/auth.json "$tmp/auth.json"
fi
if [ -f /root/.codex/config.toml ]; then
  cp /root/.codex/config.toml "$tmp/config.toml"
fi
if [ -f /etc/profile.d/agentmom-proxy.sh ]; then
  . /etc/profile.d/agentmom-proxy.sh
fi
out="$tmp/last-message.txt"
CODEX_HOME="$tmp" timeout 180 codex exec \
  --ignore-user-config \
  --skip-git-repo-check \
  --dangerously-bypass-approvals-and-sandbox \
  -o "$out" \
  -C /workspace \
  {prompt} </dev/null
cat "$out"
"#
    );
    checked_shell(sandbox, &script).await
}

pub(crate) async fn proxy_smoke(sandbox: &Sandbox) -> Result<()> {
    checked_shell(
        sandbox,
        r#"
set -eu
if [ -f /root/.codex/auth.json ] || [ -f /root/.hermes/auth.json ]; then
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
node --version
npm --version
uv --version
codex --version
opencode --version
hermes --help >/tmp/mom-hermes-help.txt 2>&1 || true
head -20 /tmp/mom-hermes-help.txt
echo "== codex doctor =="
codex doctor --summary --ascii --no-color || true
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
