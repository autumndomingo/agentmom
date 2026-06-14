# microvm.nix QA fixes

- [x] Fail `execute`/`hermes` jobs when the guest command exits nonzero.
- [x] Add a Nix-installed guest launcher so generic commands get Hermes/proxy env deterministically.
- [x] Route Hermes job commands through the guest launcher.
- [x] Route CLI Hermes and ACP preflight through the same guest wrappers.
- [x] Move Hermes dashboard process supervision into a guest systemd service.
- [x] Align generated Hermes config with the pinned Hermes schema.
- [x] Delete dead `openai-codex` guest config fallback and guest OpenAI placeholder env.
- [x] Disable conflicting guest SSH host-key generation when Agent Mom installs pinned keys.
- [x] Add focused local tests and run local checks.
- [x] Remove the legacy Rust Hermes config writer and `workspace refresh-config`; rebuild/recreate with the Nix guest definition instead.
- [x] Add and run UTM e2e coverage for first-user login, workspace lifecycle, proxy smoke, Hermes dashboard, previews, TUI sessions, OpenRouter inference, failed-command status, and cleanup.
- [x] Align the dev UTM proxy allowlist with production for Hermes metadata hosts.
- [ ] Package or disable Hermes lazy provider/plugin dependencies so production guests do not install from PyPI at inference time.
