{ inputs, ... }:
{
  perSystem =
    { pkgs, system, ... }:
    let
      # NOTE: Duplicated because this is in a separate flake-parts partition
      # than ./packages.nix
      mkZed = import ../toolchain.nix { inherit inputs; };
      zed-editor = mkZed pkgs;

      # mdBook pinned to 0.4.40 via a dedicated nixpkgs input, because the docs
      # rely on behavior that newer mdBook releases break (see
      # `crates/docs_preprocessor/Cargo.toml`).
      mdbook = (import inputs.nixpkgs-mdbook { inherit system; }).mdbook;

      rustBin = inputs.rust-overlay.lib.mkRustBin { } pkgs;
      rustToolchain = rustBin.fromRustupToolchainFile ../../rust-toolchain.toml;

      baseEnv =
        (zed-editor.overrideAttrs (attrs: {
          passthru.env = attrs.env;
        })).env; # exfil `env`; it's not in drvAttrs

      # cargo-shear as CI pins it (`.github/workflows/run_tests.yml`), since `script/clippy` passes
      # flags older releases lack. It needs a newer rustc than nixpkgs' default.
      cargoShear =
        (pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        }).buildRustPackage
          rec {
            pname = "cargo-shear";
            version = "1.13.4";
            src = pkgs.fetchCrate {
              inherit pname version;
              hash = "sha256-bTTNiDWkbyjxFo59Hkeah23lQEGiuH5Xrm6z7SUNtoQ=";
            };
            cargoHash = "sha256-Mhxrcc9ErTYIV+yuIROuMdhbxrFdZZ2yAAXK0WRWcRI=";
            # Its integration tests fail in the Nix build with missing files; the unit tests pass.
            cargoTestFlags = [ "--lib" ];
          };

      # Musl cross-compiler for building remote_server
      muslCross = pkgs.pkgsCross.musl64;

      # Cargo build timings wrapper script
      wrappedCargo = pkgs.writeShellApplication {
        name = "cargo";
        runtimeInputs = [ pkgs.nodejs ];
        text =
          let
            pathToCargoScript = ./. + "/../../script/cargo";
          in
          ''
            NIX_WRAPPER=1 CARGO=${rustToolchain}/bin/cargo ${pathToCargoScript} "$@"
          '';
      };
    in
    {
      devShells.default = (pkgs.mkShell.override { inherit (zed-editor) stdenv; }) {
        name = "zed-editor-dev";
        inputsFrom = [ zed-editor ];

        # `packages` below puts this shell's cargo first on PATH, which would bypass mbx's Cargo
        # shim (written by `mbx setup`); put the shim back in front when it's installed. mbx then
        # runs the next cargo on PATH, so the timing wrapper still applies.
        shellHook = ''
          mbx_cargo_shim_dir="''${XDG_DATA_HOME:-$HOME/.local/share}/mbx/bin"
          if [ -x "$mbx_cargo_shim_dir/cargo" ]; then
            PATH="$mbx_cargo_shim_dir:$PATH"
          fi
          unset mbx_cargo_shim_dir
          # A host `LD_LIBRARY_PATH` outranks the binaries' RUNPATH, so libraries built against a
          # newer glibc than this shell's would load and fail to start.
          unset LD_LIBRARY_PATH
        '';

        packages =
          with pkgs;
          [
            wrappedCargo # must be first, to shadow the `cargo` provided by `rustToolchain`
            rustToolchain # cargo, rustc, and rust-toolchain.toml components included
            cargo-nextest
            cargo-hakari
            cargoShear
            cargo-zigbuild
            # TODO: package protobuf-language-server for editing zed.proto
            # TODO: add other tools used in our scripts

            # `build.nix` adds this to the `zed-editor` wrapper (see `postFixup`)
            # we'll just put it on `$PATH`:
            nodejs_22
            zig

            # Documentation tooling: `nix develop -c mdbook build docs`. The docs
            # preprocessor runs through `docs/book.toml`'s `cargo run` default rather
            # than a prebuilt binary, because building it here from the working tree
            # rebuilt Zed's dependencies whenever the tree changed.
            mdbook

            # A11y testing infra
            gobject-introspection
            at-spi2-core
            (python3.withPackages (ps: [
              ps.pyatspi
              ps.pygobject3
            ]))
          ]
          ++ lib.optionals stdenv.hostPlatform.isLinux [ accerciser ];

        env =
          (removeAttrs baseEnv [
            "LK_CUSTOM_WEBRTC" # download the staticlib during the build as usual
            "ZED_UPDATE_EXPLANATION" # allow auto-updates
            "CARGO_PROFILE" # let you specify the profile
            "TARGET_DIR"
          ])
          // {
            # note: different than `$FONTCONFIG_FILE` in `build.nix` – this refers to relative paths
            # outside the nix store instead of to `$src`
            FONTCONFIG_FILE = pkgs.makeFontsConf {
              fontDirectories = [
                "./assets/fonts/lilex"
                "./assets/fonts/ibm-plex-sans"
              ];
            };
            PROTOC = "${pkgs.protobuf}/bin/protoc";

            ZED_ZSTD_MUSL_LIB = "${pkgs.pkgsCross.musl64.pkgsStatic.zstd.out}/lib";
            # For aws-lc-sys musl cross-compilation
            CC_x86_64_unknown_linux_musl = "${muslCross.stdenv.cc}/bin/x86_64-unknown-linux-musl-gcc";
          };
      };
    };
}
