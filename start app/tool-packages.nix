{ pkgs, unfreePkgs }:
let
  inherit (pkgs) lib;
  bundle = name: paths: pkgs.buildEnv {
    inherit name paths;
    pathsToLink = [ "/bin" ];
  };
  rustToolchain = bundle "ombp-rust-toolchain" [ pkgs.rustc pkgs.cargo pkgs.clippy pkgs.gcc ];
  texToolchain = pkgs.texlive.withPackages (ps: [ ps.scheme-basic ps.latexmk ps.chktex ]);
  phpDebug = pkgs.vscode-extensions.xdebug.php-debug;
  rubyLsp = pkgs.ruby-lsp.overrideAttrs (old: {
    # Nix owns server updates; leave composed-bundle setup and project dependency validation intact.
    postInstall = (old.postInstall or "") + ''
      substituteInPlace "$out/${pkgs.ruby.gemPath}/gems/ruby-lsp-${pkgs.ruby-lsp.version}/lib/ruby_lsp/setup_bundler.rb" \
        --replace-fail "# If the last updated file doesn't exist or was updated more than 4 hours ago, we should update" \
          '# Nix supplies server updates; project bundles are still checked on every startup.' \
        --replace-fail '!@last_updated_path.exist? || Time.parse(@last_updated_path.read) < (Time.now - FOUR_HOURS)' \
          'false'
    '';
  });
  bashDebugSource = pkgs.fetchurl {
    url = "https://github.com/rogalmic/vscode-bash-debug/releases/download/untagged-438733f35feb8659d939/bash-debug-0.3.9.vsix";
    hash = "sha256-dgUmXrPeZD8IF9mxhw7sDTan3loNUGKO21mTfxUV+rw=";
  };
in {
  marksman = pkgs.marksman;
  emmet = pkgs.emmet-language-server;
  typescript-lsp = pkgs.typescript-language-server;
  # SDK discovery needs a real bin/tsc and package.json; keep the native compiler's library layout intact.
  typescript = pkgs.runCommand "ombp-typescript-${pkgs.typescript.version}" {
    nativeBuildInputs = [ pkgs.makeWrapper ];
  } ''
    mkdir -p "$out/bin"
    makeWrapper ${pkgs.typescript}/bin/tsc "$out/bin/tsc"
    ln -s tsc "$out/bin/tsgo"
    cat > "$out/package.json" <<'JSON'
    ${builtins.toJSON { name = "typescript"; version = pkgs.typescript.version; bin = { tsc = "bin/tsc"; tsgo = "bin/tsgo"; }; }}
    JSON
  '';
  html-lsp = pkgs.vscode-langservers-extracted;
  nil = bundle "ombp-nil" [ pkgs.nil pkgs.nix ];
  nixd = bundle "ombp-nixd" [ pkgs.nixd pkgs.nix ];
  java-lsp = pkgs.jdt-language-server;
  rust-analyzer = bundle "ombp-rust-analyzer" [ pkgs.rust-analyzer rustToolchain ];
  clangd = pkgs.clang-tools;
  zls = bundle "ombp-zls" [ pkgs.zls pkgs.zig ];
  gopls = bundle "ombp-gopls" [ pkgs.gopls pkgs.go ];
  biome = pkgs.biome;
  denols = pkgs.deno;
  tailwindcss = pkgs.tailwindcss-language-server;
  svelte = pkgs.svelte-language-server;
  vue-language-server = pkgs.vue-language-server;
  astro = pkgs.astro-language-server.overrideAttrs (old: {
    # OMP's default init options omit tsdk; explicit project SDK paths still win.
    postInstall = (old.postInstall or "") + ''
      substituteInPlace "$out/lib/node_modules/astro-language-server/packages/language-tools/language-server/dist/nodeServer.js" \
        --replace-fail 'const tsdk = params.initializationOptions?.typescript?.tsdk;' \
        'const tsdk = params.initializationOptions?.typescript?.tsdk ?? "${pkgs.typescript_5}/lib/node_modules/typescript/lib";'
    '';
  });
  pyright = pkgs.pyright;
  basedpyright = pkgs.basedpyright;
  pylsp = pkgs.python3Packages.python-lsp-server;
  ty = pkgs.ty;
  ruff = pkgs.ruff;
  metals = pkgs.metals;
  hls = bundle "ombp-hls" [ pkgs.haskell-language-server pkgs.ghc pkgs.cabal-install ];
  ocamllsp = bundle "ombp-ocamllsp" [ pkgs.ocamlPackages.ocaml-lsp pkgs.ocamlPackages.ocaml pkgs.ocamlPackages.dune_3 ];
  expert = bundle "ombp-expert" [ pkgs.beamPackages.expert pkgs.beamPackages.elixir ];
  gleam = pkgs.gleam;
  solargraph = pkgs.solargraph;
  ruby-lsp = pkgs.symlinkJoin {
    name = "ombp-ruby-lsp";
    paths = [ rubyLsp ];
    nativeBuildInputs = [ pkgs.makeWrapper ];
    # Nix's gem launcher changes Gem.paths in-process, but composed-bundle subprocesses need GEM_PATH.
    postBuild = ''
      wrapProgram "$out/bin/ruby-lsp" --suffix GEM_PATH : ${lib.makeSearchPath pkgs.ruby.gemPath
        (lib.closePropagation [ rubyLsp pkgs.rubyPackages.debug pkgs.ruby ])}
    '';
  };
  rubocop = pkgs.rubocop;
  bashls = bundle "ombp-bashls" [ pkgs.bash-language-server pkgs.shellcheck ];
  lua-language-server = pkgs.lua-language-server;
  intelephense = unfreePkgs.intelephense;
  phpactor = pkgs.phpactor;
  omnisharp = pkgs.runCommand "ombp-omnisharp" { } ''
    mkdir -p "$out/bin"
    ln -s ${pkgs.omnisharp-roslyn}/bin/OmniSharp "$out/bin/omnisharp"
  '';
  yamlls = pkgs.yaml-language-server;
  terraformls = pkgs.terraform-ls;
  dockerls = pkgs.dockerfile-language-server;
  helm-ls = bundle "ombp-helm-ls" [ pkgs.helm-ls pkgs.yaml-language-server ];
  ols = bundle "ombp-ols" [ pkgs.ols pkgs.odin ];
  dartls = pkgs.dart;
  texlab = bundle "ombp-texlab" [ pkgs.texlab texToolchain ];
  graphql = pkgs.graphql-language-service-cli;
  prismals = pkgs.prisma-language-server;
  vimls = pkgs.vim-language-server;
  swiftlint = pkgs.swiftlint;

  rust-toolchain = rustToolchain;
  go = pkgs.go;
  zig = pkgs.zig;
  jdk = pkgs.jdk;
  ghc = pkgs.ghc;
  cabal = pkgs.cabal-install;
  stack = pkgs.stack;
  ocaml = pkgs.ocamlPackages.ocaml;
  dune = pkgs.ocamlPackages.dune_3;
  elixir = pkgs.beamPackages.elixir;
  erlang = pkgs.beamPackages.erlang;
  rebar3 = pkgs.beamPackages.rebar3;
  ruby = pkgs.ruby;
  php = pkgs.php;
  dotnet-sdk = pkgs.dotnet-sdk;
  odin = pkgs.odin;
  texlive = texToolchain;
  maven = pkgs.maven;
  gradle = pkgs.gradle;
  terraform = unfreePkgs.terraform;
  helm = pkgs.kubernetes-helm;
  pnpm = pkgs.pnpm;
  yarn = pkgs.yarn;
  gcc = pkgs.gcc;

  staticcheck = pkgs.go-tools;
  golangci-lint = pkgs.golangci-lint;
  mypy = pkgs.python3Packages.mypy;
  pylint = pkgs.python3Packages.pylint;
  flake8 = pkgs.python3Packages.flake8;
  eslint = pkgs.eslint;
  vue-tsc = pkgs.vue-language-server.overrideAttrs (_: {
    pname = "vue-tsc";
    buildPhase = ''
      runHook preBuild
      pnpm run build packages/tsc
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      mkdir -p "$out/bin" "$out/lib/language-tools"
      cp -r node_modules packages extensions "$out/lib/language-tools/"
      # vue-tsc embeds the JavaScript compiler API; native TypeScript 7 cannot supply it.
      mkdir -p "$out/lib/language-tools/packages/tsc/node_modules"
      ln -sfn ${pkgs.typescript_5}/lib/node_modules/typescript "$out/lib/language-tools/packages/tsc/node_modules/typescript"
      makeWrapper ${pkgs.nodejs-slim}/bin/node "$out/bin/vue-tsc" \
        --add-flags "$out/lib/language-tools/packages/tsc/bin/vue-tsc.js"
      runHook postInstall
    '';
  });
  oxlint = pkgs.oxlint;
  stylelint = pkgs.stylelint;
  phpstan = pkgs.phpstan;
  psalm = pkgs.php84Packages.psalm;
  shellcheck = pkgs.shellcheck;
  hlint = pkgs.hlint;
  tflint = pkgs.tflint;
  luacheck = pkgs.luaPackages.luacheck;
  actionlint = pkgs.actionlint;

  gdb = pkgs.gdb;
  lldb-dap = pkgs.lldb;
  codelldb = pkgs.vscode-extensions.vadimcn.vscode-lldb.adapter;
  debugpy = pkgs.python3.withPackages (ps: [ ps.debugpy ]);
  delve = pkgs.delve;
  js-debug-adapter = pkgs.runCommand "ombp-js-debug-adapter" { } ''
    mkdir -p "$out/bin" "$out/share/ombp"
    ln -s ${pkgs.vscode-js-debug}/bin/js-debug "$out/bin/js-debug-adapter"
    ln -s ${pkgs.vscode-js-debug}/lib/node_modules/js-debug/dist/src/dapDebugServer.js "$out/share/ombp/dapDebugServer.js"
  '';
  netcoredbg = pkgs.netcoredbg;
  kotlin-debug-adapter = pkgs.stdenvNoCC.mkDerivation {
    pname = "kotlin-debug-adapter";
    version = "0.4.4";
    src = pkgs.fetchurl {
      url = "https://github.com/fwcd/kotlin-debug-adapter/releases/download/0.4.4/adapter.zip";
      hash = "sha256-OHTLre0P24Ipo4EWeJWwpsr4i3rf+rxpD89ab7ZdEbY=";
    };
    nativeBuildInputs = [ pkgs.unzip pkgs.makeWrapper ];
    dontBuild = true;
    installPhase = ''
      runHook preInstall
      mkdir -p "$out/lib/kotlin-debug-adapter" "$out/bin"
      cp -r bin lib "$out/lib/kotlin-debug-adapter/"
      patchShebangs "$out/lib/kotlin-debug-adapter/bin"
      makeWrapper "$out/lib/kotlin-debug-adapter/bin/kotlin-debug-adapter" "$out/bin/kotlin-debug-adapter" \
        --set JAVA_HOME ${pkgs.jdk} --prefix PATH : ${lib.makeBinPath [ pkgs.coreutils pkgs.findutils ]}
      runHook postInstall
    '';
    meta.license = lib.licenses.mit;
  };
  rdbg = pkgs.rubyPackages.debug;
  php-debug-adapter = pkgs.writeShellScriptBin "php-debug-adapter" ''
    exec ${pkgs.nodejs}/bin/node ${phpDebug}/share/vscode/extensions/xdebug.php-debug/out/phpDebug.js "$@"
  '';
  bash-debug-adapter = pkgs.stdenvNoCC.mkDerivation {
    pname = "bash-debug-adapter";
    version = "0.3.9";
    src = bashDebugSource;
    nativeBuildInputs = [ pkgs.unzip pkgs.makeWrapper ];
    unpackPhase = ''
      unzip "$src"
      cd extension
    '';
    dontBuild = true;
    # The standalone DAP process does not receive VS Code's contributed launch defaults.
    postPatch = ''
      substituteInPlace out/bashDebug.js --replace-fail \
        'this.launchArgs = args;' \
        'args = {cwd: process.cwd(), args: [], argsString: "", env: {}, pathBash: "${pkgs.bash}/bin/bash", pathBashdb: "bashdb", pathBashdbLib: require("path").join(__dirname, "../bashdb_dir"), pathCat: "${pkgs.coreutils}/bin/cat", pathMkfifo: "${pkgs.coreutils}/bin/mkfifo", pathPkill: "${pkgs.procps}/bin/pkill", ...args}; this.launchArgs = args;'
    '';
    installPhase = ''
      runHook preInstall
      mkdir -p "$out/lib/bash-debug" "$out/bin" "$out/share"
      cp -r out node_modules bashdb_dir "$out/lib/bash-debug/"
      # This adapter's --tty_in protocol needs its bundled BashDB, not stock bashdb.
      substituteInPlace "$out/lib/bash-debug/bashdb_dir/bashdb" \
        --replace-fail 'typeset  prefix=/usr' "typeset prefix=$out" \
        --replace-fail '@BASHDB_MAIN@' "$out/lib/bash-debug/bashdb_dir/bashdb-main.inc"
      chmod +x "$out/lib/bash-debug/bashdb_dir/bashdb"
      patchShebangs "$out/lib/bash-debug/bashdb_dir"
      ln -s "$out/lib/bash-debug/bashdb_dir" "$out/share/bashdb"
      ln -s "$out/lib/bash-debug/bashdb_dir/bashdb" "$out/bin/bashdb"
      makeWrapper ${pkgs.nodejs}/bin/node "$out/bin/bash-debug-adapter" \
        --add-flags "$out/lib/bash-debug/out/bashDebug.js" \
        --prefix PATH : "$out/bin:${lib.makeBinPath [ pkgs.bash pkgs.coreutils pkgs.procps ]}"
      runHook postInstall
    '';
    meta.license = [ lib.licenses.mit lib.licenses.gpl2Plus ];
  };
  flutter = pkgs.runCommand "ombp-flutter" { } ''
    mkdir -p "$out/bin" "$out/share/ombp"
    ln -s ${pkgs.flutter}/bin/flutter "$out/bin/flutter"
    ln -s ${pkgs.flutter} "$out/share/ombp/flutter-sdk"
    ln -s ${pkgs.writeShellScript "ombp-flutter-dart" ''
      args=("$@")
      if [[ "''${args[0]-}" == debug_adapter ]]; then
        for ((i = 1; i < ''${#args[@]}; i++)); do
          case "''${args[i]}" in
            --flutter-sdk-path)
              # SDK normalization removes its default empty value, leaving a bare final flag.
              sdk="''${args[i+1]:-${pkgs.flutter}}"
              unset 'args[i]' 'args[i+1]'
              FLUTTER_ROOT="$sdk" exec "$sdk/bin/flutter" "''${args[@]}"
              ;;
            --flutter-sdk-path=*)
              sdk="''${args[i]#*=}"
              sdk="''${sdk:-${pkgs.flutter}}"
              unset 'args[i]'
              FLUTTER_ROOT="$sdk" exec "$sdk/bin/flutter" "''${args[@]}"
              ;;
          esac
        done
      fi
      exec ${pkgs.flutter}/bin/dart "''${args[@]}"
    ''} "$out/bin/dart"
  '';

  ffmpeg = assert lib.versionAtLeast pkgs.ffmpeg.version "8.1"; bundle "ombp-ffmpeg" [ pkgs.ffmpeg.bin ];
  yt-dlp = pkgs.yt-dlp;
  trafilatura = pkgs.python3Packages.trafilatura;
  lynx = pkgs.lynx;
  direnv = pkgs.direnv;
  lspmux = pkgs.lspmux;
  vhs = pkgs.vhs;
  glab = pkgs.glab;
  tmux = pkgs.tmux;
  zellij = pkgs.zellij;
  sd = pkgs.sd;
  ast-grep = (pkgs.ast-grep.override { enableLegacySg = true; }).overrideAttrs (old: {
    # The legacy sg launcher invokes ast-grep while generating its completions.
    postInstall = ''export PATH="$out/bin:$PATH"'' + "\n" + (old.postInstall or "");
  });
} // lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == "x86_64-linux") {
  sourcekit-lsp = bundle "ombp-sourcekit-lsp" [ pkgs.sourcekit-lsp pkgs.swift pkgs.swiftpm ];
  swift = bundle "ombp-swift-toolchain" [ pkgs.swift pkgs.swiftpm ];
}
