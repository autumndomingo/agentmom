# Agent Mom

`mom` is a small Rust CLI for managing Alpine microsandbox VMs for Codex and
Hermes.

It uses the microsandbox Rust SDK directly. It does not shell out to
`npx microsandbox` for lifecycle operations.

## Commands

```sh
mom create mybox --replace
mom list
mom enter mybox
mom exec mybox -- pwd
mom codex mybox "Reply exactly ok"
mom hermes mybox -- --help
mom doctor mybox
mom stop mybox
mom start mybox
mom rm mybox --force
```

## Single-Host Fleet Worker

The next iteration treats a user workspace as the durable unit and the VM as
replaceable compute. A workspace has:

- one SQLite row in `MOM_STATE_DIR/fleet.db`
- one microsandbox VM named `mom-<workspace>`
- one microsandbox named volume mounted at `/workspace`

```sh
mom workspace create alice --user user_123 --replace
mom workspace list
mom workspace exec alice -- pwd
mom workspace codex alice "Reply exactly ok"
mom workspace inspect alice
mom workspace events alice --since 2h
mom workspace backup alice
mom workspace stop alice
mom workspace rm alice --force
mom node status
mom node list
mom node inspect pika-build
mom node cordon pika-build
mom node drain pika-build
mom node uncordon pika-build
mom db status
mom db backup
mom monitor check
```

Run the worker against a local or central API:

```sh
mom api
MOM_API_URL=http://127.0.0.1:8080 mom worker
```

The worker:

- starts workspaces whose desired state is `running`
- stops idle workspaces after their configured idle timeout
- backs up workspace volumes when their backup interval is due
- claims and runs queued workspace jobs from `mom api`

Backups use restic. Set `RESTIC_REPOSITORY` and the usual restic credentials in
the service environment before enabling scheduled backups. Each backup is tagged
with `agentmom` and the workspace name, and the recorded artifact stores the
restic snapshot ID.

Before backing up, Agent Mom gracefully stops the workspace VM so the named
volume is in a consistent state. If the workspace was desired-running, it is
started again after backup.

For local testing without touching the default microsandbox home:

```sh
export MOM_STATE_DIR="$PWD/.state/mom"
export MSB_HOME="$PWD/.state/msb"
cargo run --bin mom -- workspace list
cargo run --bin mom -- api --bind 127.0.0.1:8080
MOM_API_URL=http://127.0.0.1:8080 cargo run --bin mom -- worker --once
```

## Fleet Operations

The central API owns the fleet catalog at `MOM_STATE_DIR/fleet.db`. Workers own
host-local microsandbox VMs and named volumes. If a worker host is lost, the
central catalog still records workspace ownership and backup artifacts, but the
host-local volume is recovered from restic rather than live-migrated.

Use node lifecycle commands before planned host work:

```sh
mom node list
mom node inspect mom-1
mom node cordon mom-1    # stop new placements, keep assigned work running
mom node drain mom-1     # stop new claims for assigned work
mom node uncordon mom-1  # require a fresh heartbeat before scheduling resumes
mom node retire mom-2    # intentionally removed host; excluded from stale checks
```

Use catalog backup commands on the API host:

```sh
mom db status
mom db backup --output /var/lib/agentmom/catalog-backups/fleet-$(date -u +%Y%m%dT%H%M%SZ).db
```

`mom db backup` uses SQLite `VACUUM INTO`, refuses to overwrite existing files,
and checks the fleet schema version before copying. It backs up the central
catalog only; workspace volume data is backed up separately by restic jobs.

`mom monitor check` is a small systemd-friendly health check:

```sh
mom monitor check \
  --api-url http://127.0.0.1:8080 \
  --min-ready-nodes 1 \
  --max-stale-nodes 0 \
  --max-queued-age-secs 300 \
  --failed-job-lookback-secs 900 \
  --max-recent-failed-jobs 0
```

It checks `/health/ready`, node freshness, ready-node count, queued-job age, and
recent failed jobs. `/metrics` exposes the same operational surface for
Prometheus-style scraping: workspace totals by status, job totals by status,
node totals by status, stale node count, and oldest queued job age.

## Host Config

`mom create` requires a host config file at `~/.config/mom/config.json`.
Set `MOM_CONFIG=/path/to/config.json` to use a different file.

```json
{
  "codex_auth_path": "~/.codex/auth.json",
  "hermes_profile": "main",
  "hermes_model": "gpt-5.5",
  "snapshot_name": "mom-alpine-agent-base",
  "credential_mode": "vm-auth-json"
}
```

Required assumptions:

- `credential_mode` is either `vm-auth-json` or `openrouter-proxy`.
- `vm-auth-json` requires `codex_auth_path` to exist and contain Codex CLI OAuth tokens. This copies credentials into the VM.
- `openrouter-proxy` requires `credential_proxy_url` and `credential_proxy_ca_path`. It writes proxy env into the VM and expects iron-proxy to inject the OpenRouter API key on the host.
- `hermes_profile` is the guest profile name to create.
- `hermes_model` is the default Hermes model for the selected mode. Use an `openai-codex` model in `vm-auth-json` mode and an OpenRouter model ID in `openrouter-proxy` mode.
- `snapshot_name` is the prebuilt microsandbox snapshot to boot new VMs from.

`create` uses `snapshot_name` by default. If the snapshot is missing, Agent Mom builds
it once from the `alpine` image by installing `nodejs`, `npm`, `python3`, `uv`,
`@openai/codex`, and `hermes-agent`, then snapshots the stopped builder VM.
Pass `--rebuild-snapshot` to refresh that base, or `--no-snapshot` to force the
slow direct-Alpine provisioning path.

Each new VM is then patched with auth and Hermes config for the selected mode.

In `vm-auth-json` mode:

- `codex_auth_path` -> `/root/.codex/auth.json`
- a generated `/root/.codex/config.toml` with `approval_policy = "never"` and `sandbox_mode = "danger-full-access"`
- OpenAI Codex tokens from `codex_auth_path` -> `/root/.hermes-agent/<hermes_profile>/auth.json`
- a minimal generated Hermes `config.yaml` selecting `openai-codex`

In `openrouter-proxy` mode:

- no Codex/Hermes auth files are written, and stale auth files are removed on `mom workspace refresh-config`
- `/etc/profile.d/agentmom-proxy.sh` exports proxy variables and sentinel API-key values
- Hermes `config.yaml` selects `provider: openrouter`
- the configured iron-proxy CA is installed into the VM trust store

These are one-time writes, not bind mounts. The base snapshot may contain the
auth/proxy config present when it was built, and each create overwrites auth from
the current host config. Host Hermes profiles, sessions, custom providers, MCP
entries, memories, plugins, and local paths are not copied. After creation, the
VM has its own filesystem and no host directory sharing.

## Build

```sh
nix develop
cargo build
```

or:

```sh
nix build
```

## UI

The UI is a React app served by `mom api`. The browser uses same-origin `/api`
routes, so the public service can be a single `agentmom-api` process.

```sh
nix develop
cd ui
npm install
npm run build
cd ..
MOM_UI_DIST=ui/dist cargo run --bin mom -- api --bind 127.0.0.1:8080
```

Open <http://127.0.0.1:8080>. Hermes/OpenCode launch requests are routed from
the API to the workspace's assigned worker over that worker's private
`worker.url`.

## NixOS Service

The flake exports `nixosModules.agentmom`. A host can layer the worker on top of
its existing NixOS config:

```nix
{
  imports = [
    inputs.agentmom.nixosModules.agentmom
  ];

  services.agentmom = {
    enable = true;
    package = inputs.agentmom.packages.${pkgs.system}.mom;
    nodeId = "pika-build";
    logFormat = "json";
    stateDir = "/var/lib/agentmom";
    microsandboxHome = "/var/lib/agentmom/microsandbox";
    configFile = /etc/agentmom/config.json;
  };
}
```

For multi-host mode, enable the API on one host and workers on each VM host:

```nix
services.agentmom = {
  enable = true;
  package = inputs.agentmom.packages.${pkgs.system}.mom;
  nodeId = "pika-build";
  logFormat = "json";
  stateDir = "/var/lib/agentmom";
  microsandboxHome = "/var/lib/agentmom/microsandbox";

  api = {
    enable = true;
    bind = "127.0.0.1:8080";
  };

  worker = {
    enable = true;
    apiUrl = "http://127.0.0.1:8080";
    bind = "127.0.0.1:9090";
    url = "http://127.0.0.1:9090";
    intervalSeconds = 5;
  };

  ui.enable = true;

  workerTokenFile = "/run/secrets/agentmom-worker-token";

  capacity = {
    cpus = 32;
    memoryMib = 131072;
    activeWorkspaces = 48;
    diskReserveMib = 102400;
  };

  catalogBackup = {
    enable = true;
    onCalendar = "*:0/15";
  };

  monitorCheck = {
    enable = true;
    minReadyNodes = 1;
    maxStaleNodes = 0;
    maxQueuedAgeSeconds = 300;
  };
};
```

`mom api` stores workspaces, jobs, nodes, events, and backup artifacts in SQLite.
Workers keep using host-local microsandbox volumes and claim jobs through
`POST /worker/claim`; `GET /worker/events?node_id=...` is only a low-latency
wake signal.

The API/UI routes are intended to sit behind Tailscale or an authenticated
reverse proxy. Do not bind the API to a public interface without adding that
outer auth layer.

Workers also expose private control endpoints, such as
`POST /worker/services/{service}/open`, used by the API to open Hermes/OpenCode
tunnels on the host that owns the workspace VM. Bind these endpoints to
localhost for single-host deployments or to a Tailscale/private address for
multi-host deployments.

Worker endpoints require a bearer token through `workerTokenFile`,
`MOM_WORKER_TOKEN`, or `MOM_WORKER_TOKEN_FILE`.

## Real Fleet Tests

`tests/real_fleet.rs` is ignored by default and intended for deployed-host
smoke testing. Prefer an SSH tunnel to the API host so tests do not depend on
public auth:

```sh
ssh -L 18080:127.0.0.1:8080 mom-ctrl -N
```

Then run read-only checks:

```sh
export AGENTMOM_REAL_API_URL=http://127.0.0.1:18080
export AGENTMOM_REAL_WORKER_TOKEN="$(ssh mom-ctrl 'cat /run/agenix/agentmom-worker-token')"
export AGENTMOM_REAL_NODE_A=mom-1
just real-fleet-test
```

Workspace-creating and backup tests are opt-in because they touch real
microsandbox state:

```sh
export AGENTMOM_REAL_ALLOW_CREATE=1
export AGENTMOM_REAL_ALLOW_BACKUP=1
export AGENTMOM_REAL_ALLOW_CATALOG_BACKUP=1
export AGENTMOM_REAL_API_SSH_HOST=mom-ctrl
just real-fleet-test
```
