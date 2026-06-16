# Agent Mom Staging

Staging source of truth is the `stage` branch in:

```bash
~/code/agentmom/worktrees/microvm-fast-resume-live
```

Use this tree for stage-only runtime work until it is merged back to a mainline
branch.

## Hosts

- API/controller: `mom-stage-ctrl`, `204.168.131.33`, `https://stage.agentmom.xyz`
- Worker: `mom-stage-1`, public `135.181.179.143`, Tailscale `100.92.189.28`
- Stage worker node ID: `mom-stage-1`
- Stage worker test endpoints: enabled with `MOM_ENABLE_TEST_ENDPOINTS=1`

## Deploy

Deploy both stage roles from configs:

```bash
cd ~/configs
just agentmom-stage-switch
```

Deploy only the controller or worker:

```bash
cd ~/configs
just switch mom-stage-ctrl
just switch mom-stage-1
```

Stage is allowed to skip the expensive worker `mom node ensure-runtime` prestart
check. Run it manually when changing host runtime prerequisites:

```bash
ssh justin@100.92.189.28 'mom_bin=$(systemctl show -P ExecStart agentmom-worker.service | sed -n "s/.*path=\([^ ;]*\/bin\/mom\).*/\1/p" | head -1); sudo env $(systemctl show -P Environment agentmom-worker.service) "$mom_bin" node ensure-runtime'
```

## Latency Test

Run the true suspended-VM round trip:

```bash
cd ~/code/agentmom/worktrees/microvm-fast-resume-live
just stage-e2e-suspend-latency
```

The recipe keeps one browser websocket open, suspends the microVM, verifies the
unit is inactive and no Cloud Hypervisor process is running, then sends the
stage-only `mom/test/guest-ping` JSON-RPC method. That method wakes the VM on
the normal Hermes ACP path and removes model latency from the measurement.

Expected good signal:

- `suspend_control_ms=... status=suspended vm_state=suspended unit=inactive vm_process_present=False`
- `open_ws_prompt_to_acp_connected_ms` under about 500ms
- `open_ws_guest_ping_response_ms` under about 500ms on a good run

## Caveats

- Stage uses the same auth secret, invite code, and worker token material as prod.
- `mom-stage-1` is the old `mom-2` dedicated host; the configs path is still
  `~/configs/hosts/hetzner`.
- `mom-stage-1` currently boots BIOS GRUB from `nvme1n1`; keep bootloader config
  aligned with that disk or `nixos-rebuild switch` will not persist across reboot.
- Keep `MOM_ENABLE_TEST_ENDPOINTS=1` stage-only.
