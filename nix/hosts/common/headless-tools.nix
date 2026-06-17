{pkgs, ...}: {
  environment.systemPackages = import ./headless-packages.nix {inherit pkgs;};
}
