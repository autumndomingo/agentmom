{
  config,
  agentgranny2,
  agentmom,
  pkgs,
  inputs,
  ...
}: {
  imports = [
    ../common/hetzner-cloud-vps.nix
    ./disk-config.nix
    agentgranny2.nixosModules.agentgranny2
    agentmom.nixosModules.agentmom
  ];

  networking.hostName = "mom-ctrl";

  justinsConfig = {
    githubSsh.enable = false;
    tailscaleAuthKey.enable = false;
  };

  networking.firewall = {
    allowedTCPPorts = [22 80 443];
    interfaces.tailscale0.allowedTCPPorts = [22 8080];
  };

  services.caddy = {
    enable = true;
    virtualHosts."granny.agentmom.xyz".extraConfig = ''
      basic_auth {
        justin $2a$14$QDfbgZiy1V7A3lIw7C0.huP8tXfFtrLWJbmbhIGNOUspOWarRsXz2
      }
      reverse_proxy 127.0.0.1:7392
    '';
    virtualHosts."agentmom.xyz".extraConfig = ''
      @mom1Tunnel path_regexp mom1Tunnel ^/tunnels/mom-1/(41[0-9]{3})(/.*)?$
      handle @mom1Tunnel {
        basic_auth {
          justin $2a$14$QDfbgZiy1V7A3lIw7C0.huP8tXfFtrLWJbmbhIGNOUspOWarRsXz2
        }
        rewrite * {re.mom1Tunnel.2}
        reverse_proxy 100.81.250.67:{re.mom1Tunnel.1}
      }

      @workerHosts {
        path /worker/*
        remote_ip 65.108.234.158
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
    virtualHosts."stage.agentmom.xyz".extraConfig = ''
      @stageTunnel path_regexp stageTunnel ^/tunnels/mom-stage-1/(41[0-9]{3})(/.*)?$
      handle @stageTunnel {
        basic_auth {
          justin $2a$14$QDfbgZiy1V7A3lIw7C0.huP8tXfFtrLWJbmbhIGNOUspOWarRsXz2
        }
        rewrite * {re.stageTunnel.2}
        reverse_proxy 135.181.179.143:{re.stageTunnel.1}
      }

      @stageWorkerHosts {
        path /worker/*
        remote_ip 135.181.179.143
      }

      handle @stageWorkerHosts {
        reverse_proxy 204.168.131.33:8080
      }

      handle /worker/* {
        respond "forbidden" 403
      }

      handle {
        reverse_proxy 204.168.131.33:8080
      }
    '';
  };

  services.agentmom = {
    enable = true;
    package = agentmom.packages.${pkgs.stdenv.hostPlatform.system}.mom;
    stateDir = "/var/lib/agentmom";
    nodeId = "mom-ctrl";
    logFormat = "json";
    auth = {
      secretFile = config.age.secrets.agentmom-auth-secret.path;
    };
    workerNodeTokenFiles = {
      mom-1 = config.age.secrets.agentmom-worker-token-mom-1.path;
    };
    workerUrlAllowlist = [
      "http://100.81.250.67:9090"
    ];
    api = {
      enable = true;
      bind = "127.0.0.1:8080";
    };
    catalogBackup = {
      enable = true;
      onCalendar = "*:0/15";
      resticEnvFile = config.age.secrets.agentmom-restic-env.path;
    };
    monitorCheck = {
      enable = true;
      minReadyNodes = 1;
      maxStaleNodes = 0;
      maxQueuedAgeSeconds = 300;
      failedJobLookbackSeconds = 900;
      maxRecentFailedJobs = 0;
      maxBackupAgeSeconds = 86400;
      maxStaleScheduledBackups = 0;
      maxRecentBackupFailures = 0;
    };
    worker.enable = false;
    credentialProxy.enable = false;
  };

  services.agentgranny2 = {
    enable = true;
    package = agentgranny2.packages.${pkgs.stdenv.hostPlatform.system}.agentgranny2;
    stateDir = "/var/lib/agentgranny2";
    workspaceDir = "/var/lib/agentgranny2/workspace";
    openRouterKeyFile = config.age.secrets.agentgranny2-openrouter-api-key.path;
    smolvm.package = agentgranny2.packages.${pkgs.stdenv.hostPlatform.system}.smolvm;
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
  age.secrets.agentgranny2-openrouter-api-key = {
    file = ../../secrets/openrouter-api-key.age;
    owner = "agentgranny2";
    group = "agentgranny2";
    mode = "0400";
  };

  system.stateVersion = "25.05";
}
