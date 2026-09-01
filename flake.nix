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

      # `homeModules` is the name the flake schema recognises; `homeManagerModules`
      # is the older spelling many configs still import by. Keep both.
      homeModules.themed = ./nix/hm-module.nix;

      # Same module, but with `services.themed.package` defaulted to this flake's
      # per-system build, so consumers need not add the overlay.
      homeModules.default =
        { pkgs, lib, ... }:
        {
          imports = [ ./nix/hm-module.nix ];
          config.services.themed.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.themed;
        };

      homeManagerModules = self.homeModules;

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
