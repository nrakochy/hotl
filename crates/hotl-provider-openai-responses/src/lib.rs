//! OpenAI **Responses API** provider (`POST {base}/responses`).
//!
//! The third wire dialect on the same `Provider`/`StreamEvent` contract as
//! the Messages dialect (`hotl-provider-anthropic`) and the chat-completions
//! dialect (`hotl-provider-openai`). Selected by
//! `HOTL_MODEL=openai-responses/<model>`, opt-in: `openai/` stays the
//! documented door for every OpenAI-compatible endpoint, because this
//! dialect exists for one reason — OpenAI's current reasoning models reject
//! `reasoning_effort` next to function tools on `/v1/chat/completions`, so
//! the effort ladder is unusable there and needs `/v1/responses`.
//!
//! Reuses the chat dialect's base-URL and key configuration
//! (`HOTL_OPENAI_BASE_URL` / `OPENAI_API_KEY` / the api-key helper): a proxy
//! that serves `/v1/chat/completions` at a base serves `/v1/responses` at
//! the same base.
//!
//! Cross-provider translation follows the corpus rule: this dialect replays
//! its own `reasoning` items verbatim (a stateless tool loop 400s without
//! them) and drops foreign signed thinking; the other dialects drop
//! `reasoning` in turn (`hotl_provider::transform::strip_foreign_reasoning`
//! and the Anthropic converter's own filter).
