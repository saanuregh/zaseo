{ inputs, ... }:
{
  perSystem =
    {
      pkgs,
      lib,
      system,
      ...
    }:
    let
      mkZaseo = import ../toolchain.nix { inherit inputs; };
      zaseo = mkZaseo pkgs;
    in
    {
      packages = {
        default = zaseo;
        inherit zaseo;
        debug = zaseo.override { profile = "dev"; };
      };
    }
    // lib.optionalAttrs (lib.hasSuffix "linux" system) {
      checks = {
        a11y-test = import ../tests/a11y.nix {
          inherit pkgs inputs;
        };
      }
      // import ../tests/sandboxing { inherit pkgs inputs; };
    };
}
