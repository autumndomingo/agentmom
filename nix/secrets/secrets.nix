# Agenix secrets configuration
# This file defines which keys can decrypt which secrets.
#
# Keys:
#   - yubikey_primary: Daily driver YubiKey (no-pin, tap only)
#   - yubikey_backup: Backup YubiKey (pin once per session)
#   - server: Configs NixOS hosts
#
# To edit a secret:
#   cd ~/code/agentmom
#   agenix -e nix/secrets/<name>.age -i ~/configs/yubikeys/keys.txt
#
# To rekey all:
#   cd ~/code/agentmom/nix/secrets
#   agenix -r -i ~/configs/yubikeys/keys.txt

	let
	  # YubiKey: Primary (daily driver) - Slot 2, no-pin
	  yubikey_primary = "age1yubikey1q0zhu9e7zrj48zmnpx4fg07c0drt9f57e26uymgxa4h3fczwutzjjp5a6y5"; # gitleaks:allow (public age recipient)

	  # YubiKey: Backup - Slot 1
	  yubikey_backup = "age1yubikey1qtdv7spad78v4yhrtrts6tvv5wc80vw6mah6g64m9cr9l3ryxsf2jdx8gs9"; # gitleaks:allow (public age recipient)

  # Server key (deployed to /etc/age/key.txt on configs hosts)
  server = "age1mtf29wt0we3adcja7k0ylc9hmf2fns3c44qz9g663l0ydepxqdrq94jzzf";
  pika_build = "age16293kjnhamtq3e4nu0q8ydcguy0eajesmkvakrxhudaqtdgq6dqqc38vjv";

  # Key groups
  personalKeys = [ yubikey_primary yubikey_backup ];
  serverKeys = [ server pika_build ];
  allKeys = personalKeys ++ serverKeys;
in {
  # Agent Mom fleet secrets (need server key for remote nixos-rebuild)
  "github-ssh-key.age".publicKeys = allKeys;
  "tailscale-auth-key.age".publicKeys = allKeys;
  "openrouter-api-key.age".publicKeys = allKeys;
  "agentmom-auth-secret.age".publicKeys = allKeys;
  "agentmom-bootstrap-admin-code.age".publicKeys = allKeys;
  "agentmom-worker-token-mom-1.age".publicKeys = allKeys;
  "agentmom-worker-token-mom-stage-1.age".publicKeys = allKeys;
  "agentmom-restic-env.age".publicKeys = allKeys;

  # Builder credentials used by NixOS hosts. This key authenticates to
  # nixbuild.net; it is not a Nix store signing key and must not be placed in
  # build inputs.
  "nixbuild-net-key.age".publicKeys = allKeys;
}
