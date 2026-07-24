{
  description = "Terminal-native coding agent harness with a tmux watch dashboard";

  inputs = {
    # The branch is not the pin — flake.lock is. The branch only decides what
    # `nix flake update` fetches. nixos-unstable has passed Hydra, so rustc /
    # cargo / stdenv arrive prebuilt from cache.nixos.org, and it is closest to
    # what a nixpkgs PR would build against.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      # Explicit list rather than flake-utils: one fewer transitive input in
      # every consumer's lock, and a consumer's `follows` would not have
      # reached it anyway.
      systems = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # Read the version, never declare it. `scripts/release.sh` is the sole
      # writer of the workspace version; Nix must not become another literal to
      # keep in sync.
      #
      # This must be the workspace ROOT manifest: builtins.fromTOML is a plain
      # TOML parser and does not resolve Cargo's workspace inheritance, so
      # crates/hotl/Cargo.toml yields `version = { workspace = true; }` — an
      # attrset, not a string.
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      cargoVersion = cargoToml.workspace.package.version;

      # A build from master is not v0.5.0, and calling it that makes
      # `nix profile list` lie. nixpkgs' unreleased-snapshot convention is
      # `<version>-unstable-YYYY-MM-DD`. Date rather than shortRev because
      # `nix profile upgrade` compares version strings and dates sort; source
      # identity is already carried by the store path. The `or` fallback keeps
      # dirty working trees evaluable.
      date = self.lastModifiedDate or "19700101000000";
      version = "${cargoVersion}-unstable-${builtins.substring 0 4 date}-${builtins.substring 4 2 date}-${builtins.substring 6 2 date}";
    in
    {
      packages = forAllSystems (pkgs: rec {
        hotl = pkgs.rustPlatform.buildRustPackage {
          pname = "hotl";
          inherit version;

          src = self;

          # importCargoLock, not fetchCargoVendor: Cargo.lock already carries a
          # sha256 per crate, so this needs no hash of our own and keeps
          # building across dependency changes forever. nixpkgs uses the other
          # backend (cargoHash) because it has no in-tree lockfile to read.
          cargoLock.lockFile = ./Cargo.lock;

          # Tests off here, on in `checks.package`. Every consumer build
          # compiles all workspace members locally; running the suite on top
          # roughly doubles a `home-manager switch`. `nix flake check` and CI
          # still run it.
          doCheck = false;

          # versionCheckHook greps --version output for the derivation version,
          # which the -unstable- suffix will never match. The hook does real
          # work in nixpkgs (where version == tag == CARGO_PKG_VERSION); here it
          # would only ever fail.
          doInstallCheck = false;

          meta = {
            description = "Terminal-native coding agent harness with a tmux watch dashboard";
            homepage = "https://github.com/nrakochy/hotl";
            license = pkgs.lib.licenses.agpl3Plus;
            mainProgram = "hotl";
            platforms = pkgs.lib.platforms.unix;
          };
        };
        default = hotl;
      });

      # Full suite, on the same derivation the package output builds — so
      # `nix flake check` stays predictive of the nixpkgs build, which keeps
      # tests on.
      checks = forAllSystems (pkgs: {
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.default.overrideAttrs (_: {
          doCheck = true;
          # The nix sandbox has HOME=/homeless-shelter, which is not writable.
          preCheck = "export HOME=$(mktemp -d)";
        });
      });

      # nixpkgs' own toolchain, not rust-overlay: it shares the cache.nixos.org
      # binaries the package build already pulls, and keeps the devShell honest
      # about the compiler the package is built with.
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            rust-analyzer
          ];
        };
      });

      # Extends the repo's `cargo fmt --check` gate to the new .nix files.
      formatter = forAllSystems (pkgs: pkgs.nixfmt-rfc-style);
    };
}
