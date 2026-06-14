{ config, lib, pkgs, defaultNixpkgsUrl ? "github:NixOS/nixpkgs/nixpkgs-unstable", defaultMicrovmNixUrl ? "github:microvm-nix/microvm.nix", defaultHermesAgentUrl ? "github:NousResearch/hermes-agent", ... }:

let
  cfg = config.services.agentmom;
  yaml = pkgs.formats.yaml { };
  json = pkgs.formats.json { };
  generatedConfigFile = json.generate "agentmom-config.json" {
    schema_version = 1;
    credentials = {
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
      bootstrap_admin_code_file = cfg.auth.bootstrapAdminCodeFile;
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
    MOM_MICROVM_STATE_DIR = cfg.microvm.stateDir;
    MOM_MICROVM_WORKSPACE_DIR = cfg.microvm.workspaceDir;
    MOM_MICROVM_BRIDGE = cfg.microvm.bridge;
    MOM_MICROVM_TAP_PREFIX = cfg.microvm.tapPrefix;
    MOM_MICROVM_CIDR = cfg.microvm.cidr;
    MOM_MICROVM_HOST_IP = cfg.microvm.hostAddress;
    MOM_MICROVM_EXTERNAL_INTERFACE = cfg.microvm.externalInterface;
    MOM_MICROVM_NIXPKGS_URL = cfg.microvm.nixpkgsUrl;
    MOM_MICROVM_NIX_URL = cfg.microvm.microvmNixUrl;
    MOM_HERMES_AGENT_URL = cfg.microvm.hermesAgentUrl;
    MOM_MICROVM_SYSTEM = pkgs.stdenv.hostPlatform.system;
    MOM_NODE_ID = cfg.nodeId;
    MOM_LOG_FORMAT = cfg.logFormat;
    MOM_SESSION_COOKIE_SECURE = if cfg.auth.secureCookies then "1" else "0";
    MOM_CAPACITY_CPUS = toString cfg.capacity.cpus;
    MOM_CAPACITY_MEMORY_MIB = toString cfg.capacity.memoryMib;
    MOM_CAPACITY_ACTIVE_WORKSPACES = toString cfg.capacity.activeWorkspaces;
    MOM_CAPACITY_DISK_RESERVE_MIB = toString cfg.capacity.diskReserveMib;
  }
  // lib.optionalAttrs (cfg.cutoverWipeMarker != null) {
    MOM_CUTOVER_WIPE_MARKER = cfg.cutoverWipeMarker;
  }
  // {
    MOM_CONFIG = toString effectiveConfigFile;
  }
  // lib.optionalAttrs (cfg.workerTokenFile != null) {
    MOM_WORKER_TOKEN_FILE = cfg.workerTokenFile;
  }
  // lib.optionalAttrs (cfg.workerNodeTokenFiles != { }) {
    MOM_WORKER_TOKEN_FILES = lib.concatStringsSep "," (
      lib.mapAttrsToList (node: path: "${node}=${path}") cfg.workerNodeTokenFiles
    );
  }
  // lib.optionalAttrs (effectiveWorkerUrlAllowlist != [ ]) {
    MOM_WORKER_URL_ALLOWLIST = lib.concatStringsSep "," effectiveWorkerUrlAllowlist;
  };
  effectiveWorkerUrlAllowlist = lib.unique (
    cfg.workerUrlAllowlist ++ lib.optional (cfg.worker.url != null) cfg.worker.url
  );

  commonPath = with pkgs; [
    bash
    coreutils
    curl
    dbus
    iproute2
    nix
    openssh
    restic
    systemd
  ];
  tmpfilesReadyUnits = [
    "systemd-tmpfiles-setup.service"
    "systemd-tmpfiles-resetup.service"
  ];
  microvmCidrAddress = builtins.head (lib.splitString "/" cfg.microvm.cidr);
  microvmCidrOctets = lib.splitString "." microvmCidrAddress;
  microvmCidrPrefix = lib.concatStringsSep "." (lib.take 3 microvmCidrOctets);
  microvmBridgePrefixLength = "24";
  workerBindParts = lib.splitString ":" cfg.worker.bind;
  workerBindPort = builtins.fromJSON (lib.last workerBindParts);
  microvmRunner = pkgs.writeShellScript "agentmom-microvm-runner" ''
    set -eu
    name="$1"
    state_dir="${cfg.microvm.stateDir}/machines/$name"
    cd "$state_dir"
    virtiofsd_pid=""
    vm_pid=""
    cleanup() {
      status="$?"
      trap - EXIT INT TERM
      if [ -n "$vm_pid" ]; then
        if kill -0 "$vm_pid" >/dev/null 2>&1 && [ -x result/bin/microvm-shutdown ]; then
          ${pkgs.coreutils}/bin/timeout 15s result/bin/microvm-shutdown >/dev/null 2>&1 || true
        fi
        if kill -0 "$vm_pid" >/dev/null 2>&1; then
          kill "$vm_pid" >/dev/null 2>&1 || true
        fi
        wait "$vm_pid" >/dev/null 2>&1 || true
      fi
      if [ -n "$virtiofsd_pid" ]; then
        kill "$virtiofsd_pid" >/dev/null 2>&1 || true
        wait "$virtiofsd_pid" >/dev/null 2>&1 || true
      fi
      if [ -x result/bin/tap-down ]; then
        result/bin/tap-down >/dev/null 2>&1 || true
      fi
      exit "$status"
    }
    trap cleanup EXIT INT TERM
    runner_input_hash() {
      for input in flake.nix spec.json microvm-workspace.nix hermes-agent-package.nix; do
        if [ ! -f "$input" ]; then
          echo "missing required microVM runner input $state_dir/$input" >&2
          return 1
        fi
      done
      {
        for input in flake.nix flake.lock spec.json microvm-workspace.nix hermes-agent-package.nix agentmom-proxy.crt; do
          if [ -e "$input" ]; then
            ${pkgs.coreutils}/bin/sha256sum "$input"
          else
            printf 'missing  %s\n' "$input"
          fi
        done
      } | ${pkgs.coreutils}/bin/sha256sum | ${pkgs.coreutils}/bin/cut -d' ' -f1
    }
    input_hash="$(runner_input_hash)"
    built_hash=""
    if [ -f .runner-input-hash ]; then
      built_hash="$(cat .runner-input-hash)"
    fi
    if [ ! -x result/bin/microvm-run ] || [ "$input_hash" != "$built_hash" ]; then
      ${pkgs.nix}/bin/nix build --no-write-lock-file --extra-experimental-features 'nix-command flakes' .#runner -o result
      rebuilt_hash="$(runner_input_hash)"
      if [ "$rebuilt_hash" != "$input_hash" ]; then
        echo "microVM runner inputs changed while building $state_dir; retry the start" >&2
        exit 1
      fi
      printf '%s\n' "$input_hash" > .runner-input-hash.tmp
      mv .runner-input-hash.tmp .runner-input-hash
      rm -f .runner-built
    fi
    if [ -x result/bin/tap-up ]; then
      result/bin/tap-up
    fi
    if [ -x result/bin/virtiofsd-run ]; then
      for socket_file in result/share/microvm/virtiofs/*/socket; do
        [ -e "$socket_file" ] || continue
        rm -f "$(cat "$socket_file")"
      done
      result/bin/virtiofsd-run &
      virtiofsd_pid="$!"
      for socket_file in result/share/microvm/virtiofs/*/socket; do
        [ -e "$socket_file" ] || continue
        socket="$(cat "$socket_file")"
        i=0
        while [ "$i" -lt 600 ]; do
          [ -S "$socket" ] && break
          i=$((i + 1))
          sleep 0.05
        done
        if [ ! -S "$socket" ]; then
          echo "timed out waiting for virtiofs socket $socket" >&2
          exit 1
        fi
      done
    fi
    result/bin/microvm-run &
    vm_pid="$!"
    wait "$vm_pid"
  '';
in
{
  options.services.agentmom = {
    enable = lib.mkEnableOption "Agent Mom workspace worker";

    package = lib.mkOption {
      type = lib.types.package;
      description = "Package that provides the mom binary.";
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

    microvm = {
      enable = lib.mkEnableOption "cold-start microvm.nix workspace runtime";

      stateDir = lib.mkOption {
        type = lib.types.str;
        default = "${cfg.stateDir}/microvms";
        description = "Directory containing generated microvm.nix workspace flakes and runtime metadata.";
      };

      workspaceDir = lib.mkOption {
        type = lib.types.str;
        default = "${cfg.microvm.stateDir}/workspaces";
        description = "Host directory containing per-workspace directories shared into guests with virtiofs.";
      };

      bridge = lib.mkOption {
        type = lib.types.str;
        default = "agentmom0";
        description = "Host bridge that microvm.nix tap devices attach to.";
      };

      tapPrefix = lib.mkOption {
        type = lib.types.str;
        default = "amvm";
        description = "Prefix used by generated tap device names.";
      };

      cidr = lib.mkOption {
        type = lib.types.str;
        default = "192.168.83.0/24";
        description = "IPv4 CIDR reserved for Agent Mom microVM guests.";
      };

      hostAddress = lib.mkOption {
        type = lib.types.str;
        default = "192.168.83.1";
        description = "Host bridge address used as the guest default gateway.";
      };

      externalInterface = lib.mkOption {
        type = lib.types.str;
        default = "eth0";
        description = "Host interface used for microVM guest NAT.";
      };

      kvmKernelModule = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "kvm-amd";
        description = "Host CPU-specific KVM module to load, for example kvm-amd or kvm-intel.";
      };

      nixpkgsUrl = lib.mkOption {
        type = lib.types.str;
        default = defaultNixpkgsUrl;
        defaultText = lib.literalExpression ''"path:/nix/store/...-source"'';
        description = "Nixpkgs flake URL used by generated microvm.nix workspace flakes.";
      };

      microvmNixUrl = lib.mkOption {
        type = lib.types.str;
        default = defaultMicrovmNixUrl;
        defaultText = lib.literalExpression ''"path:/nix/store/...-source"'';
        description = "microvm.nix flake URL used by generated workspace flakes.";
      };

      hermesAgentUrl = lib.mkOption {
        type = lib.types.str;
        default = defaultHermesAgentUrl;
        defaultText = lib.literalExpression ''"path:/nix/store/...-source"'';
        description = "Hermes Agent flake URL used by generated workspace flakes.";
      };

    };

    cutoverWipeMarker = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Optional one-time marker name. When absent from stateDir, prestart moves old Agent Mom catalog/runtime state aside before starting services.";
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

    credentials = {
      proxyUrl = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "OpenRouter proxy URL written into guest environments.";
      };

      proxyCaPath = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "CA certificate path trusted by guests for the OpenRouter proxy.";
      };
    };

    guest = {
      hermesProfile = lib.mkOption {
        type = lib.types.str;
        default = "main";
        description = "Hermes profile name created inside guest VMs.";
      };

      model = lib.mkOption {
        type = lib.types.str;
        default = "gpt-5.5";
        description = "Default OpenRouter model written into guest Hermes config.";
      };
    };

    auth = {
      secretFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Runtime file containing the Agent Mom browser-session and invite HMAC secret.";
      };

      bootstrapAdminCodeFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Runtime file containing the first admin login code used to bootstrap an empty Agent Mom catalog.";
      };

      secureCookies = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Whether Agent Mom browser session cookies include the Secure attribute. Disable only for HTTP-only local deployments.";
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
        description = "Host address used for Hermes service tunnels created by this worker.";
      };

      serviceTunnelBaseUrl = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Browser-visible base URL for worker service tunnels, without the port.";
      };

      serviceTunnelPortRange = lib.mkOption {
        type = lib.types.nullOr (lib.types.submodule {
          options = {
            from = lib.mkOption {
              type = lib.types.port;
              default = 41000;
              description = "First TCP port Agent Mom may use for worker service tunnels.";
            };
            to = lib.mkOption {
              type = lib.types.port;
              default = 41999;
              description = "Last TCP port Agent Mom may use for worker service tunnels.";
            };
          };
        });
        default = null;
        description = "Optional bounded TCP port range for worker service tunnels.";
      };

      openFirewall = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Open the worker control port and configured service tunnel range in the NixOS firewall.";
      };

      firewallInterface = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "tailscale0";
        description = "Optional interface whose firewall should be opened for worker control and service tunnel traffic.";
      };

      resticEnvFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Optional runtime environment file with RESTIC_* and S3-compatible credentials for workspace backups.";
      };

      ensureRuntime = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Run mom node ensure-runtime before starting the worker so deploys fail unless the microvm.nix host prerequisites are present.";
      };
    };

    credentialProxy = {
      enable = lib.mkEnableOption "iron-proxy credential injection for Agent Mom guests";

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
        default = "${cfg.microvm.hostAddress}:1080";
        description = "CONNECT/SOCKS5 listener used by guest HTTP(S)_PROXY settings.";
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
        default = "http://${cfg.microvm.hostAddress}:1080";
        description = "Proxy URL written into Agent Mom guest configuration.";
      };

      caCert = lib.mkOption {
        type = lib.types.str;
        default = "${cfg.credentialProxy.stateDir}/ca.crt";
        description = "CA certificate path trusted by guests.";
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
          "dl-cdn.alpinelinux.org"
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
      description = "Optional runtime file containing this worker's bearer token. Also used as a single-token API fallback for local/dev deployments.";
    };

    workerNodeTokenFiles = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = "API-side map of node IDs to runtime files containing per-node worker bearer tokens.";
    };

    workerUrlAllowlist = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Exact worker URLs the API is allowed to accept from registering workers.";
    };
  };

  config = lib.mkIf cfg.enable {
    users.groups =
      lib.optionalAttrs cfg.createUser {
        ${cfg.group} = { };
      }
      // lib.optionalAttrs cfg.microvm.enable {
        microvm = { };
      };
    users.users =
      lib.optionalAttrs cfg.createUser {
        ${cfg.user} = {
          isSystemUser = true;
          group = cfg.group;
          home = cfg.stateDir;
          createHome = true;
          extraGroups = [ "kvm" ];
        };
      }
      // lib.optionalAttrs cfg.microvm.enable {
        microvm = {
          isSystemUser = true;
          group = "microvm";
          extraGroups = [ "kvm" ];
        };
      };

    systemd.tmpfiles.rules = [
      "d ${cfg.stateDir} 0750 ${cfg.user} ${cfg.group} - -"
    ] ++ lib.optionals cfg.microvm.enable [
      "d ${cfg.microvm.stateDir} 0750 ${cfg.user} ${cfg.group} - -"
      "d ${cfg.microvm.stateDir}/machines 0750 ${cfg.user} ${cfg.group} - -"
      "d ${cfg.microvm.workspaceDir} 0750 ${cfg.user} ${cfg.group} - -"
    ] ++ lib.optionals cfg.catalogBackup.enable [
      "d ${cfg.catalogBackup.outputDir} 0750 ${cfg.user} ${cfg.group} - -"
    ] ++ lib.optionals cfg.credentialProxy.enable [
      "d ${cfg.credentialProxy.stateDir} 0755 root root - -"
    ];

    networking.firewall.allowedTCPPorts =
      lib.mkIf (cfg.worker.enable && cfg.worker.openFirewall && cfg.worker.firewallInterface == null) [
        workerBindPort
      ];
    networking.firewall.allowedTCPPortRanges =
      lib.mkIf (cfg.worker.enable && cfg.worker.openFirewall && cfg.worker.firewallInterface == null && cfg.worker.serviceTunnelPortRange != null) [
        cfg.worker.serviceTunnelPortRange
      ];
    networking.firewall.interfaces = lib.mkMerge [
      (lib.mkIf (cfg.microvm.enable && cfg.credentialProxy.enable) {
        ${cfg.microvm.bridge}.allowedTCPPorts = [ 1080 ];
      })
      (lib.mkIf (cfg.worker.enable && cfg.worker.openFirewall && cfg.worker.firewallInterface != null) {
        ${cfg.worker.firewallInterface} = {
          allowedTCPPorts = [ workerBindPort ];
          allowedTCPPortRanges =
            lib.optionals (cfg.worker.serviceTunnelPortRange != null) [
              cfg.worker.serviceTunnelPortRange
            ];
        };
      })
    ];

    networking.nat = lib.mkIf (cfg.microvm.enable && !cfg.credentialProxy.enable) {
      enable = true;
      externalInterface = cfg.microvm.externalInterface;
      internalInterfaces = [ cfg.microvm.bridge ];
    };

    boot.kernelModules = lib.mkIf cfg.microvm.enable ([
      "bridge"
      "tap"
      "tun"
      "vhost_net"
    ] ++ lib.optional (cfg.microvm.kvmKernelModule != null) cfg.microvm.kvmKernelModule);

    systemd.services.agentmom-cutover-wipe = lib.mkIf (cfg.cutoverWipeMarker != null) {
      description = "Agent Mom one-time cutover state wipe";
      wantedBy = [ "multi-user.target" ];
      before = [
        "agentmom-api.service"
        "agentmom-worker.service"
        "agentmom-catalog-backup.service"
        "agentmom-monitor-check.service"
      ];
      after = tmpfilesReadyUnits;
      path = [
        pkgs.coreutils
        pkgs.gnugrep
        pkgs.systemd
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        TimeoutStartSec = "40min";
      };
      script = ''
        set -eu
        state_dir=${lib.escapeShellArg cfg.stateDir}
        marker=${lib.escapeShellArg "${cfg.stateDir}/.${cfg.cutoverWipeMarker}"}
        if [ -e "$marker" ]; then
          exit 0
        fi
        stop_units=(
          agentmom-monitor-check.timer
          agentmom-catalog-backup.timer
          agentmom-monitor-check.service
          agentmom-catalog-backup.service
          agentmom-worker.service
          agentmom-api.service
        )
        for unit in "''${stop_units[@]}"
        do
          state="$(systemctl is-active "$unit" 2>/dev/null || true)"
          case "$state" in
            active|activating|deactivating)
              if ! systemctl stop --job-mode=fail "$unit" >/dev/null 2>&1; then
                echo "refusing Agent Mom cutover wipe because stopping $unit would replace a queued systemd job" >&2
                exit 1
              fi
              ;;
          esac
        done
        for attempt in $(seq 1 60)
        do
          still_active=""
          for unit in "''${stop_units[@]}"
          do
            state="$(systemctl is-active "$unit" 2>/dev/null || true)"
            case "$state" in
              active|activating|deactivating)
              still_active="''${still_active} $unit"
                ;;
            esac
          done
          if [ -z "$still_active" ]; then
            break
          fi
          if [ "$attempt" = 60 ]; then
            echo "refusing Agent Mom cutover wipe while units are still active:$still_active" >&2
            exit 1
          fi
          sleep 1
        done
        systemctl list-units --all --plain --no-legend 'agentmom-microvm@*.service' | while read -r unit _rest
        do
          [ -n "$unit" ] || continue
          systemctl stop "$unit"
        done
        if systemctl list-units --plain --no-legend --state=active 'agentmom-microvm@*.service' | grep -q .; then
          echo "refusing Agent Mom cutover wipe while microVM units are still active" >&2
          systemctl list-units --plain --no-legend --state=active 'agentmom-microvm@*.service' >&2 || true
          exit 1
        fi
        stamp="$(date -u +%Y%m%dT%H%M%SZ)"
        archive="$state_dir/cutover-archive-$stamp"
        mkdir -p "$archive"
        paths=(
          ${lib.escapeShellArg "${cfg.stateDir}/fleet.db"}
          ${lib.escapeShellArg "${cfg.stateDir}/fleet.db-shm"}
          ${lib.escapeShellArg "${cfg.stateDir}/fleet.db-wal"}
          ${lib.escapeShellArg "${cfg.stateDir}/microsandbox"}
        )
        ${lib.optionalString cfg.microvm.enable ''
          paths+=(
            ${lib.escapeShellArg "${cfg.microvm.stateDir}/machines"}
            ${lib.escapeShellArg "${cfg.microvm.workspaceDir}"}
            ${lib.escapeShellArg "${cfg.microvm.stateDir}/.machine-state.lock"}
            ${lib.escapeShellArg "${cfg.microvm.stateDir}/.machine-state.flock"}
          )
        ''}
        for path in "''${paths[@]}"
        do
          if [ -e "$path" ]; then
            mv "$path" "$archive/"
          fi
        done
        ${lib.optionalString cfg.microvm.enable ''
          install -d -m 0750 -o ${lib.escapeShellArg cfg.user} -g ${lib.escapeShellArg cfg.group} ${lib.escapeShellArg cfg.microvm.stateDir}
          install -d -m 0750 -o ${lib.escapeShellArg cfg.user} -g ${lib.escapeShellArg cfg.group} ${lib.escapeShellArg "${cfg.microvm.stateDir}/machines"}
          install -d -m 0750 -o ${lib.escapeShellArg cfg.user} -g ${lib.escapeShellArg cfg.group} ${lib.escapeShellArg cfg.microvm.workspaceDir}
        ''}
        touch "$marker"
        chown ${lib.escapeShellArg cfg.user}:${lib.escapeShellArg cfg.group} "$marker"
        ${lib.optionalString cfg.catalogBackup.enable ''
          systemctl start agentmom-catalog-backup.timer >/dev/null 2>&1 || true
        ''}
        ${lib.optionalString cfg.monitorCheck.enable ''
          systemctl start agentmom-monitor-check.timer >/dev/null 2>&1 || true
        ''}
      '';
    };

    assertions = [
      {
        assertion = !cfg.worker.enable || cfg.worker.apiUrl != null;
        message = "services.agentmom.worker.apiUrl is required when services.agentmom.worker.enable is true.";
      }
      {
        assertion = !cfg.api.enable || cfg.configFile != null || cfg.auth.secretFile != null;
        message = "services.agentmom.auth.secretFile is required when the generated config is used by services.agentmom.api.";
      }
      {
        assertion = !cfg.api.enable || cfg.configFile != null || cfg.auth.bootstrapAdminCodeFile != null;
        message = "services.agentmom.auth.bootstrapAdminCodeFile is required when the generated config is used by services.agentmom.api.";
      }
      {
        assertion = !cfg.credentialProxy.enable || cfg.credentialProxy.package != null;
        message = "services.agentmom.credentialProxy.package is required when credentialProxy.enable is true.";
      }
      {
        assertion = !cfg.credentialProxy.enable || cfg.credentialProxy.openaiApiKeyFile != null || cfg.credentialProxy.openrouterApiKeyFile != null;
        message = "services.agentmom.credentialProxy.openaiApiKeyFile or openrouterApiKeyFile is required when credentialProxy.enable is true.";
      }
      {
        assertion = !cfg.worker.enable || cfg.microvm.enable;
        message = "services.agentmom.microvm.enable is required when services.agentmom.worker.enable is true.";
      }
      {
        assertion = !cfg.microvm.enable || builtins.match "[0-9]+\\.[0-9]+\\.[0-9]+\\.0/24" cfg.microvm.cidr != null;
        message = "services.agentmom.microvm.cidr must be a /24 network ending in .0, for example 192.168.83.0/24.";
      }
      {
        assertion = !cfg.microvm.enable || cfg.microvm.hostAddress == "${microvmCidrPrefix}.1";
        message = "services.agentmom.microvm.hostAddress must be the .1 address inside services.agentmom.microvm.cidr.";
      }
      {
        assertion = !cfg.worker.enable || cfg.workerTokenFile != null;
        message = "services.agentmom.workerTokenFile is required when the Agent Mom worker service is enabled.";
      }
      {
        assertion = !cfg.api.enable || cfg.workerTokenFile != null || cfg.workerNodeTokenFiles != { };
        message = "services.agentmom.workerTokenFile or services.agentmom.workerNodeTokenFiles is required when the Agent Mom API service is enabled.";
      }
      {
        assertion = !cfg.api.enable || cfg.configFile != null || effectiveWorkerUrlAllowlist != [ ];
        message = "services.agentmom.workerUrlAllowlist is required when the generated config is used by services.agentmom.api.";
      }
      {
        assertion = cfg.configFile != null || !cfg.worker.enable || cfg.credentials.proxyUrl != null || cfg.credentialProxy.enable;
        message = "services.agentmom.credentials.proxyUrl or services.agentmom.credentialProxy.enable is required for workers when the generated config is used.";
      }
      {
        assertion = cfg.configFile != null || !cfg.worker.enable || cfg.credentials.proxyCaPath != null || cfg.credentialProxy.enable;
        message = "services.agentmom.credentials.proxyCaPath or services.agentmom.credentialProxy.enable is required for workers when the generated config is used.";
      }
      {
        assertion = !cfg.catalogBackup.enable || cfg.api.enable;
        message = "services.agentmom.api.enable is required when services.agentmom.catalogBackup.enable is true.";
      }
      {
        assertion = cfg.worker.serviceTunnelPortRange == null || cfg.worker.serviceTunnelPortRange.from <= cfg.worker.serviceTunnelPortRange.to;
        message = "services.agentmom.worker.serviceTunnelPortRange.from must be <= to.";
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
      wants = [ "network-online.target" ]
        ++ lib.optionals (cfg.cutoverWipeMarker != null) [ "agentmom-cutover-wipe.service" ];
      requires = lib.optionals (cfg.cutoverWipeMarker != null) [ "agentmom-cutover-wipe.service" ];
      path = commonPath;
      environment = commonEnvironment // {
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
        ++ lib.optionals (cfg.cutoverWipeMarker != null) [ "agentmom-cutover-wipe.service" ]
        ++ lib.optionals cfg.microvm.enable [ "agentmom-microvm-bridge.service" ]
        ++ lib.optionals cfg.credentialProxy.enable [ "agentmom-credential-proxy.service" ];
      wants = [ "network-online.target" ]
        ++ lib.optionals (cfg.cutoverWipeMarker != null) [ "agentmom-cutover-wipe.service" ]
        ++ lib.optionals cfg.microvm.enable [ "agentmom-microvm-bridge.service" ]
        ++ lib.optionals cfg.credentialProxy.enable [ "agentmom-credential-proxy.service" ];
      requires = lib.optionals (cfg.cutoverWipeMarker != null) [ "agentmom-cutover-wipe.service" ]
        ++ lib.optionals cfg.microvm.enable [ "agentmom-microvm-bridge.service" ]
        ++ lib.optionals cfg.credentialProxy.enable [ "agentmom-credential-proxy.service" ];
      path = commonPath;
      environment = commonEnvironment // {
        MOM_API_URL = cfg.worker.apiUrl;
        MOM_WORKER_BIND = cfg.worker.bind;
        MOM_SERVICE_TUNNEL_BIND_HOST = cfg.worker.serviceTunnelBindHost;
      } // lib.optionalAttrs (cfg.worker.url != null) {
        MOM_WORKER_URL = cfg.worker.url;
      } // lib.optionalAttrs (cfg.worker.serviceTunnelBaseUrl != null) {
        MOM_SERVICE_TUNNEL_BASE_URL = cfg.worker.serviceTunnelBaseUrl;
      } // lib.optionalAttrs (cfg.worker.serviceTunnelPortRange != null) {
        MOM_SERVICE_TUNNEL_PORT_RANGE = "${toString cfg.worker.serviceTunnelPortRange.from}-${toString cfg.worker.serviceTunnelPortRange.to}";
      };
      serviceConfig = {
        Type = "simple";
        User = "root";
        Group = "root";
        ExecStartPre = lib.optional cfg.worker.ensureRuntime (pkgs.writeShellScript "agentmom-worker-ensure-runtime" ''
          set -eu
          ${cfg.package}/bin/mom config doctor
          ${cfg.package}/bin/mom node ensure-runtime
        '');
        ExecStart = "${cfg.package}/bin/mom worker --interval ${toString cfg.worker.intervalSeconds}";
        Restart = "always";
        RestartSec = "5s";
        TimeoutStartSec = "30min";
        TimeoutStopSec = "35min";
        WorkingDirectory = cfg.stateDir;
      } // lib.optionalAttrs (cfg.worker.resticEnvFile != null) {
        EnvironmentFile = cfg.worker.resticEnvFile;
      };
    };

    systemd.services.agentmom-microvm-bridge = lib.mkIf cfg.microvm.enable {
      description = "Agent Mom microvm.nix guest bridge";
      wantedBy = [ "multi-user.target" ];
      after = tmpfilesReadyUnits;
      path = [ pkgs.iproute2 ];
      unitConfig.RequiresMountsFor = [
        cfg.microvm.stateDir
        cfg.microvm.workspaceDir
      ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      script = ''
        if ! ip link show ${lib.escapeShellArg cfg.microvm.bridge} >/dev/null 2>&1; then
          ip link add ${lib.escapeShellArg cfg.microvm.bridge} type bridge
        fi
        ip addr replace ${lib.escapeShellArg cfg.microvm.hostAddress}/${microvmBridgePrefixLength} dev ${lib.escapeShellArg cfg.microvm.bridge}
        ip link set ${lib.escapeShellArg cfg.microvm.bridge} up
      '';
    };

    systemd.services."agentmom-microvm@" = lib.mkIf cfg.microvm.enable {
      description = "Agent Mom microvm.nix workspace %i";
      after = [
        "agentmom-microvm-bridge.service"
        "network-online.target"
      ] ++ tmpfilesReadyUnits;
      wants = [
        "agentmom-microvm-bridge.service"
        "network-online.target"
      ];
      requires = [ "agentmom-microvm-bridge.service" ];
      path = commonPath ++ [
        pkgs.iproute2
        pkgs.nix
      ];
      environment = commonEnvironment;
      unitConfig.RequiresMountsFor = [
        cfg.microvm.stateDir
        cfg.microvm.workspaceDir
      ];
      serviceConfig = {
        Type = "simple";
        User = "root";
        Group = "root";
        ExecStart = "${microvmRunner} %i";
        Restart = "on-failure";
        RestartSec = "2s";
        SuccessExitStatus = "143";
        WorkingDirectory = cfg.microvm.stateDir;
        KillMode = "mixed";
        TimeoutStartSec = "30min";
        TimeoutStopSec = "90s";
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
      after = [ "network-online.target" ] ++ lib.optionals cfg.microvm.enable [ "agentmom-microvm-bridge.service" ];
      wants = [ "network-online.target" ] ++ lib.optionals cfg.microvm.enable [ "agentmom-microvm-bridge.service" ];
      requires = lib.optionals cfg.microvm.enable [ "agentmom-microvm-bridge.service" ];
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
