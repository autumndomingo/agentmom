{
  lib,
  buildNpmPackage,
  makeWrapper,
  nodejs_24,
}:

buildNpmPackage {
  pname = "agentmom";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ./..;
    filter = path: type:
      let
        base = baseNameOf path;
      in
      !(base == ".git"
        || base == ".direnv"
        || base == ".env"
        || base == ".env.prod"
        || base == ".agentmom"
        || base == ".agentmom-auth"
        || base == ".smoke-workspace"
        || base == "dist"
        || base == "node_modules"
        || base == "projects"
        || base == "result"
        || base == "workspaces"
        || base == "worktrees"
        || base == "npm-debug.log");
  };

  nodejs = nodejs_24;
  npmDepsFetcherVersion = 2;
  npmDepsHash = "sha256-CXke70xSg7JaRyhE6KGxD65lYTPIOjU8WTuEPZlzy2Y=";
  makeCacheWritable = true;

  nativeBuildInputs = [ makeWrapper ];

  npmBuildScript = "build";

  installPhase = ''
    runHook preInstall

    mkdir -p "$out/share/agentmom" "$out/bin"
    cp -R dist src scripts package.json package-lock.json node_modules "$out/share/agentmom/"

    makeWrapper ${nodejs_24}/bin/node "$out/bin/agentmom" \
      --set NODE_ENV production \
      --chdir "$out/share/agentmom" \
      --add-flags "$out/share/agentmom/node_modules/tsx/dist/cli.mjs" \
      --add-flags "$out/share/agentmom/src/server.ts"

    runHook postInstall
  '';

  meta = {
    description = "Lean Pi-backed Agent Mom web shell";
    mainProgram = "agentmom";
  };
}
