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

Build before switching:

```bash
just fleet-build-stage
just fleet-build-prod
```

Deploy:

```bash
just deploy-stage
just deploy-prod
just deploy-workers
just deploy-node mom-stage-1
```

Inspect failed systemd units across a selector:

```bash
just fleet-status
just fleet-status @stage
```

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
