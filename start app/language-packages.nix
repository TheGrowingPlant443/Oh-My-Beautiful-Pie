{ pkgs }:
let
  inherit (pkgs) lib;
  beam = pkgs.beam27Packages;
  elixirSource = pkgs.fetchFromGitHub {
    owner = "elixir-lsp";
    repo = "elixir-ls";
    tag = "v0.31.1";
    hash = "sha256-H7u2rcH0qSiswC6aHdaFdM8IyEpXS74RQrVFuJx35Lo=";
  };
  communityModules = pkgs.fetchurl {
    url = "https://github.com/tlaplus/CommunityModules/releases/download/202610262215/CommunityModules.jar";
    hash = "sha256-XB7PxMQOMLWiZYZDta/pEWMA/+HRoidHU4Zfjgy6RJM=";
  };
in
{
  tlaplus = pkgs.ocamlPackages.buildDunePackage {
    pname = "tlapm-lsp";
    version = "unstable-2026-11-03";
    dunePackages = [ "tlapm" ];
    minimalOCamlVersion = "5.0";
    src = pkgs.fetchFromGitHub {
      owner = "tlaplus";
      repo = "tlapm";
      rev = "568722253719604cea2cbfaff537c58208084a87";
      hash = "sha256-VFoe8LfUP7F+3FUBc5BuzMkKbK4o1tuIr5+4puvNyHg=";
    };
    nativeBuildInputs = [ pkgs.unzip ];
    buildInputs = with pkgs.ocamlPackages; [
      dune-site
      dune-build-info
      sexp_diff
      sexplib
      cmdliner
      camlzip
      re2
      ppx_inline_test
      ppx_assert
      ppx_deriving
      xmlm
      lsp
      eio_main
    ];
    # Z3 is bundled; projects needing Isabelle/TLA+, Zenon, LS4 or SANY must supply them.
    postPatch = ''
      rm deps/{isabelle,ls4,tla2tools.jar,zenon}/dune
      substituteInPlace deps/z3/dune \
        --replace-fail '(run "make" "-C" "." "z3")' '(run "cp" "${pkgs.z3}/bin/z3" "z3")'
      substituteInPlace library/dune \
        --replace-fail '(run "wget" "https://github.com/tlaplus/CommunityModules/releases/latest/download/CommunityModules.jar")' \
        '(run "cp" "${communityModules}" "CommunityModules.jar")'
    '';
    doCheck = false;
    postInstall = ''
      test -x "$out/bin/tlapm_lsp"
      test -x "$out/bin/tlapm"
    '';
    meta = {
      description = "TLA+ proof-manager language server with Z3";
      homepage = "https://github.com/tlaplus/tlapm";
      license = lib.licenses.bsd2;
      mainProgram = "tlapm_lsp";
      platforms = lib.platforms.linux;
    };
  };

  kotlin-lsp = pkgs.stdenv.mkDerivation (finalAttrs: {
    pname = "kotlin-lsp";
    version = "263.7084.0";
    src = pkgs.fetchurl {
      url = "https://download.jetbrains.com/language-server/kotlin-server/${finalAttrs.version}/kotlin-server-${finalAttrs.version}${lib.optionalString pkgs.stdenv.hostPlatform.isAarch64 "-aarch64"}.tar.gz";
      hash = {
        x86_64-linux = "sha256-rgxLAeF6tzpKUienjqmebcvj5JlgAxWyJ1mk5M3dEd8=";
        aarch64-linux = "sha256-RQq3wdgOSBBqpCm0jLQibYLKEqiiQARHrgI4Z5R4DKw=";
      }.${pkgs.stdenv.hostPlatform.system};
    };
    nativeBuildInputs = [ pkgs.autoPatchelfHook pkgs.makeWrapper ];
    buildInputs = [ pkgs.stdenv.cc.cc.lib ];
    dontBuild = true;
    # Match the archive's required Java 25 without an unpatched bundled JVM.
    postPatch = ''
      rm -r jbr
      ln -s ${pkgs.jetbrains.jdk-no-jcef.home} jbr
    '';
    installPhase = ''
      runHook preInstall
      mkdir -p "$out/lib/kotlin-lsp" "$out/bin"
      cp -a . "$out/lib/kotlin-lsp/"
      makeWrapper "$out/lib/kotlin-lsp/bin/intellij-server" "$out/bin/kotlin-lsp" \
        --set JAVA_HOME ${pkgs.jetbrains.jdk-no-jcef.home} \
        --set JDK_HOME ${pkgs.jetbrains.jdk-no-jcef.home}
      runHook postInstall
    '';
    meta = {
      description = "JetBrains Kotlin language server";
      homepage = "https://github.com/Kotlin/kotlin-lsp";
      # The release also contains proprietary Air/Fleet components.
      license = lib.licenses.unfree;
      mainProgram = "kotlin-lsp";
      platforms = [ "x86_64-linux" "aarch64-linux" ];
    };
  });

  erlangls = pkgs.stdenvNoCC.mkDerivation {
    pname = "erlang-ls";
    version = "1.1.0";
    src = pkgs.fetchurl {
      url = "https://github.com/erlang-ls/erlang_ls/releases/download/1.1.0/erlang_ls-linux-27.tar.gz";
      hash = "sha256-yu322OUavU24/pl9GG3+dqHcIcU8PU9qSVAp5LYg09A=";
    };
    sourceRoot = ".";
    nativeBuildInputs = [ pkgs.makeWrapper ];
    dontBuild = true;
    installPhase = ''
      runHook preInstall
      install -Dm644 erlang_ls "$out/lib/erlang-ls/erlang_ls"
      makeWrapper ${beam.erlang}/bin/escript "$out/bin/erlang_ls" \
        --add-flags "$out/lib/erlang-ls/erlang_ls" \
        --prefix PATH : ${lib.makeBinPath [ beam.erlang beam.rebar3 ]}
      ln -s ${beam.erlang}/bin/{erl,erlc,escript,dialyzer,epmd} "$out/bin/"
      ln -s ${beam.rebar3}/bin/rebar3 "$out/bin/"
      runHook postInstall
    '';
    meta = {
      description = "Archived Erlang LS final release, using its matching OTP 27 runtime";
      homepage = "https://github.com/erlang-ls/erlang_ls";
      license = lib.licenses.asl20;
      mainProgram = "erlang_ls";
      # The release escript contains only portable BEAM code and snippets, no NIFs.
      platforms = [ "x86_64-linux" "aarch64-linux" ];
    };
  };

  elixirls = beam.mixRelease {
    pname = "elixir-ls";
    version = "0.31.1";
    src = elixirSource;
    mixFodDeps = beam.fetchMixDeps {
      pname = "elixir-ls-deps";
      version = "0.31.1";
      src = elixirSource;
      hash = "sha256-N6G/Nq+9P5XcjokkuVYr5I7xGyphs+B/HZOt4BsmNeE=";
    };
    dontMixDepsCompile = true;
    configurePhase = ''
      # Mix uses this value verbatim while entering umbrella apps and dependencies.
      export MIX_BUILD_PATH="$PWD/_build/prod"
      runHook preConfigure
      mix deps.compile --no-deps-check
      runHook postConfigure
    '';
    # Start compiled applications directly; upstream scripts run Mix.install at launch.
    installPhase = ''
      runHook preInstall
      mkdir -p "$out/lib" "$out/bin"
      cp -RL _build/prod/lib/. "$out/lib/"
      test -f "$out/lib/language_server/ebin/Elixir.ElixirLS.LanguageServer.beam"
      test -f "$out/lib/debug_adapter/ebin/Elixir.ElixirLS.DebugAdapter.beam"
      # Keep byte-counted stdio and UTF-8 filenames even without a host locale.
      makeWrapper ${beam.elixir}/bin/elixir "$out/bin/elixir-ls" \
        --prefix ERL_LIBS : "$out/lib:${beam.hex}/lib/erlang/lib" \
        --prefix PATH : ${lib.makeBinPath [ beam.elixir beam.erlang beam.rebar3 pkgs.git ]} \
        --add-flags "--erl '+fnu -kernel standard_io_encoding latin1 +sbwt none +sbwtdcpu none +sbwtdio none'" \
        --add-flags "-e 'ElixirLS.LanguageServer.main()'"
      makeWrapper ${beam.elixir}/bin/elixir "$out/bin/elixir-ls-debugger" \
        --prefix ERL_LIBS : "$out/lib:${beam.hex}/lib/erlang/lib" \
        --prefix PATH : ${lib.makeBinPath [ beam.elixir beam.erlang beam.rebar3 pkgs.git ]} \
        --add-flags "--erl '+fnu -kernel standard_io_encoding latin1 +sbwt none +sbwtdcpu none +sbwtdio none'" \
        --add-flags "-e 'ElixirLS.DebugAdapter.main()'"
      ln -s ${beam.elixir}/bin/{elixir,elixirc,mix} "$out/bin/"
      ln -s ${beam.erlang}/bin/{erl,erlc,escript,dialyzer,epmd} "$out/bin/"
      ln -s ${beam.rebar3}/bin/rebar3 "$out/bin/"
      runHook postInstall
    '';
    meta = {
      description = "Offline-built Elixir language server and debug adapter";
      homepage = "https://github.com/elixir-lsp/elixir-ls";
      license = lib.licenses.asl20;
      mainProgram = "elixir-ls";
      platforms = [ "x86_64-linux" "aarch64-linux" ];
    };
  };
}
