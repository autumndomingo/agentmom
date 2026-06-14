{ spec, hermesAgentPackage ? null }:
{ lib, modulesPath, pkgs, ... }:

let
  hermesHome = "/root/.hermes-agent/${spec.hermes_profile}";
  hermesPackage =
    if hermesAgentPackage != null then
      hermesAgentPackage pkgs
    else
      pkgs.hermes-agent or (throw "Agent Mom guests require a hermes-agent package");
  proxyScript = lib.optionalString (spec.credential_proxy_url != null) ''
    export HTTP_PROXY=${spec.credential_proxy_url}
    export HTTPS_PROXY=${spec.credential_proxy_url}
    export ALL_PROXY=${spec.credential_proxy_url}
    export OPENAI_API_KEY=agentmom-proxy
    export OPENROUTER_API_KEY=agentmom-proxy
  '';
  codexConfig = ''
    approval_policy = "never"
    sandbox_mode = "danger-full-access"
  '';
  hermesConfig = if spec.credential_mode == "openrouter-proxy" then ''
    default_provider: openrouter
    providers:
      openrouter:
        provider: openrouter
        model: ${spec.hermes_model}
        api_key_env: OPENROUTER_API_KEY
  '' else ''
    default_provider: openai-codex
    providers:
      openai-codex:
        provider: openai-codex
        model: ${spec.hermes_model}
  '';
  optionalPackage = name:
    lib.optional (builtins.hasAttr name pkgs) (builtins.getAttr name pkgs);
in
{
  imports = [
    (modulesPath + "/profiles/minimal.nix")
  ];

  networking.hostName = spec.name;
  networking.useDHCP = false;
  system.stateVersion = "25.05";
  boot.initrd.systemd.enable = false;
  console.enable = false;

  microvm = {
    hypervisor = "cloud-hypervisor";
    optimize.enable = true;
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
        tag = "workspace";
        source = spec.workspace_dir;
        mountPoint = "/workspace";
        socket = "/run/agentmom-${spec.name}-workspace-virtiofs.sock";
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
    extraInputRules = ''
      ip saddr ${spec.host_ip} tcp dport 22 accept
      tcp dport 22 drop
    '';
  };
  users.users.root.hashedPassword = "";
  users.users.root.openssh.authorizedKeys.keys = [ spec.ssh_public_key ];
  security.enableWrappers = false;
  security.sudo.enable = false;
  systemd.oomd.enable = false;
  systemd.services.systemd-random-seed.enable = lib.mkForce false;
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
  ++ [ hermesPackage ];

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
    listenAddresses = [
      {
        addr = "0.0.0.0";
        port = 22;
      }
    ];
    settings = {
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
      PermitRootLogin = "yes";
      UsePAM = false;
    };
  };
}
