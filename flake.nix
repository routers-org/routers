{
  description = "Routers — Rust-Based Routing Tooling for System-Agnostic Maps";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  inputs.systems.url = "github:nix-systems/default";
  inputs.flake-utils = {
    url = "github:numtide/flake-utils";
    inputs.systems.follows = "systems";
  };

  # Rust toolchain with the wasm targets needed to build the routers_wasm
  # component; nixpkgs' `rustc` ships no wasm std.
  inputs.fenix = {
    url = "github:nix-community/fenix";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    { nixpkgs, flake-utils, fenix, ... }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        inherit (pkgs) lib;

        # A single stable toolchain carrying rust-src plus the wasm targets the
        # component build and its `-Z build-std` fallback need.
        fenixPkgs = fenix.packages.${system};
        rustToolchain = fenixPkgs.combine [
          fenixPkgs.stable.rustc
          fenixPkgs.stable.cargo
          fenixPkgs.stable.clippy
          fenixPkgs.stable.rustfmt
          fenixPkgs.stable.rust-src
          fenixPkgs.targets.wasm32-unknown-unknown.stable.rust-std
          fenixPkgs.targets.wasm32-wasip1.stable.rust-std
          fenixPkgs.targets.wasm32-wasip2.stable.rust-std
        ];

        # Linked by openssl-sys/aws-lc-sys, dlopen'd by eframe in routers_viewer.
        libs =
          with pkgs;
          [ fontconfig openssl zlib ]
          ++ lib.optionals stdenv.hostPlatform.isLinux [
            libGL
            libx11
            libxcursor
            libxi
            libxkbcommon
            libxrandr
            vulkan-loader
            wayland
          ];

        # The GKE component is for `kubectl` and `helm`, which call it as an
        # exec credential plugin.
        gcloud = pkgs.google-cloud-sdk.withExtraComponents [
          pkgs.google-cloud-sdk.components.gke-gcloud-auth-plugin
        ];

        # RustRover is unfree, so it is resolved from its own nixpkgs
        # instance: `nix develop` stays usable without `allowUnfree`, which
        # only the `.#rustrover` shell below needs.
        unfreePkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
        };

        # The IDE reads the toolchain from a path typed into its settings, so
        # the shell links one at a fixed, per-checkout location that survives
        # store-path churn.
        ideToolchain = ".rustrover/toolchain";

        mkShell' = extraPackages: pkgs.mkShell {
          buildInputs = libs;

          packages = extraPackages ++ (with pkgs; [
            bashInteractive

            # Rust toolchain (rustc/cargo/clippy/rustfmt + wasm targets).
            rustToolchain
            rust-analyzer

            # WebAssembly component toolchain (libs/routers_wasm): build the
            # component, transpile consumers with jco (via pnpm dlx), run it
            # under wasmtime, optimise with wasm-opt.
            wasm-tools
            cargo-component
            wasmtime
            binaryen
            nodejs_22
            pnpm

            protobuf
            buf

            cargo-audit
            cargo-codspeed
            cargo-insta
            cargo-nextest
            git-cliff

            just
            pre-commit
            git-lfs
            curl
            unzip

            kubectl
            kubernetes-helm

            # The infrastructure itself lives in routers-org/infrastructure;
            # gcloud here is for kubectl, helm and pushing to Artifact Registry.
            gcloud
            rclone
            osmium-tool

            pkg-config
            cmake
            perl
            rustPlatform.bindgenHook

            natscli
          ]);

          env = {
            PROTOC = lib.getExe' pkgs.protobuf "protoc";
            OPENSSL_NO_VENDOR = "1";
            # Matches the toolchain above (rust-analyzer + `-Z build-std`).
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          };

          shellHook = ''
            export LD_LIBRARY_PATH="${lib.makeLibraryPath libs}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"

            # `buf generate` needs this plugin and nixpkgs has no derivation for
            # it. Pinned to the workspace's `buffa` version.
            export PATH="$PWD/.cargo-tools/bin:$PATH"
            mkdir -p .cargo-tools && echo "*" > .cargo-tools/.gitignore
            [ -x .cargo-tools/bin/protoc-gen-buffa-packaging ] || \
              cargo install --locked --quiet --root .cargo-tools \
                protoc-gen-buffa-packaging@0.6.0

            [ -d schema/src/proto ] || echo "run 'buf generate' before building"

            # Stable toolchain paths for IDEs that cannot follow a store path
            # (RustRover asks for both of these under Settings -> Rust).
            mkdir -p ${ideToolchain}
            # The indexer walks these under whatever umask the shell was
            # started with, so make them traversable and readable outright.
            chmod a+rx .rustrover ${ideToolchain}
            echo "*" > .rustrover/.gitignore
            ln -sfn "${rustToolchain}/bin" ${ideToolchain}/bin
            ln -sfn "${rustToolchain}/lib" ${ideToolchain}/lib
            if command -v rustrover > /dev/null; then
              echo "RustRover, Settings -> Rust:"
              echo "  toolchain location:    $PWD/${ideToolchain}/bin"
              echo "  standard library:      $PWD/${ideToolchain}/lib/rustlib/src/rust/library"
            fi
          '';
        };
      in
      {
        devShells.default = mkShell' [ ];

        # `nix develop .#rustrover` adds the IDE itself; launch it from the
        # shell with `rustrover .` so it inherits PATH, PROTOC and the linker
        # flags, then point Settings -> Rust at the paths printed on entry.
        devShells.rustrover = mkShell' [ unfreePkgs.jetbrains.rust-rover ];
      }
    );
}
