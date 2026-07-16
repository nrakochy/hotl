# hotl — a human-on-the-loop terminal agent dashboard

[![crates.io](https://img.shields.io/crates/v/hotl.svg)](https://crates.io/crates/hotl)

Run it in a pane of your own; it discovers AI-agent processes across your
terminal multiplexer, shows their live status, gives an (optional) audible ping
when an agent is waiting on your input, and lets you jump focus to an agent.

**Docs — install, usage, config, keys:** see [`crates/hotl/README.md`](crates/hotl/README.md)
(also rendered on the [crates.io page](https://crates.io/crates/hotl)).

## Releasing

Cut a release with the helper script — it bumps the workspace version, commits,
tags `vX.Y.Z`, and pushes. The tag triggers the crates.io publish and the
prebuilt-binary/installer workflows.

    scripts/release.sh patch    # 0.1.0 -> 0.1.1  (bug fix)
    scripts/release.sh minor    # 0.1.0 -> 0.2.0  (feature, or breaking pre-1.0)
    scripts/release.sh major    # 0.1.0 -> 1.0.0
    scripts/release.sh 0.4.2    # explicit version

Versions are immutable on crates.io — always go up, never reuse one. The tag
must match the `[workspace.package]` version (the script keeps them in sync).
