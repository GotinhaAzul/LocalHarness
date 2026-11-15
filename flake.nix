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
          localharness = pkgs.rustPlatform.buildRustPackage {
            pname = "localharness";
            version = cargoToml.package.version;
            src = ./.;
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.installShellFiles ];
            postInstall = ''
              installShellCompletion --cmd lh \
                --bash <($out/bin/lh completions bash) \
                --zsh <($out/bin/lh completions zsh) \
                --fish <($out/bin/lh completions fish)
              $out/bin/lh man > lh.1
              installManPage lh.1
            '';
            meta = with pkgs.lib; {
              description = cargoToml.package.description;
              homepage = cargoToml.package.repository;
              license = with licenses; [
                mit
                asl20
              ];
              mainProgram = "lh";
              platforms = platforms.linux;
            };
          };
        in
        {
          inherit localharness;
          default = localharness;
        }
      );

      overlays.default = final: _prev: {
        localharness = self.packages.${final.stdenv.hostPlatform.system}.localharness;
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
