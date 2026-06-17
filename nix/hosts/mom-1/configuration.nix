{ config, lib, pkgs, modulesPath, agentmom, inputs, ... }:

{
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    ../common/cachix-justinmoon.nix
    ../common/server-base.nix
    ../common/nixbuild-net.nix
    ./disk-config.nix
    agentmom.nixosModules.agentmom
  ];

  networking.hostName = "mom-1";
  networking.useDHCP = true;
  networking.dhcpcd.extraConfig = "nohook hostname";
  networking.nameservers = [ "1.1.1.1" "8.8.8.8" ];
  networking.firewall.enable = true;
  networking.firewall.allowPing = true;
  networking.firewall.allowedTCPPorts = [ 80 443 ];
  networking.firewall.interfaces.tailscale0 = {
    allowedTCPPorts = [ 22 9090 ];
    allowedTCPPortRanges = [
      {
        from = 41000;
        to = 41999;
      }
    ];
  };

  justinsConfig.githubSsh.enable = false;
  justinsConfig.tailscaleAuthKey.enable = false;

  boot.loader.grub = {
    enable = true;
    device = "/dev/nvme1n1";
    devices = lib.mkForce [ "/dev/nvme1n1" ];
    efiSupport = true;
    efiInstallAsRemovable = true;
  };

  boot.kernelParams = [ "net.ifnames=0" ];

  services.openssh = {
    enable = true;
    openFirewall = false;
    settings = {
      PasswordAuthentication = false;
      PermitRootLogin = "prohibit-password";
    };
  };
  services.dbus.implementation = "dbus";

  services.caddy.enable = false;

  users.users.justin = {
    extraGroups = [ "kvm" ];
    openssh.authorizedKeys.keys = lib.mkAfter [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHkHMm0PowS9PCnC7Ci8opBPYUVOTTIlWXqXn42uR3t8 kimchiramen2026@gmail.com"
    ];
  };

  services.agentmom = {
    enable = true;
    package = agentmom.packages.${pkgs.stdenv.hostPlatform.system}.mom;
    stateDir = "/var/lib/agentmom";
    nodeId = "mom-1";
    logFormat = "json";
    workerTokenFile = config.age.secrets.agentmom-worker-token-mom-1.path;
    microvm = {
      enable = true;
      stateDir = "/var/lib/agentmom/microvms";
      workspaceDir = "/var/lib/agentmom/microvms/workspaces";
      bridge = "agentmom0";
      cidr = "192.168.83.0/24";
      hostAddress = "192.168.83.1";
      externalInterface = "eth0";
      kvmKernelModule = "kvm-amd";
    };
    guest = {
      hermesProfile = "main";
      model = "openai/gpt-5.5";
    };
    credentialProxy = {
      enable = true;
      package = agentmom.packages.${pkgs.stdenv.hostPlatform.system}.iron-proxy;
      stateDir = "/var/lib/agentmom/iron-proxy";
      openrouterApiKeyFile = config.age.secrets.agentmom-openrouter-api-key.path;
    };
    capacity = {
      cpus = 32;
      memoryMib = 131072;
      activeWorkspaces = 48;
      diskReserveMib = 102400;
    };
    api = {
      enable = false;
      bind = "0.0.0.0:8080";
    };
    worker = {
      enable = true;
      apiUrl = "https://agentmom.xyz";
      bind = "0.0.0.0:9090";
      url = "http://100.81.250.67:9090";
      intervalSeconds = 5;
      serviceTunnelBindHost = "0.0.0.0";
      serviceTunnelBaseUrl = "https://agentmom.xyz/tunnels/mom-1/{port}/";
      serviceTunnelPortRange = {
        from = 41000;
        to = 41999;
      };
      ensureRuntime = false;
      openFirewall = true;
      firewallInterface = "tailscale0";
      resticEnvFile = config.age.secrets.agentmom-restic-env.path;
    };
  };

  environment.systemPackages = [
    config.services.agentmom.package
  ];

  systemd.tmpfiles.rules = [
    "d /var/lib/agentmom/secrets 0700 agentmom agentmom - -"
  ];
  systemd.services.agentmom-credential-proxy.serviceConfig.WorkingDirectory = lib.mkForce "/";

  age.identityPaths = [ "/etc/age/key.txt" ];
  age.secrets.agentmom-openrouter-api-key = {
    file = ../../secrets/openrouter-api-key.age;
    owner = "root";
    group = "root";
    mode = "0400";
  };
  age.secrets.agentmom-worker-token-mom-1 = {
    file = ../../secrets/agentmom-worker-token-mom-1.age;
    owner = "agentmom";
    group = "agentmom";
    mode = "0400";
  };
  age.secrets.agentmom-restic-env = {
    file = ../../secrets/agentmom-restic-env.age;
    owner = "agentmom";
    group = "agentmom";
    mode = "0400";
  };

  documentation.man.cache.enable = false;

  system.stateVersion = "25.05";
}
