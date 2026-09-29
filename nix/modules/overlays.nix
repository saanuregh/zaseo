{ inputs, ... }:
{
  flake.overlays.default =
    final: _:
    let
      mkZaseo = import ../toolchain.nix { inherit inputs; };
    in
    {
      zaseo = mkZaseo final;
    };
}
