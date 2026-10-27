{ pkgs }:
pkgs.importNpmLock.buildNodeModules {
  npmRoot = ./.;
  packageLock = builtins.fromJSON (builtins.readFile ./registry-lock.json);
  nodejs = pkgs.nodejs_24;
  derivationArgs = {
    pname = "omp-manager-sdk";
    version = "18.12.6";
    # The bridge consumes SDK source. Do not run optional ONNX/native download hooks.
    npm_config_ignore_scripts = "true";
    npmRebuildFlags = [ "--ignore-scripts" ];
    dontNpmBuild = true;
  };
}
