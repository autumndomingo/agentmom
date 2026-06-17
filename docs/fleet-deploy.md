# Agent Mom Fleet Deploys

Agent Mom production and staging NixOS hosts are deployed from this repo with
Colmena.

## Hosts

| Node | Tags | Role |
|------|------|------|
| `mom-ctrl` | `agentmom`, `prod`, `ctrl`, `prod-ctrl` | Production API/controller |
| `mom-1` | `agentmom`, `prod`, `worker`, `prod-worker` | Production worker |
| `mom-stage-ctrl` | `agentmom`, `stage`, `ctrl`, `stage-ctrl` | Staging API/controller |
| `mom-stage-1` | `agentmom`, `stage`, `worker`, `stage-worker` | Staging worker |

The fleet inventory lives in `nix/fleet/inventory.nix`. Add future workers there
as host facts: environment, role, SSH target, module path, and capacity.

## Commands

Happy path:

```bash
just pre-merge
just deploy-stage-and-check
just deploy-prod-and-check
```

Deploy only:

```bash
just deploy-stage
just deploy-prod
```

Deploy one role or node:

```bash
just deploy-stage-ctrl
just deploy-stage-workers
just deploy-prod-ctrl
just deploy-prod-workers
just deploy-node mom-stage-1
```

Use ordered production deploys when changing service startup, nginx, or worker
registration behavior:

```bash
just deploy-prod-ordered-and-check
```

Fast health checks:

```bash
just check-stage
just check-prod
```

These check API readiness, UI health, controller and worker failed units, worker
health, monitor status, and registered fleet nodes.

Build without switching:

```bash
just fleet-build-stage
just fleet-build-prod
```

Inspect failed systemd units through Colmena:

```bash
just fleet-status
just fleet-status @stage
```

Disposable-state reset:

```bash
just reset-stage-state
just deploy-stage-and-check

just reset-prod-state
just deploy-prod-and-check
```

Reset commands stop Agent Mom services, stop workspace microVM units, move the
fleet catalog and microVM state into timestamped `reset-archives` directories,
and leave services stopped for the next deploy.

## SSH

Colmena uses normal OpenSSH configuration. Keep host-specific connection details
such as `HostKeyAlias`, `IdentityFile`, and `ProxyJump` in `~/.ssh/config`.

Expected aliases:

- `mom-ctrl`
- `mom-1`
- `mom-stage-1`

`mom-stage-ctrl` currently deploys through its public IP, `204.168.131.33`.

## Safety

The hive sets `allowApplyAll = false`, so deploys must name a node or tag with
`--on`. Use one-node deploys for SSH, firewall, bootloader, or network changes.

Colmena does not have deploy-rs magic rollback. If a host becomes unreachable,
use console/provider access or a reachable SSH path and run:

```bash
sudo nixos-rebuild switch --rollback
```
