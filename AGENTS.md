Read `~/configs/GLOBAL-AGENTS.md` (fallback: https://raw.githubusercontent.com/justinmoon/configs/master/GLOBAL-AGENTS.md). Skip if both unavailable.

# Agent Mom

## Development

Run the development environment with `just dev` on a Linux/KVM host. For remote
real-runtime work, use `lab`:

```sh
ssh lab
cd ~/code/agentmom
just dev
```

Do not start the UI/API or worker with ad hoc `cargo run` commands unless you
are intentionally debugging the process startup path. The `just dev` recipe
builds the UI, chooses available localhost ports, writes `.state/config.dev.json`,
starts `mom api` and `mom worker`, and uses the real microvm.nix runtime.
