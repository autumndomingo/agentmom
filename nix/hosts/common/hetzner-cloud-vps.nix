{ lib, modulesPath, ... }:

{
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
      (modulesPath + "/profiles/qemu-guest.nix")
      ./cachix-justinmoon.nix
      ./server-base.nix
      ./nixbuild-net.nix
    ];

  networking = {
    useDHCP = true;
    nameservers = [ "1.1.1.1" "8.8.8.8" ];
    firewall = {
      enable = true;
      allowPing = true;
      allowedTCPPorts = lib.mkDefault [ 22 ];
      interfaces.tailscale0.allowedTCPPorts = lib.mkDefault [ 22 ];
    };
  };

  services.openssh = {
    enable = true;
    openFirewall = false;
    settings = {
      PasswordAuthentication = false;
      PermitRootLogin = "prohibit-password";
    };
  };

  services.tailscale.useRoutingFeatures = "client";

  boot.loader.grub = {
    enable = true;
    device = lib.mkDefault "/dev/sda";
    efiSupport = true;
    efiInstallAsRemovable = true;
  };

  boot.kernelParams = [ "net.ifnames=0" ];

  age.identityPaths = [ "/etc/age/key.txt" ];

  documentation.man.cache.enable = false;
}
