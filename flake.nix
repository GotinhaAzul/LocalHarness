{
  description = "Local-first agent harness for coding and security work";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems =
        f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          totality = pkgs.rustPlatform.buildRustPackage {
            pname = "totality";
            version = cargoToml.package.version;
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.installShellFiles ];
            postInstall = ''
              installShellCompletion --cmd tot \
                --bash <($out/bin/tot completions bash) \
                --zsh <($out/bin/tot completions zsh) \
                --fish <($out/bin/tot completions fish)
              $out/bin/tot man > tot.1
              installManPage tot.1
            '';
            meta = with pkgs.lib; {
              description = cargoToml.package.description;
              homepage = cargoToml.package.repository;
              license = with licenses; [
                mit
                asl20
              ];
              mainProgram = "tot";
              platforms = platforms.linux;
            };
          };
        in
        {
          inherit totality;
          default = totality;
        }
      );

      overlays.default = final: _prev: {
        totality = self.packages.${final.stdenv.hostPlatform.system}.totality;
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            cargo-audit
            cargo-deny
            shellcheck
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
