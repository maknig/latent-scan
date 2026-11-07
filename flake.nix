{
  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
  inputs.rust-overlay = {
    url = "github:oxalica/rust-overlay";
    inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);
    in
    {
      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; overlays = [ rust-overlay.overlays.default ]; };
          inherit (pkgs.stdenv.hostPlatform) isLinux;
          rust = pkgs.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" "clippy" "rustfmt" ];
          };
          # Linux windowing/GL libs, loaded at runtime by eframe; macOS uses system frameworks.
          linuxLibs = with pkgs; [
            libxkbcommon
            libGL
            wayland
            vulkan-loader
            libx11
            libxcursor
            libxi
            libxrandr
          ];
        in
        {
          # sane-backends provides `scanimage`, which latent-scan-sane drives.
          default = pkgs.mkShell ({
            buildInputs = [ rust pkgs.pkg-config pkgs.sane-backends ];
          } // pkgs.lib.optionalAttrs isLinux {
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath linuxLibs;
          });
        });
    };
}
