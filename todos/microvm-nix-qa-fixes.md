# microvm.nix QA fixes

- [x] Fail `execute`/`hermes` jobs when the guest command exits nonzero.
- [x] Add a Nix-installed guest launcher so generic commands get Hermes/proxy env deterministically.
- [x] Route Hermes job commands through the guest launcher.
- [x] Align generated Hermes config with the pinned Hermes schema.
- [x] Delete dead `openai-codex` guest config fallback and guest OpenAI placeholder env.
- [x] Disable conflicting guest SSH host-key generation when Agent Mom installs pinned keys.
- [x] Add focused local tests and run local checks.
