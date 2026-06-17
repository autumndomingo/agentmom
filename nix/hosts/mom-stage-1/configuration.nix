{
  config,
  lib,
  pkgs,
  modulesPath,
  agentmom,
  inputs,
  ...
}: let
  sshKeys = import ./ssh-keys.nix;
  userAuthorizedKeys = sshKeys.userAuthorizedKeys;
  rootAuthorizedKeys = sshKeys.rootAuthorizedKeys;
in {
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    ../common/cachix-justinmoon.nix
    ../common/headless-tools.nix
    ../common/nixbuild-net.nix
    ../common/tailscale.nix
    ./disk-config.nix
    agentmom.nixosModules.agentmom
  ];

  i18n.defaultLocale = "en_US.UTF-8";
  i18n.extraLocaleSettings = {
    LC_ADDRESS = "en_US.UTF-8";
    LC_IDENTIFICATION = "en_US.UTF-8";
    LC_MEASUREMENT = "en_US.UTF-8";
    LC_MONETARY = "en_US.UTF-8";
    LC_NAME = "en_US.UTF-8";
    LC_NUMERIC = "en_US.UTF-8";
    LC_PAPER = "en_US.UTF-8";
    LC_TELEPHONE = "en_US.UTF-8";
    LC_TIME = "en_US.UTF-8";
  };

  time.timeZone = "America/Los_Angeles";

  nix = {
    settings = {
      experimental-features = ["nix-command" "flakes"];
      auto-optimise-store = true;
      trusted-users = ["root" "justin"];
    };
    gc = {
      automatic = true;
      dates = "weekly";
      options = "--delete-older-than 30d";
    };
  };

  nixpkgs.config.allowUnfree = true;

  boot.loader.grub = {
    enable = true;
    # mom-stage-1 boots BIOS GRUB from this disk. The other NVMe is data-only
    # and has no BIOS boot partition, so installing GRUB there makes
    # `nixos-rebuild switch` fail and leaves deploys non-persistent.
    device = "/dev/disk/by-id/nvme-eui.00000000000000018ce38e05001f1973";
    devices = lib.mkForce ["/dev/disk/by-id/nvme-eui.00000000000000018ce38e05001f1973"];
    efiSupport = false;
  };

  boot.kernelParams = ["net.ifnames=0"];

  networking = {
    hostName = "mom-stage-1";
    useDHCP = true;
    firewall = {
      enable = true;
      allowPing = true;
      allowedTCPPorts = [];
      extraCommands = ''
        iptables -A nixos-fw -p tcp -s 204.168.131.33 --dport 9090 -j nixos-fw-accept
        iptables -A nixos-fw -p tcp -s 204.168.131.33 --dport 41000:41999 -j nixos-fw-accept
      '';
      interfaces.tailscale0 = {
        allowedTCPPorts = [22 9090];
        allowedTCPPortRanges = [
          {
            from = 41000;
            to = 41999;
          }
        ];
      };
      # nostr-vpn tun interface — allow SSH over the VPN tunnel
      interfaces.utun100.allowedTCPPorts = [22];
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

  justinsConfig.tailscaleAuthKey.enable = true;
  services.tailscale.useRoutingFeatures = "client";

  users.users.justin = {
    isNormalUser = true;
    description = "justin";
    extraGroups = ["wheel" "kvm"];
    openssh.authorizedKeys.keys = userAuthorizedKeys;
  };

  users.users.root.openssh.authorizedKeys.keys = rootAuthorizedKeys;

  security.sudo.wheelNeedsPassword = false;

  services.agentmom = {
    enable = true;
    package = agentmom.packages.${pkgs.stdenv.hostPlatform.system}.mom;
    stateDir = "/data/agentmom";
    nodeId = "mom-stage-1";
    logFormat = "json";
    workerTokenFile = config.age.secrets.agentmom-worker-token-mom-stage-1.path;
    microvm = {
      enable = true;
      stateDir = "/data/agentmom/microvms";
      workspaceDir = "/data/agentmom/microvms/workspaces";
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
      stateDir = "/data/agentmom/iron-proxy";
      openrouterApiKeyFile = config.age.secrets.agentmom-openrouter-api-key.path;
    };
    capacity = {
      cpus = 8;
      memoryMib = 65536;
      activeWorkspaces = 24;
      diskReserveMib = 102400;
    };
    api.enable = false;
    worker = {
      enable = true;
      apiUrl = "https://stage.agentmom.xyz";
      bind = "0.0.0.0:9090";
      url = "http://135.181.179.143:9090";
      intervalSeconds = 5;
      serviceTunnelBindHost = "0.0.0.0";
      serviceTunnelBaseUrl = "https://stage.agentmom.xyz/tunnels/mom-stage-1/{port}/";
      serviceTunnelPortRange = {
        from = 41000;
        to = 41999;
      };
      ensureRuntime = false;
      openFirewall = true;
      firewallInterface = "tailscale0";
    };
  };

  environment.systemPackages = [
    config.services.agentmom.package
  ];

  age.identityPaths = ["/etc/age/key.txt"];
  systemd.services.agentmom-worker.environment.MOM_ENABLE_TEST_ENDPOINTS = "1";
  age.secrets.agentmom-openrouter-api-key = {
    file = ../../secrets/openrouter-api-key.age;
    owner = "root";
    group = "root";
    mode = "0400";
  };
  age.secrets.agentmom-worker-token-mom-stage-1 = {
    file = ../../secrets/agentmom-worker-token-mom-stage-1.age;
    owner = "agentmom";
    group = "agentmom";
    mode = "0400";
  };
  systemd.services.agentmom-credential-proxy.serviceConfig.WorkingDirectory = lib.mkForce "/";

  documentation.man.cache.enable = false;

  system.stateVersion = "25.05";
}
