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
    "secret_file": "/run/secrets/agentmom-auth-secret",
    "bootstrap_admin_code_file": "/run/secrets/agentmom-bootstrap-admin-code"
  }
}
```

Required assumptions:

- `credentials.proxy_url` and `credentials.proxy_ca_path` are required for guest configuration.
- `guest.hermes_profile` is the Hermes profile name created in the guest.
- `guest.model` is the default model written into the generated Hermes config.
- `auth.secret_file` is required for `mom api`; it signs browser sessions.
- `auth.bootstrap_admin_code_file` is required for `mom api`; an empty DB only creates the first admin when the login supplies this code.

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
- `MOM_MICROVM_NIXPKGS_URL` and `MOM_MICROVM_NIX_URL`: flake inputs used by generated workspace flakes.

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
    cutoverWipeMarker = "microvm-cutover-v2";

    microvm = {
      enable = true;
      stateDir = "/var/lib/agentmom/microvms";
      workspaceDir = "/var/lib/agentmom/microvms/workspaces";
      bridge = "agentmom0";
      cidr = "192.168.83.0/24";
      hostAddress = "192.168.83.1";
      externalInterface = "eth0";
      kvmKernelModule = "kvm-amd";
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
      bootstrapAdminCodeFile = "/run/secrets/agentmom-bootstrap-admin-code";
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
state:

```sh
export AGENTMOM_REAL_ALLOW_CREATE=1
export AGENTMOM_REAL_ALLOW_BACKUP=1
export AGENTMOM_REAL_ALLOW_CATALOG_BACKUP=1
export AGENTMOM_REAL_API_SSH_HOST=mom-ctrl
export AGENTMOM_REAL_ADMIN_EMAIL=you@example.com
export AGENTMOM_REAL_ADMIN_CODE="$(ssh mom-ctrl 'cat /run/agenix/agentmom-bootstrap-admin-code')"
just real-fleet-test-prod-mutating
```

Use the intended production admin email for `AGENTMOM_REAL_ADMIN_EMAIL`. On a
freshly wiped catalog, that login will consume the first-admin bootstrap path;
do not leave it at a test address.
