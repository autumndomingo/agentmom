{ pkgs, inputs }:

let
  hermes-agent-src = pkgs.applyPatches {
    name = "hermes-agent-patched-source";
    src = inputs.hermes-agent;
    patches = [ ];
    postPatch = ''
      substituteInPlace nix/lib.nix \
        --replace-fail 'npmDepsHash = "sha256-m9cjbjzi4SaFCjODfdrawS5e+1ag+MpRn528/upSNqo=";' \
                       'npmDepsHash = "sha256-kbjJksq7limRIYqP3DwI+GNgCXkG96tXcsQqmuEedxo=";'
    '';
  };

  npm-lockfile-fix = pkgs.python312Packages.buildPythonApplication {
    pname = "npm-lockfile-fix";
    version = "0.1.0";
    src = inputs.hermes-agent.inputs.npm-lockfile-fix;
    pyproject = true;
    build-system = [ pkgs.python312Packages.setuptools ];
    propagatedBuildInputs = with pkgs.python312Packages; [
      requests
      setuptools
    ];
    dontwrapPythonPrograms = true;
    doCheck = false;
    meta.mainProgram = "npm-lockfile-fix";
  };
in
pkgs.callPackage "${hermes-agent-src}/nix/hermes-agent.nix" {
  inherit (inputs.hermes-agent.inputs) uv2nix pyproject-nix pyproject-build-systems;
  inherit npm-lockfile-fix;
  extraDependencyGroups = [ "messaging" ];
  rev = inputs.hermes-agent.rev or null;
}
