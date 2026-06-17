{ pkgs }:

pkgs.cloud-hypervisor.overrideAttrs (old: {
  patches = (old.patches or [ ]) ++ [
    ./cloud-hypervisor-virtiofs-restore-order.patch
  ];
})
