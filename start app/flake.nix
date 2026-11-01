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
      # Keep OMBP's 720x480 minimum non-fatal, give the nested desktop the full host allocation,
      # stop busy clients overflowing libwayland's 4 KiB connection buffer, and let the browser
      # display maximize (keeping Chrome's address bar) instead of fullscreening.
      weston = pkgs.weston.overrideAttrs (old: {
        patches = (old.patches or [ ]) ++ [ ./weston-min-size.patch ./weston-frameless.patch ./weston-client-buffer.patch
                                            ./weston-kiosk-maximize.patch ];
      });
      source = pkgs.lib.cleanSourceWith {
        src = self;
        filter = path: _type: !(builtins.elem (baseNameOf path) [
          ".quality-coverage" ".coverage" "__pycache__" "result"
        ]);
      };
      # The sandbox has no /etc/fonts: include fontconfig's own conf.d so generic families (monospace,
      # sans-serif) resolve; without them WebKit cannot lay out OMBP's xterm terminal and it stays blank.
      fonts = pkgs.writeText "ombp-fonts.conf" ''
        <?xml version="1.0"?>
        <!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
        <fontconfig>
          <include>${pkgs.makeFontsConf { fontDirectories = [ pkgs.dejavu_fonts ]; }}</include>
          <include ignore_missing="yes">${pkgs.fontconfig.out}/etc/fonts/conf.d</include>
        </fontconfig>
      '';
      fusermount = pkgs.writeShellScriptBin "fusermount" ''
        for helper in /run/wrappers/bin/fusermount /usr/bin/fusermount /usr/bin/fusermount3; do
          if [ -x "$helper" ]; then exec "$helper" "$@"; fi
        done
        echo "A host setuid fusermount or fusermount3 helper is required" >&2
        exit 1
      '';
      # OMBP finds browsers by these names; each opens the browser in its own nested desktop window.
      browser = pkgs.runCommand "ombp-sandbox-browser" { } ''
        mkdir -p "$out/bin" "$out/libexec"
        { echo "#!${python}/bin/python3"; cat ${./browser.py}; } > "$out/libexec/ombp-browser"
        chmod +x "$out/libexec/ombp-browser"
        for name in google-chrome-stable google-chrome chromium chromium-browser chrome brave-browser; do
          ln -s "$out/libexec/ombp-browser" "$out/bin/$name"
        done
      '';
      # Everyday CLI tools for the AI inside the sandbox; files and network still pass the gateways.
      tools = pkgs.buildEnv {
        name = "ombp-sandbox-tools";
        paths = with pkgs; [
          glib.bin procps psmisc util-linux findutils gnugrep gnused gawk diffutils patch file which less
          gnutar gzip bzip2 xz zstd unzip zip curl wget jq ripgrep fd rsync tree gnumake bc strace lsof
          ncurses libnotify
        ];
        pathsToLink = [ "/bin" ];
      };
    in { inherit pkgs python testPython fusermount source weston fonts browser tools; };
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
      inherit (environment system) pkgs python fusermount source weston fonts browser tools;
    in {
      default = pkgs.writeShellApplication {
        name = "ombp-start";
        runtimeInputs = [ python fusermount pkgs.nix pkgs.podman pkgs.dbus pkgs.gnome-keyring pkgs.libsecret ];
        text = ''
          export FUSE_LIBRARY_PATH=${pkgs.lib.getLib pkgs.fuse}/lib/libfuse.so.2
          export START_APP_PYTHON=${python}/bin/python3
          export START_APP_DBUS=${pkgs.dbus}/bin/dbus-run-session
          export START_APP_CERTUTIL=${pkgs.nssTools}/bin/certutil
          export START_APP_KEYRING=${pkgs.gnome-keyring}/bin/gnome-keyring-daemon
          export START_APP_MESA=${pkgs.mesa}
          export START_APP_SHELL=${pkgs.bashInteractive}/bin/bash
          export START_APP_COREUTILS=${pkgs.coreutils}/bin
          export START_APP_WESTON=${weston}/bin/weston
          export START_APP_FONTCONFIG=${fonts}
          export START_APP_PODMAN=${pkgs.podman}/bin
          export START_APP_GH=${pkgs.gh}/bin
          export START_APP_OPENSSH=${pkgs.openssh}/bin
          export START_APP_CHROMIUM=${pkgs.chromium}/bin/chromium
          export START_APP_BROWSER_NAMES=${browser}/bin
          export START_APP_TOOLS=${tools}/bin
          exec ${python}/bin/python3 ${source}/app.py "$@"
        '';
      };
    });
    devShells = each (system: let inherit (environment system) pkgs python testPython fusermount weston fonts browser tools; in {
      default = pkgs.mkShell {
        packages = [ testPython fusermount pkgs.nix pkgs.podman pkgs.dbus pkgs.gnome-keyring pkgs.libsecret ];
        FUSE_LIBRARY_PATH = "${pkgs.lib.getLib pkgs.fuse}/lib/libfuse.so.2";
        START_APP_PYTHON = "${python}/bin/python3";
        START_APP_DBUS = "${pkgs.dbus}/bin/dbus-run-session";
        START_APP_CERTUTIL = "${pkgs.nssTools}/bin/certutil";
        START_APP_KEYRING = "${pkgs.gnome-keyring}/bin/gnome-keyring-daemon";
        START_APP_MESA = "${pkgs.mesa}";
        START_APP_SHELL = "${pkgs.bashInteractive}/bin/bash";
        START_APP_COREUTILS = "${pkgs.coreutils}/bin";
        START_APP_WESTON = "${weston}/bin/weston";
        START_APP_FONTCONFIG = "${fonts}";
        START_APP_PODMAN = "${pkgs.podman}/bin";
        START_APP_GH = "${pkgs.gh}/bin";
        START_APP_OPENSSH = "${pkgs.openssh}/bin";
        START_APP_CHROMIUM = "${pkgs.chromium}/bin/chromium";
        START_APP_BROWSER_NAMES = "${browser}/bin";
        START_APP_TOOLS = "${tools}/bin";
      };
    });
  };
}
