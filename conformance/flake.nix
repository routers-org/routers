{
  description = "routers-conformance — map-matching benchmark environment";

  inputs = {
    nixpkgs.url     = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;

          # valhalla is not officially supported on macOS in nixpkgs; allow it.
          config.allowUnsupportedSystem = true;

          overlays = [
            # macOS SDK 14+ deprecates sprintf; prime_server (valhalla dep)
            # uses it throughout and treats warnings as errors.  Suppress.
            # GDAL's Python test suite crashes with a BPT trap on macOS
            # (Python 3.13 / sandbox interaction); disable for the build.
            (final: prev: prev.lib.optionalAttrs prev.stdenv.isDarwin {
              prime-server = prev.prime-server.overrideAttrs (old: {
                env = (old.env or {}) // {
                  NIX_CFLAGS_COMPILE =
                    ((old.env or {}).NIX_CFLAGS_COMPILE or "")
                    + " -Wno-deprecated-declarations";
                };
              });

              gdal = prev.gdal.overrideAttrs (_old: {
                doCheck        = false;
                doInstallCheck = false;
              });

              # valhalla's custom FindSQLite3.cmake only searches Homebrew paths
              # and uses NO_DEFAULT_PATH on macOS.  It checks SQLITE3_INCLUDE_DIR
              # and SQLITE3_LIBRARY (all-caps) as pre-set cache variables.
              valhalla = prev.valhalla.overrideAttrs (old: {
                cmakeFlags = (old.cmakeFlags or []) ++ [
                  "-DSQLITE3_INCLUDE_DIR=${prev.sqlite.dev}/include"
                  "-DSQLITE3_LIBRARY=${prev.sqlite.out}/lib/libsqlite3.dylib"
                ];
              });
            })
          ];
        };

        # ── per-app runtime dependency sets ───────────────────────────────
        valhallaDeps     = [ pkgs.valhalla pkgs.osmium-tool pkgs.jq ];
        graphhopperDeps  = [ pkgs.graphhopper ];

        fmmBuildDeps = [
          pkgs.cmake
          pkgs.pkg-config
          pkgs.gdal
          pkgs.boost
          pkgs.libosmium
          pkgs.nlohmann_json   # used by fmm_server; preferred over FetchContent
          # cpp-httplib is not in nixpkgs — CMakeLists.txt FetchContents it
        ];

        fmmRunDeps  = [
          pkgs.gdal
          pkgs.osmium-tool
          # pbf_to_shp.py uses osgeo.ogr for shapefile writing
          (pkgs.python3.withPackages (ps: [ ps.gdal ]))
        ];

        # ── helper: turn a script file into a nix app ─────────────────────
        mkScriptApp = { name, runtimeInputs, scriptFile }:
          let
            drv = pkgs.writeShellApplication {
              inherit name runtimeInputs;
              # When invoked via `nix run`, $0 is in the nix store, so
              # script-relative path computation breaks.  Export CONFORMANCE_DIR
              # anchored to $PWD (where the user invoked `nix run` from) so all
              # scripts can compute their paths correctly.
              text = ''
                export CONFORMANCE_DIR="''${CONFORMANCE_DIR:-$PWD}"
              '' + builtins.readFile scriptFile;
            };
          in
          { type = "app"; program = "${drv}/bin/${name}"; };

      in {
        # ── dev shell: all tools in one place ─────────────────────────────
        devShells.default = pkgs.mkShell {
          buildInputs = valhallaDeps ++ graphhopperDeps ++ fmmBuildDeps ++ fmmRunDeps;

          shellHook = ''
            export CONFORMANCE_DIR="$PWD"
            export CONFORMANCE_PBF="''${CONFORMANCE_PBF:-$CONFORMANCE_DIR/../libs/routers_fixtures/resources/los-angeles-minified.osm.pbf}"
            export CONFORMANCE_WORK="''${CONFORMANCE_WORK:-$CONFORMANCE_DIR/.work}"

            echo ""
            echo "  routers-conformance dev shell"
            echo "  PBF:  $CONFORMANCE_PBF"
            echo "  WORK: $CONFORMANCE_WORK"
            echo ""
            echo "  One-time setup:"
            echo "    nix run .#setup-valhalla      build Valhalla tiles"
            echo "    nix run .#setup-fmm           clone+build FMM + convert road network"
            echo "    nix run .#setup-graphhopper   pre-import GraphHopper graph (optional)"
            echo ""
            echo "  Benchmark (each recipe manages its own service lifecycle):"
            echo "    just conform::valhalla         port 8002"
            echo "    just conform::graphhopper      port 8989"
            echo "    just conform::fmm              port 9090"
            echo ""
          '';
        };

        # ── apps ──────────────────────────────────────────────────────────
        apps = {
          setup-valhalla = mkScriptApp {
            name           = "setup-valhalla";
            runtimeInputs  = valhallaDeps;
            scriptFile     = ./scripts/setup-valhalla.sh;
          };

          start-valhalla = mkScriptApp {
            name           = "start-valhalla";
            runtimeInputs  = valhallaDeps;
            scriptFile     = ./scripts/start-valhalla.sh;
          };

          setup-graphhopper = mkScriptApp {
            name           = "setup-graphhopper";
            runtimeInputs  = graphhopperDeps;
            scriptFile     = ./scripts/setup-graphhopper.sh;
          };

          start-graphhopper = mkScriptApp {
            name           = "start-graphhopper";
            runtimeInputs  = graphhopperDeps;
            scriptFile     = ./scripts/start-graphhopper.sh;
          };

          setup-fmm =
            # Several nix packages are split into dev (headers/cmake) and out
            # (libraries) outputs that the writeShellApplication PATH mechanism
            # won't expose automatically.  Embed their store paths directly so
            # cmake — running outside a derivation — can find config files.
            let
              boostDev = pkgs.boost.dev;
              ompLib   = pkgs.llvmPackages.openmp;
              ompDev   = pkgs.llvmPackages.openmp.dev;
              drv = pkgs.writeShellApplication {
                name          = "setup-fmm";
                runtimeInputs = fmmBuildDeps ++ fmmRunDeps ++ [ boostDev ompLib ompDev ];
                text = ''
                  export CONFORMANCE_DIR="''${CONFORMANCE_DIR:-$PWD}"
                  # boost.dev: headers + BoostConfig.cmake
                  export CMAKE_PREFIX_PATH="${boostDev}''${CMAKE_PREFIX_PATH:+;$CMAKE_PREFIX_PATH}"
                  # OpenMP: Apple clang needs these set explicitly; FindOpenMP can't auto-detect
                  export OMP_LIBRARY="${ompLib}/lib/libomp.dylib"
                  export OMP_INCLUDE="${ompDev}/include"
                  # FMM's cmake never calls include_directories(OpenMP_INCLUDE_DIRS).
                  # Expose omp.h to the compiler via CPATH (honoured by Apple clang).
                  export CPATH="${ompDev}/include''${CPATH:+:$CPATH}"
                '' + builtins.readFile ./scripts/setup-fmm.sh;
              };
            in
            { type = "app"; program = "${drv}/bin/setup-fmm"; };

          start-fmm = mkScriptApp {
            name           = "start-fmm";
            runtimeInputs  = fmmRunDeps;
            scriptFile     = ./scripts/start-fmm.sh;
          };
        };
      }
    );
}
