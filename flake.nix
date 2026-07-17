{
  description = "Agent Mom: a lean Pi-backed local web shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    agenix = {
      url = "github:ryantm/agenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    disko = {
      url = "github:nix-community/disko";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    colmena = {
      url = "github:nix-community/colmena";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, agenix, disko, colmena, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system:
          f { pkgs = nixpkgs.legacyPackages.${system}; inherit system; });
      mkHostModules = module: [
        agenix.nixosModules.default
        disko.nixosModules.disko
        module
      ];
      mkHost = module:
        nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          specialArgs = { inherit self; };
          modules = mkHostModules module;
        };
      mkColmenaNode = { module, targetHost, tags }: {
        imports = mkHostModules module;
        deployment = {
          inherit targetHost tags;
          targetUser = "justin";
          buildOnTarget = true;
          replaceUnknownProfiles = true;
        };
      };
    in {
      devShells = forAllSystems ({ pkgs, system, ... }: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            git
            just
            nodejs_24
            podman
            agenix.packages.${system}.default
            colmena.packages.${system}.colmena
          ];

          shellHook = ''
            export IN_NIX_SHELL=1
          '';
        };
      });

      packages = forAllSystems ({ pkgs, system, ... }:
        let
          smolvmReleaseSystems = [ "aarch64-linux" "x86_64-linux" ];
          agentmom = pkgs.callPackage ./nix/package.nix { };
        in
        {
          inherit agentmom;
          default = agentmom;
        } // nixpkgs.lib.optionalAttrs (builtins.elem system smolvmReleaseSystems) {
          smolvm = pkgs.callPackage ./nix/smolvm-release.nix { };
        });

      apps = forAllSystems ({ pkgs, system, ... }: {
        default = {
          type = "app";
          program = "${self.packages.${system}.agentmom}/bin/agentmom";
        };
      });

      nixosModules.default = self.nixosModules.agentmomWeb;
      nixosModules.agentmom = self.nixosModules.agentmomWeb;
      nixosModules.agentmomWeb = import ./nix/module.nix { inherit self; };
      nixosModules.stageHost = import ./nix/hosts/stage.nix { inherit self; };
      nixosModules.prodHost = import ./nix/hosts/prod.nix { inherit self; };

      nixosConfigurations.mom-stage-1 = mkHost ./nix/hosts/mom-stage-1/configuration.nix;
      nixosConfigurations.mom-1 = mkHost ./nix/hosts/mom-1/configuration.nix;

      colmenaHive = colmena.lib.makeHive {
        meta = {
          name = "agentmom";
          description = "Agent Mom production host";
          allowApplyAll = false;
          nixpkgs = import nixpkgs {
            system = "x86_64-linux";
            config.allowUnfree = true;
          };
          specialArgs = { inherit self; };
        };

        mom-1 = mkColmenaNode {
          module = ./nix/hosts/mom-1/configuration.nix;
          targetHost = "mom-1.quokka-fir.ts.net";
          tags = [ "agentmom" "prod" ];
        };

        mom-stage-1 = mkColmenaNode {
          module = ./nix/hosts/mom-stage-1/configuration.nix;
          targetHost = "100.92.189.28";
          tags = [ "agentmom" "stage" ];
        };
      };
    };
}
