{
  # Development environment only. framelight itself has nothing to do with Nix:
  # it runs on stock SteamOS and is installed there with packaging/install.sh.
  description = "framelight development shell";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            # The toolchain comes from rust-toolchain.toml through rustup, as it
            # does without Nix.
            rustup
            cargo-zigbuild
            zig
            cmake
            # fl-devhost and tools/e2e
            ffmpeg
            nodejs
            chromium
          ];
          # libpulse is dlopen'd; Nix has no system library path to find it in.
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.libpulseaudio ];
        };
      });
    };
}
