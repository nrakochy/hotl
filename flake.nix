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

          # Nothing here demands $HOME, but /homeless-shelter is not writable
          # and a single test reaching for it would be an opaque failure.
          preCheck = "export HOME=$(mktemp -d)";

          # macOS refuses to nest one Seatbelt sandbox inside another, and
          # nix's darwin builder is itself Seatbelt — so `/usr/bin/sandbox-exec`
          # inside it dies with `sandbox_apply: Operation not permitted`
          # (exit 71). hotl's probe() only checks that the binary *exists*, so
          # under nix it concludes the floor is enforced, and every subprocess
          # a hook spawns is killed before it can run.
          #
          # These fourteen are exactly the tests that need a subprocess to run
          # *through* the floor and succeed, so they are the ones the nesting
          # kills. Everything else on darwin — 200+ tests — still runs. They
          # are not skipped anywhere else: CI runs them on a real macOS runner,
          # where the sandbox is not nested and they pass.
          #
          # Enumerated by reproducing the constraint directly rather than by
          # peeling one nix build at a time:
          #   sandbox-exec -p '(version 1)(allow default)' \
          #     cargo test --workspace --locked --no-fail-fast
          # An outer Seatbelt profile makes hotl's inner sandbox-exec fail the
          # same way the nix builder does. Worth redoing that way if this list
          # ever needs revisiting — cargo's fail-fast hides later binaries
          # (hotl-tools never even ran until the first eight were skipped).
          #
          # Kept as an explicit list rather than a `shell_hooks::` prefix so it
          # cannot silently grow to cover a future test that fails for an
          # unrelated reason.
          #
          # Linux keeps the whole suite — Landlock rulesets stack, so the floor
          # applies normally inside the nix builder. Stated as reasoning, not
          # measurement: this was resolved on aarch64-darwin only.
          checkFlags = pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin (
            map (t: "--skip=${t}") [
              # hotl bin — a hook subprocess must run to completion
              "agent::tests::one_shot_exit_path_actually_runs_notification_and_session_end_hooks"
              "shell_hooks::tests::identity_env_is_not_spoofable_by_a_hooks_own_env_table"
              "shell_hooks::tests::matcher_scopes_a_shell_hook_to_named_tools"
              "shell_hooks::tests::post_hook_replaces_result_and_none_when_unconfigured"
              "shell_hooks::tests::pre_hook_denies_over_stdio"
              "shell_hooks::tests::stdin_envelope_carries_the_claude_compat_hook_event_name"
              "shell_hooks::tests::stop_hook_can_block_with_a_reason"
              "shell_hooks::tests::user_prompt_hook_returns_additional_context_via_the_claude_schema_shape"
              # hotl tests/tui_e2e.rs — asserts a resolved `✓ bash` tool card,
              # which needs the bash tool to actually succeed. The sibling
              # tests that only exercise deny/ask paths pass, which is what
              # points at the floor rather than at PTY allocation.
              "prompt_stream_ask_allow_done_golden"
              # hotl-tools — tool execution and the floor's own assertions
              "builtins::tests::bash_captures_exit_and_timeout"
              "builtins::tests::grep_finds_matches_and_reports_no_matches_cleanly"
              "diagnostics::tests::reports_failures_and_stays_silent_when_clean"
              "sandbox::tests::seatbelt_confines_writes"
              "sandbox::tests::seatbelt_egress_off_confines_to_loopback"
            ]
          );

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
        # doCheck is the only difference — preCheck and checkFlags live on the
        # package itself, so what runs here is what a nixpkgs build would run.
        package = self.packages.${pkgs.stdenv.hostPlatform.system}.default.overrideAttrs (_: {
          doCheck = true;
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
