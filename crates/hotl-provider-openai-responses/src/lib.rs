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

use hotl_provider::{Effort, EffortLadder, SamplingRequest, ToolDef, ALL_EFFORTS};
use hotl_types::Item;
use serde_json::{json, Value};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Marks a failed tool result in the Responses dialect.
///
/// Anthropic carries this structurally (`"is_error": true`); this wire has no
/// equivalent field on a `function_call_output` item, so the signal is
/// in-band or it is lost. Without it the model cannot tell a failure from a
/// success and the errors-are-prompts loop silently stops working (T3-13).
/// Twin of the constant in `hotl-provider-openai`. Do not remove as cosmetic.
const TOOL_ERROR_PREFIX: &str = "[tool_error] ";

/// The Responses serializer. Public so the provider (`stream()`) and the
/// body-render tests share one entry point; there is no legacy re-render
/// mid-loop here, so the body is built exactly once per request.
pub fn body_for(req: &SamplingRequest) -> Value {
    // Derived once per request from static catalog data. The OpenAI family
    // is uncatalogued, so this fails open (images always sent) — a
    // non-vision server answers with its own 400, surfaced honestly.
    let send_images = hotl_provider::catalog::supports_images(&req.model);
    let mut input = Vec::new();
    for item in req.items.iter() {
        convert_item(item, &mut input, send_images);
    }
    // The ephemeral suffix keeps its wire position in this dialect too:
    // after every durable item, before MOIM. Caching is implicit here, but
    // the ordering is what the engine's byte-identity claim is made over,
    // so it is not this crate's to reorder.
    for item in req.ephemeral_tail.iter() {
        convert_item(item, &mut input, send_images);
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
    // `cache` stays an Anthropic-surface knob: caching is implicit here and
    // this dialect emits no breakpoints under ANY `CachePolicy`.
    //
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

fn convert_item(item: &Item, out: &mut Vec<Value>, send_images: bool) {
    match item {
        // System rides `instructions`, never `input`.
        Item::System { .. } | Item::Unknown => {}
        // INVARIANT: an imageless user item stays `"content": <plain string>`
        // — byte-identical to the pre-image wire (the chat dialect's
        // invariant, kept here so gateways that reject array content for
        // plain text keep working). Image parts precede the text part,
        // matching every other dialect: one provider-neutral ordering rule.
        Item::User { text, images, .. } => {
            if send_images && !images.is_empty() {
                let mut parts: Vec<Value> = images
                    .iter()
                    .map(|img| {
                        json!({
                            "type": "input_image",
                            "image_url": format!("data:{};base64,{}", img.media_type, img.data)
                        })
                    })
                    .collect();
                parts.push(json!({"type": "input_text", "text": text}));
                out.push(json!({"role": "user", "content": parts}));
            } else {
                let rendered =
                    hotl_provider::transform::text_with_omitted_images(text, images.len());
                out.push(json!({"role": "user", "content": rendered.as_ref()}));
            }
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
            for r in results {
                // T3-13 twin: Responses has no structured error field on
                // `function_call_output` either, so the signal is in-band.
                let output = if r.is_error && !r.content.starts_with(TOOL_ERROR_PREFIX) {
                    format!("{TOOL_ERROR_PREFIX}{}", r.content)
                } else {
                    r.content.clone()
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

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_types::ToolResultItem;

    fn sampling_req() -> SamplingRequest {
        SamplingRequest {
            model: "gpt-5.6-luna-1".into(),
            max_tokens: 16,
            system: "".into(),
            items: std::sync::Arc::new(vec![Item::User {
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
        }
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
        let body = body_for(&req);
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
        req.ephemeral_tail = std::sync::Arc::new(vec![Item::User {
            text: "<todos>\n[~] a\n</todos>".into(),
            synthetic: Some(hotl_types::SyntheticReason::Todos),
            images: Vec::new(),
        }]);
        req.turn_context = Some("<turn-context/>".into());
        let body = body_for(&req);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["content"], "hi");
        assert_eq!(input[1]["content"], "<todos>\n[~] a\n</todos>");
        assert_eq!(input[2]["content"], "<turn-context/>");
        assert_eq!(input.len(), 3);
    }

    /// An empty system leaves `instructions` off the wire entirely.
    #[test]
    fn an_empty_system_emits_no_instructions_key() {
        let body = body_for(&sampling_req());
        assert!(body.get("instructions").is_none());
    }

    /// The byte-conservatism rule, all four corners (0029 tracker row 71):
    /// only effort-set requests speak `reasoning` at all.
    #[test]
    fn effort_gating_covers_all_four_corners() {
        // Unset → no key, whatever `thinking` says.
        let mut req = sampling_req();
        assert!(body_for(&req).get("reasoning").is_none());
        req.thinking = true;
        assert!(body_for(&req).get("reasoning").is_none());
        // Set + thinking → the resolved rung, with summaries on.
        req.effort = Some(Effort::XHigh);
        let body = body_for(&req);
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["reasoning"]["summary"], "auto");
        // Set + thinking off → the off-switch wins; no summary.
        req.thinking = false;
        let body = body_for(&req);
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
        req.items = std::sync::Arc::new(vec![Item::Assistant {
            blocks: vec![
                reasoning.clone(),
                json!({"type": "tool_use", "id": "call_1", "name": "read", "input": {"path": "a.rs"}}),
                json!({"type": "text", "text": "done"}),
            ],
        }]);
        let body = body_for(&req);
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
        req.items = std::sync::Arc::new(vec![Item::Assistant {
            blocks: vec![
                json!({"type": "thinking", "thinking": "secret chain", "signature": "sig=="}),
                json!({"type": "redacted_thinking", "data": "d"}),
                json!({"type": "text", "text": "ok"}),
            ],
        }]);
        let body = body_for(&req);
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
        req.items = std::sync::Arc::new(vec![Item::ToolResults {
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
        let body = body_for(&req);
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
        let body = body_for(&sampling_req());
        assert_eq!(body["input"][0]["content"], "hi");
    }

    /// An image-bearing user item renders as the parts form: data-URL image
    /// parts first, the text part last (the provider-neutral ordering).
    /// The model is uncatalogued → the gate fails open.
    #[test]
    fn image_parts_render_as_data_urls_before_the_text_part() {
        let mut req = sampling_req();
        req.items = std::sync::Arc::new(vec![Item::User {
            text: "what is this?".into(),
            synthetic: None,
            images: vec![hotl_types::UserImage {
                media_type: "image/png".into(),
                data: "aW1n".into(),
            }],
        }]);
        let body = body_for(&req);
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
}
