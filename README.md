# Agent Mom

`mom` is a Rust CLI, worker, API, and browser UI for durable Hermes workspaces
backed by NixOS microVMs.

The runtime is `microvm.nix` with Cloud Hypervisor and a host virtiofs workspace
directory mounted at `/workspace`. Agent Mom generates a small declarative
per-workspace flake, starts it through a systemd template unit, and talks to the
guest over SSH.

## Workspace Model

Agent Mom treats a user workspace as the durable unit and the VM as replaceable
compute. A workspace has:

- one SQLite row in `MOM_STATE_DIR/fleet.db`
- one VM named `mom-<workspace>`
- one host workspace directory under `MOM_MICROVM_WORKSPACE_DIR`

```sh
mom workspace create alice --user user_123 --replace
mom workspace list
mom workspace exec alice -- pwd
mom workspace hermes alice -- --help
mom workspace inspect alice
mom workspace events alice --since 2h
mom workspace backup alice
mom workspace stop alice
mom workspace rm alice --force
mom node status
mom node list
mom node inspect mom-1
mom node cordon mom-1
mom node drain mom-1
mom node uncordon mom-1
mom db status
mom db backup
mom monitor check
```

Run the worker against a local or central API:

```sh
mom api
MOM_API_URL=http://127.0.0.1:8080 mom worker
```

The worker starts desired-running workspaces, stops idle workspaces, backs up
workspace directories with restic, and claims queued jobs from `mom api`.

## Host Config

`mom workspace create` requires a host config file at
`~/.config/mom/config.json`. Set `MOM_CONFIG=/path/to/config.json` to use a
different file. Production NixOS deployments should let the Nix module generate
this non-secret JSON from typed `services.agentmom.*` options.

```json
{
  "schema_version": 1,
  "credentials": {
    "proxy_url": "http://192.168.83.1:1080",
    "proxy_ca_path": "/var/lib/agentmom/iron-proxy/ca.crt"
  },
  "guest": {
    "hermes_profile": "main",
    "model": "openai/gpt-5.5"
  },
  "auth": {
    "secret_file": "/run/secrets/agentmom-auth-secret"
  }
}
```

Required assumptions:

- `credentials.proxy_url` and `credentials.proxy_ca_path` are required for guest configuration.
- `guest.hermes_profile` is the Hermes profile name created in the guest.
- `guest.model` is the default model written into the generated Hermes config.
- `auth.secret_file` is required for `mom api`; it signs browser sessions.
- On an empty catalog, the first signup creates the admin user. Existing users log in with email and password.

`mom config doctor` validates the configured file and prints a redacted
effective config. `mom node ensure-runtime` checks host prerequisites for the
microvm.nix runtime; there is no mutable base image contract in this first
microvm iteration.

## Runtime State

Relevant environment variables:

- `MOM_MICROVM_STATE_DIR`: generated VM specs, flakes, SSH keys, and state files.
- `MOM_MICROVM_WORKSPACE_DIR`: host directories shared into guests as `/workspace`.
- `MOM_MICROVM_BRIDGE`: bridge used by generated tap devices.
- `MOM_MICROVM_CIDR`: IPv4 CIDR for deterministic guest addresses, for example `192.168.83.0/24`.
- `MOM_MICROVM_HOST_IP`: host bridge address used as the guest default gateway.
- `MOM_MICROVM_NIXPKGS_URL`, `MOM_MICROVM_NIX_URL`, and `MOM_HERMES_AGENT_URL`: flake inputs used by generated workspace flakes. Production deploys should pass pinned revisions or immutable `path:/nix/store/...` inputs so cached workspace runners cannot drift from their generated input hash.

Backups use restic. Set `RESTIC_REPOSITORY` and the usual restic credentials in
the worker service environment before enabling scheduled backups. Agent Mom
stops a running workspace before backing up its host workspace directory, then
starts it again if it was desired-running.

## Development

```sh
nix develop
cargo build
cargo test
```

or:

```sh
nix build
```

For the local API/UI/worker loop:

```sh
just dev
```

`just dev` writes `.state/dev.env`, builds the UI, chooses available localhost
ports, starts iron-proxy on host port `1080`, starts `mom api`, and starts
`mom worker`. Runtime state stays under `.state/microvms`.

For UTM-backed dev, one instance maps to one UTM VM. The default instance keeps
the historical `AgentMom-Dev` VM and paths:

```sh
just dev-utm
```

Run another isolated VM by naming the instance:

```sh
MOM_DEV_UTM_INSTANCE=signup just dev-utm
```

Non-default instances use `AgentMom-$instance`, `.state/dev-utm-$instance`,
`.state/logs/dev-utm-$instance`, `/home/mom/agentmom-$instance`, and isolated
guest microVM state. List known Agent Mom UTM VMs with:

```sh
just dev-utm-list
```

### Web App Previews

A host-side agent or operator can register a web app running inside a workspace
VM so the browser UI shows it in the Preview pane:

```sh
mom workspace preview register alice --preview web --port 3000
mom workspace preview list alice
mom workspace preview remove alice web
```

`register` asks the workspace's assigned worker to open an SSH tunnel from the
host to `127.0.0.1:<port>` inside the VM, stores the returned URL in the
catalog, and prints that URL. Use `--host` for a different VM-local host and
`--path` for a non-root preview path. The UI polls the registered preview list
only while its Preview pane is open.

With `just dev` running, use `just dev-smoke` in another shell to check the API
health endpoint and cookie-based admin login. `just dev-reset` stops the dev
stack and deletes `.state/` and `dev/iron-proxy/`; it keeps `.env` and build
caches.

## NixOS Service

The flake exports `nixosModules.agentmom`.

```nix
{
  imports = [
    inputs.agentmom.nixosModules.agentmom
  ];

  services.agentmom = {
    enable = true;
    package = inputs.agentmom.packages.${pkgs.system}.mom;
    nodeId = "mom-1";
    logFormat = "json";
    stateDir = "/var/lib/agentmom";
    cutoverWipeMarker = "microvm-fast-start-cutover-v1";

    microvm = {
      enable = true;
      stateDir = "/var/lib/agentmom/microvms";
      workspaceDir = "/var/lib/agentmom/microvms/workspaces";
      bridge = "agentmom0";
      cidr = "192.168.83.0/24";
      hostAddress = "192.168.83.1";
      externalInterface = "eth0";
      kvmKernelModule = "kvm-amd";
      hermesAgentUrl = "path:${inputs.agentmom.inputs.hermes-agent.outPath}";
    };

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
      serviceTunnelPortRange = {
        from = 41000;
        to = 41999;
      };
    };
    workerUrlAllowlist = [ "http://127.0.0.1:9090" ];

    credentialProxy = {
      enable = true;
      package = inputs.agentmom.packages.${pkgs.system}.iron-proxy;
      openrouterApiKeyFile = "/run/secrets/openrouter-api-key";
    };

    guest.model = "openai/gpt-5.5";
    workerTokenFile = "/run/secrets/agentmom-worker-token";
    auth = {
      secretFile = "/run/secrets/agentmom-auth-secret";
      secureCookies = true;
    };
  };
}
```

The module declares the worker/API services, microVM state directories, a host
bridge service, and `agentmom-microvm@.service` for cold-start workspace VMs.
Workers expose private control endpoints such as
`POST /worker/services/{service}/open`, used by the API to open Hermes tunnels
on the host that owns a workspace. Bind these endpoints to localhost for
single-host deployments or to a Tailscale/private address for multi-host
deployments.
For multi-host deployments, set `worker.openFirewall = true` and
`worker.firewallInterface = "tailscale0"` or declare equivalent host firewall
rules so the API can reach the worker control port and browsers can reach the
configured service tunnel range.
When `credentialProxy.enable = true`, the module does not enable direct NAT for
the guest bridge; guests reach the built-in proxy on the bridge. The proxy
injects configured model-provider credentials and logs allowlist misses by
default, but it does not block arbitrary guest egress unless
`credentialProxy.warnOnly = false` is set intentionally.

For multi-host production, give the API a per-node token map instead of a
single shared worker token:

```nix
services.agentmom.workerNodeTokenFiles = {
  mom-1 = "/run/secrets/agentmom-worker-token-mom-1";
  mom-2 = "/run/secrets/agentmom-worker-token-mom-2";
};
services.agentmom.workerUrlAllowlist = [
  "http://100.81.250.67:9090"
  "http://100.92.189.28:9090"
];
```

Each worker host still sets only its own `workerTokenFile`. This binds worker
API calls to the node identity they claim.

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
just real-fleet-test-prod
```

Workspace-creating and backup tests are opt-in because they touch real runtime
state. Treat these as a deploy gate for microVM runtime changes; read-only
checks alone do not prove that guests boot, SSH is reachable, Hermes is present,
or the Hermes service tunnel can open:

```sh
export AGENTMOM_REAL_ALLOW_CREATE=1
export AGENTMOM_REAL_ALLOW_BACKUP=1
export AGENTMOM_REAL_ALLOW_CATALOG_BACKUP=1
export AGENTMOM_REAL_API_SSH_HOST=mom-ctrl
export AGENTMOM_REAL_ADMIN_EMAIL=you@example.com
export AGENTMOM_REAL_ADMIN_PASSWORD=...
just real-fleet-test-prod-mutating
```

Use the intended production admin email for `AGENTMOM_REAL_ADMIN_EMAIL`. On a
freshly wiped catalog, that signup creates the first admin; do not leave it at a
test address.

## Production Cutovers

`cutoverWipeMarker` is intentionally destructive and one-shot. Bump it to a new
marker name for each planned wipe; reusing a marker that already exists in
`stateDir` is a no-op. The service archives `fleet.db`, legacy microsandbox
state, and microVM machine/workspace directories before the API or worker starts.
The cutover unit stops Agent Mom API, worker, backup, and monitor services before
moving state, then restarts enabled backup/monitor timers after the marker is
written.

For non-destructive deploys, cordon or otherwise drain workers before switching
if you need predictable rollout latency. Workers stop claiming after SIGTERM and
let active jobs finish, so systemd can intentionally wait for long VM, backup, or
restore work rather than killing it mid-side-effect.

Rolling back after this branch has started workspaces is not just a NixOS
generation switch. Generated machine directories are durable and may still
contain fast-start definitions that use the read-only host `/nix/store` virtiofs
share. If that path is the rollback reason, stop workspace VMs and archive or
regenerate those machine directories as part of the rollback.

After switching roles, run the strict monitor check, not just `/health/ready`.
The ready endpoint only proves the API can open the catalog; `mom monitor check`
also verifies fresh workers, queue age, failed jobs, and backup health.

Fast-start guests can read the host Nix store through a read-only virtiofs
mount. Keep secrets in runtime secret files such as agenix outputs, never baked
into derivations or other Nix store paths that workspace guests should not see.
