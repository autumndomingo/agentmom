Read `~/configs/GLOBAL-AGENTS.md` (fallback: https://raw.githubusercontent.com/justinmoon/configs/master/GLOBAL-AGENTS.md). Skip if both unavailable.

# Agent Mom

## Development

Run the local development environment with `just dev`. Do not start the UI/API
or worker with ad hoc `cargo run` commands unless you are intentionally
debugging the process startup path. The `just dev` recipe builds the UI, chooses
available localhost ports, uses `config.dev.json`, starts `mom api` and
`mom worker`, and uses the real microvm.nix runtime.

For UTM-backed dev, use one instance per concurrent stack:

```sh
MOM_DEV_UTM_INSTANCE=<name> just dev-utm
```

The default `just dev-utm` uses `AgentMom-Dev`. Named instances use separate UTM
VMs, hostnames, host state/log dirs, guest checkout dirs, and guest microVM
state. Instance names must use lowercase letters, numbers, and internal dashes.
List them with `just dev-utm-list`.
