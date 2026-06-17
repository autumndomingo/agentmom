{
  self,
  inputs,
  nixpkgs,
  colmena,
  disko,
  microvm,
}:
let
  lib = nixpkgs.lib;
  inventory = import ./inventory.nix;
  specialArgs = {
    inherit inputs microvm;
    agentmom = self;
  };
  mkNodeModules = node: [
    disko.nixosModules.disko
    inputs.agenix.nixosModules.default
    node.module
  ];
  mkDeployment = name: node: {
    targetHost = node.targetHost;
    targetUser = node.targetUser or "justin";
    buildOnTarget = node.buildOnTarget or true;
    replaceUnknownProfiles = true;
    tags =
      [
        "agentmom"
        node.env
        node.role
        "${node.env}-${node.role}"
      ]
      ++ (node.tags or []);
  };
  mkColmenaNode = name: node: {
    imports = mkNodeModules node;
    deployment = mkDeployment name node;
  };
  mkNixosConfiguration = name: node:
    nixpkgs.lib.nixosSystem {
      system = node.system or "x86_64-linux";
      modules = mkNodeModules node;
      inherit specialArgs;
    };
in
{
  colmenaHive = colmena.lib.makeHive ({
    meta = {
      name = "agentmom";
      description = "Agent Mom NixOS fleet";
      allowApplyAll = false;
      nixpkgs = import nixpkgs {
        system = "x86_64-linux";
        config.allowUnfree = true;
      };
      inherit specialArgs;
    };
  } // lib.mapAttrs mkColmenaNode inventory.nodes);

  nixosConfigurations = lib.mapAttrs mkNixosConfiguration inventory.nodes;
}
