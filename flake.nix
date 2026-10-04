{
  description = "A small LNURL-pay frontend for ldk-server";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay.url = "github:oxalica/rust-overlay";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
      crane,
    }:
    {
      nixosModules.default = import ./nix/module.nix self;
    }
    // flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;
        src = craneLib.cleanCargoSource ./.;
        commonArgs = {
          inherit src;
          strictDeps = true;
        };
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        package = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
          }
        );
      in
      {
        packages = {
          default = package;
          ldk-lnurl-server = package;
        };

        checks = {
          inherit package;
          formatting = craneLib.cargoFmt { inherit src; };
          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- -D warnings";
            }
          );
          tests = craneLib.cargoTest (
            commonArgs
            // {
              inherit cargoArtifacts;
            }
          );
        }
        // nixpkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux (
          let
            evaluatedService =
              (nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  self.nixosModules.default
                  {
                    system.stateVersion = "25.11";
                    services.ldk-lnurl-server = {
                      enable = true;
                      inherit package;
                      publicUrl = "https://pay.example.com/";
                      username = "alice";
                      tlsCertificateFile = "/run/secrets/ldk-tls-cert";
                      macaroonFile = "/run/secrets/ldk-macaroon";
                      adminTokenFile = "/run/secrets/lnurl-admin-token";
                    };
                  }
                ];
              }).config.systemd.services.ldk-lnurl-server;
            moduleEvaluation =
              assert evaluatedService.environment.LNURL_USERNAME == "alice";
              assert evaluatedService.environment.LNURL_DATABASE_PATH == "/var/lib/ldk-lnurl-server/lnurl.redb";
              assert builtins.elem "macaroon:/run/secrets/ldk-macaroon"
                evaluatedService.serviceConfig.LoadCredential;
              assert builtins.elem "admin-token:/run/secrets/lnurl-admin-token"
                evaluatedService.serviceConfig.LoadCredential;
              pkgs.runCommand "ldk-lnurl-server-nixos-module" { } "touch $out";
          in
          {
            nixos-module = moduleEvaluation;
          }
        );

        devShells.default = craneLib.devShell {
          packages = with pkgs; [
            jq
            just
            nixfmt
            rust-analyzer
          ];
        };

        formatter = pkgs.nixfmt;
      }
    );
}
