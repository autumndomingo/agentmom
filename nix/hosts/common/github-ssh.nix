# GitHub SSH key for all hosts (bypasses YubiKey for git operations)
# Requires agenix module to be loaded in the host's flake configuration
{ config, lib, pkgs, ... }:

let
  cfg = config.justinsConfig.githubSsh;
  sshKeyPath = "/home/justin/.ssh/id_ed25519_github";
in
{
  options.justinsConfig.githubSsh.enable = lib.mkOption {
    type = lib.types.bool;
    default = true;
    description = "Install the shared GitHub SSH key from agenix.";
  };

  config = lib.mkIf cfg.enable {
  # Age key for agenix decryption (same location on all hosts)
  age.identityPaths = [ "/etc/age/key.txt" ];

  age.secrets.github-ssh-key = {
    file = ../../secrets/github-ssh-key.age;
    mode = "0600";
    owner = "justin";
    group = "users";
    path = sshKeyPath;
  };

  # Ensure the .ssh directory exists with strict permissions
  systemd.tmpfiles.rules = [
    "d /home/justin/.ssh 0700 justin users -"
  ];

  # Add GitHub to known_hosts for git operations
  programs.ssh.knownHosts."github.com" = {
    hostNames = [ "github.com" ];
    publicKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl";
  };
  };
}
