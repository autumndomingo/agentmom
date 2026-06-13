{ config, lib, pkgs, ... }:

let
  cfg = config.services.agentmom;
  yaml = pkgs.formats.yaml { };
  json = pkgs.formats.json { };
  generatedConfigFile = json.generate "agentmom-config.json" {
    schema_version = 1;
    runtime = {
      snapshot_name = cfg.runtime.snapshotName;
    };
    credentials = {
      mode = cfg.credentials.mode;
      codex_auth_path = cfg.credentials.codexAuthPath;
      opencode_auth_path = cfg.credentials.opencodeAuthPath;
      proxy_url =
        if cfg.credentials.proxyUrl != null then cfg.credentials.proxyUrl
        else if cfg.credentialProxy.enable then cfg.credentialProxy.guestProxyUrl
        else null;
      proxy_ca_path =
        if cfg.credentials.proxyCaPath != null then cfg.credentials.proxyCaPath
        else if cfg.credentialProxy.enable then cfg.credentialProxy.caCert
        else null;
    };
    guest = {
      hermes_profile = cfg.guest.hermesProfile;
      model = cfg.guest.model;
    };
    auth = {
      secret_file = cfg.auth.secretFile;
    };
    features = {
      opencode = cfg.features.opencode;
    };
  };
  effectiveConfigFile =
    if cfg.configFile != null then cfg.configFile else generatedConfigFile;
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
  // {
    MOM_CONFIG = toString effectiveConfigFile;
  }
  // lib.optionalAttrs (cfg.microsandboxPackage != null) {
    MSB_PATH = "${cfg.microsandboxPackage}/bin/msb";
  }
  // lib.optionalAttrs (cfg.workerTokenFile != null) {
    MOM_WORKER_TOKEN_FILE = cfg.workerTokenFile;
  }
  // lib.optionalAttrs (cfg.workerUrlAllowlist != [ ]) {
    MOM_WORKER_URL_ALLOWLIST = lib.concatStringsSep "," cfg.workerUrlAllowlist;
  };

  commonPath = with pkgs; [
    bash
    coreutils
    curl
    openssh
    restic
  ] ++ lib.optional (cfg.microsandboxPackage != null) cfg.microsandboxPackage;
  tmpfilesReadyUnits = [
    "systemd-tmpfiles-setup.service"
    "systemd-tmpfiles-resetup.service"
  ];
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
      description = "Optional externally managed Agent Mom config.json. When unset, the module generates a structured non-secret config.";
    };

    runtime = {
      snapshotName = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Required microsandbox base snapshot name used for new workspaces.";
      };
    };

    credentials = {
      mode = lib.mkOption {
        type = lib.types.enum [ "vm-auth-json" "openrouter-proxy" ];
        default = "vm-auth-json";
        description = "Credential strategy used when configuring guest sandboxes.";
      };

      codexAuthPath = lib.mkOption {
        type = lib.types.str;
        default = "~/.codex/auth.json";
        description = "Host path copied into guests when credentials.mode is vm-auth-json.";
      };

      opencodeAuthPath = lib.mkOption {
        type = lib.types.str;
        default = "~/.local/share/opencode/auth.json";
        description = "Host path used to seed OpenCode auth when credentials.mode is vm-auth-json.";
      };

      proxyUrl = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Proxy URL written into guest environments when credentials.mode is openrouter-proxy.";
      };

      proxyCaPath = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "CA certificate path trusted by guests when credentials.mode is openrouter-proxy.";
      };
    };

    guest = {
      hermesProfile = lib.mkOption {
        type = lib.types.str;
        default = "main";
        description = "Hermes profile name created inside guest sandboxes.";
      };

      model = lib.mkOption {
        type = lib.types.str;
        default = "gpt-5.5";
        description = "Default model written into guest Hermes, Codex, and OpenCode config.";
      };
    };

    auth = {
      secretFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Runtime file containing the Agent Mom browser-session and invite HMAC secret.";
      };
    };

    features = {
      opencode = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Expose OpenCode service launch controls in the browser UI and API.";
      };
    };

    catalogBackup = {
      enable = lib.mkEnableOption "scheduled Agent Mom SQLite catalog backups";

      outputDir = lib.mkOption {
        type = lib.types.str;
        default = "${cfg.stateDir}/catalog-backups";
        description = "Directory for SQLite catalog backup files created by mom db backup.";
      };

      onCalendar = lib.mkOption {
        type = lib.types.str;
        default = "*:0/15";
        description = "systemd OnCalendar expression for catalog backups.";
      };

      randomizedDelaySec = lib.mkOption {
        type = lib.types.str;
        default = "2m";
        description = "Randomized delay for the catalog backup timer.";
      };

      persistent = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Whether missed catalog backup timer runs should execute after boot.";
      };

      resticEnvFile = lib.mkOption {
        type = lib.types.nullOr lib.types.path;
        default = null;
        description = "Optional EnvironmentFile containing RESTIC_* credentials used to upload catalog backups off-host.";
      };
    };

    monitorCheck = {
      enable = lib.mkEnableOption "scheduled lightweight Agent Mom health checks";

      apiUrl = lib.mkOption {
        type = lib.types.str;
        default = "http://127.0.0.1:8080";
        description = "Agent Mom API URL checked through /health/ready.";
      };

      onCalendar = lib.mkOption {
        type = lib.types.str;
        default = "*:0/1";
        description = "systemd OnCalendar expression for monitor checks.";
      };

      randomizedDelaySec = lib.mkOption {
        type = lib.types.str;
        default = "10s";
        description = "Randomized delay for the monitor check timer.";
      };

      minReadyNodes = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 1;
        description = "Minimum fresh ready nodes required before the monitor check fails.";
      };

      maxStaleNodes = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = "Maximum stale node count allowed before the monitor check fails.";
      };

      maxQueuedAgeSeconds = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 300;
        description = "Maximum age of the oldest queued job before the monitor check fails.";
      };

      failedJobLookbackSeconds = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 900;
        description = "Lookback window for recent failed job alerting.";
      };

      maxRecentFailedJobs = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = "Maximum failed jobs allowed in the lookback window before the monitor check fails.";
      };

      maxBackupAgeSeconds = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = "Maximum backup age for workspaces with scheduled backups. 0 disables the check.";
      };

      maxStaleScheduledBackups = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = "Maximum scheduled-backup workspaces older than maxBackupAgeSeconds before the monitor check fails.";
      };

      maxRecentBackupFailures = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = "Maximum backup failure events allowed in the failed-job lookback window before the monitor check fails.";
      };

      onFailureUnits = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "Optional systemd units triggered by OnFailure when monitor checks fail.";
      };
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

      bind = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1:9090";
        description = "Private HTTP bind address for worker-local control endpoints.";
      };

      url = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "URL the central API should use to reach this worker. Set to a Tailscale/private URL for multi-host deployments.";
      };

      serviceTunnelBindHost = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1";
        description = "Host address used for Hermes/OpenCode service tunnels created by this worker.";
      };

      serviceTunnelBaseUrl = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Browser-visible base URL for worker service tunnels, without the port.";
      };

      resticEnvFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Optional runtime environment file with RESTIC_* and S3-compatible credentials for workspace backups.";
      };

      ensureBaseSnapshot = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Run mom node ensure-base before starting the worker so deploys fail unless the required versioned base snapshot exists and passes doctor.";
      };
    };

    ui = {
      enable = lib.mkEnableOption "serving the Agent Mom web UI from agentmom-api";

      bind = lib.mkOption {
        type = lib.types.str;
        default = "127.0.0.1:8787";
        description = "Deprecated. The UI is now served by agentmom-api.";
      };

      apiUrl = lib.mkOption {
        type = lib.types.str;
        default = "http://127.0.0.1:8080";
        description = "Deprecated. The browser uses same-origin /api routes.";
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
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Optional runtime file containing the bearer token used by API worker endpoints.";
    };

    workerUrlAllowlist = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Exact worker URLs the API is allowed to accept from registering workers.";
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
    ] ++ lib.optionals cfg.catalogBackup.enable [
      "d ${cfg.catalogBackup.outputDir} 0750 ${cfg.user} ${cfg.group} - -"
    ] ++ lib.optionals cfg.credentialProxy.enable [
      "d ${cfg.credentialProxy.stateDir} 0755 root root - -"
    ];

    assertions = [
      {
        assertion = !cfg.worker.enable || cfg.worker.apiUrl != null;
        message = "services.agentmom.worker.apiUrl is required when services.agentmom.worker.enable is true.";
      }
      {
        assertion = !cfg.ui.enable || cfg.api.enable;
        message = "services.agentmom.api.enable is required when services.agentmom.ui.enable is true.";
      }
      {
        assertion = !cfg.api.enable || cfg.configFile != null || cfg.auth.secretFile != null;
        message = "services.agentmom.auth.secretFile is required when the generated config is used by services.agentmom.api.";
      }
      {
        assertion = !cfg.credentialProxy.enable || cfg.credentialProxy.package != null;
        message = "services.agentmom.credentialProxy.package is required when credentialProxy.enable is true.";
      }
      {
        assertion = !(cfg.api.enable || cfg.worker.enable) || cfg.workerTokenFile != null;
        message = "services.agentmom.workerTokenFile is required when the Agent Mom API or worker service is enabled.";
      }
      {
        assertion = !cfg.worker.enable || cfg.configFile != null || cfg.runtime.snapshotName != null;
        message = "services.agentmom.runtime.snapshotName is required when the generated config is used by services.agentmom.worker.";
      }
      {
        assertion = cfg.credentials.mode != "openrouter-proxy" || cfg.configFile != null || cfg.credentials.proxyUrl != null || cfg.credentialProxy.enable;
        message = "services.agentmom.credentials.proxyUrl or services.agentmom.credentialProxy.enable is required for openrouter-proxy mode.";
      }
      {
        assertion = cfg.credentials.mode != "openrouter-proxy" || cfg.configFile != null || cfg.credentials.proxyCaPath != null || cfg.credentialProxy.enable;
        message = "services.agentmom.credentials.proxyCaPath or services.agentmom.credentialProxy.enable is required for openrouter-proxy mode.";
      }
      {
        assertion = !cfg.catalogBackup.enable || cfg.api.enable;
        message = "services.agentmom.api.enable is required when services.agentmom.catalogBackup.enable is true.";
      }
      {
        assertion = !cfg.monitorCheck.enable || cfg.api.enable;
        message = "services.agentmom.api.enable is required when services.agentmom.monitorCheck.enable is true.";
      }
    ];

    systemd.services.agentmom-api = lib.mkIf cfg.api.enable {
      description = "Agent Mom central API";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ] ++ tmpfilesReadyUnits;
      wants = [ "network-online.target" ];
      path = commonPath;
      environment = commonEnvironment // lib.optionalAttrs cfg.ui.enable {
        MOM_UI_DIST = "${cfg.package}/share/agentmom/ui";
      };
      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        ExecStart = "${cfg.package}/bin/mom api --bind ${cfg.api.bind}";
        Restart = "always";
        RestartSec = "5s";
        TimeoutStopSec = "10s";
        WorkingDirectory = cfg.stateDir;
      };
    };

    systemd.services.agentmom-worker = lib.mkIf cfg.worker.enable {
      description = "Agent Mom central API worker";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ] ++ tmpfilesReadyUnits
        ++ lib.optionals cfg.credentialProxy.enable [ "agentmom-credential-proxy.service" ];
      wants = [ "network-online.target" ]
        ++ lib.optionals cfg.credentialProxy.enable [ "agentmom-credential-proxy.service" ];
      requires = lib.optionals cfg.credentialProxy.enable [ "agentmom-credential-proxy.service" ];
      path = commonPath;
      environment = commonEnvironment // {
        MOM_API_URL = cfg.worker.apiUrl;
        MOM_WORKER_BIND = cfg.worker.bind;
        MOM_SERVICE_TUNNEL_BIND_HOST = cfg.worker.serviceTunnelBindHost;
      } // lib.optionalAttrs (cfg.worker.url != null) {
        MOM_WORKER_URL = cfg.worker.url;
      } // lib.optionalAttrs (cfg.worker.serviceTunnelBaseUrl != null) {
        MOM_SERVICE_TUNNEL_BASE_URL = cfg.worker.serviceTunnelBaseUrl;
      };
      serviceConfig = {
        Type = "simple";
        User = cfg.user;
        Group = cfg.group;
        ExecStartPre = lib.optional cfg.worker.ensureBaseSnapshot (pkgs.writeShellScript "agentmom-worker-ensure-base" ''
          set -eu
          ${cfg.package}/bin/mom config doctor
          ${cfg.package}/bin/mom node ensure-base
        '');
        ExecStart = "${cfg.package}/bin/mom worker --interval ${toString cfg.worker.intervalSeconds}";
        Restart = "always";
        RestartSec = "5s";
        TimeoutStartSec = "30min";
        TimeoutStopSec = "10s";
        WorkingDirectory = cfg.stateDir;
      } // lib.optionalAttrs (cfg.worker.resticEnvFile != null) {
        EnvironmentFile = cfg.worker.resticEnvFile;
      };
    };

    systemd.services.agentmom-catalog-backup = lib.mkIf cfg.catalogBackup.enable {
      description = "Agent Mom SQLite catalog backup";
      after = [ "agentmom-api.service" ] ++ tmpfilesReadyUnits;
      path = commonPath;
      environment = commonEnvironment;
      serviceConfig = {
        Type = "oneshot";
        User = cfg.user;
        Group = cfg.group;
        WorkingDirectory = cfg.stateDir;
        ExecStart = pkgs.writeShellScript "agentmom-catalog-backup" ''
          set -eu
          install -d -m 0750 ${lib.escapeShellArg cfg.catalogBackup.outputDir}
          ts="$(date -u +%Y%m%dT%H%M%SZ)"
          backup_path="${cfg.catalogBackup.outputDir}/fleet-$ts.db"
          ${cfg.package}/bin/mom db backup --output "$backup_path"
          ${lib.optionalString (cfg.catalogBackup.resticEnvFile != null) ''
            restic backup "$backup_path" --tag agentmom --tag agentmom-catalog --tag fleet-catalog
          ''}
        '';
      } // lib.optionalAttrs (cfg.catalogBackup.resticEnvFile != null) {
        EnvironmentFile = cfg.catalogBackup.resticEnvFile;
      };
    };

    systemd.timers.agentmom-catalog-backup = lib.mkIf cfg.catalogBackup.enable {
      description = "Agent Mom SQLite catalog backup timer";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnCalendar = cfg.catalogBackup.onCalendar;
        RandomizedDelaySec = cfg.catalogBackup.randomizedDelaySec;
        Persistent = cfg.catalogBackup.persistent;
        Unit = "agentmom-catalog-backup.service";
      };
    };

    systemd.services.agentmom-monitor-check = lib.mkIf cfg.monitorCheck.enable {
      description = "Agent Mom lightweight monitor check";
      after = [ "agentmom-api.service" ] ++ tmpfilesReadyUnits;
      path = commonPath;
      environment = commonEnvironment;
      unitConfig = lib.optionalAttrs (cfg.monitorCheck.onFailureUnits != [ ]) {
        OnFailure = cfg.monitorCheck.onFailureUnits;
      };
      serviceConfig = {
        Type = "oneshot";
        User = cfg.user;
        Group = cfg.group;
        WorkingDirectory = cfg.stateDir;
        ExecStart = pkgs.writeShellScript "agentmom-monitor-check" ''
          set -eu
          for attempt in 1 2 3 4 5 6; do
            if ${cfg.package}/bin/mom monitor check \
              --api-url ${lib.escapeShellArg cfg.monitorCheck.apiUrl} \
              --min-ready-nodes ${toString cfg.monitorCheck.minReadyNodes} \
              --max-stale-nodes ${toString cfg.monitorCheck.maxStaleNodes} \
              --max-queued-age-secs ${toString cfg.monitorCheck.maxQueuedAgeSeconds} \
              --failed-job-lookback-secs ${toString cfg.monitorCheck.failedJobLookbackSeconds} \
              --max-recent-failed-jobs ${toString cfg.monitorCheck.maxRecentFailedJobs} \
              --max-backup-age-secs ${toString cfg.monitorCheck.maxBackupAgeSeconds} \
              --max-stale-scheduled-backups ${toString cfg.monitorCheck.maxStaleScheduledBackups} \
              --max-recent-backup-failures ${toString cfg.monitorCheck.maxRecentBackupFailures}; then
              exit 0
            fi
            if [ "$attempt" = 6 ]; then
              exit 1
            fi
            sleep 2
          done
        '';
      };
    };

    systemd.timers.agentmom-monitor-check = lib.mkIf cfg.monitorCheck.enable {
      description = "Agent Mom lightweight monitor check timer";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnCalendar = cfg.monitorCheck.onCalendar;
        RandomizedDelaySec = cfg.monitorCheck.randomizedDelaySec;
        Persistent = true;
        Unit = "agentmom-monitor-check.service";
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
