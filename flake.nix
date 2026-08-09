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

          # The grep tool spawns the `rg` binary, so the suite needs it on
          # PATH. nativeCheckInputs, so it lands only where doCheck is on —
          # inert here, active in `checks.package`, and the same input a
          # nixpkgs build (tests on) would want. Nothing reaches runtime: a
          # nix-installed hotl still finds `rg` only if the user has it, and
          # degrades to a legible error if not.
          nativeCheckInputs = [ pkgs.ripgrep ];

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
          # These twenty-five are exactly the tests that need a subprocess to
          # run *through* the floor and succeed, so they are the ones the
          # nesting kills. Everything else on darwin — 200+ tests — still
          # runs. They are not skipped anywhere else: CI runs them on a real
          # macOS runner, where the sandbox is not nested and they pass. Seven
          # (the configured-writable seatbelt behaviors and bash-execution
          # pins) joined the original fourteen with the 0.7.0 `[sandbox]`
          # work, enumerated with the sandbox-exec reproduction below.
          #
          # Three more joined 2026-08-09, and the reason they took two releases
          # to land is the trap this comment already warns about: the list fell
          # behind the code twice and fail-fast showed only the first casualty
          # each time. `seatbelt_denies_protected_subpaths_under_cwd` was red
          # from v0.8.0, the two plan-mode tests from v0.9.0, and the plan-mode
          # binary ran first — so the seatbelt one never even executed in CI.
          # Add to this list only from a full `--no-fail-fast` enumeration.
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
          # Linux was "keeps the whole suite" by reasoning, not measurement —
          # and the ubuntu builder never actually reached hotl-tools until
          # 0.7.0 peeled the failures ahead of it. Measured 2026-07-27:
          # Landlock itself works in the builder (bash runs confined), but the
          # probe's outside-the-floor witness dir cannot exist there — the
          # default /var/tmp is absent, and every writable path sits under
          # TMPDIR, which the floor covers by construction. The eight
          # linux-skipped tests below need that witness, directly or by
          # asserting a confinement verdict; the verdict-agnostic probe tests
          # (memoization, verdict-matches-host) still run. Real Linux
          # enforcement coverage lives in ci.yml's harness job on the raw
          # runner.
          checkFlags =
            # The loop-overhead perf gates (hotl-testkit tests/loop_overhead.rs)
            # compare against a baseline committed from real hardware, and the
            # nix builder's scratch filesystem makes sync_data() nearly free:
            # the teeth check's deliberate regression lands under the 3x band
            # there (2.96x measured on the ubuntu runner), and a loaded runner
            # can drift the other way. Skipped on every platform — plain
            # `cargo test` on real hardware is where the gate keeps its teeth.
            map (t: "--skip=${t}") [
              "loop_overhead_stays_within_the_regression_band"
              "gate_would_catch_a_real_regression"
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isDarwin (
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
                "builtins::tests::bash_preserves_stdout_stderr_interleaving"
                "builtins::tests::bash_reports_the_exit_status_structurally"
                "builtins::tests::grep_finds_matches_and_reports_no_matches_cleanly"
                "builtins::tests::timeout_kills_the_whole_process_group"
                "diagnostics::tests::reports_failures_and_stays_silent_when_clean"
                "sandbox::tests::seatbelt_allows_writes_under_a_configured_extra_dir"
                "sandbox::tests::seatbelt_blocks_a_connect_to_a_denied_daemon_socket"
                "sandbox::tests::seatbelt_confines_writes"
                "sandbox::tests::seatbelt_denies_apple_events_without_breaking_plain_applescript"
                "sandbox::tests::seatbelt_denies_protected_subpaths_under_cwd"
                "sandbox::tests::seatbelt_denies_reading_a_carved_path"
                "sandbox::tests::seatbelt_egress_off_confines_to_loopback"
                # hotl-tools tests/sandbox_extras.rs and tests/sandbox_read_carve.rs
                # — never reached in the nix macos log (fail-fast stopped at the
                # lib target); caught by the sandbox-exec reproduction above.
                "configured_extras_widen_the_floor_and_keep_the_probe_sound"
                "the_carve_denies_hotls_own_run_dir_through_the_process_global_path"
                # hotl-engine tests/plan_mode.rs — both assert bash actually
                # ran (one checks its stdout), so the floor must succeed.
                "plan_plus_ask_prompts_for_bash"
                "plan_plus_bypass_still_runs_bash"
                # hotl-testkit
                "tests::negative_max_turns_never_caps"
              ]
            )
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux (
              map (t: "--skip=${t}") [
                # hotl-tools lib — each asserts real confinement, which starts
                # at the probe's outside-the-floor witness write
                "diagnostics::tests::a_diagnostic_is_confined_and_we_can_tell_it_actually_ran"
                "sandbox::linux_tests::abi_below_v3_is_not_silently_certified"
                "sandbox::linux_tests::landlock_allows_writes_under_a_configured_extra_dir"
                "sandbox::linux_tests::landlock_confines_truncate_by_path"
                "sandbox::linux_tests::landlock_confines_writes"
                "sandbox::linux_tests::landlock_denies_reading_a_carved_path"
                "sandbox::linux_tests::landlock_narrowing_keeps_ancestor_listing_working"
                # hotl-tools tests/ — same witness. Unreached in the failing
                # log (cargo stops at the lib target); classified by reading
                # their probe_dir()/verify_confinement_with call paths.
                "configured_extras_widen_the_floor_and_keep_the_probe_sound"
                "probe_refuses_a_mechanism_that_does_not_confine"
                "probe_leaves_no_file_behind_when_the_write_escapes"
                "the_carve_denies_hotls_own_run_dir_through_the_process_global_path"
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
