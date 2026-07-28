---
title: 'Updating hotl'
description: How `hotl update` works, what it verifies, and what it refuses to touch.
---

```
hotl update                  # install the latest release
hotl update --check          # look, don't touch
hotl update --version 0.7.0  # install a specific release (including older)
hotl update -y               # don't ask before replacing the binary
```

hotl contacts the release feed **only when you run this command**. There is no
background check, no startup probe, and nothing to turn off.

## It replaces only what it installed

A self-replacing update is right for a binary that was dropped on disk with no
bookkeeping. It is wrong everywhere else — overwriting a `cargo install`ed
binary leaves `~/.cargo/.crates.toml` claiming the old version, and the next
`cargo install` silently reverts you.

So `hotl update` works out how this copy got here and acts accordingly:

| Installed via | `hotl update` does |
|---|---|
| the installer script, or a tarball you unpacked | replaces the binary in place |
| `cargo install hotl` | prints `cargo install --locked hotl` |
| `nix profile install` | prints `nix profile upgrade hotl` |
| Homebrew | prints `brew upgrade hotl` |
| a source build (`target/release/hotl`) | prints `git pull && cargo build --release` |

The installer script and `cargo install` both put the binary in
`~/.cargo/bin/hotl`, so the path alone can't separate them — hotl checks
whether cargo's own `.crates.toml` records `hotl`.

## What it verifies

1. Reads `dist-manifest.json` from the release over HTTPS. That names the
   version, the archive for your platform, and its SHA-256.
2. Downloads the archive and checks that SHA-256 **in process, before
   decompressing anything**. The gzip and tar readers only ever see bytes that
   already matched.
3. Unpacks only the executable, refusing absolute or `..` paths.
4. Writes it beside the current binary, runs `--version` on it, and requires
   the expected version before going further. A truncated or wrong-architecture
   download is caught here, while your working binary is still in place.
5. Renames the new file over the old one. The rename is atomic and safe while
   hotl is running — the live process keeps its own copy open.

Any failure leaves the original binary untouched.

### What the checksum does not prove

The checksum travels in the same document, from the same host, over the same
TLS connection as the archive. It catches a corrupted or truncated download.
It does **not** prove who built the release: anyone who could replace the
archive could replace the hash next to it.

Closing that needs a signature made with a key that never touches CI, checked
against a public key compiled into hotl. That is not shipped yet. Until it is,
`hotl update` trusts exactly what every other install path already trusts —
GitHub over TLS — and no more. It is not a weaker channel than
`curl … | sh`, `cargo install`, or `nix profile install`; it is the same
trust, stated plainly.

## Refusals

- **`security-enforced` builds.** The published binaries are ordinary builds.
  Replacing an enforced binary with one would quietly drop the enforced
  posture, so hotl refuses and tells you to rebuild.
- **Platforms with no published binary.** Releases cover macOS (arm64, x86_64)
  and Linux glibc (arm64, x86_64). On musl or anything else, build from source.
- **Releases before the `.tar.gz` switch.** Archives up to v0.7.1 were
  `.tar.xz`, which this build does not decode. `hotl update --version 0.7.0`
  says so and points at the installer script.
- **An unwritable install directory.** hotl reports which directory needs to be
  writable. It never escalates privileges on its own.

## Notes

- `hotl update` does not consult `[network].egress`. That setting governs what
  the *agent* may reach; this is a command you typed.
- A plain `hotl update` never moves you onto a prerelease. `--version` does.
- The binary is replaced, not your config or session data. To remove those, see
  [uninstall](../uninstall/).
