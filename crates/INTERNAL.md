# Internal component of hotl

This crate is an internal building block of
[**hotl**](https://crates.io/crates/hotl) — a human-on-the-loop terminal AI
agent with a tmux watch dashboard. It is published only so the `hotl` binary
can be `cargo install`ed from crates.io.

**No semver promise attaches to this crate's API.** The workspace's library
crates version in lockstep with the binary and their interfaces change
freely between releases — pin an exact version or don't depend on it.

- User docs: <https://nrakochy.github.io/hotl/>
- Repository & architecture: <https://github.com/nrakochy/hotl>
