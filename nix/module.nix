{ config, lib, pkgs, ... }:

let
  cfg = config.services.agentmom;
  yaml = pkgs.formats.yaml { };
  credentialProxyConfig = yaml.generate "agentmom-iron-proxy.yaml" {
    dns = {
      listen = cfg.credentialProxy.dnsListen;
      proxy_ip = "127.0.0.1";
    };
    proxy = {
      http_listen = cfg.credentialProxy.httpListen;
      https_listen = cfg.credentialProxy.httpsListen;
      tunnel_listen = cfg.credentialProxy.tunnelListen;
      upstream_response_header_timeout = cfg.credentialProxy.upstreamResponseHeaderTimeout;
    };
    tls = {
      mode = "mitm";
      ca_cert = cfg.credentialProxy.caCert;
      ca_key = cfg.credentialProxy.caKey;
    };
    metrics = {
      listen = cfg.credentialProxy.metricsListen;
    };
    transforms = [
      {
        name = "allowlist";
        config = {
          domains = cfg.credentialProxy.allowedDomains;
        } // lib.optionalAttrs cfg.credentialProxy.warnOnly {
          warn = true;
        };
      }
      {
        name = "secrets";
        config = {
          secrets =
            lib.optionals (cfg.credentialProxy.openaiApiKeyFile != null) [
              {
                source = {
                  type = "file";
                  path = toString cfg.credentialProxy.openaiApiKeyFile;
                };
                inject = {
                  header = "Authorization";
                  formatter = "Bearer {{ .Value }}";
                };
                rules = [
                  {
                    host = "api.openai.com";
                    paths = [ "/v1/*" ];
                  }
                ];
              }
            ]
            ++ lib.optionals (cfg.credentialProxy.openrouterApiKeyFile != null) [
              {
                source = {
                  type = "file";
                  path = toString cfg.credentialProxy.openrouterApiKeyFile;
                };
                inject = {
                  header = "Authorization";
                  formatter = "Bearer {{ .Value }}";
                };
                rules = [
                  {
                    host = "openrouter.ai";
                    paths = [ "/api/v1/*" ];
                  }
                ];
              }
            ];
        };
      }
    ];
    log = {
      level = cfg.credentialProxy.logLevel;
    };
  };
  commonEnvironment = {
    MOM_STATE_DIR = cfg.stateDir;
    MSB_HOME = cfg.microsandboxHome;
    MOM_NODE_ID = cfg.nodeId;
    MOM_LOG_FORMAT = cfg.logFormat;
    MOM_CAPACITY_CPUS = toString cfg.capacity.cpus;
    MOM_CAPACITY_MEMORY_MIB = toString cfg.capacity.memoryMib;
    MOM_CAPACITY_ACTIVE_WORKSPACES = toString cfg.capacity.activeWorkspaces;
    MOM_CAPACITY_DISK_RESERVE_MIB = toString cfg.capacity.diskReserveMib;
  }
  // lib.optionalAttrs (cfg.configFile != null) {
    MOM_CONFIG = toString cfg.configFile;
  }
  // lib.optionalAttrs (cfg.microsandboxPackage != null) {
    MSB_PATH = "${cfg.microsandboxPackage}/bin/msb";
  }
  // lib.optionalAttrs (cfg.workerTokenFile != null) {
    MOM_WORKER_TOKEN_FILE = toString cfg.workerTokenFile;
  };

  commonPath = with pkgs; [
    bash
    restic
  ] ++ lib.optional (cfg.microsandboxPackage != null) cfg.microsandboxPackage;
in
{
  options.services.agentmom = {
    enable = lib.mkEnableOption "Agent Mom microsandbox workspace worker";

    package = lib.mkOption {
      type = lib.types.package;
      description = "Package that provides the mom binary.";
    };

    microsandboxPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = null;
      description = "Package that provides the msb binary and libkrunfw runtime bundle.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "agentmom";
      description = "User that runs the Agent Mom worker.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "agentmom";
      description = "Group that runs the Agent Mom worker.";
    };

    createUser = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Whether to create the Agent Mom service user and group.";
    };

    stateDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/agentmom";
      description = "Directory for Agent Mom's SQLite catalog and local backup fallback.";
    };

    microsandboxHome = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/agentmom/microsandbox";
      description = "MSB_HOME used by microsandbox on this worker.";
    };

    nodeId = lib.mkOption {
      type = lib.types.str;
      default = config.networking.hostName or "agentmom-node";
      description = "Stable node identifier recorded in logs and workspace events.";
    };

    logFormat = lib.mkOption {
      type = lib.types.enum [ "text" "json" ];
      default = "json";
      description = "Agent Mom log format.";
    };

    configFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Agent Mom config.json containing Codex/Hermes seed configuration.";
    };

    intervalSeconds = lib.mkOption {
      type = lib.types.ints.positive;
      default = 30;
      description = "Scheduler and backup loop interval.";
    };

    capacity = {
      cpus = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = "Advertised CPU capacity. 0 means informational only.";
      };

      memoryMib = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = "Advertised memory capacity in MiB. 0 means informational only.";
      };

      activeWorkspaces = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = "Maximum active workspaces before the worker refuses new claims. 0 disables this limit.";
      };

      diskReserveMib = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 10240;
        description = "Minimum free disk in MiB to reserve before claiming more work.";
      };
    };

    api = {
      enable = lib.mkEnableOption "Agent Mom central API service";

      bind = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1:8080";
        description = "HTTP bind address for mom api.";
      };
    };

    worker = {
      enable = lib.mkEnableOption "Agent Mom central-API worker service";

      apiUrl = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Central Agent Mom API URL used by mom worker.";
      };

      intervalSeconds = lib.mkOption {
        type = lib.types.ints.positive;
        default = 5;
        description = "Fallback polling interval for mom worker.";
      };
    };

    ui = {
      enable = lib.mkEnableOption "Agent Mom web UI service";

      bind = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1:8787";
        description = "HTTP bind address for mom-ui.";
      };

      apiUrl = lib.mkOption {
        type = lib.types.str;
        default = "http://127.0.0.1:8080";
        description = "Agent Mom API URL used by mom-ui.";
      };
    };

    credentialProxy = {
      enable = lib.mkEnableOption "iron-proxy credential injection for Agent Mom sandboxes";

      package = lib.mkOption {
        type = lib.types.nullOr lib.types.package;
        default = null;
        description = "Package that provides the iron-proxy binary.";
      };

      stateDir = lib.mkOption {
        type = lib.types.str;
        default = "${cfg.stateDir}/iron-proxy";
        description = "Directory for the iron-proxy CA material.";
      };

      tunnelListen = lib.mkOption {
        type = lib.types.str;
        default = "0.0.0.0:1080";
        description = "CONNECT/SOCKS5 listener used by sandbox HTTP(S)_PROXY settings.";
      };

      httpListen = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1:18080";
        description = "HTTP MITM listener for redirected traffic.";
      };

      httpsListen = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1:18443";
        description = "HTTPS MITM listener for redirected traffic.";
      };

      dnsListen = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1:18053";
        description = "DNS listener. Agent Mom currently uses explicit proxy settings, so this stays local.";
      };

      guestProxyUrl = lib.mkOption {
        type = lib.types.str;
        default = "http://192.168.83.1:1080";
        description = "Proxy URL written into Agent Mom guest configuration.";
      };

      caCert = lib.mkOption {
        type = lib.types.str;
        default = "${cfg.credentialProxy.stateDir}/ca.crt";
        description = "CA certificate path trusted by sandboxes.";
      };

      caKey = lib.mkOption {
        type = lib.types.str;
        default = "${cfg.credentialProxy.stateDir}/ca.key";
        description = "CA private key path used by iron-proxy.";
      };

      openaiApiKeyFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "File containing the OpenAI API key injected for api.openai.com.";
      };

      openrouterApiKeyFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "File containing the OpenRouter API key injected for openrouter.ai.";
      };

      allowedDomains = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [
          "api.openai.com"
          "openrouter.ai"
          "github.com"
          "api.github.com"
          "raw.githubusercontent.com"
          "*.githubusercontent.com"
          "objects.githubusercontent.com"
          "registry.npmjs.org"
          "nodejs.org"
          "pypi.org"
          "files.pythonhosted.org"
          "astral.sh"
        ];
        description = "Domains allowed through iron-proxy.";
      };

      warnOnly = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Log allowlist misses without blocking them.";
      };

      logLevel = lib.mkOption {
        type = lib.types.enum [ "debug" "info" "warn" "error" ];
        default = "info";
        description = "iron-proxy log level.";
      };

      metricsListen = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1:19090";
        description = "Prometheus metrics listener for iron-proxy.";
      };

      upstreamResponseHeaderTimeout = lib.mkOption {
        type = lib.types.str;
        default = "5m";
        description = "Maximum time iron-proxy waits for upstream response headers.";
      };
    };

    workerTokenFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Optional file containing the bearer token used by API worker endpoints.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.groups = lib.mkIf cfg.createUser {
      ${cfg.group} = { };
    };
    users.users = lib.mkIf cfg.createUser {
      ${cfg.user} = {
        isSystemUser = true;
        group = cfg.group;
        home = cfg.stateDir;
        createHome = true;
        extraGroups = [ "kvm" ];
      };
    };

    systemd.tmpfiles.rules = [
      "d ${cfg.stateDir} 0750 ${cfg.user} ${cfg.group} - -"
      "d ${cfg.microsandboxHome} 0750 ${cfg.user} ${cfg.group} - -"
    ] ++ lib.optionals cfg.credentialProxy.enable [
      "d ${cfg.credentialProxy.stateDir} 0755 root root - -"
    ];

    assertions = [
      {
        assertion = !cfg.worker.enable || cfg.worker.apiUrl != null;
        message = "services.agentmom.worker.apiUrl is required when services.agentmom.worker.enable is true.";
      }
      {
        assertion = !cfg.ui.enable || cfg.ui.apiUrl != null || (!cfg.api.enable && !cfg.worker.enable);
        message = "services.agentmom.ui.apiUrl is required when services.agentmom.ui.enable is true with API/worker mode.";
      }
      {
        assertion = !cfg.credentialProxy.enable || cfg.credentialProxy.package != null;
        message = "services.agentmom.credentialProxy.package is required when credentialProxy.enable is true.";
      }
    ];

    systemd.services.agentmom = lib.mkIf (!cfg.api.enable && !cfg.worker.enable) {
      description = "Agent Mom microsandbox workspace worker";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      path = commonPath;
      environment = commonEnvironment;
      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        ExecStart = "${cfg.package}/bin/mom daemon --interval ${toString cfg.intervalSeconds}";
        Restart = "always";
        RestartSec = "10s";
        WorkingDirectory = cfg.stateDir;
      };
    };

    systemd.services.agentmom-api = lib.mkIf cfg.api.enable {
      description = "Agent Mom central API";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      path = commonPath;
      environment = commonEnvironment;
      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        ExecStart = "${cfg.package}/bin/mom api --bind ${cfg.api.bind}";
        Restart = "always";
        RestartSec = "5s";
        WorkingDirectory = cfg.stateDir;
      };
    };

    systemd.services.agentmom-worker = lib.mkIf cfg.worker.enable {
      description = "Agent Mom central API worker";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      path = commonPath;
      environment = commonEnvironment // {
        MOM_API_URL = cfg.worker.apiUrl;
      };
      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        ExecStart = "${cfg.package}/bin/mom worker --interval ${toString cfg.worker.intervalSeconds}";
        Restart = "always";
        RestartSec = "5s";
        WorkingDirectory = cfg.stateDir;
      };
    };

    systemd.services.agentmom-ui = lib.mkIf cfg.ui.enable {
      description = "Agent Mom web UI";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" "agentmom-api.service" ];
      wants = [ "network-online.target" ];
      path = commonPath;
      environment = commonEnvironment
        // {
          MOM_UI_BIND = cfg.ui.bind;
          MOM_API_URL = cfg.ui.apiUrl;
        };
      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        ExecStart = "${cfg.package}/bin/mom-ui";
        Restart = "always";
        RestartSec = "5s";
        WorkingDirectory = cfg.stateDir;
      };
    };

    systemd.services.agentmom-credential-proxy = lib.mkIf cfg.credentialProxy.enable {
      description = "Agent Mom credential egress proxy";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      path = [ pkgs.openssl ];
      serviceConfig = {
        Type = "simple";
        User = "root";
        Group = "root";
        ExecStartPre = pkgs.writeShellScript "agentmom-credential-proxy-prestart" ''
          set -eu
          install -d -m 0755 -o root -g root ${lib.escapeShellArg cfg.credentialProxy.stateDir}
          if [ ! -s ${lib.escapeShellArg cfg.credentialProxy.caCert} ] || [ ! -s ${lib.escapeShellArg cfg.credentialProxy.caKey} ]; then
            tmpdir="$(mktemp -d ${lib.escapeShellArg cfg.credentialProxy.stateDir}/ca.XXXXXX)"
            openssl genrsa -out "$tmpdir/ca.key" 4096
            openssl req -x509 -new -nodes \
              -key "$tmpdir/ca.key" \
              -sha256 -days 3650 \
              -subj "/CN=agentmom iron-proxy CA" \
              -addext "basicConstraints=critical,CA:TRUE" \
              -addext "keyUsage=critical,keyCertSign" \
              -out "$tmpdir/ca.crt"
            install -m 0400 -o root -g root "$tmpdir/ca.key" ${lib.escapeShellArg cfg.credentialProxy.caKey}
            install -m 0444 -o root -g root "$tmpdir/ca.crt" ${lib.escapeShellArg cfg.credentialProxy.caCert}
            rm -rf "$tmpdir"
          fi
        '';
        ExecStart = "${cfg.credentialProxy.package}/bin/iron-proxy -config ${credentialProxyConfig}";
        Restart = "always";
        RestartSec = "5s";
        WorkingDirectory = cfg.credentialProxy.stateDir;
      };
    };
  };
}
