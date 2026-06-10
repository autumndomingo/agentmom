use std::{env, fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};
use microsandbox::{
    MicrosandboxError, Sandbox, Snapshot, SnapshotDestination, sandbox::SandboxStatus,
};
use serde::Deserialize;
use serde_json::{Value, json};

const IMAGE: &str = "alpine";
const LABEL_MANAGED: &str = "hvm.managed";
const LABEL_VERSION: &str = "hvm.version";
const GUEST_CODEX_HOME: &str = "/root/.codex";
const GUEST_HERMES_HOME: &str = "/root/.hermes-agent";
const BASE_BUILDER_NAME: &str = "hvm-base-builder";

#[derive(Debug, Parser)]
#[command(
    name = "hvm",
    about = "Small VM manager for Alpine microsandbox agent boxes"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create and provision a new Alpine VM.
    Create(CreateArgs),
    /// List hvm-managed VMs.
    List {
        /// Include sandboxes not created by hvm.
        #[arg(long)]
        all: bool,
    },
    /// Start a stopped VM in the background.
    Start { name: String },
    /// Stop a VM.
    Stop { name: String },
    /// Remove a VM, stopping it first if needed.
    Rm {
        name: String,
        /// Do not ask for confirmation.
        #[arg(short, long)]
        force: bool,
    },
    /// Run a command in a VM and print captured output.
    Exec {
        name: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Open an interactive shell in a VM.
    Enter { name: String },
    /// Run Codex inside a VM.
    Codex {
        name: String,
        #[arg(required = true)]
        prompt: Vec<String>,
    },
    /// Run Hermes inside a VM.
    Hermes {
        name: String,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Run basic tool checks inside a VM.
    Doctor { name: String },
}

#[derive(Debug, Args)]
struct CreateArgs {
    name: String,
    /// Replace an existing VM with this name.
    #[arg(long)]
    replace: bool,
    /// vCPUs to allocate.
    #[arg(long, default_value_t = 2)]
    cpus: u8,
    /// Memory in MiB.
    #[arg(long, default_value_t = 2048)]
    memory: u64,
    /// Rebuild the base snapshot before creating the VM.
    #[arg(long)]
    rebuild_snapshot: bool,
    /// Provision directly from Alpine instead of the base snapshot.
    #[arg(long)]
    no_snapshot: bool,
}

#[derive(Debug, Deserialize)]
struct HvmConfig {
    codex_auth_path: PathBuf,
    #[serde(default = "default_hermes_profile")]
    hermes_profile: String,
    #[serde(default = "default_hermes_model")]
    hermes_model: String,
    #[serde(default = "default_snapshot_name")]
    snapshot_name: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Create(args) => create(args).await,
        Command::List { all } => list(all).await,
        Command::Start { name } => start(&name).await,
        Command::Stop { name } => stop(&name).await,
        Command::Rm { name, force } => remove(&name, force).await,
        Command::Exec { name, command } => {
            let sandbox = running_sandbox(&name).await?;
            run_guest_command(&sandbox, command).await
        }
        Command::Enter { name } => {
            let sandbox = running_sandbox(&name).await?;
            let code = sandbox
                .attach_with("/bin/sh", |a| a.args(["-l"]).env("TERM", "xterm-256color"))
                .await?;
            std::process::exit(code);
        }
        Command::Codex { name, prompt } => {
            let sandbox = running_sandbox(&name).await?;
            run_codex(&sandbox, &prompt.join(" ")).await
        }
        Command::Hermes { name, args } => {
            let sandbox = running_sandbox(&name).await?;
            let mut command = vec!["hermes".to_string()];
            command.extend(args);
            run_guest_command(&sandbox, command).await
        }
        Command::Doctor { name } => {
            let sandbox = running_sandbox(&name).await?;
            doctor(&sandbox).await
        }
    }
}

async fn create(args: CreateArgs) -> Result<()> {
    println!("creating {} from {IMAGE}", args.name);
    let config = load_hvm_config()?;

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

    if args.no_snapshot {
        builder = builder.image(IMAGE);
    } else {
        builder = builder.from_snapshot(&config.snapshot_name);
    }

    if args.replace {
        builder = builder.replace();
    }

    let sandbox = builder
        .create()
        .await
        .with_context(|| format!("create sandbox '{}'", args.name))?;

    if args.no_snapshot {
        provision_base(&sandbox, &config.hermes_profile).await?;
    } else {
        configure_guest_profile(&sandbox, &config.hermes_profile).await?;
    }
    apply_guest_auth_config(&sandbox, &config).await?;
    doctor(&sandbox).await?;

    println!("stopping {} to persist filesystem changes", args.name);
    sandbox.stop().await?;
    println!("created {}", args.name);
    Ok(())
}

async fn apply_guest_auth_config(sandbox: &Sandbox, config: &HvmConfig) -> Result<()> {
    println!("writing VM auth/config from host config");
    let codex_auth_path = resolve_required_file(&config.codex_auth_path, "codex_auth_path")?;
    let codex_auth = fs::read(&codex_auth_path)
        .with_context(|| format!("read {}", codex_auth_path.display()))?;
    let hermes_auth = codex_auth_as_hermes_auth(&codex_auth_path)?;
    let hermes_home = format!("{GUEST_HERMES_HOME}/{}", config.hermes_profile);

    let fs = sandbox.fs();
    fs.mkdir("/workspace").await?;
    fs.mkdir(GUEST_CODEX_HOME).await?;
    fs.mkdir(GUEST_HERMES_HOME).await?;
    fs.mkdir(&hermes_home).await?;
    fs.mkdir(&format!("{hermes_home}/home")).await?;
    fs.write(&format!("{GUEST_CODEX_HOME}/auth.json"), codex_auth)
        .await?;
    fs.write(
        &format!("{hermes_home}/config.yaml"),
        hermes_config_yaml(&config.hermes_model).as_bytes(),
    )
    .await?;
    fs.write(
        &format!("{hermes_home}/SOUL.md"),
        hermes_soul_md().as_bytes(),
    )
    .await?;
    fs.write(&format!("{hermes_home}/auth.json"), hermes_auth.as_bytes())
        .await?;

    let hermes_home_q = shell_quote(&hermes_home);
    checked_shell(
        sandbox,
        &format!(
            r#"
set -eu
chmod 700 /root/.codex /root/.hermes-agent {hermes_home_q}
chmod 600 /root/.codex/auth.json {hermes_home_q}/auth.json {hermes_home_q}/config.yaml {hermes_home_q}/SOUL.md
ln -sfn {hermes_home_q} /root/.hermes
sync
"#
        ),
    )
    .await
}

async fn ensure_base_snapshot(config: &HvmConfig, rebuild: bool) -> Result<()> {
    if rebuild {
        println!("rebuilding base snapshot {}", config.snapshot_name);
        let _ = Snapshot::remove(&config.snapshot_name, true).await;
    } else {
        match Snapshot::open(&config.snapshot_name).await {
            Ok(snapshot) => {
                println!(
                    "using base snapshot {} ({})",
                    config.snapshot_name,
                    snapshot.digest()
                );
                return Ok(());
            }
            Err(MicrosandboxError::SnapshotNotFound(_)) => {
                println!(
                    "base snapshot {} not found; building it",
                    config.snapshot_name
                );
            }
            Err(error) => return Err(error).context("open base snapshot"),
        }
    }

    build_base_snapshot(config).await
}

async fn build_base_snapshot(config: &HvmConfig) -> Result<()> {
    let codex_auth_path = resolve_required_file(&config.codex_auth_path, "codex_auth_path")?;
    let codex_files = codex_auth_files(&codex_auth_path)?;
    let hermes_auth = codex_auth_as_hermes_auth(&codex_auth_path)?;
    let hermes_profile_name = config.hermes_profile.clone();
    let hermes_model = config.hermes_model.clone();

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
            let mut patch = patch
                .mkdir("/workspace", Some(0o755))
                .mkdir(GUEST_CODEX_HOME, Some(0o700))
                .mkdir(GUEST_HERMES_HOME, Some(0o700))
                .mkdir(&hermes_home, Some(0o700))
                .mkdir(format!("{hermes_home}/home"), Some(0o700))
                .text(
                    format!("{hermes_home}/config.yaml"),
                    hermes_config_yaml(&hermes_model),
                    Some(0o600),
                    true,
                )
                .text(
                    format!("{hermes_home}/SOUL.md"),
                    hermes_soul_md(),
                    Some(0o600),
                    true,
                )
                .text(
                    format!("{hermes_home}/auth.json"),
                    hermes_auth,
                    Some(0o600),
                    true,
                );

            for (host, guest) in codex_files {
                patch = patch.copy_file(host, guest, Some(0o600), true);
            }

            patch
        });

    let sandbox = builder
        .create()
        .await
        .with_context(|| format!("create base builder '{BASE_BUILDER_NAME}'"))?;
    provision_base(&sandbox, &config.hermes_profile).await?;
    doctor(&sandbox).await?;
    checked_shell(&sandbox, "sync").await?;

    println!("stopping {BASE_BUILDER_NAME} before snapshot");
    sandbox.stop().await?;

    let snapshot = Snapshot::builder(BASE_BUILDER_NAME)
        .destination(SnapshotDestination::Name(config.snapshot_name.clone()))
        .force()
        .create()
        .await
        .with_context(|| format!("create snapshot '{}'", config.snapshot_name))?;
    println!(
        "created base snapshot {} ({})",
        config.snapshot_name,
        snapshot.digest()
    );

    Sandbox::remove(BASE_BUILDER_NAME).await?;
    Ok(())
}

async fn provision_base(sandbox: &Sandbox, hermes_profile: &str) -> Result<()> {
    println!("installing Alpine packages, uv, Codex, and Hermes");
    checked_shell(
        sandbox,
        r#"
set -eu
apk add --no-cache \
  bash \
  ca-certificates \
  curl \
  git \
  nodejs \
  npm \
  python3
if ! command -v uv >/dev/null 2>&1; then
  curl -LsSf https://astral.sh/uv/install.sh | sh
fi
export PATH="/root/.local/bin:$PATH"
npm install -g @openai/codex
UV_LINK_MODE=copy uv tool install --python 3.13 --force hermes-agent
ln -sf /root/.local/bin/uv /usr/local/bin/uv
ln -sf /root/.local/bin/uvx /usr/local/bin/uvx
ln -sf /root/.local/bin/hermes /usr/local/bin/hermes
ln -sf /root/.local/bin/hermes-agent /usr/local/bin/hermes-agent
ln -sf /root/.local/bin/hermes-acp /usr/local/bin/hermes-acp
mkdir -p /workspace /root/.codex /root/.hermes-agent
"#,
    )
    .await?;
    configure_guest_profile(sandbox, hermes_profile).await
}

async fn configure_guest_profile(sandbox: &Sandbox, hermes_profile: &str) -> Result<()> {
    let hermes_home = format!("{GUEST_HERMES_HOME}/{hermes_profile}");
    let hermes_home_q = shell_quote(&hermes_home);
    checked_shell(
        sandbox,
        &format!(
            r#"
set -eu
mkdir -p /workspace /root/.codex /root/.hermes-agent {hermes_home_q}
ln -sfn {hermes_home_q} /root/.hermes
cat >/etc/profile.d/hvm.sh <<'EOF'
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

async fn list(all: bool) -> Result<()> {
    let handles = Sandbox::list().await?;
    println!("{:<24} {:<10} IMAGE", "NAME", "STATUS");
    for handle in handles {
        let config = handle.config()?;
        let managed = config
            .labels
            .get(LABEL_MANAGED)
            .is_some_and(|value| value == "true");
        if !all && !managed {
            continue;
        }

        println!(
            "{:<24} {:<10} {}",
            handle.name(),
            format!("{:?}", handle.status()),
            image_label(&config)
        );
    }
    Ok(())
}

async fn start(name: &str) -> Result<()> {
    let handle = Sandbox::get(name).await?;
    if handle.status() == SandboxStatus::Running {
        println!("{name} already running");
        return Ok(());
    }
    let sandbox = handle.start_detached().await?;
    println!("started {}", sandbox.name());
    Ok(())
}

async fn stop(name: &str) -> Result<()> {
    let handle = Sandbox::get(name).await?;
    handle.stop_with_timeout(Duration::from_secs(10)).await?;
    println!("stopped {name}");
    Ok(())
}

async fn remove(name: &str, force: bool) -> Result<()> {
    if !force {
        bail!("refusing to remove {name} without --force");
    }

    if let Ok(handle) = Sandbox::get(name).await {
        if handle.status() == SandboxStatus::Running || handle.status() == SandboxStatus::Draining {
            handle.stop_with_timeout(Duration::from_secs(10)).await?;
        }
    }

    Sandbox::remove(name).await?;
    println!("removed {name}");
    Ok(())
}

async fn running_sandbox(name: &str) -> Result<Sandbox> {
    let handle = Sandbox::get(name)
        .await
        .with_context(|| format!("find sandbox '{name}'"))?;
    match handle.status() {
        SandboxStatus::Running | SandboxStatus::Draining => handle
            .connect_with_timeout(Duration::from_secs(30))
            .await
            .with_context(|| format!("connect to running sandbox '{name}'")),
        SandboxStatus::Stopped | SandboxStatus::Crashed | SandboxStatus::Paused => handle
            .start()
            .await
            .with_context(|| format!("start sandbox '{name}'")),
    }
}

async fn run_codex(sandbox: &Sandbox, prompt: &str) -> Result<()> {
    let prompt = shell_quote(prompt);
    let script = format!(
        r#"
set -eu
tmp="$(mktemp -d /root/hvm-codex.XXXXXX)"
trap 'rm -rf "$tmp"' EXIT
cp /root/.codex/auth.json "$tmp/auth.json"
if [ -f /root/.codex/config.toml ]; then
  cp /root/.codex/config.toml "$tmp/config.toml"
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

async fn doctor(sandbox: &Sandbox) -> Result<()> {
    checked_shell(
        sandbox,
        r#"
set -u
echo "== tools =="
node --version
npm --version
uv --version
codex --version
hermes --help >/tmp/hvm-hermes-help.txt 2>&1 || true
head -20 /tmp/hvm-hermes-help.txt
echo "== codex doctor =="
codex doctor --summary --ascii --no-color || true
"#,
    )
    .await
}

async fn run_guest_command(sandbox: &Sandbox, command: Vec<String>) -> Result<()> {
    let (cmd, args) = command
        .split_first()
        .ok_or_else(|| anyhow!("missing command"))?;
    let output = sandbox.exec(cmd, args.iter().cloned()).await?;
    print!("{}", output.stdout()?);
    eprint!("{}", output.stderr()?);
    if !output.status().success {
        bail!("guest command exited with {}", output.status().code);
    }
    Ok(())
}

async fn checked_shell(sandbox: &Sandbox, script: &str) -> Result<()> {
    let output = sandbox.shell(script).await?;
    print!("{}", output.stdout()?);
    eprint!("{}", output.stderr()?);
    if !output.status().success {
        bail!("guest shell command exited with {}", output.status().code);
    }
    Ok(())
}

fn codex_auth_files(codex_auth_path: &PathBuf) -> Result<Vec<(PathBuf, String)>> {
    let candidates = [(
        codex_auth_path.clone(),
        format!("{GUEST_CODEX_HOME}/auth.json"),
    )];

    let mut files = Vec::new();
    for (host, guest) in candidates {
        if host.exists() {
            let real = host
                .canonicalize()
                .with_context(|| format!("resolve {}", host.display()))?;
            println!("will copy {} to {guest}", real.display());
            files.push((real, guest));
        } else {
            bail!(
                "required Codex auth file does not exist: {}",
                host.display()
            );
        }
    }
    Ok(files)
}

fn codex_auth_as_hermes_auth(auth_path: &PathBuf) -> Result<String> {
    let raw =
        fs::read_to_string(&auth_path).with_context(|| format!("read {}", auth_path.display()))?;
    let codex_auth: Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", auth_path.display()))?;
    let tokens = codex_auth
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("{} does not contain a tokens object", auth_path.display()))?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("{} is missing tokens.access_token", auth_path.display()))?;
    let refresh_token = tokens
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("{} is missing tokens.refresh_token", auth_path.display()))?;
    let last_refresh = codex_auth
        .get("last_refresh")
        .and_then(Value::as_str)
        .unwrap_or("2026-01-01T00:00:00Z");
    let account_id = codex_auth
        .get("account_id")
        .and_then(Value::as_str)
        .unwrap_or("codex-cli");

    let payload = json!({
        "version": 1,
        "active_provider": "openai-codex",
        "providers": {
            "openai-codex": {
                "tokens": tokens,
                "last_refresh": last_refresh,
                "auth_mode": "chatgpt",
                "label": "Codex CLI"
            }
        },
        "credential_pool": {
            "openai-codex": [
                {
                    "id": format!("codex-cli-{account_id}"),
                    "label": "Codex CLI",
                    "source": "device_code",
                    "auth_type": "oauth",
                    "priority": 0,
                    "access_token": access_token,
                    "refresh_token": refresh_token,
                    "last_refresh": last_refresh,
                    "last_status": Value::Null,
                    "last_status_at": Value::Null,
                    "last_error_code": Value::Null,
                    "last_error_reason": Value::Null,
                    "last_error_message": Value::Null,
                    "last_error_reset_at": Value::Null
                }
            ]
        }
    });

    println!(
        "will seed Hermes OpenAI Codex auth from {}",
        auth_path.display()
    );
    Ok(serde_json::to_string_pretty(&payload)?)
}

fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| anyhow!("could not determine home directory"))
}

fn load_hvm_config() -> Result<HvmConfig> {
    let path = match env::var_os("HVM_CONFIG") {
        Some(value) => PathBuf::from(value),
        None => home_dir()?.join(".config").join("hvm").join("config.json"),
    };
    let raw = fs::read_to_string(&path).with_context(|| {
        format!(
            "read hvm config {}; create it or set HVM_CONFIG",
            path.display()
        )
    })?;
    serde_json::from_str(&raw).with_context(|| format!("parse hvm config {}", path.display()))
}

fn resolve_required_file(path: &PathBuf, key: &str) -> Result<PathBuf> {
    let expanded = expand_tilde(path)?;
    expanded.canonicalize().with_context(|| {
        format!(
            "{key} does not point at a readable file: {}",
            expanded.display()
        )
    })
}

fn expand_tilde(path: &PathBuf) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    Ok(path.clone())
}

fn default_hermes_profile() -> String {
    "main".to_string()
}

fn default_hermes_model() -> String {
    "gpt-5.5".to_string()
}

fn default_snapshot_name() -> String {
    "hvm-alpine-agent-base".to_string()
}

fn hermes_config_yaml(model: &str) -> String {
    let model = serde_json::to_string(model).expect("serializing a string cannot fail");
    format!(
        r#"model:
  provider: openai-codex
  default: {model}
  api_mode: codex_responses
terminal:
  backend: local
  cwd: /workspace
  persistent_shell: true
  timeout: 600
approvals:
  mode: off
toolsets:
  - all
"#
    )
}

fn hermes_soul_md() -> &'static str {
    "You are running inside an isolated hvm microsandbox. Work in /workspace.\n"
}

fn image_label(config: &microsandbox::sandbox::SandboxConfig) -> String {
    format!("{:?}", config.image)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
