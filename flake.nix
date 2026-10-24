{
  description = "OMP Manager: Linux and macOS development shell (the Windows build does not use Nix)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAll (pkgs:
        let
          inherit (pkgs) lib stdenv;
          # Tree-sitter grammars used by the System overview parser worker (bridge/source-index).
          # Missing grammars are reported by the app as unavailable; they never break the build.
          sourceIndexPython = pkgs.python3.withPackages (ps: [ ps.tree-sitter ]);
          linuxLibs = with pkgs; [
            webkitgtk_4_1 gtk3 libsoup_3 glib cairo pango gdk-pixbuf atk harfbuzz
            librsvg openssl glib-networking libayatana-appindicator
          ];
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.rustc pkgs.cargo pkgs.clippy pkgs.rustfmt pkgs.pkg-config pkgs.cmake
              pkgs.nodejs_24 pkgs.bun pkgs.git sourceIndexPython
            ] ++ lib.optionals stdenv.hostPlatform.isLinux (linuxLibs ++ [ pkgs.patchelf pkgs.xdg-utils ]);

            OMP_SYSTEM_OVERVIEW_PYTHON = "${sourceIndexPython}/bin/python3";
          } // lib.optionalAttrs stdenv.hostPlatform.isLinux {
            LD_LIBRARY_PATH = lib.makeLibraryPath linuxLibs;
            GIO_MODULE_DIR = "${pkgs.glib-networking}/lib/gio/modules";
            XDG_DATA_DIRS = lib.concatStringsSep ":" [
              "${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}"
              "${pkgs.gtk3}/share/gsettings-schemas/${pkgs.gtk3.name}"
            ];
          };
        });
    };
}
