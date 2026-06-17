{ lib, ... }:

{
  nix.settings = {
    substituters = lib.mkAfter [
      "https://justinmoon.cachix.org"
    ];
    trusted-public-keys = lib.mkAfter [
      "justinmoon.cachix.org-1:fisbhGDGdi2dK+Y/DI82s6PAmu9Q0XlTVGRKc7/x3vQ="
    ];
  };
}
