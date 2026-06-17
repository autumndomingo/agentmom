{
  userAuthorizedKeys = [
    # 1Password SSH key (backup)
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIK9qcRB7tF1e8M9CX8zoPfNmQgWqvnee0SKASlM0aMlm mail@justinmoon.com"
    # Hetzner dedicated key (no YubiKey needed)
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIA8Oc7Gtaqck72Y5G92STRSEe/Yl7983H89dMFzMcmI/ hetzner-ssh-key"
    # YubiKey primary
    "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIOvnevaL7FO+n13yukLu23WNfzRUPzZ2e3X/BBQLieapAAAABHNzaDo= justin@yubikey-primary"
    # YubiKey backup
    "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIMrMVMYKXjA7KuxacP6RexsSfXrkQhwOKwGAfJExDxYZAAAABHNzaDo= justin@yubikey-backup"
  ];

  rootAuthorizedKeys = [
    # 1Password SSH key (backup)
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIK9qcRB7tF1e8M9CX8zoPfNmQgWqvnee0SKASlM0aMlm mail@justinmoon.com"
    # Hetzner dedicated key (no YubiKey needed)
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMQAESH5V93dq/DPAr3rw4XOfKbzc4KKinwl6FPF+Ai hetzner-ssh-key"
    # YubiKey primary
    "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIOvnevaL7FO+n13yukLu23WNfzRUPzZ2e3X/BBQLieapAAAABHNzaDo= justin@yubikey-primary"
    # YubiKey backup
    "sk-ssh-ed25519@openssh.com AAAAGnNrLXNzaC1lZDI1NTE5QG9wZW5zc2guY29tAAAAIMrMVMYKXjA7KuxacP6RexsSfXrkQhwOKwGAfJExDxYZAAAABHNzaDo= justin@yubikey-backup"
  ];
}
