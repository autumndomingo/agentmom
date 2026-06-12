{
  description = "Agent Mom: small Alpine microsandbox VM manager for Codex and Hermes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, crane, rust-overlay }:
    let
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      eachSystem = nixpkgs.lib.genAttrs systems;
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
          commonArgs = {
            src = craneLib.cleanCargoSource ./.;
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
            postPatch = nixpkgs.lib.optionalString (builtins.hasAttr system microsandboxBundleHashes && builtins.hasAttr system microsandboxAgentdHashes) ''
              mkdir -p "$TMPDIR/nix-vendor"
              cp -R -L "$cargoVendorDir"/. "$TMPDIR/nix-vendor"/
              sed -i.bak "s|$cargoVendorDir|$TMPDIR/nix-vendor|g" "$TMPDIR/nix-vendor/config.toml"
              rm "$TMPDIR/nix-vendor/config.toml.bak"
              chmod -R u+w "$TMPDIR/nix-vendor"

              mkdir -p "$TMPDIR/nix-vendor/build"
              touch "$TMPDIR/nix-vendor/Cargo.toml"
              cp ${microsandbox-runtime}/bin/msb "$TMPDIR/nix-vendor/build/msb"
              cp ${microsandbox-runtime}/lib/libkrunfw.*.*.* "$TMPDIR/nix-vendor/build/"
              cp ${microsandbox-agentd}/bin/agentd "$TMPDIR/nix-vendor/build/agentd"
              cargoVendorDir="$TMPDIR/nix-vendor"
            '';
            preBuild = ''
              export HOME="$TMPDIR/home"
              export MSB_HOME="$TMPDIR/microsandbox"
              export XDG_CACHE_HOME="$TMPDIR/cache"
              mkdir -p "$HOME" "$MSB_HOME" "$XDG_CACHE_HOME"
            '' + nixpkgs.lib.optionalString (builtins.hasAttr system microsandboxBundleHashes && builtins.hasAttr system microsandboxAgentdHashes) ''
              export CI=1
              mkdir -p build
              cp ${microsandbox-runtime}/bin/msb build/msb
              cp ${microsandbox-runtime}/lib/libkrunfw.*.*.* build/
              cp ${microsandbox-agentd}/bin/agentd build/agentd
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
          microsandboxVersion = "0.5.6";
          microsandboxBundleHashes = {
            x86_64-linux = "sha256-tVCx9fB4XY+2+eEyLCFq+m/k6DAg6n82ThIwgFDYTlU=";
            aarch64-linux = "sha256-kpbLJI9u3phg625gKeMtlszwm6NOKy5vazPAiZvHjoc=";
            aarch64-darwin = "sha256-RifDi+7vNI9g8tQIyHhhjJ6firQzmjAgQFgEFXs98t4=";
          };
          microsandboxBundlePlatforms = {
            x86_64-linux = "linux-x86_64";
            aarch64-linux = "linux-aarch64";
            aarch64-darwin = "darwin-aarch64";
          };
          microsandboxAgentdHashes = {
            x86_64-linux = "sha256-7LRrjBMjQoPkyItM+MMrcVCYWCaxgE8s0KVbfTcEQ+o=";
            aarch64-linux = "sha256-BHBGigWM/ydms0Or7E6A6/8SUo8/43mjSJgx2lIWhRg=";
            aarch64-darwin = "sha256-BHBGigWM/ydms0Or7E6A6/8SUo8/43mjSJgx2lIWhRg=";
          };
          microsandboxAgentdPlatforms = {
            x86_64-linux = "x86_64";
            aarch64-linux = "aarch64";
            aarch64-darwin = "aarch64";
          };
          microsandbox-agentd = pkgs.stdenv.mkDerivation {
            pname = "microsandbox-agentd";
            version = microsandboxVersion;
            src = pkgs.fetchurl {
              url = "https://github.com/superradcompany/microsandbox/releases/download/v${microsandboxVersion}/agentd-${microsandboxAgentdPlatforms.${system}}";
              hash = microsandboxAgentdHashes.${system};
            };
            dontUnpack = true;
            installPhase = ''
              runHook preInstall
              install -Dm755 "$src" "$out/bin/agentd"
              runHook postInstall
            '';
          };
          microsandbox-runtime = pkgs.stdenv.mkDerivation {
            pname = "microsandbox-runtime";
            version = microsandboxVersion;
            src = pkgs.fetchurl {
              url = "https://github.com/superradcompany/microsandbox/releases/download/v${microsandboxVersion}/microsandbox-${microsandboxBundlePlatforms.${system}}.tar.gz";
              hash = microsandboxBundleHashes.${system};
            };
            sourceRoot = ".";
            nativeBuildInputs = nixpkgs.lib.optionals pkgs.stdenv.isLinux [
              pkgs.autoPatchelfHook
            ];
            buildInputs = nixpkgs.lib.optionals pkgs.stdenv.isLinux [
              pkgs.libcap_ng
              pkgs.stdenv.cc.cc.lib
            ];
            installPhase = ''
              runHook preInstall
              install -Dm755 msb $out/bin/msb
              lib="$(echo libkrunfw.*)"
              install -Dm644 "$lib" "$out/lib/$lib"
              case "$lib" in
                *.so.*)
                  ln -s "$lib" "$out/lib/libkrunfw.so.5"
                  ln -s libkrunfw.so.5 "$out/lib/libkrunfw.so"
                  ;;
                *.dylib)
                  ln -s "$lib" "$out/lib/libkrunfw.dylib"
                  ;;
              esac
              runHook postInstall
            '';
          };
        in
        {
          default = mom;
          mom = mom;
          iron-proxy = iron-proxy;
        } // nixpkgs.lib.optionalAttrs (builtins.hasAttr system microsandboxBundleHashes) {
          microsandbox-runtime = microsandbox-runtime;
        } // nixpkgs.lib.optionalAttrs (builtins.hasAttr system microsandboxAgentdHashes) {
          microsandbox-agentd = microsandbox-agentd;
        });

      apps = eachSystem (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.mom}/bin/mom";
        };
      });

      nixosModules.default = import ./nix/module.nix;
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
          wranglerDev = pkgs.writeShellApplication {
            name = "wrangler";
            runtimeInputs = [ pkgs.nodejs ];
            text = ''
              exec npx --yes wrangler@4 "$@"
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
                pkgs.just
                pkgs.nodejs
                pkgs.pkg-config
                pkgs.rust-analyzer
                wranglerDev
              ]
              ++ nixpkgs.lib.optionals pkgs.stdenv.isLinux [
                pkgs.libcap_ng
                pkgs.openssl
              ];
            RUST_BACKTRACE = "1";
          };
        });
    };
}
