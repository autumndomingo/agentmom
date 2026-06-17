{
  nodes = {
    mom-ctrl = {
      env = "prod";
      role = "ctrl";
      targetHost = "mom-ctrl";
      module = ../hosts/mom-ctrl/configuration.nix;
    };

    mom-1 = {
      env = "prod";
      role = "worker";
      targetHost = "mom-1";
      module = ../hosts/mom-1/configuration.nix;
      capacity = {
        cpus = 32;
        memoryMib = 131072;
        activeWorkspaces = 48;
      };
    };

    mom-stage-ctrl = {
      env = "stage";
      role = "ctrl";
      targetHost = "204.168.131.33";
      module = ../hosts/mom-stage-ctrl/configuration.nix;
    };

    mom-stage-1 = {
      env = "stage";
      role = "worker";
      targetHost = "mom-stage-1";
      module = ../hosts/mom-stage-1/configuration.nix;
      capacity = {
        cpus = 8;
        memoryMib = 65536;
        activeWorkspaces = 24;
      };
    };
  };
}
