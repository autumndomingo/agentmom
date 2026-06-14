{
  description = "Agent Mom: isolated workspace VM manager for Codex and Hermes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    microvm = {
      url = "github:microvm-nix/microvm.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    hermes-agent = {
      url = "github:NousResearch/hermes-agent";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, crane, rust-overlay, microvm, hermes-agent }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      eachSystem = nixpkgs.lib.genAttrs systems;
      nixpkgsInputUrl = "path:${nixpkgs.outPath}";
      microvmInputUrl = "path:${microvm.outPath}";
    in
    {
      packages = eachSystem (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rustToolchain = pkgs.rust-bin.stable.latest.default;
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              let
                rel = pkgs.lib.removePrefix "${toString ./.}/" (toString path);
              in
              (craneLib.filterCargoSources path type
                || rel == "nix"
                || rel == "nix/microvm-workspace.nix"
                || rel == "nix/hermes-agent-package.nix")
              && !(pkgs.lib.hasPrefix "tests/" rel);
          };
          cargoVendorDir = craneLib.vendorCargoDeps {
            inherit src;
          };
          commonArgs = {
            inherit src cargoVendorDir;
            strictDeps = true;
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs =
              nixpkgs.lib.optionals pkgs.stdenv.isDarwin [
                pkgs."apple-sdk"
              ]
              ++ nixpkgs.lib.optionals pkgs.stdenv.isLinux [
                pkgs.libcap_ng
                pkgs.openssl
              ];
            preBuild = ''
              export HOME="$TMPDIR/home"
              export XDG_CACHE_HOME="$TMPDIR/cache"
              mkdir -p "$HOME" "$XDG_CACHE_HOME"
            '';
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          ui = pkgs.buildNpmPackage {
            pname = "agent-mom-ui";
            version = "0.1.0";
            src = ./ui;
            npmDepsHash = "sha256-8K/nuaBIqeWYc/i3wvDnj9JED59PHhgSH9wN3E6h6Eg=";
            installPhase = ''
              runHook preInstall
              mkdir -p "$out/share/agentmom/ui"
              cp -R dist/. "$out/share/agentmom/ui/"
              runHook postInstall
            '';
          };
          mom = craneLib.buildPackage (commonArgs // {
            inherit cargoArtifacts;
            doCheck = false;
            postInstall = ''
              mkdir -p "$out/share/agentmom"
              cp -R ${ui}/share/agentmom/ui "$out/share/agentmom/ui"
              chmod -R u+w "$out/share/agentmom/ui"
            '';
          });
          ironProxyVersion = "0.42.0";
          ironProxyHashes = {
            x86_64-linux = "sha256-kOxFtUUiCjU2pFtEnIZWuUhPMYmGcneX/BVWa89FT3Q=";
            aarch64-linux = "sha256-r6hall8ox/QyaJiRNawqr5hnogXMME9/v1Jy5LCSNz8=";
            x86_64-darwin = "sha256-oneK0h6nwiijGkP23RQjxRzucPvZxSojIMyZsNskFlw=";
            aarch64-darwin = "sha256-XN+Fx5vxPGQjqsE4xlbBhQqO8l1B9CtGmEwe5M0ED2o=";
          };
          ironProxyPlatforms = {
            x86_64-linux = "linux_amd64";
            aarch64-linux = "linux_arm64";
            x86_64-darwin = "darwin_amd64";
            aarch64-darwin = "darwin_arm64";
          };
          iron-proxy = pkgs.stdenv.mkDerivation {
            pname = "iron-proxy";
            version = ironProxyVersion;
            src = pkgs.fetchurl {
              url = "https://github.com/ironsh/iron-proxy/releases/download/v${ironProxyVersion}/iron-proxy_${ironProxyVersion}_${ironProxyPlatforms.${system}}.tar.gz";
              hash = ironProxyHashes.${system};
            };
            sourceRoot = ".";
            installPhase = ''
              runHook preInstall
              install -Dm755 iron-proxy $out/bin/iron-proxy
              runHook postInstall
            '';
          };
          hermes-agent-package = import ./nix/hermes-agent-package.nix {
            inherit pkgs;
            inputs = { inherit hermes-agent; };
          };
        in
        {
          default = mom;
          mom = mom;
          iron-proxy = iron-proxy;
          hermes-agent = hermes-agent-package;
        });

      apps = eachSystem (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.mom}/bin/mom";
        };
      });

      nixosModules.default = moduleArgs@{ config, lib, pkgs, ... }:
        import ./nix/module.nix (moduleArgs // {
          defaultNixpkgsUrl = nixpkgsInputUrl;
          defaultMicrovmNixUrl = microvmInputUrl;
          defaultHermesAgentUrl = "path:${hermes-agent.outPath}";
        });
      nixosModules.agentmom = self.nixosModules.default;

      devShells = eachSystem (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          rustToolchain = pkgs.rust-bin.stable.latest.default;
          momDev = pkgs.writeShellApplication {
            name = "mom";
            runtimeInputs = [ rustToolchain ];
            text = ''
              exec cargo run --bin mom -- "$@"
            '';
          };
          playwrightNodeModules = pkgs.runCommand "agentmom-playwright-node-modules" { } ''
            mkdir -p "$out"
            ln -s ${pkgs.playwright} "$out/playwright"
            ln -s ${pkgs.playwright} "$out/playwright-core"
          '';
          playwrightCli = pkgs.writeShellApplication {
            name = "playwright";
            runtimeInputs = [ pkgs.nodejs ];
            text = ''
              export NODE_PATH="${playwrightNodeModules}:''${NODE_PATH:-}"
              export PLAYWRIGHT_BROWSERS_PATH="${pkgs.playwright.browsers}"
              export PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1
              exec node ${pkgs.playwright}/cli.js "$@"
            '';
          };
        in
        {
          default = pkgs.mkShell {
            packages =
              [
                momDev
                rustToolchain
                pkgs.cargo-nextest
                pkgs.curl
                self.packages.${system}.iron-proxy
                pkgs.just
                pkgs.lsof
                pkgs.nodejs
                pkgs.playwright.browsers
                pkgs.pkg-config
                playwrightCli
                pkgs.rust-analyzer
              ]
              ++ nixpkgs.lib.optionals pkgs.stdenv.isLinux [
                pkgs.libcap_ng
                pkgs.openssl
              ];
            NODE_PATH = "${playwrightNodeModules}";
            PLAYWRIGHT_BROWSERS_PATH = "${pkgs.playwright.browsers}";
            PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";
            RUST_BACKTRACE = "1";
          };
        });
    };
}
