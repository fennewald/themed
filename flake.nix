{
  description = "themed — one theme, replicated across a small trusted fleet";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      themed =
        pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "themed";
          version = (fromTOML (builtins.readFile ./Cargo.toml)).package.version;
          src = self;
          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            description = "Last-write-wins theme register replicated across a small trusted fleet";
            mainProgram = "themed";
            platforms = systems;
          };
        };
    in
    {
      overlays.default = final: _prev: { themed = themed final; };

      packages = forAllSystems (pkgs: {
        themed = themed pkgs;
        default = themed pkgs;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
          ];
        };
      });
    };
}
