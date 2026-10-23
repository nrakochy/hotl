# ACP Registry draft (not filed)

`agent.json` + `icon.svg` in the shape of github.com/agentclientprotocol/registry.
Not submitted: the registry's CI speaks ACP v1 (`protocolVersion: 1`) and needs an
`authMethods` entry; hotl speaks its own `0.1` dialect (plan 0076, owner 2026-10-22).
`sha256` values are filled at release time from each archive's `.sha256` asset;
`version` must match the crate version (a test in `crates/hotl/src/main.rs` pins it).
