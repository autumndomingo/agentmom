{ spec, hermesAgentPackage ? null }:
{ lib, modulesPath, pkgs, ... }:

let
  hermesHome = "/root/.hermes-agent/${spec.hermes_profile}";
  hermesPackage =
    if hermesAgentPackage != null then
      hermesAgentPackage pkgs
    else
      pkgs.hermes-agent or (throw "Agent Mom guests require a hermes-agent package");
  agentmomRun = pkgs.writeShellScriptBin "agentmom-run" ''
    set -eu
    if [ "''${1:-}" = "--" ]; then
      shift
    fi
    if [ "$#" -eq 0 ]; then
      echo "usage: agentmom-run -- command [args...]" >&2
      exit 64
    fi
    if [ -f /etc/profile.d/mom.sh ]; then
      . /etc/profile.d/mom.sh
    fi
    if [ -f /etc/profile.d/agentmom-proxy.sh ]; then
      . /etc/profile.d/agentmom-proxy.sh
    fi
    export HOME=/root
    cd /workspace
    exec "$@"
  '';
  agentmomHermes = pkgs.writeShellScriptBin "agentmom-hermes" ''
    exec ${agentmomRun}/bin/agentmom-run -- hermes "$@"
  '';
  agentmomHermesAcp = pkgs.writeShellScriptBin "agentmom-hermes-acp" ''
    exec ${agentmomRun}/bin/agentmom-run -- ${pkgs.runtimeShell} -c '
      if command -v hermes-acp >/dev/null 2>&1; then
        exec hermes-acp "$@"
      fi
      if command -v hermes >/dev/null 2>&1; then
        exec hermes acp "$@"
      fi
      echo "hermes-acp/hermes acp is not installed" >&2
      exit 127
    ' agentmom-hermes-acp "$@"
  '';
  agentmomHermesDashboard = pkgs.writeShellScriptBin "agentmom-hermes-dashboard" ''
    set -eu
    port=9119
    cd /workspace
    hermes_bin="$(readlink -f "$(command -v hermes)")"
    hermes_prefix="$(dirname "$(dirname "$hermes_bin")")"
    hermes_web_dist="$hermes_prefix/share/hermes-agent/web_dist"
    if [ ! -d "$hermes_web_dist" ]; then
      echo "Hermes web_dist is missing at $hermes_web_dist; rebuild the Nix guest package" >&2
      exit 1
    fi
    export HERMES_WEB_DIST="$hermes_web_dist"
    exec hermes dashboard --host 0.0.0.0 --port "$port" --no-open --insecure --skip-build
  '';
  agentmomHermesDashboardStart = pkgs.writeShellScriptBin "agentmom-hermes-dashboard-start" ''
    set -eu
    port=9119
    health_path=/api/status
    probe_hermes_dashboard() {
      timeout 2s wget -q -O /dev/null --timeout=1 "http://127.0.0.1:$port$health_path" >/dev/null 2>&1
    }
    if probe_hermes_dashboard; then
      exit 0
    fi
    systemctl start agentmom-hermes-dashboard.service
    for _ in $(seq 1 90); do
      if probe_hermes_dashboard; then
        exit 0
      fi
      sleep 1
    done
    systemctl status --no-pager agentmom-hermes-dashboard.service >&2 || true
    journalctl -u agentmom-hermes-dashboard.service -n 120 --no-pager >&2 || true
    exit 1
  '';
  proxyScript = lib.optionalString (spec.credential_proxy_url != null) ''
    export HTTP_PROXY=${spec.credential_proxy_url}
    export HTTPS_PROXY=${spec.credential_proxy_url}
    export ALL_PROXY=${spec.credential_proxy_url}
    export OPENROUTER_API_KEY=agentmom-proxy
    export NODE_EXTRA_CA_CERTS=/usr/local/share/ca-certificates/agentmom-proxy.crt
    export REQUESTS_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt
    export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
  '';
  guestEnv = {
    HOME = "/root";
    HERMES_HOME = hermesHome;
    CODEX_HOME = "/root/.codex";
  } // lib.optionalAttrs (spec.credential_proxy_url != null) {
    HTTP_PROXY = spec.credential_proxy_url;
    HTTPS_PROXY = spec.credential_proxy_url;
    ALL_PROXY = spec.credential_proxy_url;
    OPENROUTER_API_KEY = "agentmom-proxy";
    NODE_EXTRA_CA_CERTS = "/usr/local/share/ca-certificates/agentmom-proxy.crt";
    REQUESTS_CA_BUNDLE = "/etc/ssl/certs/ca-certificates.crt";
    SSL_CERT_FILE = "/etc/ssl/certs/ca-certificates.crt";
  };
  codexConfig = ''
    approval_policy = "never"
    sandbox_mode = "danger-full-access"
  '';
  guestHostName =
    if builtins.stringLength spec.name > 63 then
      "${builtins.substring 0 62 spec.name}x"
    else
      spec.name;
  hermesConfig = ''
    model:
      provider: openrouter
      default: ${spec.hermes_model}
    terminal:
      backend: local
      cwd: /workspace
      persistent_shell: true
      timeout: 600
    approvals:
      mode: off
    toolsets:
      - all
    env:
      HTTP_PROXY: ${spec.credential_proxy_url}
      HTTPS_PROXY: ${spec.credential_proxy_url}
      ALL_PROXY: ${spec.credential_proxy_url}
      OPENROUTER_API_KEY: agentmom-proxy
      NODE_EXTRA_CA_CERTS: /usr/local/share/ca-certificates/agentmom-proxy.crt
      REQUESTS_CA_BUNDLE: /etc/ssl/certs/ca-certificates.crt
      SSL_CERT_FILE: /etc/ssl/certs/ca-certificates.crt
  '';
  optionalPackage = name:
    lib.optional (builtins.hasAttr name pkgs) (builtins.getAttr name pkgs);
in
{
  imports = [
    (modulesPath + "/profiles/minimal.nix")
  ];

  networking.hostName = guestHostName;
  networking.useDHCP = false;
  system.stateVersion = "25.05";
  boot.initrd.systemd.enable = false;
  console.enable = false;

  microvm = {
    hypervisor = "cloud-hypervisor";
    optimize.enable = true;
    storeOnDisk = false;
    vcpu = spec.cpus;
    mem = spec.memory_mib;
    registerWithMachined = true;
    interfaces = [
      {
        type = "tap";
        id = spec.tap;
        mac = spec.mac;
        tap.vhost = true;
      }
    ];
    shares = [
      {
        proto = "virtiofs";
        tag = "ro-store";
        source = "/nix/store";
        mountPoint = "/nix/.ro-store";
        socket = "/run/agentmom-${spec.name}-store-virtiofs.sock";
        readOnly = true;
        cache = "always";
      }
      {
        proto = "virtiofs";
        tag = "workspace";
        source = spec.workspace_dir;
        mountPoint = "/workspace";
        socket = "/run/agentmom-${spec.name}-workspace-virtiofs.sock";
        cache = "never";
      }
      {
        proto = "virtiofs";
        tag = "agentmom-secrets";
        source = spec.ssh_host_key_dir;
        mountPoint = "/run/agentmom-secrets";
        socket = "/run/agentmom-${spec.name}-secrets-virtiofs.sock";
        readOnly = true;
        cache = "never";
      }
    ];
    socket = "control.socket";
    binScripts.tap-up = lib.mkAfter ''
      ${lib.getExe' pkgs.iproute2 "ip"} link set dev '${spec.tap}' master '${spec.host_bridge}'
      ${lib.getExe' pkgs.iproute2 "ip"} link set dev '${spec.tap}' up
    '';
  };
  microvm.virtiofsd.inodeFileHandles = "prefer";

  systemd.network.enable = true;
  systemd.network.networks."10-eth0" = {
    matchConfig.MACAddress = spec.mac;
    address = [ "${spec.guest_ip}/24" ];
    gateway = [ spec.host_ip ];
    dns = [ "1.1.1.1" "8.8.8.8" ];
    networkConfig.IPv6AcceptRA = false;
  };

  networking.firewall = {
    enable = true;
    allowedTCPPorts = [ 22 ];
  };
  users.users.root.hashedPassword = "";
  users.users.root.openssh.authorizedKeys.keys = [ spec.ssh_public_key ];
  security.enableWrappers = false;
  security.sudo.enable = false;
  systemd.oomd.enable = false;
  systemd.services.systemd-random-seed.enable = lib.mkForce false;
  systemd.services.sshd-keygen.enable = lib.mkForce false;
  systemd.services."systemd-update-utmp".enable = lib.mkForce false;
  systemd.timers.fstrim.enable = lib.mkForce false;
  systemd.timers."systemd-tmpfiles-clean".enable = lib.mkForce false;
  systemd.targets.getty.wants = lib.mkForce [ ];
  systemd.targets.getty.wantedBy = lib.mkForce [ ];
  systemd.services."getty@tty1".enable = lib.mkForce false;
  systemd.services."serial-getty@ttyS0".enable = lib.mkForce false;
  systemd.services.console-getty.enable = lib.mkForce false;

  environment.systemPackages = with pkgs; [
    bash
    cacert
    coreutils
    curl
    findutils
    git
    gnugrep
    gnused
    iproute2
    nettools
    nodejs
    openssh
    python3
    uv
    wget
  ]
  ++ optionalPackage "codex"
  ++ optionalPackage "opencode"
  ++ [ hermesPackage agentmomRun agentmomHermes agentmomHermesAcp agentmomHermesDashboard agentmomHermesDashboardStart ];

  environment.etc."profile.d/agentmom-proxy.sh".text = proxyScript;
  environment.etc."profile.d/mom.sh".text = ''
    export HERMES_HOME=${hermesHome}
    export CODEX_HOME=/root/.codex
  '';

  systemd.tmpfiles.rules = [
    "d /workspace 0755 root root - -"
    "d /root/.codex 0700 root root - -"
    "d /root/.hermes-agent 0700 root root - -"
    "d ${hermesHome} 0700 root root - -"
    "d ${hermesHome}/home 0700 root root - -"
    "d /root/.local 0700 root root - -"
    "d /root/.local/share 0700 root root - -"
    "d /root/.local/share/opencode 0700 root root - -"
    "d /root/.config 0700 root root - -"
    "d /root/.config/opencode 0700 root root - -"
  ];

  systemd.services.agentmom-hermes-dashboard = {
    description = "Agent Mom Hermes dashboard";
    after = [ "network-online.target" ];
    wants = [ "network-online.target" ];
    path = [ hermesPackage pkgs.coreutils ];
    environment = guestEnv;
    serviceConfig = {
      Type = "simple";
      WorkingDirectory = "/workspace";
      ExecStart = "${agentmomHermesDashboard}/bin/agentmom-hermes-dashboard";
      Restart = "on-failure";
      RestartSec = "2s";
    };
  };

  system.activationScripts.agentmomGuestConfig.text = ''
    install -d -m 0700 /root/.codex ${hermesHome} ${hermesHome}/home /root/.local/share/opencode /root/.config/opencode
    cat > /root/.codex/config.toml <<'EOF'
${codexConfig}
EOF
    cat > ${hermesHome}/config.yaml <<'EOF'
${hermesConfig}
EOF
    cat > ${hermesHome}/SOUL.md <<'EOF'
You are running inside an isolated Agent Mom microvm.nix VM. Work in /workspace.
EOF
    cat > /root/.config/opencode/opencode.json <<'EOF'
{"provider":"openrouter","model":"${spec.hermes_model}"}
EOF
    ln -sfn ${hermesHome} /root/.hermes
    chmod 0600 /root/.codex/config.toml ${hermesHome}/config.yaml ${hermesHome}/SOUL.md /root/.config/opencode/opencode.json
  '';

  security.pki.certificateFiles =
    lib.optional (spec.credential_proxy_ca_file != null) ./agentmom-proxy.crt;

  services.openssh = {
    enable = true;
    startWhenNeeded = false;
    openFirewall = false;
    hostKeys = [
      {
        path = "/etc/ssh/ssh_host_ed25519_key";
        type = "ed25519";
      }
    ];
    listenAddresses = [
      {
        addr = "0.0.0.0";
        port = 22;
      }
    ];
    settings = {
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
      PerSourcePenalties = "no";
      PermitRootLogin = "yes";
      UsePAM = false;
    };
  };
  systemd.services.agentmom-ssh-host-key = {
    description = "Install Agent Mom pinned SSH host key";
    before = [ "sshd.service" ];
    requiredBy = [ "sshd.service" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    script = ''
      for _ in $(${lib.getExe' pkgs.coreutils "seq"} 1 100); do
        if [ -r /run/agentmom-secrets/ssh_host_ed25519_key ] && [ -r /run/agentmom-secrets/ssh_host_ed25519_key.pub ]; then
          break
        fi
        ${lib.getExe' pkgs.coreutils "sleep"} 0.1
      done

      ${lib.getExe' pkgs.coreutils "test"} -r /run/agentmom-secrets/ssh_host_ed25519_key
      ${lib.getExe' pkgs.coreutils "test"} -r /run/agentmom-secrets/ssh_host_ed25519_key.pub
      ${lib.getExe' pkgs.coreutils "install"} -d -m 0755 /etc/ssh
      ${lib.getExe' pkgs.coreutils "install"} -m 0600 /run/agentmom-secrets/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key
      ${lib.getExe' pkgs.coreutils "install"} -m 0644 /run/agentmom-secrets/ssh_host_ed25519_key.pub /etc/ssh/ssh_host_ed25519_key.pub
    '';
  };
  systemd.services.sshd = {
    after = [ "agentmom-ssh-host-key.service" ];
    requires = [ "agentmom-ssh-host-key.service" ];
  };
}
