# hvm

`hvm` is a small Rust CLI for managing Alpine microsandbox VMs for Codex and
Hermes.

It uses the microsandbox Rust SDK directly. It does not shell out to
`npx microsandbox` for lifecycle operations.

## Commands

```sh
hvm create mybox --replace
hvm list
hvm enter mybox
hvm exec mybox -- pwd
hvm codex mybox "Reply exactly ok"
hvm hermes mybox -- --help
hvm doctor mybox
hvm stop mybox
hvm start mybox
hvm rm mybox --force
```

## Host Config

`hvm create` requires a host config file at `~/.config/hvm/config.json`.
Set `HVM_CONFIG=/path/to/config.json` to use a different file.

```json
{
  "codex_auth_path": "~/.codex/auth.json",
  "hermes_profile": "main",
  "hermes_model": "gpt-5.5",
  "snapshot_name": "hvm-alpine-agent-base"
}
```

Required assumptions:

- `codex_auth_path` exists and contains Codex CLI OAuth tokens.
- `hermes_profile` is the guest profile name to create.
- `hermes_model` is the default Hermes model for `openai-codex`.
- `snapshot_name` is the prebuilt microsandbox snapshot to boot new VMs from.

`create` uses `snapshot_name` by default. If the snapshot is missing, hvm builds
it once from the `alpine` image by installing `nodejs`, `npm`, `python3`, `uv`,
`@openai/codex`, and `hermes-agent`, then snapshots the stopped builder VM.
Pass `--rebuild-snapshot` to refresh that base, or `--no-snapshot` to force the
slow direct-Alpine provisioning path.

Each new VM is then patched with OpenAI/Codex auth and Hermes config:

- `codex_auth_path` -> `/root/.codex/auth.json`
- OpenAI Codex tokens from `codex_auth_path` -> `/root/.hermes-agent/<hermes_profile>/auth.json`
- a minimal generated Hermes `config.yaml` selecting `openai-codex`

These are one-time writes, not bind mounts. The base snapshot may contain the
auth present when it was built, and each create overwrites auth from the current
host config. Host Hermes profiles, sessions, custom providers, MCP entries,
memories, plugins, and local paths are not copied. After creation, the VM has
its own filesystem and no host directory sharing.

## Build

```sh
nix develop
cargo build
```

or:

```sh
nix build
```
