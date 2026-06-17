# Stage Host Dev

Read when you need a real Agent Mom dev loop on `mom-stage-1` with Cloud Hypervisor, systemd, the host bridge, and the credential proxy.

This is the preferred replacement for UTM when testing runtime behavior. It is intentionally simple: run API and worker binaries from a checkout in a tmux session on `mom-stage-1`, while reusing the installed stage host runtime services.

## Why

Mac tests and fake-worker tests do not exercise `/dev/kvm`, Cloud Hypervisor, `agentmom-microvm@.service`, tap devices, or the stage credential proxy. Nested `v` VMs currently add too much Nix-store friction for Agent Mom itself.

Stage-host dev gives you:

- a dev API/catalog under `.state/stage-host-dev`
- the checkout's current Rust/UI code
- the real stage microVM runtime under `/data/agentmom/microvms`
- the installed `agentmom0` bridge and credential proxy
- real `agentmom-microvm@...` units

## Start

On your Mac:

```bash
ssh dev@mom-stage-1 -t 'tmux new -A -s agentmom-dev'
```

Inside tmux on `mom-stage-1`:

```bash
cd ~/agentmom
just stage-host-dev
```

In another Mac terminal:

```bash
ssh -N -L 18787:127.0.0.1:18787 dev@mom-stage-1
```

Open:

```text
http://127.0.0.1:18787/admin
```

The first signup creates the dev admin user for this dev catalog.

## Reset

Inside the same checkout on `mom-stage-1`:

```bash
just stage-host-dev-reset
```

This stops the dev API/worker and any `agentmom-microvm@dev-*.service` units. Use workspace names prefixed with `dev-` so cleanup is predictable.

To wipe the dev catalog:

```bash
rm -rf .state/stage-host-dev
```

## Important Tradeoff

The dev catalog is isolated, but the microVM runtime directory is shared with stage:

```text
/data/agentmom/microvms
```

This is deliberate. The installed `agentmom-microvm@.service` has that path baked into its systemd environment. Sharing it is what lets checkout-built worker code start real Cloud Hypervisor VMs without a NixOS deploy.

Use `dev-*` workspace names. Do not use a stage/prod workspace name unless you intentionally want to test against that VM.
