{ config, pkgs, ... }:

{
  age.secrets.nixbuild-net-key = {
    file = ../../secrets/nixbuild-net-key.age;
    owner = "root";
    group = "root";
    mode = "0400";
  };

  programs.ssh.knownHosts."eu.nixbuild.net" = {
    hostNames = [ "eu.nixbuild.net" ];
    publicKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPIQCZc54poJ8vqawd8TraNryQeJnvH1eLpIDgbiqymM";
  };

  environment.etc."ssh/ssh_config.d/nixbuild-net.conf".text = ''
    Host eu.nixbuild.net
      User justin
      PubkeyAcceptedKeyTypes ssh-ed25519
      ServerAliveInterval 60
      IPQoS throughput
      IdentityFile ${config.age.secrets.nixbuild-net-key.path}
      SetEnv NIXBUILDNET_KEEP_BUILDS_RUNNING=true NIXBUILDNET_DEFAULT_CPU=32 NIXBUILDNET_MIN_CPU=16 NIXBUILDNET_MAX_CPU=64 NIXBUILDNET_DEFAULT_MEM_PER_CPU=4096
  '';

  nix = {
    distributedBuilds = true;
    buildMachines = [
      {
        hostName = "eu.nixbuild.net";
        protocol = "ssh-ng";
        sshUser = "justin";
        sshKey = config.age.secrets.nixbuild-net-key.path;
        system = "x86_64-linux";
        maxJobs = 100;
        speedFactor = 25;
        supportedFeatures = [ "benchmark" "big-parallel" "kvm" ];
      }
    ];
    settings = {
      builders-use-substitutes = true;
      max-jobs = 4;
      cores = 16;
      download-buffer-size = 256 * 1024 * 1024;
    };
  };

  environment.systemPackages = [ pkgs.openssh ];
}
