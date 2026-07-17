# Agent Mom Nix deployment

This repo owns the deployable service shape: app package, systemd unit, Podman,
smolvm, state directories, and optional same-host Caddy routes.

Production deploys use Colmena:

```bash
just deploy-prod
```

That applies the `mom-1` NixOS host from this repo and then runs a small health
check against the local service on the host.

The equivalent direct NixOS switch is:

```bash
nixos-rebuild switch --flake .#mom-1 --target-host justin@mom-1.quokka-fir.ts.net
```

If another repo owns the surrounding host config, it should only import one of
these modules:

```nix
inputs.agentmom.nixosModules.stageHost
inputs.agentmom.nixosModules.prodHost
```

or import the base module and set options directly:

```nix
{
  imports = [ inputs.agentmom.nixosModules.agentmomWeb ];

  services.agentmomWeb = {
    enable = true;
    stateDir = "/data/agentmom-web";
    workspaceDir = "/data/agentmom-web/workspace";
    envFile = "/run/agenix/agentmom-secrets";
    deploymentBaseDomain = "agentmom.xyz";

    caddy = {
      enable = true;
      publicDomain = "agentmom.xyz";
    };
  };
}
```

The app env file is passed with systemd credentials, so the source secret can
stay root-only. Podman is enabled by the module and the service user gets
subuid/subgid ranges for rootless container builds.

Application secrets live in `nix/secrets/secrets.age`. It is an env file read
through a systemd credential and must contain:

```env
OPENROUTER_API_KEY=...
AGENTMOM_TELEGRAM_BOT_TOKEN=...
BRAVE_API_KEY=...
```

Edit it with agenix:

```bash
RULES=nix/secrets/secrets.nix agenix -e nix/secrets/secrets.age -i ~/configs/yubikeys/keys.txt
```

The stage and prod host modules assume Caddy runs on the same host as the app.
For prod, `agentmom.xyz` and `*.agentmom.xyz` should point at
`mom-1.quokka-fir.ts.net`.
