{
  description = "Independent OMBP start app and rootless Podman access gateways";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/5adc50831c259f87faeb09ddf930d43597197473";
  outputs = { self, nixpkgs }: let
    systems = [ "x86_64-linux" "aarch64-linux" ];
    each = nixpkgs.lib.genAttrs systems;
    environment = system: let
      pkgs = nixpkgs.legacyPackages.${system};
      python = pkgs.python3.withPackages (ps: [ ps.fusepy ps.mitmproxy ]);
      testPython = pkgs.python3.withPackages (ps: [ ps.fusepy ps.mitmproxy ps.coverage ]);
      # Keep OMBP's 720x480 minimum non-fatal and give the nested desktop the full host allocation.
      weston = pkgs.weston.overrideAttrs (old: {
        patches = (old.patches or [ ]) ++ [ ./weston-min-size.patch ./weston-frameless.patch ];
      });
      source = pkgs.lib.cleanSourceWith {
        src = self;
        filter = path: _type: !(builtins.elem (baseNameOf path) [
          ".quality-coverage" ".coverage" "__pycache__" "result"
        ]);
      };
      fusermount = pkgs.writeShellScriptBin "fusermount" ''
        for helper in /run/wrappers/bin/fusermount /usr/bin/fusermount /usr/bin/fusermount3; do
          if [ -x "$helper" ]; then exec "$helper" "$@"; fi
        done
        echo "A host setuid fusermount or fusermount3 helper is required" >&2
        exit 1
      '';
    in { inherit pkgs python testPython fusermount source weston; };
  in {
    checks = each (system: let inherit (environment system) pkgs python source; in {
      syntax = pkgs.runCommand "ombp-start-syntax" { nativeBuildInputs = [ python pkgs.nodejs_24 ]; } ''
        export PYTHONPYCACHEPREFIX="$TMPDIR/bytecode"
        python3 -m compileall -q ${source}
        node --check ${source}/ui.js
        touch "$out"
      '';
    });
    packages = each (system: let
      inherit (environment system) pkgs python fusermount source weston;
    in {
      default = pkgs.writeShellApplication {
        name = "ombp-start";
        runtimeInputs = [ python fusermount pkgs.nix pkgs.podman pkgs.dbus pkgs.gnome-keyring pkgs.libsecret ];
        text = ''
          export FUSE_LIBRARY_PATH=${pkgs.lib.getLib pkgs.fuse}/lib/libfuse.so.2
          export START_APP_PYTHON=${python}/bin/python3
          export START_APP_DBUS=${pkgs.dbus}/bin/dbus-run-session
          export START_APP_KEYRING=${pkgs.gnome-keyring}/bin/gnome-keyring-daemon
          export START_APP_MESA=${pkgs.mesa}
          export START_APP_SHELL=${pkgs.bashInteractive}/bin/bash
          export START_APP_COREUTILS=${pkgs.coreutils}/bin
          export START_APP_WESTON=${weston}/bin/weston
          export START_APP_FONTCONFIG=${pkgs.makeFontsConf { fontDirectories = [ pkgs.dejavu_fonts ]; }}
          exec ${python}/bin/python3 ${source}/app.py "$@"
        '';
      };
    });
    devShells = each (system: let inherit (environment system) pkgs python testPython fusermount weston; in {
      default = pkgs.mkShell {
        packages = [ testPython fusermount pkgs.nix pkgs.podman pkgs.dbus pkgs.gnome-keyring pkgs.libsecret ];
        FUSE_LIBRARY_PATH = "${pkgs.lib.getLib pkgs.fuse}/lib/libfuse.so.2";
        START_APP_PYTHON = "${python}/bin/python3";
        START_APP_DBUS = "${pkgs.dbus}/bin/dbus-run-session";
        START_APP_KEYRING = "${pkgs.gnome-keyring}/bin/gnome-keyring-daemon";
        START_APP_MESA = "${pkgs.mesa}";
        START_APP_SHELL = "${pkgs.bashInteractive}/bin/bash";
        START_APP_COREUTILS = "${pkgs.coreutils}/bin";
        START_APP_WESTON = "${weston}/bin/weston";
        START_APP_FONTCONFIG = "${pkgs.makeFontsConf { fontDirectories = [ pkgs.dejavu_fonts ]; }}";
      };
    });
  };
}
