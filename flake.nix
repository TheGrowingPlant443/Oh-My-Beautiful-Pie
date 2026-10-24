{
  description = "OMP Manager: Linux and macOS packages and development shell (Windows does not use Nix)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      perSystem = pkgs:
        let
          inherit (pkgs) lib stdenv;
          sdk = import ./nix/omp-sdk { inherit pkgs; };
          # Tree-sitter grammars used by the System overview parser worker (bridge/source-index).
          # Missing grammars are reported by the app as unavailable; they never break the build.
          sourceIndexPython = pkgs.python3.withPackages (ps: [ ps.tree-sitter ] ++
            map (name: ps.${name}) (lib.filter (name: builtins.hasAttr name ps) [
              "tree-sitter-python" "tree-sitter-javascript" "tree-sitter-typescript" "tree-sitter-go"
              "tree-sitter-rust" "tree-sitter-java" "tree-sitter-groovy" "tree-sitter-c" "tree-sitter-cpp"
              "tree-sitter-ruby" "tree-sitter-c-sharp" "tree-sitter-kotlin" "tree-sitter-scala"
              "tree-sitter-php" "tree-sitter-lua" "tree-sitter-swift" "tree-sitter-bash" "tree-sitter-powershell"
              "tree-sitter-zig" "tree-sitter-elixir" "tree-sitter-objc" "tree-sitter-julia"
              "tree-sitter-fortran" "tree-sitter-verilog"
            ]));
          linuxLibs = with pkgs; [
            webkitgtk_4_1 gtk3 libsoup_3 glib cairo pango gdk-pixbuf atk harfbuzz
            librsvg openssl glib-networking libayatana-appindicator gsettings-desktop-schemas
          ];
          # Keep the complete bridge/resource trees: Rust embeds these at compile time.
          # A checkout can contain very large build outputs and nested worktrees.
          source = lib.cleanSourceWith {
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
              && (toString path == toString ./. || builtins.elem root [
                "src" "src-tauri" "bridge" "resources" "package.json" "package-lock.json"
                "index.html" "vite.config.ts" "svelte.config.js" "tsconfig.json"
              ]);
          };
          frontend = pkgs.buildNpmPackage {
            pname = "omp-manager-frontend";
            version = "0.1.0";
            src = source;
            nodejs = pkgs.nodejs_24;
            # Every download is integrity-pinned by the checked-in npm lockfile.
            npmDeps = pkgs.importNpmLock { npmRoot = ./.; };
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
          package = pkgs.rustPlatform.buildRustPackage {
            pname = "omp-manager";
            version = "0.1.0";
            src = source;
            cargoLock.lockFile = ./src-tauri/Cargo.lock;
            cargoRoot = "src-tauri";
            buildAndTestSubdir = "src-tauri";
            # The Cargo dependency feature embeds dist without needing the Tauri CLI.
            buildFeatures = [ "tauri/custom-protocol" ];
            nativeBuildInputs = [ pkgs.pkg-config pkgs.cmake pkgs.makeWrapper ]
              ++ lib.optionals stdenv.hostPlatform.isLinux [ pkgs.wrapGAppsHook3 ];
            buildInputs = lib.optionals stdenv.hostPlatform.isLinux linuxLibs
              # Darwin frameworks come from the SDK supplied by the Nix stdenv.
              ++ lib.optionals stdenv.hostPlatform.isDarwin [ pkgs.libiconv ];
            preBuild = ''
              cp -r ${frontend} dist
            '';
            # Desktop/keyring/container smoke tests require an actual user session.
            doCheck = false;
            dontWrapGApps = true;
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
          };
        in
        {
          inherit package frontend sdk;
          shell = pkgs.mkShell ({
            packages = [
              pkgs.rustc pkgs.cargo pkgs.clippy pkgs.rustfmt pkgs.pkg-config pkgs.cmake
              pkgs.nodejs_24 pkgs.bun pkgs.git sourceIndexPython
            ] ++ lib.optionals stdenv.hostPlatform.isLinux (linuxLibs ++ [ pkgs.patchelf pkgs.xdg-utils pkgs.libsecret ])
              ++ lib.optionals stdenv.hostPlatform.isDarwin [ pkgs.libiconv ];
            OMP_SYSTEM_OVERVIEW_PYTHON = "${sourceIndexPython}/bin/python3";
            OMP_SDK_DIR = "${sdk}/node_modules/@oh-my-pi/pi-coding-agent";
          } // lib.optionalAttrs stdenv.hostPlatform.isLinux {
            LD_LIBRARY_PATH = lib.makeLibraryPath linuxLibs;
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
