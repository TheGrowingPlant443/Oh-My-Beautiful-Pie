{
  description = "OMP Manager: Linux and macOS packages and development shell (Windows does not use Nix)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  inputs.fenix = {
    url = "github:nix-community/fenix";
    inputs.nixpkgs.follows = "nixpkgs";
  };
  inputs.crane.url = "github:ipetkov/crane";

  outputs = { self, nixpkgs, fenix, crane }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      perSystem = pkgs:
        let
          inherit (pkgs) lib stdenv;
          craneLib = crane.mkLib pkgs;
          sdk = import ./nix/omp-sdk { inherit pkgs; };
          sdkCommand = pkgs.writeShellScript "omp-manager-cli" ''
            unset PI_COMPILED
            exec "''${BUN_BIN:-${pkgs.bun}/bin/bun}" ${sdk}/node_modules/@oh-my-pi/pi-coding-agent/dist/cli.js "$@"
          '';
          # PHP needs both upstream entry points, not the generic single-grammar binding.
          phpGrammar = pkgs.python3Packages.buildPythonPackage {
            pname = "tree-sitter-php";
            inherit (pkgs.tree-sitter-grammars.tree-sitter-php) version src;
            pyproject = true;
            build-system = with pkgs.python3Packages; [ setuptools wheel ];
            postPatch = ''
              substituteInPlace pyproject.toml --replace-fail 'license = "LICENSE"' 'license = "MIT"'
            '';
            nativeCheckInputs = with pkgs.python3Packages; [ tree-sitter pytestCheckHook ];
            pytestFlags = [ "bindings/python/tests" ];
            pythonImportsCheck = [ "tree_sitter_php" ];
          };
          # Tree-sitter grammars used by the System overview parser worker (bridge/source-index).
          sourceIndexPythonWith = extra: pkgs.python3.withPackages (ps:
            let
              # The visitors require these upstream schemas, not Nixpkgs' alternative grammars.
              jvmGrammar = language: version: src:
                ps.tree-sitter-grammars."tree-sitter-${language}".override {
                  grammarDrv = pkgs.tree-sitter-grammars."tree-sitter-${language}".overrideAttrs {
                    inherit version src;
                  };
                };
              grammarPackages = ps.tree-sitter-grammars // ps // {
                tree-sitter-php = phpGrammar;
                tree-sitter-groovy = jvmGrammar "groovy" "0.1.2" (pkgs.fetchFromGitHub {
                  owner = "amaanq";
                  repo = "tree-sitter-groovy";
                  rev = "70efb0b9b50f95bcbd89dcfd42b275e0304e10cf";
                  hash = "sha256-usgT3dOq5Tg1wet4jCcS47Dn+2psl7dPRjcimjZClBk=";
                });
                tree-sitter-kotlin = jvmGrammar "kotlin" "1.1.0" (pkgs.fetchFromGitHub {
                  owner = "tree-sitter-grammars";
                  repo = "tree-sitter-kotlin";
                  rev = "77dd60ea0a9003ce062c9728a513ffe1aaff8c82";
                  hash = "sha256-6jjK5rA/lEdsYDboU7wGfzEiRdZo44SrLlcgWci0xa4=";
                });
              };
            in
            [ ps.tree-sitter ] ++ extra ps ++
            map (name: grammarPackages.${name}) [
              "tree-sitter-python" "tree-sitter-javascript" "tree-sitter-typescript" "tree-sitter-go"
              "tree-sitter-rust" "tree-sitter-java" "tree-sitter-groovy" "tree-sitter-c" "tree-sitter-cpp"
              "tree-sitter-ruby" "tree-sitter-c-sharp" "tree-sitter-kotlin" "tree-sitter-scala"
              "tree-sitter-php" "tree-sitter-lua" "tree-sitter-swift" "tree-sitter-bash" "tree-sitter-powershell"
              "tree-sitter-zig" "tree-sitter-elixir" "tree-sitter-objc" "tree-sitter-julia"
              "tree-sitter-fortran" "tree-sitter-verilog"
            ]);
          sourceIndexPython = sourceIndexPythonWith (_: []);
          coveragePython = sourceIndexPythonWith (ps: [ ps.coverage ]);
          testNodeModules = pkgs.importNpmLock.buildNodeModules {
            npmRoot = frontendSource;
            nodejs = pkgs.nodejs_24;
            derivationArgs = {
              npmFlags = [ "--ignore-scripts" ];
              npmRebuildFlags = [ "--ignore-scripts" ];
            };
          };
          coverageCargoVendor = pkgs.rustPlatform.importCargoLock {
            lockFile = ./src-tauri/Cargo.lock;
          };
          # Keep coverage's compiler, profiler runtime and readers from one upstream toolchain.
          coverageRust = fenix.packages.${stdenv.hostPlatform.system}.stable.withComponents [
            "cargo" "rustc" "rust-std" "rustfmt" "clippy" "llvm-tools-preview"
          ];
          coverageLlvm = "${coverageRust}/lib/rustlib/${stdenv.hostPlatform.rust.rustcTarget}/bin";
          coverageTool = (pkgs.cargo-llvm-cov.override {
            rustPlatform = pkgs.makeRustPlatform { cargo = coverageRust; rustc = coverageRust; };
          }).overrideAttrs (old: {
            env = (old.env or {}) // {
              LLVM_COV = "${coverageLlvm}/llvm-cov";
              LLVM_PROFDATA = "${coverageLlvm}/llvm-profdata";
            };
          });
          linuxLibs = with pkgs; [
            webkitgtk_4_1 gtk3 libsoup_3 glib cairo pango gdk-pixbuf atk harfbuzz
            librsvg openssl glib-networking libayatana-appindicator gsettings-desktop-schemas
          ];
          # Keep the complete bridge/resource trees: Rust embeds these at compile time.
          # A checkout can contain very large build outputs and nested worktrees.
          sourceFor = roots: lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              let
                relative = lib.removePrefix (toString ./. + "/") (toString path);
                parts = lib.splitString "/" relative;
                root = builtins.head parts;
              in
              lib.cleanSourceFilter path type
              && !(builtins.any (part: builtins.elem part [
                "target" "node_modules" "dist" ".worktrees" "worktrees" "result" "__pycache__"
              ]) parts)
              && (toString path == toString ./. || builtins.elem root roots);
          };
          frontendRoots = [
            "src" "package.json" "package-lock.json" "index.html"
            "vite.config.ts" "svelte.config.js" "tsconfig.json"
          ];
          frontendSource = sourceFor frontendRoots;
          source = sourceFor ([ "src-tauri" "bridge" "resources" ] ++ frontendRoots);
          frontend = pkgs.buildNpmPackage {
            pname = "omp-manager-frontend";
            version = "0.1.0";
            src = frontendSource;
            nodejs = pkgs.nodejs_24;
            # Every download is integrity-pinned by the checked-in npm lockfile.
            npmDeps = pkgs.importNpmLock { npmRoot = frontendSource; };
            npmConfigHook = pkgs.importNpmLock.npmConfigHook;
            npmRebuildFlags = [ "--ignore-scripts" ];
            npmFlags = [ "--ignore-scripts" ];
            installPhase = ''
              runHook preInstall
              mkdir -p "$out"
              cp -r dist/. "$out/"
              runHook postInstall
            '';
          };
          runtimeTools = [ pkgs.bun pkgs.nodejs_24 pkgs.git sourceIndexPython ]
            ++ lib.optionals stdenv.hostPlatform.isLinux [ pkgs.libsecret pkgs.xdg-utils ];
          desktopItem = pkgs.makeDesktopItem {
            name = "omp-manager";
            desktopName = "OMP Manager";
            comment = "Local dashboard for independent Oh My Pi processes";
            exec = "omp-manager";
            terminal = false;
            categories = [ "Development" ];
          };
          rustArgs = {
            pname = "omp-manager";
            version = "0.1.0";
            src = source;
            postUnpack = ''sourceRoot="$sourceRoot/src-tauri"'';
            cargoLock = pkgs.writeText "Cargo.lock" (builtins.readFile ./src-tauri/Cargo.lock);
            # Both cache and application must enable the same embedded-asset feature.
            cargoExtraArgs = "--locked --features tauri/custom-protocol";
            nativeBuildInputs = [ pkgs.pkg-config pkgs.cmake pkgs.makeWrapper ]
              ++ lib.optionals stdenv.hostPlatform.isLinux [ pkgs.wrapGAppsHook3 ];
            buildInputs = lib.optionals stdenv.hostPlatform.isLinux linuxLibs
              # Darwin frameworks come from the SDK supplied by the Nix stdenv.
              ++ lib.optionals stdenv.hostPlatform.isDarwin [ pkgs.libiconv ];
            # Desktop/keyring/container smoke tests require an actual user session.
            doCheck = false;
            dontWrapGApps = true;
          };
          cargoArtifacts = craneLib.buildDepsOnly (rustArgs // {
            # Crane's dummy source puts an explicit lock at its root; this crate is nested.
            extraDummyScript = "mv $out/Cargo.lock $out/src-tauri/Cargo.lock";
          });
          package = craneLib.buildPackage (rustArgs // {
            inherit cargoArtifacts;
            preBuild = ''
              cp -r ${frontend} ../dist
            '';
            postInstall = lib.optionalString stdenv.hostPlatform.isLinux ''
              mkdir -p "$out/share/applications"
              cp ${desktopItem}/share/applications/*.desktop "$out/share/applications/"
              substituteInPlace "$out/share/applications/omp-manager.desktop" \
                --replace-fail 'Exec=omp-manager' "Exec=$out/bin/omp-manager"
            '' + lib.optionalString stdenv.hostPlatform.isDarwin ''
              mkdir -p "$out/Applications/OMP Manager.app/Contents/MacOS"
              ln -s "$out/bin/omp-manager" "$out/Applications/OMP Manager.app/Contents/MacOS/omp-manager"
              cat > "$out/Applications/OMP Manager.app/Contents/Info.plist" <<'PLIST'
              <?xml version="1.0" encoding="UTF-8"?>
              <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
              <plist version="1.0"><dict>
                <key>CFBundleExecutable</key><string>omp-manager</string>
                <key>CFBundleIdentifier</key><string>dev.udvikler.omp-manager</string>
                <key>CFBundleName</key><string>OMP Manager</string>
                <key>CFBundlePackageType</key><string>APPL</string>
                <key>CFBundleShortVersionString</key><string>0.1.0</string>
                <key>NSHighResolutionCapable</key><true/>
              </dict></plist>
              PLIST
            '';
            preFixup = ''
              makeWrapperArgs=(
                --suffix PATH : ${lib.makeBinPath runtimeTools}
                --set-default OMP_SYSTEM_OVERVIEW_PYTHON ${sourceIndexPython}/bin/python3
                --set-default OMP_SDK_DIR ${sdk}/node_modules/@oh-my-pi/pi-coding-agent
                --set-default OMP_BIN ${sdkCommand}
              )
            '' + lib.optionalString stdenv.hostPlatform.isLinux ''
              makeWrapperArgs+=( "''${gappsWrapperArgs[@]}" )
            '' + ''
              wrapProgram "$out/bin/omp-manager" "''${makeWrapperArgs[@]}"
            '';
            meta = {
              description = "Local desktop dashboard for independent Oh My Pi processes";
              mainProgram = "omp-manager";
              platforms = systems;
            };
          });
        in
        {
          inherit package frontend sdk;
          shell = pkgs.mkShell ({
            packages = [
              coverageRust pkgs.pkg-config pkgs.cmake
              pkgs.nodejs_24 pkgs.bun pkgs.git coveragePython coverageTool
            ] ++ lib.optionals stdenv.hostPlatform.isLinux (linuxLibs ++ [ pkgs.patchelf pkgs.xdg-utils pkgs.libsecret ])
              ++ lib.optionals stdenv.hostPlatform.isDarwin [ pkgs.libiconv ];
            OMP_SYSTEM_OVERVIEW_PYTHON = "${sourceIndexPython}/bin/python3";
            OMP_SDK_DIR = "${sdk}/node_modules/@oh-my-pi/pi-coding-agent";
            OMP_BIN = "${sdkCommand}";
            OMP_COVERAGE_PYTHON = "${coveragePython}/bin/python3";
            OMP_TEST_NODE_MODULES = "${testNodeModules}/node_modules";
            LLVM_COV = "${coverageLlvm}/llvm-cov";
            LLVM_PROFDATA = "${coverageLlvm}/llvm-profdata";
            OMP_COVERAGE_CARGO = "${coverageRust}/bin/cargo";
            OMP_COVERAGE_CARGO_VENDOR = "${coverageCargoVendor}";
          } // lib.optionalAttrs stdenv.hostPlatform.isLinux {
            LD_LIBRARY_PATH = lib.makeLibraryPath (linuxLibs ++ [ pkgs.dbus.lib ]);
            GIO_MODULE_DIR = "${pkgs.glib-networking}/lib/gio/modules";
            XDG_DATA_DIRS = lib.concatStringsSep ":" [
              "${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}"
              "${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}"
            ];
          });
        };
    in
    {
      packages = forAll (pkgs: let values = perSystem pkgs; in {
        default = values.package;
        omp-manager = values.package;
        frontend = values.frontend;
        sdk = values.sdk;
      });
      apps = forAll (pkgs: {
        default = {
          type = "app";
          program = "${(perSystem pkgs).package}/bin/omp-manager";
        };
      });
      devShells = forAll (pkgs: { default = (perSystem pkgs).shell; });
    };
}
