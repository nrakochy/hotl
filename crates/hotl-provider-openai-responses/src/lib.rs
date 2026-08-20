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

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use hotl_provider::key::{AuthAction, AuthRetry, KeySource};
use hotl_provider::{
    ArmGuard, Effort, EffortLadder, Provider, ProviderError, SamplingRequest, SseAssembler,
    StreamEvent, ToolDef, Warmable, ALL_EFFORTS,
};
use hotl_types::{Item, StopReason, TokenUsage};
use serde_json::{json, Value};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// §S3.2: bounds the warm request end-to-end, independent of (and shorter
/// than) the client's own `connect_timeout` — see the twin constant in
/// `hotl-provider-anthropic` for the full rationale.
const ARM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Marks a failed tool result in the Responses dialect.
///
/// Anthropic carries this structurally (`"is_error": true`); this wire has no
/// equivalent field on a `function_call_output` item, so the signal is
/// in-band or it is lost. Without it the model cannot tell a failure from a
/// success and the errors-are-prompts loop silently stops working (T3-13).
/// Twin of the constant in `hotl-provider-openai`. Do not remove as cosmetic.
const TOOL_ERROR_PREFIX: &str = "[tool_error] ";

/// The Responses serializer. Public so the provider (`stream()`), the
/// body-render tests and the testkit share one entry point. Built once per
/// capability, like the chat dialect's `legacy` flip: `explicit` is the
/// caller's decision (model gate, policy, probe latch); the renderer obeys.
pub fn body_for(req: &SamplingRequest, explicit: bool) -> Value {
    // Derived once per request from static catalog data. The OpenAI family
    // is uncatalogued, so this fails open (images always sent) — a
    // non-vision server answers with its own 400, surfaced honestly.
    let send_images = hotl_provider::catalog::supports_images(&req.model);
    let mut input = Vec::new();
    for item in req.items.iter() {
        convert_item(item, &mut input, send_images, explicit);
    }
    // Tail and MOIM ride after the last marker: under GPT-5.6's rules a
    // marker on either would rewrite the whole prefix every sample (0046).
    for item in req.ephemeral_tail.iter() {
        convert_item(item, &mut input, send_images, false);
    }
    if let Some(tc) = &req.turn_context {
        input.push(json!({"role": "user", "content": tc}));
    }
    let mut body = json!({
        "model": req.model,
        "stream": true,
        // No `previous_response_id` statefulness: hotl re-renders the full
        // history every sample (speculation byte-identity), so nothing may
        // be stored server-side.
        "store": false,
        // Encrypted reasoning content is the default under store:false on
        // current OpenAI, but the legacy `include` spelling is harmless and
        // covers older gateways.
        "include": ["reasoning.encrypted_content"],
        "max_output_tokens": req.max_tokens,
        "input": input,
    });
    if !req.system.is_empty() {
        body["instructions"] = json!(req.system.as_ref());
    }
    if !req.tools.is_empty() {
        body["tools"] = json!(req.tools.iter().map(tool_json).collect::<Vec<_>>());
    }
    // Routing key first, prefix hash second: without it every session's
    // samples collapse onto the shared system-prompt head (0045).
    if let Some(key) = &req.cache_key {
        body["prompt_cache_key"] = json!(key.as_ref());
    }
    // `Static` → explicit-only mode with a marker on every durable user-role
    // item when the model (or the override) says GPT-5.6+; `Off` → no cache
    // fields at all, byte-identical to the implicit wire.
    if explicit {
        body["prompt_cache_options"] = hotl_provider::openai_cache::options();
    }
    // Only spoken when the session opted into the ladder: an unconfigured
    // request keeps its old bytes (the 0029 byte-conservatism rule), and an
    // unsolicited `reasoning` object — even `summary` alone — 400s on
    // non-reasoning models.
    if req.effort.is_some() {
        if !req.thinking {
            // `thinking: false` wins: it is the explicit off-switch, and
            // `none` is the rung the neutral ladder deliberately does not
            // carry. No summary — there is no reasoning to summarize.
            body["reasoning"] = json!({"effort": "none"});
        } else if let Some(level) = req
            .effort
            .and_then(|e| ResponsesLadder.resolve(&req.model, e))
            .map(Effort::as_str)
        {
            body["reasoning"] = json!({"effort": level, "summary": "auto"});
        }
    }
    body
}

/// gpt-5.6 accepts `none|low|medium|high|xhigh|max`, 1:1 with hotl's rungs,
/// so every rung maps through unclamped (`none` is the off-switch, not a
/// rung). The family is deliberately uncatalogued, so there is no per-model
/// gating to do — the twin of `OpenAiLadder` in `hotl-provider-openai`.
pub struct ResponsesLadder;

impl EffortLadder for ResponsesLadder {
    fn rungs(&self, _model: &str) -> &'static [Effort] {
        ALL_EFFORTS
    }
}

/// Flattened, unlike the chat dialect's nested `function` wrapper — the
/// Responses tool shape carries name/description/parameters at the top
/// level. No `strict`: hotl's schemas are not strict-mode clean.
fn tool_json(t: &ToolDef) -> Value {
    json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.input_schema})
}

/// `mark`: put a GPT-5.6 explicit breakpoint on this item's last content
/// block. Only the durable loop ever passes `true`, so a marker on ephemeral
/// content is unrepresentable here, as on Anthropic.
fn convert_item(item: &Item, out: &mut Vec<Value>, send_images: bool, mark: bool) {
    match item {
        // System rides `instructions`, never `input`.
        Item::System { .. } | Item::Unknown => {}
        // INVARIANT: an unmarked imageless user item stays `"content": <plain
        // string>` — byte-identical to the pre-image wire (the chat dialect's
        // invariant, kept here so gateways that reject array content for
        // plain text keep working). Image parts precede the text part,
        // matching every other dialect: one provider-neutral ordering rule.
        // A marked item is always the array form: the marker needs a block.
        Item::User { text, images, .. } => {
            let with_images = send_images && !images.is_empty();
            if !with_images && !mark {
                let rendered =
                    hotl_provider::transform::text_with_omitted_images(text, images.len());
                out.push(json!({"role": "user", "content": rendered.as_ref()}));
                return;
            }
            let mut parts: Vec<Value> = Vec::new();
            let mut text_part = if with_images {
                parts.extend(images.iter().map(|img| {
                    json!({
                        "type": "input_image",
                        "image_url": format!("data:{};base64,{}", img.media_type, img.data)
                    })
                }));
                json!({"type": "input_text", "text": text})
            } else {
                let rendered =
                    hotl_provider::transform::text_with_omitted_images(text, images.len());
                json!({"type": "input_text", "text": rendered.as_ref()})
            };
            if mark {
                text_part["prompt_cache_breakpoint"] = hotl_provider::openai_cache::breakpoint();
            }
            parts.push(text_part);
            out.push(json!({"role": "user", "content": parts}));
        }
        // Blocks replay in original order, so a `reasoning` item stays
        // directly before the `function_call` it justified — the stateless
        // tool loop 400s if the pair is separated or the reasoning dropped.
        Item::Assistant { blocks } => {
            for b in blocks {
                match b.get("type").and_then(Value::as_str) {
                    // This dialect's own reasoning items: replayed verbatim
                    // (encrypted_content and all — the API validates them).
                    Some("reasoning") => out.push(b.clone()),
                    Some("text") => out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": b.get("text").and_then(Value::as_str).unwrap_or(""),
                        }],
                    })),
                    Some("tool_use") => out.push(json!({
                        "type": "function_call",
                        "call_id": b.get("id").and_then(Value::as_str).unwrap_or(""),
                        "name": b.get("name").and_then(Value::as_str).unwrap_or(""),
                        "arguments": serde_json::to_string(
                            b.get("input").unwrap_or(&Value::Null)
                        )
                        .unwrap_or_else(|_| "{}".into()),
                    })),
                    // Foreign signed thinking never crosses providers; this
                    // dialect's foreign set is thinking/redacted_thinking
                    // (it cannot call `strip_foreign_reasoning`, which would
                    // strip its own `reasoning` blocks). Unknown block types
                    // are dropped the same way the typed views skip them.
                    _ => {}
                }
            }
        }
        Item::ToolResults { results } => {
            let last = results.len().saturating_sub(1);
            for (i, r) in results.iter().enumerate() {
                // T3-13 twin: Responses has no structured error field on
                // `function_call_output` either, so the signal is in-band.
                let output = if r.is_error && !r.content.starts_with(TOOL_ERROR_PREFIX) {
                    format!("{TOOL_ERROR_PREFIX}{}", r.content)
                } else {
                    r.content.clone()
                };
                // One marker per item, on the last result: a parallel batch
                // of K results can never exhaust the four write slots.
                let output = if mark && i == last {
                    json!([{
                        "type": "input_text",
                        "text": output,
                        "prompt_cache_breakpoint": hotl_provider::openai_cache::breakpoint(),
                    }])
                } else {
                    Value::String(output)
                };
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": r.tool_use_id,
                    "output": output,
                }));
            }
        }
    }
}

/// What one wire output item folds into.
enum SlotKind {
    Message {
        text: String,
    },
    FunctionCall {
        call_id: String,
        name: String,
        args: String,
    },
    /// The verbatim item arrives whole at `response.output_item.done`;
    /// summary deltas stream through as `ThinkingDelta` without accumulating.
    Reasoning {
        item: Option<Value>,
    },
    /// An item type this assembler does not know: opens no block, streams
    /// nothing, contributes no final block (forward compat).
    Ignored,
}

/// One wire output item, keyed by its `output_index`.
struct Slot {
    /// Our block index (running count of opened blocks). `usize::MAX` for
    /// `Ignored` slots, which open no block — never read for them.
    block: usize,
    closed: bool,
    kind: SlotKind,
}

/// INVARIANT (T2-9 twin): every wire `output_index` is bounded before it
/// reaches a `Vec`. Enforced by
/// `an_absurd_output_index_is_a_parse_error_not_an_allocation`.
fn index_of(v: &Value) -> Result<usize, ProviderError> {
    let raw = v.get("output_index").and_then(Value::as_u64).unwrap_or(0);
    let index = usize::try_from(raw).unwrap_or(usize::MAX);
    if index > hotl_provider::MAX_BLOCK_INDEX {
        return Err(ProviderError::Parse(format!(
            "output_index {raw} exceeds the {} block limit",
            hotl_provider::MAX_BLOCK_INDEX
        )));
    }
    Ok(index)
}

fn str_at(v: &Value, field: &str) -> String {
    v.get(field)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Fold a complete wire item into its slot. Shared by
/// `response.output_item.done` and the terminal sweep, so a sloppy gateway
/// that skips per-item terminators still seals identically.
fn seal_slot(slot: &mut Slot, item: &Value) {
    match &mut slot.kind {
        SlotKind::FunctionCall {
            call_id,
            name,
            args,
        } => {
            // A non-empty complete `arguments` REPLACES accumulated deltas:
            // it is the authoritative whole JSON, not another fragment.
            if let Some(a) = item.get("arguments").and_then(Value::as_str) {
                if !a.is_empty() {
                    *args = a.to_string();
                }
            }
            // Backfill identity if `output_item.added` lacked it.
            if call_id.is_empty() {
                *call_id = str_at(item, "call_id");
            }
            if name.is_empty() {
                *name = str_at(item, "name");
            }
        }
        SlotKind::Reasoning { item: captured } => {
            // Verbatim, minus `status`: response-lifecycle state, not part
            // of the replayable item (replaying it is a 400 on some
            // gateways and noise on the rest).
            if !item.is_null() {
                let mut it = item.clone();
                if let Some(obj) = it.as_object_mut() {
                    obj.remove("status");
                }
                *captured = Some(it);
            }
        }
        // Message: the accumulated delta buffers are what the UI saw, and
        // T2-11 says what the UI saw is what history keeps.
        SlotKind::Message { .. } | SlotKind::Ignored => {}
    }
}

/// Folds Responses-API stream events into canonical blocks.
///
/// The shared `SseParser` strips `event:` lines and filters `[DONE]`, so
/// dispatch is on the payload's `"type"` field — every Responses payload
/// carries one. T3-14 holds per item: this dialect has per-item terminators
/// (`response.output_item.done`), so `BlockEnd` is emitted there rather than
/// synthesized at finish like the chat dialect — plus a terminal sweep so
/// every opened block closes even against a gateway that drops terminators.
#[derive(Default)]
pub struct ResponsesAssembler {
    /// Keyed by wire `output_index`.
    slots: Vec<Option<Slot>>,
    /// Our block index = running count of opened blocks (`Ignored` slots
    /// open none).
    next_block: usize,
    /// A refusal streamed: the deltas ride the message text (the human must
    /// see the refusal live; text-shaped keeps blocks canonical) and the
    /// terminal stop becomes `Refusal`.
    refused: bool,
    usage: TokenUsage,
    stop: Option<StopReason>,
    done: bool,
}

impl SseAssembler for ResponsesAssembler {
    fn handle(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ProviderError::Parse(format!("bad Responses SSE json: {e}")))?;
        match v.get("type").and_then(Value::as_str).unwrap_or("") {
            "response.output_item.added" => self.on_item_added(&v),
            "response.output_text.delta" => self.on_text_delta(&v, false),
            "response.refusal.delta" => self.on_text_delta(&v, true),
            "response.function_call_arguments.delta" => self.on_args_delta(&v),
            "response.reasoning_summary_text.delta" => self.on_summary_delta(&v),
            "response.output_item.done" => self.on_item_done(&v),
            "response.completed" => self.on_terminal(&v, None),
            "response.incomplete" => {
                let stop = match v
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                {
                    "max_output_tokens" => StopReason::MaxTokens,
                    "content_filter" => StopReason::Refusal,
                    _ => StopReason::Other,
                };
                self.on_terminal(&v, Some(stop))
            }
            // Mirror the Anthropic error arm: map the wire code to the
            // status it would have carried pre-stream, so an in-stream
            // failure classifies exactly like an HTTP-level one and
            // `retry::is_availability` / the fallback-model chain stay
            // reachable.
            "response.failed" | "error" => {
                let err = v.pointer("/response/error").unwrap_or(&v);
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let status = match err.get("code").and_then(Value::as_str) {
                    Some("rate_limit_exceeded") => 429,
                    Some("server_error") => 500,
                    _ => 400,
                };
                Err(ProviderError::Http {
                    status,
                    message: format!("in-stream error: {msg}"),
                    retry_after: None,
                })
            }
            // response.created / response.in_progress / content_part.* /
            // *_text.done / *_arguments.done / reasoning_summary_part.* and
            // anything newer: ignored (forward compat).
            _ => Ok(vec![]),
        }
    }

    fn finish(self) -> Result<StreamEvent, ProviderError> {
        if !self.done {
            // Truncation is an honest error, never a silent empty result.
            return Err(ProviderError::Parse(
                "Responses stream ended before response.completed".into(),
            ));
        }
        // Opened-block order = wire output order, so a reasoning item stays
        // before the function_call it justified.
        let mut opened: Vec<&Slot> = self
            .slots
            .iter()
            .flatten()
            .filter(|s| !matches!(s.kind, SlotKind::Ignored))
            .collect();
        opened.sort_by_key(|s| s.block);
        let mut blocks = Vec::new();
        for slot in opened {
            match &slot.kind {
                SlotKind::Message { text } => {
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                }
                SlotKind::FunctionCall {
                    call_id,
                    name,
                    args,
                } => {
                    let input: Value = if args.trim().is_empty() {
                        json!({})
                    } else {
                        // Arg healing (M3a): conservative repair before
                        // giving up.
                        hotl_provider::repair::parse_or_repair(args).ok_or_else(|| {
                            ProviderError::Parse(format!("tool args for `{name}` didn't parse"))
                        })?
                    };
                    // Exactly {type,id,name,input} with id = call_id:
                    // Anthropic echoes Assistant blocks verbatim and
                    // validates byte-for-byte; no extra fields (the `fc_`
                    // item id is deliberately not stored — see plan 0031 M2).
                    blocks.push(
                        json!({"type": "tool_use", "id": call_id, "name": name, "input": input}),
                    );
                }
                SlotKind::Reasoning { item } => {
                    // A reasoning slot still empty after the sweep would
                    // poison the next request's replay — fail loudly now.
                    let item = item.clone().ok_or_else(|| {
                        ProviderError::Parse(
                            "reasoning item never delivered (no output_item.done, absent \
                             from response.completed); its replay would be rejected"
                                .into(),
                        )
                    })?;
                    blocks.push(item);
                }
                SlotKind::Ignored => unreachable!("filtered above"),
            }
        }
        Ok(StreamEvent::Completed {
            stop: self.stop.unwrap_or(StopReason::EndTurn),
            usage: self.usage,
            blocks,
        })
    }
}

impl ResponsesAssembler {
    /// INVARIANT (T2-11 twin): a delta the assembler cannot place is a parse
    /// error, never a silent drop — the user would read text the model never
    /// sees again.
    fn slot_mut(&mut self, index: usize) -> Result<&mut Slot, ProviderError> {
        self.slots
            .get_mut(index)
            .and_then(Option::as_mut)
            .ok_or_else(|| {
                ProviderError::Parse(format!(
                    "delta for output_index {index} arrived before its \
                     response.output_item.added; the stream is malformed"
                ))
            })
    }

    fn on_item_added(&mut self, v: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let index = index_of(v)?;
        let item = v.get("item").cloned().unwrap_or(Value::Null);
        let (kind, stream_kind) = match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "message" => (
                SlotKind::Message {
                    text: String::new(),
                },
                Some("text"),
            ),
            "function_call" => (
                SlotKind::FunctionCall {
                    call_id: str_at(&item, "call_id"),
                    name: str_at(&item, "name"),
                    args: String::new(),
                },
                Some("tool_use"),
            ),
            // "thinking" is the kind the Anthropic dialect already emits;
            // the TUI folds any thinking deltas into one collapsible item.
            "reasoning" => (SlotKind::Reasoning { item: None }, Some("thinking")),
            _ => (SlotKind::Ignored, None),
        };
        if self.slots.len() <= index {
            self.slots.resize_with(index + 1, || None);
        }
        let Some(stream_kind) = stream_kind else {
            self.slots[index] = Some(Slot {
                block: usize::MAX,
                closed: false,
                kind,
            });
            return Ok(vec![]);
        };
        let block = self.next_block;
        self.next_block += 1;
        self.slots[index] = Some(Slot {
            block,
            closed: false,
            kind,
        });
        Ok(vec![StreamEvent::BlockStart {
            index: block,
            kind: stream_kind.into(),
        }])
    }

    fn on_text_delta(
        &mut self,
        v: &Value,
        refusal: bool,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let index = index_of(v)?;
        let delta = str_at(v, "delta");
        if refusal {
            self.refused = true;
        }
        let slot = self.slot_mut(index)?;
        let SlotKind::Message { text } = &mut slot.kind else {
            return Err(ProviderError::Parse(format!(
                "text delta for output_index {index} targets a non-message item"
            )));
        };
        if delta.is_empty() {
            return Ok(vec![]);
        }
        text.push_str(&delta);
        Ok(vec![StreamEvent::TextDelta {
            index: slot.block,
            text: delta,
        }])
    }

    fn on_args_delta(&mut self, v: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let index = index_of(v)?;
        let delta = str_at(v, "delta");
        let slot = self.slot_mut(index)?;
        let SlotKind::FunctionCall { args, .. } = &mut slot.kind else {
            return Err(ProviderError::Parse(format!(
                "function_call arguments delta for output_index {index} targets a \
                 non-function_call item"
            )));
        };
        args.push_str(&delta);
        Ok(vec![StreamEvent::ToolInputDelta {
            index: slot.block,
            json: delta,
        }])
    }

    fn on_summary_delta(&mut self, v: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let index = index_of(v)?;
        let delta = str_at(v, "delta");
        let slot = self.slot_mut(index)?;
        let SlotKind::Reasoning { .. } = &slot.kind else {
            return Err(ProviderError::Parse(format!(
                "reasoning summary delta for output_index {index} targets a \
                 non-reasoning item"
            )));
        };
        // Nothing accumulated: the verbatim item (summary included) arrives
        // whole at `response.output_item.done`.
        Ok(vec![StreamEvent::ThinkingDelta {
            index: slot.block,
            text: delta,
        }])
    }

    fn on_item_done(&mut self, v: &Value) -> Result<Vec<StreamEvent>, ProviderError> {
        let index = index_of(v)?;
        let item = v.get("item").cloned().unwrap_or(Value::Null);
        let slot = self.slot_mut(index)?;
        if slot.closed {
            return Ok(vec![]);
        }
        seal_slot(slot, &item);
        slot.closed = true;
        if matches!(slot.kind, SlotKind::Ignored) {
            return Ok(vec![]);
        }
        Ok(vec![StreamEvent::BlockEnd { index: slot.block }])
    }

    /// `response.completed` / `response.incomplete`: merge final usage,
    /// sweep-close every opened slot, fix the stop reason, mark done.
    fn on_terminal(
        &mut self,
        v: &Value,
        stop: Option<StopReason>,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        if let Some(u) = v.pointer("/response/usage") {
            // OpenAI counts cached and written tokens INSIDE `input_tokens`;
            // `TokenUsage` counts both outside it (Anthropic semantics).
            let cached = u
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let written = u
                .pointer("/input_tokens_details/cache_write_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if let Some(n) = u.get("input_tokens").and_then(Value::as_u64) {
                let (input, read, creation) =
                    hotl_provider::openai_cache::carve_usage(n, cached, written);
                self.usage.input_tokens = input;
                self.usage.cache_read_input_tokens = read;
                self.usage.cache_creation_input_tokens = creation;
            }
            if let Some(n) = u.get("output_tokens").and_then(Value::as_u64) {
                self.usage.output_tokens = n;
            }
        }
        // INVARIANT (T3-14): every opened block closes, even against a
        // gateway that never sent `output_item.done`. Unclosed slots are
        // backfilled from the terminal event's own copy of the output.
        let mut out = Vec::new();
        for (wire_idx, entry) in self.slots.iter_mut().enumerate() {
            let Some(slot) = entry else { continue };
            if slot.closed {
                continue;
            }
            if let Some(item) = v.pointer(&format!("/response/output/{wire_idx}")) {
                seal_slot(slot, item);
            }
            slot.closed = true;
            if !matches!(slot.kind, SlotKind::Ignored) {
                out.push(StreamEvent::BlockEnd { index: slot.block });
            }
        }
        self.stop = Some(stop.unwrap_or_else(|| {
            if self.refused {
                StopReason::Refusal
            } else if self
                .slots
                .iter()
                .flatten()
                .any(|s| matches!(s.kind, SlotKind::FunctionCall { .. }))
            {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }
        }));
        self.done = true;
        Ok(out)
    }
}

/// The one client constructor. A default `reqwest` client has no connect
/// timeout, no read timeout, and no keepalive; T1-4 traces a wedged session
/// straight back to it.
///
/// `expect` rather than a fallback to a default client on purpose: T3-12 found
/// exactly that fallback silently discarding a redirect policy and a timeout
/// elsewhere in the tree. A client that cannot be built with these options is
/// a broken TLS backend, and shipping an unbounded client instead is worse
/// than failing loudly at construction.
fn http_client(connect: std::time::Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        // §S3.2: an HTTP/2 ping every 15s, even while the connection is
        // otherwise idle, keeps a pooled connection alive across a human's
        // multi-minute pause instead of it being reclaimed and re-paying
        // DNS+TCP+TLS on the next sample.
        .http2_keep_alive_interval(std::time::Duration::from_secs(15))
        .http2_keep_alive_while_idle(true)
        .build()
        .expect("reqwest client with timeouts (TLS backend unavailable?)")
}

pub struct OpenAiResponsesProvider {
    client: reqwest::Client,
    base_url: String,
    key_source: Arc<dyn KeySource>,
    headers_timeout: std::time::Duration,
    stream_idle_timeout: std::time::Duration,
    /// §S3.2 idempotency: `0` when idle, else the generation token of the
    /// in-flight warm request — see the twin field in
    /// `hotl-provider-anthropic` for the full rationale.
    armed: Arc<AtomicU64>,
    /// `None` = decide by model name (`openai_cache::breakpoints_supported`).
    cache_breakpoints: Option<bool>,
    /// Latched by the 0046 probe: the endpoint 400'd on a cache field once.
    breakpoints_rejected: Arc<AtomicBool>,
}

impl OpenAiResponsesProvider {
    pub fn new(base_url: String, key_source: Arc<dyn KeySource>) -> Self {
        Self {
            client: http_client(hotl_provider::timeouts::CONNECT),
            base_url,
            key_source,
            headers_timeout: hotl_provider::timeouts::HEADERS,
            stream_idle_timeout: hotl_provider::timeouts::STREAM_IDLE,
            armed: Arc::new(AtomicU64::new(0)),
            cache_breakpoints: None,
            breakpoints_rejected: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Force GPT-5.6 explicit cache breakpoints on or off, overriding the
    /// model-name gate (a gateway alias that hides the version, or an
    /// endpoint known to reject `prompt_cache_options`).
    pub fn cache_breakpoints(mut self, on: bool) -> Self {
        self.cache_breakpoints = Some(on);
        self
    }

    /// The three-term decision `stream()` renders under: the policy asks for
    /// markers, the probe has not latched, and the override or the name gate
    /// says the model takes them.
    pub fn wants_explicit(&self, req: &SamplingRequest) -> bool {
        req.cache.marks_breakpoints()
            && !self.breakpoints_rejected.load(Ordering::Relaxed)
            && self
                .cache_breakpoints
                .unwrap_or_else(|| hotl_provider::openai_cache::breakpoints_supported(&req.model))
    }

    /// The Responses endpoint for this provider's base URL. Shared by
    /// `stream()` and the §S3.2 warm request so the two can never disagree
    /// about where the connection pool's origin is — the twin of
    /// `completions_url` in `hotl-provider-openai`.
    fn responses_url(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    /// Override the defaults (a slow local endpoint, or a test that wants a
    /// short bound). `connect` rebuilds the client.
    pub fn with_timeouts(
        mut self,
        connect: std::time::Duration,
        headers: std::time::Duration,
        stream_idle: std::time::Duration,
    ) -> Self {
        self.client = http_client(connect);
        self.headers_timeout = headers;
        self.stream_idle_timeout = stream_idle;
        self
    }
}

/// One send attempt, classified. Keeps the stream generator small while
/// letting it yield `Retrying` events live (during the backoff, not after).
enum Attempt {
    Ok(reqwest::Response),
    Retry {
        reason: String,
        wait: std::time::Duration,
    },
    Fail(ProviderError),
}

fn classify_send(err: ProviderError, attempt: u32, reason: String) -> Attempt {
    match hotl_provider::retry::classify(&err, attempt) {
        hotl_provider::retry::Decision::Retry { delay } => Attempt::Retry {
            reason,
            wait: hotl_provider::retry::with_jitter(delay),
        },
        hotl_provider::retry::Decision::Fatal => Attempt::Fail(err),
    }
}

async fn classify_response(resp: reqwest::Response, attempt: u32) -> Attempt {
    if resp.status().is_success() {
        return Attempt::Ok(resp);
    }
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| hotl_provider::retry::parse_retry_after(v, hotl_provider::retry::now_unix()));
    // Read the error class before the body consumes the response.
    let error_type = resp
        .headers()
        .get("x-amzn-errortype")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    let body = resp.text().await.unwrap_or_default();
    let message = hotl_provider::api_error::detail(error_type.as_deref(), &body);
    if status == 401 || status == 403 {
        return Attempt::Fail(ProviderError::Auth(message));
    }
    let err = ProviderError::Http {
        status,
        message,
        retry_after,
    };
    classify_send(err, attempt, format!("HTTP {status}"))
}

async fn send_attempt(
    client: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
    body: &Value,
    attempt: u32,
) -> Attempt {
    let mut builder = client
        .post(url)
        .header("content-type", "application/json")
        .json(body);
    if let Some(key) = api_key {
        builder = builder.bearer_auth(key);
    }
    match builder.send().await {
        Ok(resp) => classify_response(resp, attempt).await,
        Err(e) => {
            let reason = e.to_string();
            classify_send(ProviderError::Transport(reason.clone()), attempt, reason)
        }
    }
}

/// Process-wide source of `spawn_arm` generation tokens (§S3.2) — see the
/// twin static in `hotl-provider-anthropic` for the full rationale.
static NEXT_ARM_TOKEN: AtomicU64 = AtomicU64::new(1);

/// §S3.2: fire one lightweight, credential-free GET at `url` to populate
/// `client`'s connection pool. See the twin function in
/// `hotl-provider-anthropic` for the full rationale — kept duplicated rather
/// than shared, matching the dialect crates' existing convention (compare
/// `http_client`, `classify_send`, `send_attempt` above).
fn spawn_arm(client: reqwest::Client, url: String, armed: Arc<AtomicU64>) -> ArmGuard {
    if armed.load(Ordering::Acquire) != 0 {
        return ArmGuard::noop();
    }
    let token = NEXT_ARM_TOKEN.fetch_add(1, Ordering::Relaxed);
    if armed
        .compare_exchange(0, token, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return ArmGuard::noop();
    }
    let task_flag = armed.clone();
    let handle = tokio::spawn(async move {
        let _ = tokio::time::timeout(ARM_TIMEOUT, client.get(&url).send()).await;
        let _ = task_flag.compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire);
    });
    let cancel_flag = armed;
    ArmGuard::new(move || {
        handle.abort();
        let _ = cancel_flag.compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire);
    })
}

impl Warmable for OpenAiResponsesProvider {
    fn arm(&self) -> ArmGuard {
        spawn_arm(
            self.client.clone(),
            self.responses_url(),
            self.armed.clone(),
        )
    }
}

impl Provider for OpenAiResponsesProvider {
    fn arm(&self) -> ArmGuard {
        <Self as Warmable>::arm(self)
    }

    /// The chat dialect's stream loop with one probe of its own: a 4xx
    /// naming a prompt-cache field re-renders the same sample without the
    /// cache fields and latches the provider so later samples skip the probe.
    fn stream(
        &self,
        req: SamplingRequest,
    ) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
        let client = self.client.clone();
        let url = self.responses_url();
        let source = self.key_source.clone();
        let headers_timeout = self.headers_timeout;
        let stream_idle_timeout = self.stream_idle_timeout;
        let explicit = self.wants_explicit(&req);
        let mut body = body_for(&req, explicit);
        let rejected = Arc::clone(&self.breakpoints_rejected);

        Box::pin(async_stream::stream! {
            // INVARIANT (T2-16): `attempt` counts network/HTTP attempts only.
            // An auth refresh is a separate, once-per-request budget owned by
            // `AuthRetry`, so a 401 → refresh → 429 sequence still has its
            // full retry allowance.
            let mut attempts_used: u32 = 0;
            let mut auth_retry = AuthRetry::default();
            let response = loop {
                let attempt = attempts_used + 1;
                let key = match source.get().await {
                    Ok(k) => k,
                    Err(e) => {
                        yield Err(ProviderError::Auth(e.0));
                        return;
                    }
                };
                let sent = tokio::time::timeout(
                    headers_timeout,
                    send_attempt(&client, &url, key.as_deref(), &body, attempt),
                )
                .await;
                let outcome = match sent {
                    Ok(a) => a,
                    // A timed-out attempt is retryable, not terminal.
                    Err(_) => classify_send(
                        ProviderError::Transport(format!(
                            "no response headers within {}s",
                            headers_timeout.as_secs()
                        )),
                        attempt,
                        "response header timeout".into(),
                    ),
                };
                match outcome {
                    Attempt::Ok(resp) => break resp,
                    Attempt::Retry { reason, wait } => {
                        attempts_used = attempt;
                        yield Ok(StreamEvent::Retrying { attempt, reason });
                        tokio::time::sleep(wait).await;
                    }
                    Attempt::Fail(ProviderError::Auth(msg)) => {
                        // attempts_used deliberately unchanged.
                        match auth_retry.on_auth_error(source.refreshable()) {
                            AuthAction::RefreshAndRetry => match source.refresh().await {
                                Ok(()) => {
                                    yield Ok(StreamEvent::Retrying {
                                        attempt,
                                        reason: "auth failed — re-running api_key_helper".into(),
                                    });
                                }
                                Err(ke) => {
                                    yield Err(ProviderError::Auth(format!(
                                        "{msg} (key refresh also failed: {ke})"
                                    )));
                                    return;
                                }
                            },
                            AuthAction::Surface => {
                                yield Err(ProviderError::Auth(msg));
                                return;
                            }
                        }
                    }
                    Attempt::Fail(e)
                        if explicit
                            && !rejected.load(Ordering::Relaxed)
                            && hotl_provider::openai_cache::rejects_breakpoints(&e) =>
                    {
                        // A capability probe, not a failure: no retry slot,
                        // latched per provider.
                        rejected.store(true, Ordering::Relaxed);
                        body = body_for(&req, false);
                        yield Ok(StreamEvent::Retrying {
                            attempt,
                            reason: "endpoint rejects prompt-cache breakpoints — retrying \
                                     without; caching falls back to implicit".into(),
                        });
                    }
                    Attempt::Fail(e) => {
                        yield Err(e);
                        return;
                    }
                }
            };
            yield Ok(StreamEvent::Started);
            let inner = hotl_provider::drive_sse(
                response.bytes_stream(),
                ResponsesAssembler::default(),
                stream_idle_timeout,
            );
            futures_util::pin_mut!(inner);
            while let Some(ev) = inner.next().await {
                yield ev;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_types::ToolResultItem;

    fn sampling_req() -> SamplingRequest {
        SamplingRequest {
            model: "gpt-5.6-luna-1".into(),
            max_tokens: 16,
            system: "".into(),
            items: hotl_provider::arc_items(vec![Item::User {
                text: "hi".into(),
                synthetic: None,
                images: Vec::new(),
            }]),
            ephemeral_tail: std::sync::Arc::new(Vec::new()),
            tools: std::sync::Arc::from(Vec::<ToolDef>::new()),
            thinking: false,
            effort: None,
            cache: hotl_provider::CachePolicy::Off,
            turn_context: None,
            cache_key: None,
        }
    }

    /// OpenAI routes by `prompt_cache_key` first and prefix hash second; the
    /// session id is what keeps one session's samples on one cache shard.
    #[test]
    fn the_session_cache_key_rides_prompt_cache_key() {
        let mut req = sampling_req();
        req.cache_key = Some("01J0SESSIONULID".into());
        assert_eq!(body_for(&req, false)["prompt_cache_key"], "01J0SESSIONULID");
        req.cache_key = None;
        assert!(body_for(&req, false).get("prompt_cache_key").is_none());
    }

    #[test]
    fn the_body_speaks_the_responses_dialect() {
        let mut req = sampling_req();
        req.system = "sys".into();
        req.tools = vec![ToolDef {
            name: "read".into(),
            description: "d".into(),
            input_schema: json!({"type":"object"}),
        }]
        .into();
        let body = body_for(&req, false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["max_output_tokens"], 16);
        assert_eq!(body["instructions"], "sys");
        assert!(
            body.get("max_completion_tokens").is_none() && body.get("messages").is_none(),
            "no chat-completions vocabulary may leak into this dialect"
        );
        // Tools are flattened: no nested `function` wrapper, no `strict`.
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
        assert!(body["tools"][0].get("function").is_none());
        assert!(body["tools"][0].get("strict").is_none());
    }

    /// Wire order: durable items, then the ephemeral tail, then MOIM last.
    #[test]
    fn the_ephemeral_tail_keeps_its_position_before_moim() {
        let mut req = sampling_req();
        req.ephemeral_tail = hotl_provider::arc_items(vec![Item::User {
            text: "<todos>\n[~] a\n</todos>".into(),
            synthetic: Some(hotl_types::SyntheticReason::Todos),
            images: Vec::new(),
        }]);
        req.turn_context = Some("<turn-context/>".into());
        let body = body_for(&req, false);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["content"], "hi");
        assert_eq!(input[1]["content"], "<todos>\n[~] a\n</todos>");
        assert_eq!(input[2]["content"], "<turn-context/>");
        assert_eq!(input.len(), 3);
    }

    fn user(text: &str) -> Item {
        Item::User {
            text: text.into(),
            synthetic: None,
            images: Vec::new(),
        }
    }

    fn todos(text: &str) -> Item {
        Item::User {
            text: text.into(),
            synthetic: Some(hotl_types::SyntheticReason::Todos),
            images: Vec::new(),
        }
    }

    fn tool_use(id: &str) -> Item {
        Item::Assistant {
            blocks: vec![json!({"type": "tool_use", "id": id, "name": "read", "input": {}})],
        }
    }

    fn results(ids: &[&str]) -> Item {
        Item::ToolResults {
            results: ids
                .iter()
                .map(|id| ToolResultItem {
                    tool_use_id: (*id).into(),
                    content: format!("out-{id}"),
                    is_error: false,
                })
                .collect(),
        }
    }

    /// The 0046 fixture: durable history with one parallel batch, a todos
    /// tail and a MOIM — the shape GPT-5.6's implicit breakpoint punishes.
    fn static_req() -> SamplingRequest {
        let mut req = sampling_req();
        req.cache = hotl_provider::CachePolicy::Static {
            prefix_ttl: hotl_provider::CacheTtl::FiveMinutes,
        };
        req.cache_key = Some("01J0SESSIONULID".into());
        req.items = hotl_provider::arc_items(vec![
            user("hi"),
            tool_use("a"),
            results(&["a1", "a2"]),
            Item::Assistant {
                blocks: vec![json!({"type": "text", "text": "done"})],
            },
            user("more"),
        ]);
        req.ephemeral_tail = hotl_provider::arc_items(vec![todos("<todos>\n[~] a\n</todos>")]);
        req.turn_context = Some("<turn-context now_unix_ms=\"1\"/>".into());
        req
    }

    fn markers_in(v: &Value) -> usize {
        v.to_string().matches("\"prompt_cache_breakpoint\"").count()
    }

    /// D1/D2: explicit-only mode, one marker on the last block of every
    /// durable user-role item, nothing on the tail or the MOIM.
    #[test]
    fn explicit_mode_marks_every_durable_user_role_item_and_nothing_ephemeral() {
        let body = body_for(&static_req(), true);
        assert_eq!(body["prompt_cache_options"], json!({"mode": "explicit"}));
        assert_eq!(body["prompt_cache_key"], "01J0SESSIONULID");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 8, "{input:#?}");
        // "hi": array form, marker on its only block.
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "hi");
        assert_eq!(
            input[0]["content"][0]["prompt_cache_breakpoint"],
            json!({"mode": "explicit"})
        );
        // The batch: a1 stays a plain string, a2 (the last) carries the marker.
        assert_eq!(input[2]["call_id"], "a1");
        assert_eq!(input[2]["output"], "out-a1");
        assert_eq!(input[3]["call_id"], "a2");
        assert_eq!(input[3]["output"][0]["type"], "input_text");
        assert_eq!(input[3]["output"][0]["text"], "out-a2");
        assert_eq!(
            input[3]["output"][0]["prompt_cache_breakpoint"],
            json!({"mode": "explicit"})
        );
        // "more": marked.
        assert_eq!(input[5]["content"][0]["text"], "more");
        assert_eq!(markers_in(&input[5]), 1);
        // Tail and MOIM: plain strings, no marker anywhere in them.
        assert_eq!(input[6]["content"], "<todos>\n[~] a\n</todos>");
        assert_eq!(input[7]["content"], "<turn-context now_unix_ms=\"1\"/>");
        assert_eq!(markers_in(&input[6]) + markers_in(&input[7]), 0);
        assert_eq!(markers_in(&body), 3);
    }

    /// D2: with explicit mode off, the body is today's wire byte for byte —
    /// no cache fields, plain-string user content and tool outputs.
    #[test]
    fn a_pre_5_6_model_keeps_the_implicit_wire_byte_for_byte() {
        let mut req = static_req();
        req.model = "gpt-5.5".into();
        let body = body_for(&req, false);
        let expected = json!({
            "model": "gpt-5.5",
            "stream": true,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "max_output_tokens": 16,
            "prompt_cache_key": "01J0SESSIONULID",
            "input": [
                {"role": "user", "content": "hi"},
                {"type": "function_call", "call_id": "a", "name": "read", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "a1", "output": "out-a1"},
                {"type": "function_call_output", "call_id": "a2", "output": "out-a2"},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "done"}]},
                {"role": "user", "content": "more"},
                {"role": "user", "content": "<todos>\n[~] a\n</todos>"},
                {"role": "user", "content": "<turn-context now_unix_ms=\"1\"/>"},
            ],
        });
        assert_eq!(body.to_string(), expected.to_string());
        assert!(!body.to_string().contains("prompt_cache_breakpoint"));
        assert!(body.get("prompt_cache_options").is_none());
    }

    /// D1: placement is append-stable. Sample N+1 appends durable items and
    /// swaps the ephemeral tail and MOIM; every input element of N is still
    /// at the same index in N+1, markers included.
    #[test]
    fn consecutive_samples_keep_every_earlier_marker_in_place() {
        let req_n = static_req();
        let mut req_n1 = static_req();
        let mut items: Vec<Item> = req_n.items.iter().map(|i| (**i).clone()).collect();
        items.push(tool_use("b"));
        items.push(results(&["b1"]));
        req_n1.items = hotl_provider::arc_items(items);
        req_n1.ephemeral_tail = hotl_provider::arc_items(vec![todos("<todos>\n[x] a\n</todos>")]);
        req_n1.turn_context = Some("<turn-context now_unix_ms=\"2\"/>".into());

        let input_n = body_for(&req_n, true)["input"].as_array().unwrap().clone();
        let input_n1 = body_for(&req_n1, true)["input"].as_array().unwrap().clone();
        let marked = |input: &[Value]| -> Vec<usize> {
            input
                .iter()
                .enumerate()
                .filter(|(_, v)| markers_in(v) > 0)
                .map(|(i, _)| i)
                .collect()
        };
        let (m_n, m_n1) = (marked(&input_n), marked(&input_n1));
        assert_eq!(m_n, vec![0, 3, 5]);
        assert!(
            m_n1.starts_with(&m_n),
            "{m_n:?} is not a prefix of {m_n1:?}"
        );
        assert_eq!(m_n1, vec![0, 3, 5, 7]);
        // The durable prefix of N (everything before its tail) is byte-equal.
        let n = req_n.items.len() + 1; // 5 items render as 6 input elements
        assert_eq!(input_n[..n], input_n1[..n]);
    }

    /// D3: the override beats the name gate in both directions; the policy
    /// beats both.
    #[test]
    fn the_override_and_the_policy_gate_the_markers() {
        let key: Arc<dyn KeySource> = Arc::new(hotl_provider::key::StaticKey(None));
        let p = |b: String| OpenAiResponsesProvider::new(b, key.clone());
        let with_model = |m: &str| {
            let mut req = static_req();
            req.model = m.into();
            req
        };
        assert!(p("b".into())
            .cache_breakpoints(true)
            .wants_explicit(&with_model("sol-fast")));
        assert!(!p("b".into())
            .cache_breakpoints(false)
            .wants_explicit(&with_model("gpt-5.6")));
        let mut off = with_model("gpt-5.6");
        off.cache = hotl_provider::CachePolicy::Off;
        assert!(!p("b".into()).wants_explicit(&off));
        assert!(!p("b".into()).wants_explicit(&with_model("gpt-5.5")));
        assert!(p("b".into()).wants_explicit(&with_model("gpt-5.6-luna-1")));
    }

    /// An empty system leaves `instructions` off the wire entirely.
    #[test]
    fn an_empty_system_emits_no_instructions_key() {
        let body = body_for(&sampling_req(), false);
        assert!(body.get("instructions").is_none());
    }

    /// The byte-conservatism rule, all four corners (0029 tracker row 71):
    /// only effort-set requests speak `reasoning` at all.
    #[test]
    fn effort_gating_covers_all_four_corners() {
        // Unset → no key, whatever `thinking` says.
        let mut req = sampling_req();
        assert!(body_for(&req, false).get("reasoning").is_none());
        req.thinking = true;
        assert!(body_for(&req, false).get("reasoning").is_none());
        // Set + thinking → the resolved rung, with summaries on.
        req.effort = Some(Effort::XHigh);
        let body = body_for(&req, false);
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["reasoning"]["summary"], "auto");
        // Set + thinking off → the off-switch wins; no summary.
        req.thinking = false;
        let body = body_for(&req, false);
        assert_eq!(body["reasoning"]["effort"], "none");
        assert!(body["reasoning"].get("summary").is_none());
    }

    /// Reasoning items replay verbatim, in original order, each staying
    /// before the function_call it justified.
    #[test]
    fn reasoning_items_replay_verbatim_before_their_function_call() {
        let reasoning = json!({
            "type": "reasoning",
            "id": "rs_1",
            "encrypted_content": "gAAA==",
            "summary": [{"type": "summary_text", "text": "thought"}],
        });
        let mut req = sampling_req();
        req.items = hotl_provider::arc_items(vec![Item::Assistant {
            blocks: vec![
                reasoning.clone(),
                json!({"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a.rs"}}),
                json!({"type": "text", "text": "done"}),
            ],
        }]);
        let body = body_for(&req, false);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0], reasoning, "replayed byte-for-byte");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["name"], "read");
        assert_eq!(input[1]["arguments"], "{\"path\":\"a.rs\"}");
        assert_eq!(input[2]["type"], "message");
        assert_eq!(input[2]["role"], "assistant");
        assert_eq!(input[2]["content"][0]["type"], "output_text");
        assert_eq!(input[2]["content"][0]["text"], "done");
    }

    /// Foreign signed thinking never crosses providers — this dialect drops
    /// it itself (it cannot use `strip_foreign_reasoning`, which would strip
    /// its own `reasoning` blocks).
    #[test]
    fn foreign_thinking_blocks_are_dropped() {
        let mut req = sampling_req();
        req.items = hotl_provider::arc_items(vec![Item::Assistant {
            blocks: vec![
                json!({"type": "thinking", "thinking": "secret chain", "signature": "sig=="}),
                json!({"type": "redacted_thinking", "data": "d"}),
                json!({"type": "text", "text": "ok"}),
            ],
        }]);
        let body = body_for(&req, false);
        assert!(!body.to_string().contains("secret chain"));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
    }

    /// INVARIANT (T3-13 twin): a failed tool is legible as failed. The
    /// Responses wire has no error field on `function_call_output`, so the
    /// marker is in-band — but it must be there.
    #[test]
    fn tool_result_errors_are_legible_in_the_responses_dialect() {
        let mut req = sampling_req();
        req.items = hotl_provider::arc_items(vec![Item::ToolResults {
            results: vec![
                ToolResultItem {
                    tool_use_id: "ok".into(),
                    content: "all good".into(),
                    is_error: false,
                },
                ToolResultItem {
                    tool_use_id: "bad".into(),
                    content: "No such file: a.rs. Check the path with `glob`.".into(),
                    is_error: true,
                },
            ],
        }]);
        let body = body_for(&req, false);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "ok");
        assert_eq!(
            input[0]["output"], "all good",
            "success must not be decorated"
        );
        let failed = input[1]["output"].as_str().unwrap();
        assert!(failed.starts_with(TOOL_ERROR_PREFIX), "{failed}");
        assert!(
            failed.contains("Check the path"),
            "the prompt content must survive: {failed}"
        );
    }

    /// INVARIANT: an imageless user item keeps the plain-string content form,
    /// byte-identical to the pre-image wire.
    #[test]
    fn an_imageless_user_item_stays_a_plain_string() {
        let body = body_for(&sampling_req(), false);
        assert_eq!(body["input"][0]["content"], "hi");
    }

    /// An image-bearing user item renders as the parts form: data-URL image
    /// parts first, the text part last (the provider-neutral ordering).
    /// The model is uncatalogued → the gate fails open.
    #[test]
    fn image_parts_render_as_data_urls_before_the_text_part() {
        let mut req = sampling_req();
        req.items = hotl_provider::arc_items(vec![Item::User {
            text: "what is this?".into(),
            synthetic: None,
            images: vec![hotl_types::UserImage {
                media_type: "image/png".into(),
                data: "aW1n".into(),
            }],
        }]);
        let body = body_for(&req, false);
        let content = body["input"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "input_image");
        assert_eq!(content[0]["image_url"], "data:image/png;base64,aW1n");
        assert_eq!(content[1]["type"], "input_text");
        assert_eq!(content[1]["text"], "what is this?");
    }

    /// The gated path: images dropped, one deterministic omission note inside
    /// the plain-string content. Threaded directly through `convert_item`
    /// because no catalog row combines this dialect with images: false.
    #[test]
    fn a_gated_model_gets_the_plain_string_with_an_omission_note() {
        let mut out = Vec::new();
        convert_item(
            &Item::User {
                text: "see [Image #1]".into(),
                synthetic: None,
                images: vec![hotl_types::UserImage {
                    media_type: "image/png".into(),
                    data: "aW1n".into(),
                }],
            },
            &mut out,
            false,
            false,
        );
        let content = out[0]["content"].as_str().unwrap();
        assert!(content.starts_with("see [Image #1]\n\n[note: 1 attached image(s)"));
    }

    /// The full ladder is the identity — gpt-5.6's rungs are 1:1 with hotl's.
    #[test]
    fn the_ladder_passes_every_rung_through_unclamped() {
        for &e in ALL_EFFORTS {
            assert_eq!(ResponsesLadder.resolve("gpt-5.6-luna-1", e), Some(e));
        }
    }

    // ───────────────────────── assembler ─────────────────────────

    /// Feed every event, then finish. Panics on any handle error.
    fn run(events: &[&str]) -> (Vec<StreamEvent>, StreamEvent) {
        let mut a = ResponsesAssembler::default();
        let mut streamed = Vec::new();
        for e in events {
            streamed.extend(a.handle(e).unwrap());
        }
        (streamed, a.finish().unwrap())
    }

    /// OpenAI's `input_tokens` includes `cached_tokens`; `TokenUsage` counts
    /// cached tokens outside `input_tokens` (Anthropic semantics), so the
    /// assembler carves them out rather than every consumer learning a case.
    #[test]
    fn cached_tokens_are_carved_out_of_openais_input_total() {
        let (_, done) = run(&[
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":42,"output_tokens":8,"input_tokens_details":{"cached_tokens":30}}}}"#,
        ]);
        let StreamEvent::Completed { usage, .. } = done else {
            panic!("wrong terminal event")
        };
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.cache_read_input_tokens, 30);
        assert_eq!(usage.hit_ratio(), Some(30.0 / 42.0));
    }

    /// D5: `cache_write_tokens` is a cache write, not input — the guide's own
    /// 2600/2000/400 example lands as 200 input, 2000 read, 400 written, and
    /// the writes count as non-hits.
    #[test]
    fn cache_write_tokens_land_in_cache_creation_and_leave_input() {
        let (_, done) = run(&[
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":2600,"output_tokens":8,"input_tokens_details":{"cached_tokens":2000,"cache_write_tokens":400}}}}"#,
        ]);
        let StreamEvent::Completed { usage, .. } = done else {
            panic!("wrong terminal event")
        };
        assert_eq!(usage.input_tokens, 200);
        assert_eq!(usage.cache_read_input_tokens, 2000);
        assert_eq!(usage.cache_creation_input_tokens, 400);
        assert_eq!(usage.hit_ratio(), Some(2000.0 / 2600.0));
    }

    /// R3's first golden test, resurrected on the current wire (per-item
    /// terminators, `output_item.added` for every item): a different event
    /// vocabulary folds into the *same* verbatim assistant blocks and
    /// `Completed` terminal event, with no engine change.
    #[test]
    fn responses_dialect_folds_into_the_same_blocks() {
        let events = [
            r#"{"type":"response.created","response":{}}"#,
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","role":"assistant"}}"#,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"I'll read "}"#,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"the file."}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","call_id":"call_9","name":"read"}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":"}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"\"a.rs\"}"}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call_9","name":"read","arguments":"{\"path\":\"a.rs\"}"}}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":42,"output_tokens":8,"input_tokens_details":{"cached_tokens":30}}}}"#,
        ];
        let (streamed, done) = run(&events);
        let StreamEvent::Completed {
            stop,
            usage,
            blocks,
        } = done
        else {
            panic!("wrong terminal event")
        };
        // Same contract as every other dialect: verbatim blocks + usage + stop.
        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 8);
        assert_eq!(usage.cache_read_input_tokens, 30);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "I'll read the file.");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["id"], "call_9");
        assert_eq!(blocks[1]["name"], "read");
        assert_eq!(blocks[1]["input"]["path"], "a.rs");
        // Canonical tool_use blocks stay exactly {type,id,name,input} —
        // Anthropic validates echoed blocks byte-for-byte.
        assert_eq!(blocks[1].as_object().unwrap().len(), 4);
        // Deltas surfaced for live display, just like the other dialects.
        assert!(streamed
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { .. })));
        assert!(streamed
            .iter()
            .any(|e| matches!(e, StreamEvent::ToolInputDelta { .. })));
    }

    /// R3's second golden test, resurrected: truncation is an honest error.
    #[test]
    fn truncated_stream_is_an_honest_error() {
        let mut a = ResponsesAssembler::default();
        a.handle(
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
        )
        .unwrap();
        a.handle(r#"{"type":"response.output_text.delta","output_index":0,"delta":"hi"}"#)
            .unwrap();
        assert!(
            a.finish().is_err(),
            "no response.completed → error, not a silent empty result"
        );
    }

    /// INVARIANT (T3-14): every opened block closes — including via the
    /// terminal sweep when a sloppy gateway never sent `output_item.done`.
    /// The swept function_call is backfilled from `response.completed`'s own
    /// copy of the output.
    #[test]
    fn every_block_start_has_a_matching_block_end_including_the_sweep() {
        let events = [
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"hi"}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call"}}"#,
            r#"{"type":"response.completed","response":{"output":[{"type":"message"},{"type":"function_call","call_id":"call_1","name":"glob","arguments":"{\"pat\":\"*.rs\"}"}],"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        ];
        let (streamed, done) = run(&events);
        let starts: Vec<usize> = streamed
            .iter()
            .filter_map(|e| match e {
                StreamEvent::BlockStart { index, .. } => Some(*index),
                _ => None,
            })
            .collect();
        let ends: Vec<usize> = streamed
            .iter()
            .filter_map(|e| match e {
                StreamEvent::BlockEnd { index } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec![0, 1]);
        assert_eq!(ends, vec![0, 1], "every opened block must be closed");
        for i in &starts {
            let s = streamed
                .iter()
                .position(|e| matches!(e, StreamEvent::BlockStart { index, .. } if index == i))
                .unwrap();
            let e = streamed
                .iter()
                .position(|e| matches!(e, StreamEvent::BlockEnd { index } if index == i))
                .unwrap();
            assert!(s < e, "BlockEnd {i} preceded its BlockStart");
        }
        let StreamEvent::Completed { blocks, .. } = done else {
            panic!("wrong terminal event")
        };
        // The sweep's backfill sealed the never-terminated function_call.
        assert_eq!(blocks[1]["id"], "call_1");
        assert_eq!(blocks[1]["name"], "glob");
        assert_eq!(blocks[1]["input"]["pat"], "*.rs");
    }

    /// The reasoning item rides back verbatim (encrypted content and all)
    /// minus the response-lifecycle `status` field, and its summary streams
    /// live as `ThinkingDelta` — display parity with the Anthropic dialect.
    #[test]
    fn a_reasoning_item_is_captured_verbatim_with_status_stripped() {
        let events = [
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"weighing "}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"options"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"gAAA==","summary":[{"type":"summary_text","text":"weighing options"}],"status":"completed"}}"#,
            r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}"#,
        ];
        let (streamed, done) = run(&events);
        assert!(
            streamed
                .iter()
                .any(|e| matches!(e, StreamEvent::BlockStart { kind, .. } if kind == "thinking")),
            "reasoning opens as the thinking kind the TUI already handles"
        );
        let thinking: String = streamed
            .iter()
            .filter_map(|e| match e {
                StreamEvent::ThinkingDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thinking, "weighing options");
        let StreamEvent::Completed { blocks, .. } = done else {
            panic!("wrong terminal event")
        };
        assert_eq!(blocks[0]["type"], "reasoning");
        assert_eq!(blocks[0]["encrypted_content"], "gAAA==");
        assert_eq!(blocks[0]["summary"][0]["text"], "weighing options");
        assert!(
            blocks[0].get("status").is_none(),
            "status is lifecycle state, not part of the replayable item"
        );
    }

    /// `output_item.done`'s complete `arguments` string is authoritative: it
    /// replaces accumulated deltas rather than appending to them.
    #[test]
    fn complete_arguments_on_done_beat_accumulated_deltas() {
        let events = [
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"c","name":"read"}}"#,
            r#"{"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"pa"}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"c","name":"read","arguments":"{\"path\":\"a.rs\"}"}}"#,
            r#"{"type":"response.completed","response":{}}"#,
        ];
        let (_, done) = run(&events);
        let StreamEvent::Completed { blocks, .. } = done else {
            panic!("wrong terminal event")
        };
        assert_eq!(blocks[0]["input"]["path"], "a.rs");
    }

    /// A call with no arguments at all ships `input: {}`, not a parse error.
    #[test]
    fn empty_arguments_become_an_empty_object_input() {
        let events = [
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"c","name":"list"}}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","call_id":"c","name":"list","arguments":""}}"#,
            r#"{"type":"response.completed","response":{}}"#,
        ];
        let (_, done) = run(&events);
        let StreamEvent::Completed { blocks, .. } = done else {
            panic!("wrong terminal event")
        };
        assert_eq!(blocks[0]["input"], json!({}));
    }

    /// `response.incomplete` closes partial blocks and maps its reason:
    /// `max_output_tokens` → MaxTokens, `content_filter` → Refusal.
    #[test]
    fn incomplete_maps_reasons_and_closes_partial_blocks() {
        let events = [
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
            r#"{"type":"response.output_text.delta","output_index":0,"delta":"par"}"#,
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":5,"output_tokens":16}}}"#,
        ];
        let (streamed, done) = run(&events);
        assert!(
            streamed
                .iter()
                .any(|e| matches!(e, StreamEvent::BlockEnd { index: 0 })),
            "T3-14 holds on the incomplete path too"
        );
        let StreamEvent::Completed {
            stop,
            usage,
            blocks,
        } = done
        else {
            panic!("wrong terminal event")
        };
        assert_eq!(stop, StopReason::MaxTokens);
        assert_eq!(usage.output_tokens, 16);
        assert_eq!(blocks[0]["text"], "par", "partial text survives");

        let events = [
            r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}"#,
        ];
        let (_, done) = run(&events);
        let StreamEvent::Completed { stop, .. } = done else {
            panic!("wrong terminal event")
        };
        assert_eq!(stop, StopReason::Refusal);
    }

    /// Refusal deltas reach the human live as text (blocks stay canonical),
    /// and the terminal stop is Refusal.
    #[test]
    fn refusal_deltas_stream_as_text_and_stop_is_refusal() {
        let events = [
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message"}}"#,
            r#"{"type":"response.refusal.delta","output_index":0,"delta":"I can't help with that."}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}"#,
            r#"{"type":"response.completed","response":{}}"#,
        ];
        let (streamed, done) = run(&events);
        assert!(streamed.iter().any(
            |e| matches!(e, StreamEvent::TextDelta { text, .. } if text == "I can't help with that.")
        ));
        let StreamEvent::Completed { stop, blocks, .. } = done else {
            panic!("wrong terminal event")
        };
        assert_eq!(stop, StopReason::Refusal);
        assert_eq!(blocks[0]["text"], "I can't help with that.");
    }

    /// In-stream failures map to the statuses their codes would have carried
    /// pre-stream, so `retry::is_availability` and the fallback-model chain
    /// stay reachable — the Anthropic error-arm twin.
    #[test]
    fn failed_and_error_events_carry_availability_statuses() {
        let cases = [
            (
                r#"{"type":"error","code":"rate_limit_exceeded","message":"slow down"}"#,
                429,
            ),
            (
                r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"boom"}}}"#,
                500,
            ),
            (
                r#"{"type":"response.failed","response":{"error":{"code":"invalid_prompt","message":"no"}}}"#,
                400,
            ),
        ];
        for (data, want) in cases {
            let mut a = ResponsesAssembler::default();
            let Err(err) = a.handle(data) else {
                panic!("expected an error for {data}");
            };
            let ProviderError::Http { status, .. } = &err else {
                panic!("expected Http, got {err:?}");
            };
            assert_eq!(*status, want, "{data}");
            if want != 400 {
                assert!(hotl_provider::retry::is_availability(&err), "{data}");
            }
        }
    }

    /// INVARIANT (T2-9 twin): a wire `output_index` above `MAX_BLOCK_INDEX`
    /// is a parse error, never an allocation.
    #[test]
    fn an_absurd_output_index_is_a_parse_error_not_an_allocation() {
        let mut a = ResponsesAssembler::default();
        let data = r#"{"type":"response.output_item.added","output_index":4000000000,"item":{"type":"message"}}"#;
        match a.handle(data) {
            Err(ProviderError::Parse(m)) => assert!(m.contains("output_index"), "{m}"),
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    /// INVARIANT (T2-11 twin): a delta for a slot that never opened is a
    /// parse error, never a silent drop.
    #[test]
    fn a_delta_before_its_output_item_added_is_a_parse_error() {
        let mut a = ResponsesAssembler::default();
        let data = r#"{"type":"response.output_text.delta","output_index":0,"delta":"orphan"}"#;
        match a.handle(data) {
            Err(ProviderError::Parse(m)) => assert!(m.contains("output_index 0"), "{m}"),
            other => panic!("expected a parse error, got {other:?}"),
        }
    }

    /// A reasoning slot still empty after the sweep is a parse error: an
    /// un-replayable reasoning item would poison the next request.
    #[test]
    fn a_reasoning_item_never_delivered_is_a_parse_error() {
        let mut a = ResponsesAssembler::default();
        a.handle(r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#)
            .unwrap();
        // completed carries no /response/output to backfill from.
        a.handle(r#"{"type":"response.completed","response":{}}"#)
            .unwrap();
        assert!(matches!(a.finish(), Err(ProviderError::Parse(_))));
    }

    /// Unknown item types open no block, stream nothing, and contribute no
    /// final block — forward compat with tool types hotl does not speak.
    #[test]
    fn unknown_item_types_open_no_block() {
        let events = [
            r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_1"}}"#,
            r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"web_search_call","id":"ws_1"}}"#,
            r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message"}}"#,
            r#"{"type":"response.output_text.delta","output_index":1,"delta":"hi"}"#,
            r#"{"type":"response.output_item.done","output_index":1,"item":{"type":"message"}}"#,
            r#"{"type":"response.completed","response":{}}"#,
        ];
        let (streamed, done) = run(&events);
        let starts: Vec<_> = streamed
            .iter()
            .filter(|e| matches!(e, StreamEvent::BlockStart { .. }))
            .collect();
        assert_eq!(starts.len(), 1, "only the message opened a block");
        assert!(
            matches!(starts[0], StreamEvent::BlockStart { index: 0, .. }),
            "the ignored item consumed no block index"
        );
        let StreamEvent::Completed { blocks, .. } = done else {
            panic!("wrong terminal event")
        };
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"], "hi");
    }

    // ───────────────────────── HTTP level ─────────────────────────

    use std::sync::Mutex as StdMutex;

    use futures_util::future::BoxFuture;
    use hotl_provider::key::{KeyError, KeySource};

    /// Key source yielding key-1, then key-2 after refresh.
    struct FlippingKey(StdMutex<u32>);
    impl KeySource for FlippingKey {
        fn get(&self) -> BoxFuture<'_, Result<Option<String>, KeyError>> {
            let n = *self.0.lock().unwrap();
            Box::pin(async move { Ok(Some(format!("key-{n}"))) })
        }
        fn refresh(&self) -> BoxFuture<'_, Result<(), KeyError>> {
            *self.0.lock().unwrap() += 1;
            Box::pin(async { Ok(()) })
        }
        fn refreshable(&self) -> bool {
            true
        }
    }

    const SSE_OK: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hi\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
    const AUTH_401: &str = "HTTP/1.1 401 Unauthorized\r\ncontent-type: text/plain\r\ncontent-length: 11\r\nconnection: close\r\n\r\nbad api key";

    /// Serve `responses` to consecutive connections; record each request's
    /// `authorization` header (lowercased) into `seen`.
    async fn tcp_double(responses: Vec<&'static str>, seen: Arc<StdMutex<Vec<String>>>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move {
            for resp in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let mut req = String::new();
                loop {
                    let n = sock.read(&mut buf).await.unwrap();
                    req.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if req.contains("\r\n\r\n") {
                        break;
                    }
                }
                let auth = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                    .map(|l| l.split_once(':').unwrap().1.trim().to_string())
                    .unwrap_or_default();
                seen.lock().unwrap().push(auth);
                sock.write_all(resp.as_bytes()).await.unwrap();
                sock.shutdown().await.ok();
            }
        });
        base
    }

    /// Sibling of [`tcp_double`] that records each **whole request** — head
    /// and body — rather than just the auth header: read to the header
    /// break, take `content-length`, then read exactly that many more bytes.
    /// (The chat-dialect twin records the body alone; this dialect's tests
    /// also assert on the request path, so the head is kept.)
    async fn tcp_double_requests(
        responses: Vec<&'static str>,
        seen: Arc<StdMutex<Vec<String>>>,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        tokio::spawn(async move {
            for resp in responses {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 65536];
                let mut req = Vec::new();
                let head_end = loop {
                    let n = sock.read(&mut buf).await.unwrap();
                    req.extend_from_slice(&buf[..n]);
                    if let Some(p) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&req[..head_end]).to_string();
                let len: usize = head
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                    .and_then(|l| l.split_once(':'))
                    .and_then(|(_, v)| v.trim().parse().ok())
                    .unwrap_or(0);
                while req.len() < head_end + len {
                    let n = sock.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&req).to_string());
                sock.write_all(resp.as_bytes()).await.unwrap();
                sock.shutdown().await.ok();
            }
        });
        base
    }

    /// The success path over HTTP: the event sequence a consumer actually
    /// sees, including the per-item `BlockEnd` (T3-14).
    #[tokio::test]
    async fn a_successful_stream_yields_the_full_event_sequence() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![SSE_OK], seen.clone()).await;
        let p = OpenAiResponsesProvider::new(base, Arc::new(hotl_provider::key::StaticKey(None)));
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        let oks: Vec<_> = events.into_iter().map(|e| e.expect("no error")).collect();
        assert!(matches!(oks[0], StreamEvent::Started));
        assert!(oks
            .iter()
            .any(|e| matches!(e, StreamEvent::BlockStart { index: 0, .. })));
        assert!(oks
            .iter()
            .any(|e| matches!(e, StreamEvent::TextDelta { text, .. } if text == "hi")));
        assert!(
            oks.iter()
                .any(|e| matches!(e, StreamEvent::BlockEnd { index: 0 })),
            "T3-14: every dialect closes its blocks"
        );
        let Some(StreamEvent::Completed {
            stop,
            usage,
            blocks,
        }) = oks.last()
        else {
            panic!("no terminal event: {oks:?}")
        };
        assert_eq!(*stop, StopReason::EndTurn);
        assert_eq!(usage.input_tokens, 1);
        assert_eq!(blocks[0]["text"], "hi");
    }

    /// The Responses twin of the retry test.
    #[tokio::test]
    async fn a_429_is_retried_over_a_real_socket() {
        const RETRY_429: &str = "HTTP/1.1 429 Too Many Requests\r\ncontent-type: text/plain\r\nretry-after: 0\r\ncontent-length: 5\r\nconnection: close\r\n\r\nslow!";
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![RETRY_429, SSE_OK], seen.clone()).await;
        let p = OpenAiResponsesProvider::new(base, Arc::new(hotl_provider::key::StaticKey(None)));
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()), "{events:?}");
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn auth_401_refreshes_key_once_and_retries() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![AUTH_401, SSE_OK], seen.clone()).await;
        let p = OpenAiResponsesProvider::new(base, Arc::new(FlippingKey(StdMutex::new(1))));
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(
            events.iter().all(|e| e.is_ok()),
            "no error expected: {events:?}"
        );
        assert_eq!(*seen.lock().unwrap(), vec!["Bearer key-1", "Bearer key-2"]);
    }

    #[tokio::test]
    async fn static_source_auth_401_surfaces_immediately() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![AUTH_401], seen.clone()).await;
        let p = OpenAiResponsesProvider::new(
            base,
            Arc::new(hotl_provider::key::StaticKey(Some("sk".into()))),
        );
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(matches!(events.last(), Some(Err(ProviderError::Auth(_)))));
        assert_eq!(seen.lock().unwrap().len(), 1); // exactly one request — no blind retry
    }

    /// The unterminated-final-line case at the HTTP layer: a server that
    /// closes the socket right after its last `data:` line still produces a
    /// complete response.
    #[tokio::test]
    async fn a_stream_ending_without_a_trailing_newline_still_completes() {
        const SSE_NO_TRAILING_NEWLINE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"hi\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}";
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double(vec![SSE_NO_TRAILING_NEWLINE], seen.clone()).await;
        let p = OpenAiResponsesProvider::new(base, Arc::new(hotl_provider::key::StaticKey(None)));
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(
            matches!(events.last(), Some(Ok(StreamEvent::Completed { .. }))),
            "the terminal event was dropped with the unterminated line: {events:?}"
        );
    }

    /// The wire says what the dialect promises: the request hits
    /// `…/responses` (not `…/chat/completions`) and the body carries
    /// `"store":false` on every request.
    #[tokio::test]
    async fn the_request_hits_the_responses_endpoint_with_store_false() {
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double_requests(vec![SSE_OK], seen.clone()).await;
        let p = OpenAiResponsesProvider::new(base, Arc::new(hotl_provider::key::StaticKey(None)));
        let events: Vec<_> = p.stream(sampling_req()).collect::<Vec<_>>().await;
        assert!(events.iter().all(|e| e.is_ok()), "{events:?}");
        let reqs = seen.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0].starts_with("POST /v1/responses "),
            "wrong path: {}",
            reqs[0].lines().next().unwrap_or_default()
        );
        assert!(reqs[0].contains("\"store\":false"), "{}", reqs[0]);
        assert!(!reqs[0].contains("chat/completions"));
    }

    /// D4: a 400 naming a prompt-cache field is a capability probe. The same
    /// sample is re-sent without the cache fields behind a `Retrying`
    /// notice, and the provider latches so the next `stream()` never probes.
    #[tokio::test]
    async fn a_rejected_breakpoint_probes_once_then_latches() {
        const REJECT_400: &str = "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: 97\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"Unknown parameter: 'prompt_cache_options'.\",\"type\":\"invalid_request_error\"}}";
        assert_eq!(REJECT_400.split("\r\n\r\n").nth(1).unwrap().len(), 97);
        let has_cache_fields =
            |r: &str| r.contains("prompt_cache_options") || r.contains("prompt_cache_breakpoint");
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let base = tcp_double_requests(vec![REJECT_400, SSE_OK, SSE_OK], seen.clone()).await;
        let p = OpenAiResponsesProvider::new(base, Arc::new(hotl_provider::key::StaticKey(None)));
        assert!(p.wants_explicit(&static_req()));

        let events: Vec<_> = p.stream(static_req()).collect::<Vec<_>>().await;
        let oks: Vec<_> = events.into_iter().map(|e| e.expect("no error")).collect();
        assert!(
            matches!(&oks[0], StreamEvent::Retrying { reason, .. }
                if reason.contains("prompt-cache breakpoints")),
            "{oks:?}"
        );
        assert!(matches!(oks[1], StreamEvent::Started));
        assert!(matches!(oks.last(), Some(StreamEvent::Completed { .. })));
        {
            let reqs = seen.lock().unwrap();
            assert_eq!(reqs.len(), 2);
            assert!(reqs[0].contains("\"prompt_cache_options\""), "{}", reqs[0]);
            assert!(
                reqs[0].contains("\"prompt_cache_breakpoint\""),
                "{}",
                reqs[0]
            );
            assert!(!has_cache_fields(&reqs[1]), "{}", reqs[1]);
            assert!(
                reqs[1].contains("\"prompt_cache_key\""),
                "the routing key is not a breakpoint field: {}",
                reqs[1]
            );
        }

        // Latched: the second call on the same provider never probes.
        assert!(!p.wants_explicit(&static_req()));
        let events: Vec<_> = p.stream(static_req()).collect::<Vec<_>>().await;
        assert!(matches!(events[0], Ok(StreamEvent::Started)), "{events:?}");
        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        assert!(!has_cache_fields(&reqs[2]), "{}", reqs[2]);
    }

    /// §S3.2: the HTTP/2 keep-alive knobs must ride the same builder as the
    /// existing timeouts.
    #[test]
    fn the_client_enables_http2_keep_alive() {
        let src = include_str!("lib.rs");
        assert!(src.contains("http2_keep_alive_interval"));
        assert!(src.contains("http2_keep_alive_while_idle"));
    }

    /// INVARIANT (T1-4): the HTTP client is never the default one.
    /// `Client::new()` has no connect timeout, so a stalled TLS handshake
    /// hangs the session forever and Ctrl-C cannot reach it.
    #[test]
    fn the_client_is_built_with_timeouts() {
        let src = include_str!("lib.rs");
        // Split so this test's own source is not a match for itself.
        assert!(
            !src.contains(concat!("reqwest::Client", "::new()")),
            "use http_client(); a default reqwest client has no timeout of any kind"
        );
        assert!(src.contains("connect_timeout"));
    }

    /// §S3.2 unit: dropping an armed guard resets "currently arming" state
    /// synchronously.
    #[tokio::test]
    async fn spawn_arm_drop_resets_the_armed_flag_immediately() {
        let armed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let guard = spawn_arm(
            http_client(std::time::Duration::from_secs(1)),
            "http://192.0.2.1/".into(),
            armed.clone(),
        );
        assert_ne!(
            armed.load(std::sync::atomic::Ordering::Acquire),
            0,
            "arm() must mark the provider as arming"
        );
        drop(guard);
        assert_eq!(
            armed.load(std::sync::atomic::Ordering::Acquire),
            0,
            "drop must reset armed state immediately — not wait for the background \
             task or its internal timeout"
        );
    }

    /// §S3.2 unit: a second `arm()` while the first is still armed is a
    /// no-op — dropping it must not disturb the still-live first guard's
    /// state.
    #[tokio::test]
    async fn spawn_arm_second_call_while_armed_is_a_noop() {
        let armed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let client = http_client(std::time::Duration::from_secs(1));
        let g1 = spawn_arm(client.clone(), "http://192.0.2.1/".into(), armed.clone());
        let before = armed.load(std::sync::atomic::Ordering::Acquire);
        let g2 = spawn_arm(client, "http://192.0.2.1/".into(), armed.clone());
        drop(g2);
        assert_eq!(
            armed.load(std::sync::atomic::Ordering::Acquire),
            before,
            "a no-op re-arm's drop must not reset state owned by the still-live first guard"
        );
        drop(g1);
        assert_eq!(armed.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    /// §S3.2 unit: arming against a refused-connection target must not
    /// panic and must not leak its background task.
    #[tokio::test]
    async fn spawn_arm_against_a_refused_target_does_not_leak_its_task() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let armed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let guard = spawn_arm(
            http_client(std::time::Duration::from_secs(1)),
            format!("http://{addr}/"),
            armed.clone(),
        );
        // Windows refuses a connection to a dropped-listener port slowly, so
        // bound this well above ARM_TIMEOUT rather than at 1s (as the sibling
        // black-holed test already does).
        tokio::time::timeout(ARM_TIMEOUT + std::time::Duration::from_secs(3), async {
            while armed.load(std::sync::atomic::Ordering::Acquire) != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a refused warm request must not leak its background task");
        drop(guard);
    }

    /// §S3.2 unit: a target that never answers (RFC 5737 TEST-NET-1) must
    /// still be bounded by the internal arm timeout.
    #[tokio::test]
    async fn spawn_arm_against_a_black_holed_target_is_bounded_by_the_arm_timeout() {
        let armed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let guard = spawn_arm(
            http_client(std::time::Duration::from_secs(1)),
            "http://192.0.2.1/".into(),
            armed.clone(),
        );
        tokio::time::timeout(ARM_TIMEOUT + std::time::Duration::from_secs(3), async {
            while armed.load(std::sync::atomic::Ordering::Acquire) != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("a black-holed target must not hang past the internal arm timeout");
        drop(guard);
    }

    /// §S3.2 unit: a guard whose own warm request already finished must not
    /// clobber a later, still-in-flight arm when dropped late. See the twin
    /// test in `hotl-provider-anthropic` for the full rationale.
    #[tokio::test]
    async fn a_late_dropped_stale_guard_does_not_clobber_a_newer_arm() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let armed = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let client = http_client(std::time::Duration::from_secs(1));
        let g1 = spawn_arm(client.clone(), format!("http://{addr}/"), armed.clone());
        // Windows refuses a connection to a dropped-listener port slowly, so
        // bound this well above ARM_TIMEOUT rather than at 1s (as the sibling
        // black-holed test already does).
        tokio::time::timeout(ARM_TIMEOUT + std::time::Duration::from_secs(3), async {
            while armed.load(std::sync::atomic::Ordering::Acquire) != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("g1's task must finish on its own before we proceed");
        let g2 = spawn_arm(client, "http://192.0.2.1/".into(), armed.clone());
        assert_ne!(
            armed.load(std::sync::atomic::Ordering::Acquire),
            0,
            "g2 must be in flight"
        );
        drop(g1);
        assert_ne!(
            armed.load(std::sync::atomic::Ordering::Acquire),
            0,
            "dropping the stale g1 must not clobber g2's still-in-flight state"
        );
        drop(g2);
        assert_eq!(armed.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    /// The public wiring: `Provider::arm` must reach the same `Warmable`
    /// impl this crate defines.
    #[tokio::test]
    async fn provider_arm_reaches_the_warmable_impl_without_panicking() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let p = OpenAiResponsesProvider::new(
            format!("http://{addr}"),
            Arc::new(hotl_provider::key::StaticKey(None)),
        );
        let guard = Provider::arm(&p);
        drop(guard);
    }
}
