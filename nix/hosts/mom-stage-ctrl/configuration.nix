{
  config,
  agentmom,
  pkgs,
  inputs,
  ...
}: {
  imports = [
    ../common/hetzner-cloud-vps.nix
    ./disk-config.nix
    agentmom.nixosModules.agentmom
  ];

  networking.hostName = "mom-stage-ctrl";

  justinsConfig = {
    githubSsh.enable = false;
    tailscaleAuthKey.enable = false;
  };

  networking.firewall = {
    allowedTCPPorts = [22 80 443];
    extraCommands = ''
      iptables -A nixos-fw -p tcp -s 77.42.80.210 --dport 8080 -j nixos-fw-accept
    '';
    interfaces.tailscale0.allowedTCPPorts = [22 8080];
  };

  services.caddy = {
    enable = true;
    virtualHosts."stage.agentmom.xyz".extraConfig = ''
      @stageTunnel path_regexp stageTunnel ^/tunnels/mom-stage-1/(41[0-9]{3})(/.*)?$
      handle @stageTunnel {
        basic_auth {
          justin $2a$14$QDfbgZiy1V7A3lIw7C0.huP8tXfFtrLWJbmbhIGNOUspOWarRsXz2
        }
        rewrite * {re.stageTunnel.2}
        reverse_proxy 135.181.179.143:{re.stageTunnel.1}
      }

      @workerHosts {
        path /worker/*
        remote_ip 135.181.179.143
      }

      handle @workerHosts {
        reverse_proxy 127.0.0.1:8080
      }

      handle /worker/* {
        respond "forbidden" 403
      }

      handle {
        reverse_proxy 127.0.0.1:8080
      }
    '';
  };

  services.agentmom = {
    enable = true;
    package = agentmom.packages.${pkgs.stdenv.hostPlatform.system}.mom;
    stateDir = "/var/lib/agentmom";
    nodeId = "mom-stage-ctrl";
    logFormat = "json";
    auth = {
      secretFile = config.age.secrets.agentmom-auth-secret.path;
    };
    workerNodeTokenFiles = {
      mom-stage-1 = config.age.secrets.agentmom-worker-token-mom-stage-1.path;
    };
    workerUrlAllowlist = [
      "http://135.181.179.143:9090"
    ];
    api = {
      enable = true;
      bind = "0.0.0.0:8080";
    };
    monitorCheck = {
      enable = true;
      minReadyNodes = 1;
      maxStaleNodes = 0;
      maxQueuedAgeSeconds = 300;
      failedJobLookbackSeconds = 900;
      maxRecentFailedJobs = 0;
      maxBackupAgeSeconds = 0;
      maxStaleScheduledBackups = 0;
      maxRecentBackupFailures = 0;
    };
    worker.enable = false;
    credentialProxy.enable = false;
  };

  systemd.tmpfiles.rules = [
    "Z /var/lib/agentmom 0750 agentmom agentmom - -"
  ];

  age.secrets.agentmom-auth-secret = {
    file = ../../secrets/agentmom-auth-secret.age;
    owner = "agentmom";
    group = "agentmom";
    mode = "0400";
  };
  age.secrets.agentmom-bootstrap-admin-code = {
    file = ../../secrets/agentmom-bootstrap-admin-code.age;
    owner = "agentmom";
    group = "agentmom";
    mode = "0400";
  };
  age.secrets.agentmom-worker-token-mom-stage-1 = {
    file = ../../secrets/agentmom-worker-token-mom-stage-1.age;
    owner = "agentmom";
    group = "agentmom";
    mode = "0400";
  };

  system.stateVersion = "25.05";
}
