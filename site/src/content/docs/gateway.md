---
title: 'Run hotl through a gateway — hotl the agent'
description: Point hotl at any OpenAI-compatible gateway and obtain API keys from a command instead of an env var.
---

Point hotl at any OpenAI-compatible gateway (Bifrost,
LiteLLM, OpenRouter, a corporate proxy) instead of a provider directly, and
optionally obtain the API key from a command instead of an env var. Nothing
here is gateway-specific: a gateway is a base URL, and a key is whatever your
configured command prints.

## Why a gateway

A gateway gives you provider failover, key pools, and spend limits without
hotl depending on any of it. hotl composes with a gateway; it does not
require one. One rule when you do: **leave gateway-side retries off** (most
default to 0) — hotl's engine owns retry, backoff, and fallback, and two
retry layers multiply.

## Point hotl at the gateway

Any OpenAI-compatible endpoint is just a base URL. In
`~/.config/hotl/config.toml`:

```toml
[provider]
model = "openai/claude-opus-4-8"          # openai/<model-as-the-gateway-names-it>
base_url = "http://localhost:8080/v1"     # the gateway's OpenAI-compatible root
```

Or per-shell: `export HOTL_MODEL=openai/<model> HOTL_OPENAI_BASE_URL=http://localhost:8080/v1`.

The `openai/` prefix selects hotl's OpenAI-compatible dialect; everything
after it is passed to the gateway verbatim, so use whatever model name the
gateway routes (for multi-provider gateways that is often
`openai/anthropic/claude-opus-4-8` — provider prefix included).

## Obtain the key from a command (api key helper)

For gateways that issue short-lived or rotating keys, configure a command
whose stdout is the key instead of a static env var:

```toml
[provider]
api_key_helper = "my-mint-key"        # stdout (trimmed) = the key
api_key_helper_ttl_secs = 300         # optional: re-run when older than 5m
```

- Runs at session start (a broken helper fails fast with its own message,
  before you burn a prompt).
- Re-runs automatically once when the provider answers 401/403, then the
  request is retried once. A second auth failure surfaces.
- Re-runs when the cached key is older than the TTL (omit the TTL to refresh
  only at startup and on auth failures).
- **A configured helper beats `OPENAI_API_KEY`/`ANTHROPIC_API_KEY`** —
  configuring it is a deliberate act. `hotl doctor` names the active source.
- Constraints: 5 seconds to run, 64KB of stdout, non-zero exit or empty
  stdout is an error (stderr shows up in the message — print something
  useful there).

Works identically for Anthropic direct — the helper is a key *source*, not a
gateway feature.

## Worked example: Bifrost with a virtual key

[Bifrost](https://github.com/maximhq/bifrost) is a self-hostable gateway
that pools provider keys behind *virtual keys*. End to end:

```sh
# 1. Start the gateway (defaults to :8080).
npx -y @maximhq/bifrost

# 2. Configure an upstream provider key + create a virtual key in the web UI
#    (http://localhost:8080) — or script it against the governance API.
#    Store the virtual key wherever your secret tooling lives:
security add-generic-password -a "$USER" -s bifrost-vk -w "vk-…"   # macOS example

# 3. Tell hotl where the gateway is and how to fetch the key.
cat >> ~/.config/hotl/config.toml <<'EOF'
[provider]
model = "openai/anthropic/claude-opus-4-8"
base_url = "http://localhost:8080/v1"
api_key_helper = "security find-generic-password -s bifrost-vk -w"
EOF

# 4. Verify, then run.
hotl doctor        # expect: key helper: OK — gateway: …/models reachable
hotl
```

Leave Bifrost's `MaxRetries` at its default 0: the engine owns recovery.

## Endpoints that authenticate for you

Some endpoints hold the credential themselves: a corporate gateway that
terminates auth at its edge, or a local bridge that authenticates against a
CLI session you have already logged into. hotl has nothing to send them, and
requiring a key it will not use is just a wart.

Set `auth = "subscription"`. hotl then holds no credential at all:

```toml
[provider]
model    = "anthropic/claude-opus-4-8"
base_url = "http://127.0.0.1:3456"
auth     = "subscription"
```

The setting is provider-neutral — it reads identically on the other side:

```toml
[provider]
model    = "openai/gpt-5"
base_url = "http://127.0.0.1:4000/v1"
auth     = "subscription"
```

Per-shell: `export HOTL_PROVIDER_AUTH=subscription HOTL_ANTHROPIC_BASE_URL=http://127.0.0.1:3456`.

Three things worth knowing:

- **Any key in your environment is discarded, not merely unused.** If
  `ANTHROPIC_API_KEY` is still exported from earlier work, hotl will not
  forward it — a local bridge never receives a production credential you
  did not mean to give it.
- **`base_url` is required.** Without it hotl would send an empty credential
  to the vendor's own endpoint, so this fails at startup rather than as a
  confusing 401 mid-session.
- **Both URL spellings work.** `http://host:3456` and `http://host:3456/v1`
  resolve to the same endpoint, since bridges and gateways disagree about
  which half owns the version prefix.

hotl sends its normal request body and does not negotiate capabilities. If an
endpoint rejects something it does not support — prompt-cache markers,
thinking blocks — that surfaces as an ordinary error naming the field, rather
than being silently worked around. `hotl doctor` reports what the endpoint
actually answers:

```
ok    provider: claude-opus-4-8 selected (auth: subscription — no credential held)
ok    gateway: http://127.0.0.1:3456/v1/models reachable (HTTP 200)
```

## Troubleshooting

| Symptom | Meaning | Fix |
|---|---|---|
| `api_key_helper … timed out after 5s` | The helper hung (prompted for input?) | Helpers must be non-interactive; pre-authenticate your secret tool. |
| `api_key_helper … printed nothing on stdout` | Wrong flag or empty secret | Run the command by hand; it must print only the key. |
| `gateway: … rejected the key (HTTP 401)` in doctor | Key invalid/expired at the gateway | Mint a new key; check the helper fetches the current one. |
| Model errors mention an unknown model | Gateway routes by its own names | Use the gateway's model id after `openai/`, e.g. `openai/anthropic/claude-opus-4-8`. |
| `auth = "subscription" requires base_url` | Nothing to authenticate against | Set `[provider] base_url` to the endpoint, or use `auth = "api_key"`. |
| `the endpoint is not authenticated (HTTP 401)` in doctor | The endpoint's own credential lapsed | Re-authenticate the endpoint (hotl holds no key to fix). |
