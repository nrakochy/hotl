use std::collections::HashMap;
use std::path::PathBuf;

use hotl_engine::{AskReply, EngineEvent, Outcome};
use hotl_types::{Item, SyntheticReason, TodoStatus, UserImage};
use serde_json::Value;

use crate::acp::{ClientCaps, SessionOpen, SessionOpenExtras, WorktreeInfo};
use crate::acp_types::{
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, CompactionStatus,
    CompactionUpdate, Content, ContentBlock, ContentChunk, CreateElicitationRequest,
    CreateElicitationResponse, Diff, ElicitationAction, ElicitationContentValue,
    ElicitationFormMode, ElicitationMode, ElicitationPropertySchema, ElicitationSchema,
    ElicitationSchemaType, EmbeddedResource, EmbeddedResourceResource, McpServer, Meta,
    MultiSelectItems, MultiSelectPropertySchema, Notice, NoticeSeverity, PermissionOption,
    PermissionOptionKind, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    RequestPermissionOutcome, RetryMeta, SessionConfigBoolean, SessionConfigKind,
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelect,
    SessionConfigSelectOption, SessionInfoUpdate, SessionMode, SessionModeState, SessionUpdate,
    StopReason, StringMultiSelectItems, StringPropertySchema, ToolCall, ToolCallContent,
    ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
    UnstructuredCommandInput,
};

pub const META_FRAME: &str = "hotl.dev/frame";
pub const META_DENIED: &str = "hotl.dev/denied";
pub const META_AUTO_ALLOWED: &str = "hotl.dev/autoAllowed";
pub const META_FLAGGED: &str = "hotl.dev/flagged";
pub const META_CHILD: &str = "hotl.dev/child";
pub const META_PROTECTED_WHY: &str = "hotl.dev/protectedWhy";
pub const META_ASK: &str = "hotl.dev/ask";
pub const META_EGRESS: &str = "hotl.dev/egress";
pub const META_RETRY: &str = "hotl.dev/retry";
pub const META_REPLY: &str = "hotl.dev/reply";
pub const META_QUESTION: &str = "hotl.dev/question";
pub const ELICITATION_ANSWER: &str = "answer";
pub const ELICITATION_OTHER: &str = "other";
pub const OPTION_ALLOW: &str = "allow";
pub const OPTION_DENY: &str = "deny";
pub const OPTION_SECRET_READS: &str = "allow-secret-reads";

const CHILD_TEXT_MAX: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Outgoing {
    Update(SessionUpdate),
    Ext(Value),
}

#[derive(Debug, Default)]
pub struct ToolCalls {
    root: PathBuf,
    open: HashMap<String, Value>,
    settled: std::collections::HashSet<String>,
    child_text: HashMap<String, String>,
    fold: Option<String>,
    folds: u64,
}

impl ToolCalls {
    pub fn rooted(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            ..Self::default()
        }
    }
}

impl ToolCalls {
    fn path(&self, input: &Value) -> Option<String> {
        let path = input.get("path").and_then(Value::as_str)?;
        Some(self.root.join(path).display().to_string())
    }

    fn locations(&self, name: &str, input: &Value) -> Option<Vec<ToolCallLocation>> {
        if !matches!(name, "read" | "edit" | "write" | "grep") {
            return None;
        }
        let line = input
            .get("offset")
            .and_then(Value::as_u64)
            .and_then(|l| u32::try_from(l).ok());
        Some(vec![ToolCallLocation {
            path: self.path(input)?,
            line,
        }])
    }

    fn diff(
        &self,
        name: &str,
        input: &Value,
        before: Option<&hotl_tools::Before>,
    ) -> Option<ToolCallContent> {
        let text = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_string);
        let (old_text, new_text) = match (name, before) {
            ("edit", _) => (text("old_string"), text("new_string")?),
            ("write", Some(hotl_tools::Before::Absent)) => (None, text("content")?),
            ("write", Some(hotl_tools::Before::Text(old))) => (Some(old.clone()), text("content")?),
            _ => return None,
        };
        Some(ToolCallContent::Diff(Diff {
            path: self.path(input)?,
            old_text,
            new_text,
        }))
    }

    fn touch(
        &mut self,
        id: &str,
        name: &str,
        fields: ToolCallUpdateFields,
        meta: Option<Meta>,
    ) -> SessionUpdate {
        let settled = matches!(
            fields.status,
            Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
        );
        let known = self.open.contains_key(id) || self.settled.contains(id);
        if settled {
            self.open.remove(id);
            self.child_text.remove(id);
            self.settled.insert(id.to_string());
        } else if !self.settled.contains(id) {
            let input = fields.raw_input.clone();
            let slot = self.open.entry(id.to_string()).or_insert(Value::Null);
            if let Some(input) = input {
                *slot = input;
            }
        }
        if known {
            return SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: wire_id(id),
                fields,
                meta,
            });
        }
        SessionUpdate::ToolCall(ToolCall {
            tool_call_id: wire_id(id),
            title: fields.title.unwrap_or_else(|| name.to_string()),
            kind: fields.kind.unwrap_or_else(|| tool_kind(name)),
            status: fields.status.unwrap_or(ToolCallStatus::Pending),
            content: fields.content.unwrap_or_default(),
            locations: fields.locations.unwrap_or_default(),
            raw_input: fields.raw_input,
            raw_output: fields.raw_output,
            meta,
        })
    }

    fn child_text(&mut self, parent: &str, text: &str) -> Option<SessionUpdate> {
        if !self.open.contains_key(parent) {
            return None;
        }
        let all = self.child_text.entry(parent.to_string()).or_default();
        all.push_str(text);
        if all.len() > CHILD_TEXT_MAX {
            let mut cut = all.len() - CHILD_TEXT_MAX;
            while !all.is_char_boundary(cut) {
                cut += 1;
            }
            all.drain(..cut);
        }
        Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate {
            tool_call_id: wire_id(parent),
            fields: ToolCallUpdateFields {
                content: Some(vec![text_content(all)]),
                ..ToolCallUpdateFields::default()
            },
            meta: None,
        }))
    }

    pub fn announced(&self, id: &str) -> bool {
        self.open.contains_key(id) || self.settled.contains(id)
    }

    pub fn end_turn(&mut self) {
        self.open.clear();
        self.settled.clear();
        self.child_text.clear();
        self.fold = None;
    }

    fn fold_id(&mut self) -> String {
        self.folds += 1;
        format!("fold-{}", self.folds)
    }
}

pub struct AskView<'a> {
    pub call_id: &'a str,
    pub tool: &'a str,
    pub input: &'a Value,
    pub summary: &'a str,
    pub protected_why: Option<&'a str>,
}

impl ToolCalls {
    fn proposed_diff(&self, tool: &str, input: &Value) -> Option<ToolCallContent> {
        match tool {
            "edit" => self.diff(tool, input, None),
            "write" => {
                let before = hotl_tools::before_write(&self.root, input);
                self.diff(tool, input, Some(&before))
            }
            _ => None,
        }
    }

    pub fn ask(&mut self, ask: &AskView, caps: &ClientCaps) -> ToolCallUpdate {
        let diff = self.proposed_diff(ask.tool, ask.input);
        let title = match (ask.protected_why, caps.ext) {
            (Some(why), false) => format!("{} — {why}", ask.summary),
            _ => ask.summary.to_string(),
        };
        let meta = caps.ext.then(|| {
            let rows: Vec<crate::acp_ext::DiffRow> = match &diff {
                Some(ToolCallContent::Diff(d)) => {
                    crate::diffgen::unified(d.old_text.as_deref().unwrap_or(""), &d.new_text)
                        .iter()
                        .map(crate::diffgen::DiffLine::to_row)
                        .collect()
                }
                _ => Vec::new(),
            };
            let mut m = Meta::new();
            if let Some(why) = ask.protected_why {
                m.insert(META_PROTECTED_WHY.into(), Value::String(why.to_string()));
            }
            m.insert(
                META_ASK.into(),
                serde_json::to_value(crate::acp_ext::AskDetail {
                    summary: ask.summary.to_string(),
                    protected_why: ask.protected_why.map(str::to_string),
                    diff: rows,
                })
                .unwrap_or_default(),
            );
            m
        });
        let locations = self.locations(ask.tool, ask.input);
        if !self.settled.contains(ask.call_id) {
            self.open.insert(ask.call_id.to_string(), ask.input.clone());
        }
        ToolCallUpdate {
            tool_call_id: wire_id(ask.call_id),
            fields: ToolCallUpdateFields {
                kind: Some(tool_kind(ask.tool)),
                status: Some(ToolCallStatus::Pending),
                title: Some(title),
                content: diff.map(|d| vec![d]),
                locations,
                raw_input: Some(ask.input.clone()),
                raw_output: None,
            },
            meta,
        }
    }
}

fn option(id: &str, name: &str, kind: PermissionOptionKind) -> PermissionOption {
    PermissionOption {
        option_id: id.to_string(),
        name: name.to_string(),
        kind,
        meta: None,
    }
}

pub fn ask_options(summary: &str) -> Vec<PermissionOption> {
    let mut options = vec![option(
        OPTION_ALLOW,
        "Allow",
        PermissionOptionKind::AllowOnce,
    )];
    if hotl_tools::sandbox::secret_read_grant_applies(summary) {
        options.push(option(
            OPTION_SECRET_READS,
            "Allow, including secret files",
            PermissionOptionKind::AllowOnce,
        ));
    }
    options.push(option(
        OPTION_DENY,
        "Deny",
        PermissionOptionKind::RejectOnce,
    ));
    options
}

pub fn ask_reply(
    outcome: &RequestPermissionOutcome,
    meta: Option<&Meta>,
    ext: bool,
    offered: &[PermissionOption],
) -> AskReply {
    let denied = AskReply::Deny { message: None };
    let RequestPermissionOutcome::Selected(selected) = outcome else {
        return denied;
    };
    let id = selected.option_id.as_str();
    if !offered.iter().any(|o| o.option_id == id) {
        return denied;
    }
    let reply = meta.filter(|_| ext).and_then(|m| m.get(META_REPLY));
    let field = |key: &str| reply.and_then(|r| r.get(key));
    match id {
        OPTION_DENY => AskReply::Deny {
            message: field("message").and_then(Value::as_str).map(String::from),
        },
        OPTION_ALLOW | OPTION_SECRET_READS => {
            if let Some(content) = field("respond").and_then(Value::as_str) {
                return AskReply::Respond {
                    content: content.to_string(),
                };
            }
            if let Some(input) = field("input") {
                return AskReply::AllowEdited {
                    input: input.clone(),
                };
            }
            if id == OPTION_SECRET_READS {
                AskReply::AllowWithSecretReads
            } else {
                AskReply::Allow
            }
        }
        _ => denied,
    }
}

pub fn announce(call: &ToolCallUpdate) -> SessionUpdate {
    let fields = call.fields.clone();
    SessionUpdate::ToolCall(ToolCall {
        tool_call_id: call.tool_call_id.clone(),
        title: fields.title.unwrap_or_default(),
        kind: fields.kind.unwrap_or(ToolKind::Other),
        status: ToolCallStatus::Pending,
        content: fields.content.unwrap_or_default(),
        locations: fields.locations.unwrap_or_default(),
        raw_input: fields.raw_input,
        raw_output: None,
        meta: None,
    })
}

pub fn settled(tool_call_id: String, done: bool) -> SessionUpdate {
    SessionUpdate::ToolCallUpdate(ToolCallUpdate {
        tool_call_id,
        fields: ToolCallUpdateFields {
            status: Some(if done {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            }),
            ..ToolCallUpdateFields::default()
        },
        meta: None,
    })
}

pub const RESERVED_CALL_PREFIXES: &[&str] = &["egress-", "loop-", "remember-", "tool-"];

pub fn wire_id(id: &str) -> String {
    if RESERVED_CALL_PREFIXES.iter().any(|p| id.starts_with(p)) {
        format!("tool-{id}")
    } else {
        id.to_string()
    }
}

pub fn egress_tool_call(req_id: u64, host: &str, caps: &ClientCaps) -> ToolCallUpdate {
    ToolCallUpdate {
        tool_call_id: format!("egress-{req_id}"),
        fields: ToolCallUpdateFields {
            kind: Some(ToolKind::Fetch),
            status: Some(ToolCallStatus::Pending),
            title: Some(format!("Network access to {host}")),
            raw_input: Some(serde_json::json!({"host": host})),
            ..ToolCallUpdateFields::default()
        },
        meta: caps.ext.then(|| meta_of(META_EGRESS, egress_meta(host))),
    }
}

fn retry_scope_name(scope: hotl_engine::RetryScope) -> &'static str {
    match scope {
        hotl_engine::RetryScope::Sample => "sample",
        hotl_engine::RetryScope::Summarize => "summarize",
        hotl_engine::RetryScope::GoalEval => "goal_eval",
        hotl_engine::RetryScope::Title => "title",
        hotl_engine::RetryScope::Aside => "aside",
    }
}

fn egress_meta(host: &str) -> Value {
    serde_json::to_value(crate::acp_ext::Egress {
        host: host.to_string(),
    })
    .unwrap_or_default()
}

pub fn question_form(
    session_id: &str,
    question: &hotl_types::Question,
    ext: bool,
) -> CreateElicitationRequest {
    let labels: Vec<String> = question.options.iter().map(|o| o.label.clone()).collect();
    let described: Vec<String> = question
        .options
        .iter()
        .filter_map(|o| o.description.as_ref().map(|d| format!("{}: {d}", o.label)))
        .collect();
    let description = (!described.is_empty()).then(|| described.join("\n"));
    let answer = if question.multi {
        ElicitationPropertySchema::Array(MultiSelectPropertySchema {
            description,
            items: MultiSelectItems::String(StringMultiSelectItems { values: labels }),
        })
    } else {
        ElicitationPropertySchema::String(StringPropertySchema {
            description,
            enum_values: Some(labels),
        })
    };
    let other = ElicitationPropertySchema::String(StringPropertySchema {
        description: Some("Your own answer, if none of the options fit".into()),
        enum_values: None,
    });
    let mut properties = std::collections::BTreeMap::new();
    properties.insert(ELICITATION_ANSWER.to_string(), answer);
    properties.insert(ELICITATION_OTHER.to_string(), other);
    CreateElicitationRequest {
        mode: ElicitationMode::Form(ElicitationFormMode {
            session_id: session_id.to_string(),
            requested_schema: ElicitationSchema {
                type_: ElicitationSchemaType::Object,
                properties,
            },
        }),
        message: format!("{}: {}", question.header, question.prompt),
        meta: ext.then(|| meta_of(META_QUESTION, serde_json::json!(question))),
    }
}

pub fn question_answer(
    response: &CreateElicitationResponse,
    offered: &[String],
    multi: bool,
) -> hotl_types::QuestionAnswer {
    use hotl_types::QuestionAnswer;
    let ElicitationAction::Accept(accepted) = &response.action else {
        return QuestionAnswer::NoHuman;
    };
    let Some(content) = &accepted.content else {
        return QuestionAnswer::NoHuman;
    };
    let chosen: Vec<String> = match content.get(ELICITATION_ANSWER) {
        Some(ElicitationContentValue::String(label)) => vec![label.clone()],
        Some(ElicitationContentValue::StringArray(labels)) => labels.clone(),
        _ => Vec::new(),
    };
    let mut chosen: Vec<String> = chosen
        .into_iter()
        .filter(|label| offered.contains(label))
        .collect();
    if !multi {
        chosen.truncate(1);
    }
    if !chosen.is_empty() {
        return QuestionAnswer::Selected(chosen);
    }
    match content.get(ELICITATION_OTHER) {
        Some(ElicitationContentValue::String(text)) if !text.trim().is_empty() => {
            QuestionAnswer::FreeText(text.clone())
        }
        _ => QuestionAnswer::NoHuman,
    }
}

pub fn egress_options() -> Vec<PermissionOption> {
    vec![
        option(OPTION_ALLOW, "Allow", PermissionOptionKind::AllowOnce),
        option(OPTION_DENY, "Deny", PermissionOptionKind::RejectOnce),
    ]
}

pub fn egress_decision(outcome: &RequestPermissionOutcome) -> hotl_tools::net::EgressDecision {
    use hotl_tools::net::EgressDecision;
    match outcome {
        RequestPermissionOutcome::Selected(s) if s.option_id == OPTION_ALLOW => {
            EgressDecision::Allow
        }
        RequestPermissionOutcome::Selected(s) if s.option_id == OPTION_DENY => EgressDecision::Deny,
        _ => EgressDecision::NoAnswer,
    }
}

fn text_content(text: &str) -> ToolCallContent {
    ToolCallContent::Content(Content {
        content: ContentBlock::text(text),
    })
}

fn meta_of(key: &str, value: Value) -> Meta {
    let mut m = Meta::new();
    m.insert(key.to_string(), value);
    m
}

fn notice(severity: NoticeSeverity, title: String, description: String) -> SessionUpdate {
    SessionUpdate::Notice(Notice {
        severity,
        title,
        description: Some(description),
        meta: None,
    })
}

fn add_meta(update: &mut SessionUpdate, key: &str, value: Value) {
    let slot = match update {
        SessionUpdate::UserMessageChunk(c)
        | SessionUpdate::AgentMessageChunk(c)
        | SessionUpdate::AgentThoughtChunk(c) => &mut c.meta,
        SessionUpdate::ToolCall(t) => &mut t.meta,
        SessionUpdate::ToolCallUpdate(t) => &mut t.meta,
        SessionUpdate::Plan(p) => &mut p.meta,
        SessionUpdate::SessionInfoUpdate(i) => &mut i.meta,
        SessionUpdate::ConfigOptionUpdate(c) => &mut c.meta,
        SessionUpdate::Notice(n) => &mut n.meta,
        SessionUpdate::CompactionUpdate(c) => &mut c.meta,
        SessionUpdate::AvailableCommandsUpdate(_)
        | SessionUpdate::UsageUpdate(_)
        | SessionUpdate::CurrentModeUpdate(_) => return,
    };
    slot.get_or_insert_with(Meta::new)
        .insert(key.to_string(), value);
}

pub fn outgoing(event: &EngineEvent, caps: &ClientCaps, calls: &mut ToolCalls) -> Option<Outgoing> {
    if matches!(event, EngineEvent::ModelChanged { .. }) {
        return None;
    }
    let frame = wire_frame(crate::wire::update_frame(event)?);
    match standard(event, caps, calls) {
        Some(mut update) => {
            if caps.ext {
                add_meta(&mut update, META_FRAME, frame);
            }
            Some(Outgoing::Update(update))
        }
        None => caps.ext.then_some(Outgoing::Ext(frame)),
    }
}

fn wire_frame(mut frame: Value) -> Value {
    let key = match frame.get("type").and_then(Value::as_str) {
        Some(
            "tool_start" | "tool_done" | "tool_queued" | "tool_progress" | "tool_denied"
            | "tool_auto_allowed" | "tool_flagged",
        ) => "id",
        Some("child_tool" | "child_text") => "parent_id",
        _ => return frame,
    };
    if let Some(slot) = frame.get_mut(key) {
        if let Some(id) = slot.as_str() {
            *slot = Value::String(wire_id(id));
        }
    }
    frame
}

fn standard(
    event: &EngineEvent,
    caps: &ClientCaps,
    calls: &mut ToolCalls,
) -> Option<SessionUpdate> {
    let ext_meta = |key: &str, value: Value| caps.ext.then(|| meta_of(key, value));
    Some(match event {
        EngineEvent::TextDelta(t) => SessionUpdate::AgentMessageChunk(chunk(t)),
        EngineEvent::ThinkingDelta(t) => SessionUpdate::AgentThoughtChunk(chunk(t)),
        EngineEvent::ToolQueued {
            id, name, summary, ..
        } => calls.touch(
            id,
            name,
            ToolCallUpdateFields {
                status: Some(ToolCallStatus::Pending),
                title: Some(summary.clone()),
                kind: Some(tool_kind(name)),
                ..ToolCallUpdateFields::default()
            },
            None,
        ),
        EngineEvent::ToolStart {
            id,
            name,
            summary,
            input,
        } => {
            let locations = calls.locations(name, input);
            calls.touch(
                id,
                name,
                ToolCallUpdateFields {
                    kind: Some(tool_kind(name)),
                    status: Some(ToolCallStatus::InProgress),
                    title: Some(summary.clone()),
                    locations,
                    raw_input: Some(input.clone()),
                    ..ToolCallUpdateFields::default()
                },
                None,
            )
        }
        EngineEvent::ToolProgress { id, name, tail, .. } => {
            if !calls.open.contains_key(id) {
                return None;
            }
            calls.touch(
                id,
                name,
                ToolCallUpdateFields {
                    content: Some(vec![text_content(tail)]),
                    ..ToolCallUpdateFields::default()
                },
                None,
            )
        }
        EngineEvent::ToolDone {
            id,
            name,
            ok,
            output,
            before,
            ..
        } => {
            let diff = match (ok, calls.open.get(id)) {
                (true, Some(input)) => calls.diff(name, input, before.as_ref()),
                _ => None,
            };
            calls.touch(
                id,
                name,
                ToolCallUpdateFields {
                    status: Some(if *ok {
                        ToolCallStatus::Completed
                    } else {
                        ToolCallStatus::Failed
                    }),
                    content: diff.map(|d| vec![d]),
                    raw_output: Some(Value::String(output.clone())),
                    ..ToolCallUpdateFields::default()
                },
                None,
            )
        }
        EngineEvent::ToolDenied { id, name } => calls.touch(
            id,
            name,
            ToolCallUpdateFields {
                status: Some(ToolCallStatus::Failed),
                ..ToolCallUpdateFields::default()
            },
            ext_meta(META_DENIED, Value::Bool(true)),
        ),
        EngineEvent::ToolAutoAllowed { id, name, rule } => {
            let meta = ext_meta(META_AUTO_ALLOWED, Value::String(rule.clone()))?;
            calls.touch(id, name, ToolCallUpdateFields::default(), Some(meta))
        }
        EngineEvent::ToolFlagged {
            id,
            name,
            summary,
            why,
            denied,
        } => {
            let meta = ext_meta(
                META_FLAGGED,
                serde_json::to_value(crate::acp_ext::Flagged {
                    why: why.clone(),
                    denied: *denied,
                })
                .ok()?,
            )?;
            calls.touch(
                id,
                name,
                ToolCallUpdateFields {
                    status: denied.then_some(ToolCallStatus::Failed),
                    title: Some(summary.clone()),
                    ..ToolCallUpdateFields::default()
                },
                Some(meta),
            )
        }
        EngineEvent::ChildText { parent_id, text } => calls.child_text(parent_id, text)?,
        EngineEvent::ChildTool { parent_id, .. } => {
            if !calls.open.contains_key(parent_id) {
                return None;
            }
            let meta = ext_meta(META_CHILD, wire_frame(crate::wire::update_frame(event)?))?;
            SessionUpdate::ToolCallUpdate(ToolCallUpdate {
                tool_call_id: wire_id(parent_id),
                fields: ToolCallUpdateFields::default(),
                meta: Some(meta),
            })
        }
        EngineEvent::TodosChanged { items } => SessionUpdate::Plan(plan_of(items)),
        EngineEvent::SessionTitled { title, .. } => {
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate {
                title: Some(title.clone()?),
                updated_at: None,
                meta: None,
            })
        }
        EngineEvent::Compacting { .. } if caps.compaction => {
            let id = calls.fold_id();
            calls.fold = Some(id.clone());
            SessionUpdate::CompactionUpdate(CompactionUpdate {
                compaction_id: id,
                status: CompactionStatus::InProgress,
                error: None,
                meta: None,
            })
        }
        EngineEvent::Compacted { .. } if caps.compaction => {
            let id = match calls.fold.take() {
                Some(id) => id,
                None => calls.fold_id(),
            };
            SessionUpdate::CompactionUpdate(CompactionUpdate {
                compaction_id: id,
                status: CompactionStatus::Completed,
                error: None,
                meta: None,
            })
        }
        EngineEvent::Retrying {
            attempt,
            max,
            reason,
            delay_ms,
            status,
            scope,
            ..
        } if caps.notices => {
            let mut update = notice(
                NoticeSeverity::Warning,
                format!("Retrying ({attempt}/{max})"),
                format!("{reason}; next try in {:.1}s", *delay_ms as f64 / 1000.0),
            );
            if caps.ext {
                let meta = RetryMeta {
                    attempt: *attempt,
                    max: *max,
                    status: *status,
                    delay_ms: *delay_ms,
                    scope: retry_scope_name(*scope).into(),
                };
                add_meta(
                    &mut update,
                    META_RETRY,
                    serde_json::to_value(meta).unwrap_or_default(),
                );
            }
            update
        }
        EngineEvent::FallbackModel { model } if caps.notices => notice(
            NoticeSeverity::Warning,
            format!("Falling back to {model} for the rest of this turn"),
            "The session's model was unavailable; the next turn starts on it again".into(),
        ),
        EngineEvent::BudgetNotice {
            pct,
            used_usd,
            cap_usd,
        } if caps.notices => notice(
            if *pct >= 100 {
                NoticeSeverity::Error
            } else {
                NoticeSeverity::Warning
            },
            format!("Spend is at {pct}% of the session budget"),
            format!("${used_usd:.2} of ${cap_usd:.2}"),
        ),
        EngineEvent::FoldSkipped { reason } if caps.notices => notice(
            NoticeSeverity::Info,
            "Nothing was folded".into(),
            reason.clone(),
        ),
        EngineEvent::AsideFailed { question, reason } if caps.notices => notice(
            NoticeSeverity::Warning,
            "The side question failed".into(),
            format!("{question}: {reason}"),
        ),
        _ => return None,
    })
}

pub fn prompt_from_blocks(blocks: &[ContentBlock]) -> Result<(String, Vec<UserImage>), String> {
    let mut lines: Vec<String> = Vec::new();
    let mut raw_images = Vec::new();
    for (i, block) in blocks.iter().enumerate() {
        match block {
            ContentBlock::Text(t) => lines.push(t.text.clone()),
            ContentBlock::Image(img) => {
                raw_images.push((format!("prompt[{i}]"), img.mime_type.as_str(), img.data.as_str()))
            }
            ContentBlock::ResourceLink(link) => lines.push(format!("@{}", reference(&link.uri))),
            ContentBlock::Resource(EmbeddedResource {
                resource: EmbeddedResourceResource::TextResourceContents(r),
            }) => lines.push(fenced(&r.uri, &r.text)),
            ContentBlock::Resource(EmbeddedResource {
                resource: EmbeddedResourceResource::BlobResourceContents(_),
            }) => {
                return Err(format!(
                    "prompt[{i}] is a binary resource, which hotl does not read; send a resource_link instead"
                ))
            }
            ContentBlock::Audio(_) => {
                return Err(format!("prompt[{i}] is audio, which hotl does not accept"))
            }
        }
    }
    let images = crate::images::check_images(&raw_images)?;
    let text = lines.join("\n");
    if text.trim().is_empty() && images.is_empty() {
        return Err("the prompt is empty; send at least one text, image or resource block".into());
    }
    Ok((text, images))
}

fn fenced(uri: &str, text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat(longest.max(2) + 1);
    let body = text.strip_suffix('\n').unwrap_or(text);
    format!("@{}\n{fence}\n{body}\n{fence}", reference(uri))
}

fn reference(uri: &str) -> String {
    let Some(rest) = uri.strip_prefix("file://") else {
        return uri.to_string();
    };
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    let decoded = percent_decode(path);
    let bytes = decoded.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        return decoded[1..].to_string();
    }
    decoded
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn stop_reason(outcome: &Outcome) -> Result<StopReason, Value> {
    match outcome {
        Outcome::Done { .. }
        | Outcome::DoomLoop { .. }
        | Outcome::ToolFailureBudget { .. }
        | Outcome::DenialSpiral { .. } => Ok(StopReason::EndTurn),
        Outcome::Cancelled => Ok(StopReason::Cancelled),
        Outcome::TurnLimit => Ok(StopReason::MaxTurnRequests),
        Outcome::Refused => Ok(StopReason::Refusal),
        Outcome::Budget { kind, .. } if kind == "tool_calls" => Ok(StopReason::MaxTurnRequests),
        Outcome::Budget { .. } => Ok(StopReason::EndTurn),
        Outcome::Error { .. } => Err(crate::wire::outcome_frame(outcome)),
    }
}

pub fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" => ToolKind::Read,
        "edit" | "write" => ToolKind::Edit,
        "bash" => ToolKind::Execute,
        "grep" | "glob" | "recall" => ToolKind::Search,
        "web_fetch" | "web_search" => ToolKind::Fetch,
        "todo_write" | "present_plan" => ToolKind::Think,
        _ => ToolKind::Other,
    }
}

struct OpenTool {
    id: String,
    name: String,
    input: Value,
}

fn tool_call(tool: OpenTool, status: ToolCallStatus, output: Option<&str>) -> SessionUpdate {
    SessionUpdate::ToolCall(ToolCall {
        tool_call_id: wire_id(&tool.id),
        kind: tool_kind(&tool.name),
        title: tool.name,
        status,
        content: output
            .map(|text| {
                vec![ToolCallContent::Content(Content {
                    content: ContentBlock::text(text),
                })]
            })
            .unwrap_or_default(),
        locations: Vec::new(),
        raw_input: Some(tool.input),
        raw_output: None,
        meta: None,
    })
}

fn chunk(text: &str) -> ContentChunk {
    ContentChunk {
        content: ContentBlock::text(text),
        meta: None,
    }
}

fn flush_unanswered(open: &mut Vec<OpenTool>, out: &mut Vec<Outgoing>) {
    for tool in open.drain(..) {
        out.push(Outgoing::Update(tool_call(
            tool,
            ToolCallStatus::Failed,
            None,
        )));
    }
}

pub fn replay_updates(items: &[Item], ext: bool) -> Vec<Outgoing> {
    let mut out = Vec::new();
    let mut open: Vec<OpenTool> = Vec::new();
    for item in items {
        match item {
            Item::User {
                text, synthetic, ..
            } => {
                flush_unanswered(&mut open, &mut out);
                if ext && *synthetic == Some(SyntheticReason::ShellCommand) {
                    if let Some(note) = hotl_engine::parse_shell_note_text(text) {
                        out.push(Outgoing::Ext(
                            serde_json::to_value(crate::acp::ReplayUpdate::ShellReplay {
                                command: note.command,
                                output: note.output,
                                exit_code: note.exit_code,
                                end: note.end,
                            })
                            .expect("a replay update always serializes"),
                        ));
                    }
                }
                if matches!(synthetic, None | Some(SyntheticReason::Steer)) && !text.is_empty() {
                    out.push(Outgoing::Update(SessionUpdate::UserMessageChunk(chunk(
                        text,
                    ))));
                }
            }
            Item::Assistant { blocks, .. } => {
                flush_unanswered(&mut open, &mut out);
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(Value::as_str) {
                                out.push(Outgoing::Update(SessionUpdate::AgentMessageChunk(
                                    chunk(t),
                                )));
                            }
                        }
                        Some("thinking") => {
                            if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                                out.push(Outgoing::Update(SessionUpdate::AgentThoughtChunk(
                                    chunk(t),
                                )));
                            }
                        }
                        Some("tool_use") => open.push(OpenTool {
                            id: block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            name: block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            input: block.get("input").cloned().unwrap_or(Value::Null),
                        }),
                        _ => {}
                    }
                }
            }
            Item::ToolResults { results } => {
                for tool in std::mem::take(&mut open) {
                    match results.iter().find(|r| r.tool_use_id == tool.id) {
                        Some(r) => {
                            let status = if r.is_error {
                                ToolCallStatus::Failed
                            } else {
                                ToolCallStatus::Completed
                            };
                            out.push(Outgoing::Update(tool_call(tool, status, Some(&r.content))));
                        }
                        None => out.push(Outgoing::Update(tool_call(
                            tool,
                            ToolCallStatus::Failed,
                            None,
                        ))),
                    }
                }
            }
            Item::System { .. } | Item::Unknown => {}
        }
    }
    flush_unanswered(&mut open, &mut out);
    out
}

pub fn open_extras(open: &SessionOpen, default_model: &str) -> SessionOpenExtras {
    SessionOpenExtras {
        name: open.name.clone(),
        plan: open.plan,
        default_effort: open.default_effort.clone(),
        goal: open.goal.clone(),
        images: true,
        todos: open.todos.clone(),
        context_tokens: None,
        effort: open.effort.clone(),
        previous_model: open.previous_model.clone(),
        effort_schedule: open.effort_schedule.clone(),
        warnings: open.warnings.clone(),
        title: open.title.clone(),
        model: open
            .model
            .clone()
            .unwrap_or_else(|| default_model.to_string()),
        memory_mode: open.memory_mode.clone(),
        worktree: open
            .worktree
            .clone()
            .map(|(path, branch)| WorktreeInfo { path, branch }),
        color: open.color.clone(),
    }
}

pub fn session_modes(current: &str) -> SessionModeState {
    let mode = |id: &str, name: &str, description: &str| SessionMode {
        id: id.into(),
        name: name.into(),
        description: Some(description.into()),
    };
    SessionModeState {
        current_mode_id: current.into(),
        available_modes: vec![
            mode("ask", "Ask", "Ask before tools that change things"),
            mode("dontask", "Don't ask", "Deny anything that would ask"),
            mode("bypass", "Bypass", "Run every tool without asking"),
        ],
    }
}

#[derive(Debug, Clone)]
pub struct ConfigState {
    pub model: String,
    pub plan: bool,
    pub effort: String,
    pub models: Vec<String>,
    pub ext: bool,
    pub context_window: u64,
}

pub fn effort_wire(open: &SessionOpen) -> String {
    match &open.effort {
        Some(Some(level)) => level.clone(),
        Some(None) => "default".into(),
        None => open
            .default_effort
            .clone()
            .unwrap_or_else(|| "default".into()),
    }
}

fn model_choices(state: &ConfigState) -> Vec<String> {
    let mut choices = vec![state.model.clone()];
    for model in &state.models {
        if !choices.contains(model) {
            choices.push(model.clone());
        }
    }
    choices
}

pub fn config_options(state: &ConfigState) -> Vec<SessionConfigOption> {
    let level = |value: &str, name: &str| SessionConfigSelectOption {
        value: value.into(),
        name: name.into(),
        description: None,
    };
    let mut efforts = vec![level("default", "Default")];
    efforts.extend(
        hotl_provider::ALL_EFFORTS
            .iter()
            .map(|e| level(e.as_str(), e.as_str())),
    );
    vec![
        SessionConfigOption {
            id: "model".into(),
            name: "Model".into(),
            description: Some("The model this session runs".into()),
            category: Some(SessionConfigOptionCategory::Model),
            kind: SessionConfigKind::Select(SessionConfigSelect {
                current_value: state.model.clone(),
                options: model_choices(state)
                    .into_iter()
                    .map(|m| level(&m, &m))
                    .collect(),
            }),
        },
        SessionConfigOption {
            id: "effort".into(),
            name: "Effort".into(),
            description: Some("How hard the model thinks".into()),
            category: Some(SessionConfigOptionCategory::ThoughtLevel),
            kind: SessionConfigKind::Select(SessionConfigSelect {
                current_value: state.effort.clone(),
                options: efforts,
            }),
        },
        SessionConfigOption {
            id: "plan".into(),
            name: "Plan mode".into(),
            description: Some("File edits ask first while plan mode is on".into()),
            category: None,
            kind: SessionConfigKind::Boolean(SessionConfigBoolean {
                current_value: state.plan,
            }),
        },
    ]
}

pub fn plan_of(todos: &[hotl_types::Todo]) -> Plan {
    Plan {
        entries: todos
            .iter()
            .map(|t| PlanEntry {
                content: t.content.clone(),
                priority: PlanEntryPriority::Medium,
                status: match t.status {
                    TodoStatus::InProgress => PlanEntryStatus::InProgress,
                    TodoStatus::Completed => PlanEntryStatus::Completed,
                    _ => PlanEntryStatus::Pending,
                },
            })
            .collect(),
        meta: None,
    }
}

pub fn available_commands<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> AvailableCommandsUpdate {
    AvailableCommandsUpdate {
        available_commands: entries
            .into_iter()
            .map(|(name, description)| AvailableCommand {
                name: name.into(),
                description: description.into(),
                input: Some(AvailableCommandInput::Unstructured(
                    UnstructuredCommandInput {
                        hint: "optional arguments".into(),
                    },
                )),
            })
            .collect(),
    }
}

pub fn mcp_from_acp(servers: &[McpServer]) -> Result<Vec<hotl_mcp::config::ServerConfig>, String> {
    servers
        .iter()
        .map(|server| match server {
            McpServer::Stdio(s) => {
                let command = s.command.display().to_string();
                if command.trim().is_empty() {
                    return Err(format!("mcpServers: `{}` has an empty command", s.name));
                }
                Ok(hotl_mcp::config::ServerConfig {
                    description: "passed in by the client".into(),
                    args: s.args.clone(),
                    env: s
                        .env
                        .iter()
                        .map(|e| (e.name.clone(), e.value.clone()))
                        .collect(),
                    ..hotl_mcp::config::ServerConfig::stdio(&s.name, &command, &[])
                })
            }
            McpServer::Http(s) => {
                hotl_tools::plugins::validate_remote_url(&s.url)
                    .map_err(|e| format!("mcpServers: `{}`: {e}", s.name))?;
                Ok(hotl_mcp::config::ServerConfig {
                    description: "passed in by the client".into(),
                    headers: s
                        .headers
                        .iter()
                        .map(|h| (h.name.clone(), h.value.clone()))
                        .collect(),
                    ..hotl_mcp::config::ServerConfig::http(&s.name, &s.url)
                })
            }
            McpServer::Sse(s) => Err(format!(
                "mcpServers: `{}` uses SSE, which hotl does not support; use http or stdio",
                s.name
            )),
        })
        .collect()
}

pub fn merge_mcp(
    configured: &[hotl_mcp::config::ServerConfig],
    client: Vec<hotl_mcp::config::ServerConfig>,
) -> (Vec<hotl_mcp::config::ServerConfig>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut merged: Vec<_> = configured
        .iter()
        .filter(|c| {
            let clash = client.iter().any(|s| s.name == c.name);
            if clash {
                warnings.push(format!(
                    "MCP server `{}` from the client replaces the configured server of the same name",
                    c.name
                ));
            }
            !clash
        })
        .cloned()
        .collect();
    merged.extend(client);
    (merged, warnings)
}

pub fn rfc3339(time: std::time::SystemTime) -> String {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = secs % 86_400;
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        hotl_context::civil_date_utc(secs * 1000),
        day / 3600,
        day % 3600 / 60,
        day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use hotl_types::ToolResultItem;
    use serde_json::json;

    fn user(text: &str) -> Item {
        Item::User {
            text: text.into(),
            synthetic: None,
            images: Vec::new(),
        }
    }

    fn assistant(blocks: Vec<Value>) -> Item {
        Item::Assistant {
            blocks,
            model: None,
        }
    }

    fn wire(updates: &[Outgoing]) -> Vec<Value> {
        updates
            .iter()
            .map(|u| match u {
                Outgoing::Update(u) => serde_json::to_value(u).unwrap(),
                Outgoing::Ext(v) => v.clone(),
            })
            .collect()
    }

    #[test]
    fn a_turn_replays_as_chunks_and_one_tool_call_per_tool() {
        let items = vec![
            Item::System { text: "sys".into() },
            Item::User {
                text: "<memory>".into(),
                synthetic: Some(SyntheticReason::Memory),
                images: Vec::new(),
            },
            user("list files"),
            assistant(vec![
                json!({"type": "thinking", "thinking": "use ls", "signature": "s"}),
                json!({"type": "text", "text": "Listing."}),
                json!({"type": "tool_use", "id": "t1", "name": "bash", "input": {"command": "ls"}}),
                json!({"type": "tool_use", "id": "t2", "name": "read", "input": {"path": "x"}}),
            ]),
            Item::ToolResults {
                results: vec![
                    ToolResultItem {
                        tool_use_id: "t1".into(),
                        content: "a b".into(),
                        is_error: false,
                    },
                    ToolResultItem {
                        tool_use_id: "t2".into(),
                        content: "no such file".into(),
                        is_error: true,
                    },
                ],
            },
            assistant(vec![json!({"type": "text", "text": "Done."})]),
        ];
        assert_eq!(
            wire(&replay_updates(&items, true)),
            vec![
                json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "list files"}}),
                json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "use ls"}}),
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Listing."}}),
                json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "bash", "kind": "execute",
                       "status": "completed", "rawInput": {"command": "ls"},
                       "content": [{"type": "content", "content": {"type": "text", "text": "a b"}}]}),
                json!({"sessionUpdate": "tool_call", "toolCallId": "t2", "title": "read", "kind": "read",
                       "status": "failed", "rawInput": {"path": "x"},
                       "content": [{"type": "content", "content": {"type": "text", "text": "no such file"}}]}),
                json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Done."}}),
            ]
        );
    }

    #[test]
    fn an_unanswered_tool_use_replays_as_failed() {
        let items = vec![
            user("go"),
            assistant(vec![
                json!({"type": "tool_use", "id": "t9", "name": "edit", "input": {}}),
            ]),
        ];
        let out = wire(&replay_updates(&items, true));
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["toolCallId"], "t9");
        assert_eq!(out[1]["status"], "failed");
        assert_eq!(out[1]["kind"], "edit");
        assert!(out[1].get("content").is_none());
    }

    #[test]
    fn a_steer_replays_as_the_user_but_other_injections_do_not() {
        let items = vec![
            Item::User {
                text: "also check tests".into(),
                synthetic: Some(SyntheticReason::Steer),
                images: Vec::new(),
            },
            Item::User {
                text: "<todos>".into(),
                synthetic: Some(SyntheticReason::Todos),
                images: Vec::new(),
            },
        ];
        let out = wire(&replay_updates(&items, true));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"]["text"], "also check tests");
    }

    fn shell_note(text: String) -> Item {
        Item::User {
            text,
            synthetic: Some(SyntheticReason::ShellCommand),
            images: Vec::new(),
        }
    }

    #[test]
    fn a_shell_note_replays_as_a_shell_card_for_an_extension_client() {
        let items = vec![
            user("look"),
            shell_note(hotl_engine::shell_note_text(
                "sleep 999",
                "partial\n[timed out after 120s]",
                None,
                Some("timed_out"),
            )),
            shell_note("<shell-command exit=\"0\">\nmangled".into()),
            shell_note(hotl_engine::shell_note_text("ls", "a b", Some(0), None)),
        ];
        let out = wire(&replay_updates(&items, true));
        assert_eq!(
            out,
            vec![
                json!({"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "look"}}),
                json!({"type": "shell_replay", "command": "sleep 999", "output": "partial\n[timed out after 120s]", "exit_code": null, "end": "timed_out"}),
                json!({"type": "shell_replay", "command": "ls", "output": "a b", "exit_code": 0}),
            ]
        );
        let plain = wire(&replay_updates(&items, false));
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0]["sessionUpdate"], "user_message_chunk");
    }

    #[test]
    fn modes_name_the_three_permission_modes() {
        let m = serde_json::to_value(session_modes("dontask")).unwrap();
        assert_eq!(m["currentModeId"], "dontask");
        let ids: Vec<(&str, &str)> = m["availableModes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| (x["id"].as_str().unwrap(), x["name"].as_str().unwrap()))
            .collect();
        assert_eq!(
            ids,
            [
                ("ask", "Ask"),
                ("dontask", "Don't ask"),
                ("bypass", "Bypass")
            ]
        );
    }

    #[test]
    fn plan_entries_map_todo_status() {
        let todos: Vec<hotl_types::Todo> = serde_json::from_value(json!([
            {"content": "a", "status": "in_progress"},
            {"content": "b", "status": "completed"},
            {"content": "c", "status": "failed"}
        ]))
        .unwrap();
        assert_eq!(
            serde_json::to_value(plan_of(&todos)).unwrap(),
            json!({"entries": [
                {"content": "a", "priority": "medium", "status": "in_progress"},
                {"content": "b", "priority": "medium", "status": "completed"},
                {"content": "c", "priority": "medium", "status": "pending"}
            ]})
        );
    }

    fn state() -> ConfigState {
        ConfigState {
            context_window: 200_000,
            model: "anthropic/m".into(),
            plan: false,
            effort: "default".into(),
            models: vec![
                "anthropic/b".into(),
                "anthropic/m".into(),
                "anthropic/c".into(),
            ],
            ext: false,
        }
    }

    #[test]
    fn effort_option_lists_the_ladder() {
        let options = config_options(&state());
        let effort = serde_json::to_value(&options[1]).unwrap();
        assert_eq!(effort["id"], "effort");
        assert_eq!(effort["category"], "thought_level");
        assert_eq!(effort["type"], "select");
        assert_eq!(effort["currentValue"], "default");
        let values: Vec<&str> = effort["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap())
            .collect();
        assert_eq!(values, ["default", "low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn plan_is_a_boolean_option() {
        let mut st = state();
        st.plan = true;
        let plan = serde_json::to_value(&config_options(&st)[2]).unwrap();
        assert_eq!(
            plan,
            json!({"id": "plan", "name": "Plan mode", "type": "boolean", "currentValue": true,
                   "description": "File edits ask first while plan mode is on"})
        );
    }

    #[test]
    fn the_model_option_names_the_running_model() {
        let model = serde_json::to_value(&config_options(&state())[0]).unwrap();
        assert_eq!(model["id"], "model");
        assert_eq!(model["category"], "model");
        assert_eq!(model["currentValue"], "anthropic/m");
        let values: Vec<&str> = model["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap())
            .collect();
        assert_eq!(values, ["anthropic/m", "anthropic/b", "anthropic/c"]);
    }

    #[test]
    fn the_two_option_updates_serialize_with_their_tags() {
        let mode = serde_json::to_value(SessionUpdate::CurrentModeUpdate(
            crate::acp_types::CurrentModeUpdate {
                current_mode_id: "ask".into(),
            },
        ))
        .unwrap();
        assert_eq!(
            mode,
            json!({"sessionUpdate": "current_mode_update", "currentModeId": "ask"})
        );
        let options = serde_json::to_value(SessionUpdate::ConfigOptionUpdate(
            crate::acp_types::ConfigOptionUpdate {
                config_options: config_options(&state()),
                meta: None,
            },
        ))
        .unwrap();
        assert_eq!(options["sessionUpdate"], "config_option_update");
        assert_eq!(options["configOptions"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn a_client_server_replaces_a_configured_one_of_the_same_name() {
        use hotl_mcp::config::ServerConfig;
        let configured = vec![
            ServerConfig::stdio("fs", "/bin/a", &[]),
            ServerConfig::stdio("git", "/bin/g", &[]),
        ];
        let client = vec![
            ServerConfig::stdio("fs", "/bin/b", &[]),
            ServerConfig::http("docs", "https://example.com/mcp"),
        ];
        let (merged, warnings) = merge_mcp(&configured, client);
        let names: Vec<_> = merged.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["git", "fs", "docs"]);
        assert_eq!(merged[1].command.as_deref(), Some("/bin/b"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("`fs`"), "{warnings:?}");
    }

    #[test]
    fn rfc3339_formats_utc_seconds() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(rfc3339(t), "2023-11-14T22:13:20Z");
    }

    fn blocks(v: Value) -> Vec<ContentBlock> {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn stop_reason_table() {
        use hotl_engine::Outcome;
        let ok = |o: Outcome| stop_reason(&o).unwrap();
        assert_eq!(ok(Outcome::Done { text: "x".into() }), StopReason::EndTurn);
        assert_eq!(ok(Outcome::Cancelled), StopReason::Cancelled);
        assert_eq!(ok(Outcome::TurnLimit), StopReason::MaxTurnRequests);
        assert_eq!(ok(Outcome::Refused), StopReason::Refusal);
        assert_eq!(
            ok(Outcome::Budget {
                kind: "tool_calls".into(),
                used: 5.0,
                cap: 5.0
            }),
            StopReason::MaxTurnRequests
        );
        assert_eq!(
            ok(Outcome::Budget {
                kind: "cost_usd".into(),
                used: 1.0,
                cap: 1.0
            }),
            StopReason::EndTurn
        );
        assert_eq!(
            ok(Outcome::DoomLoop {
                pattern: "p".into()
            }),
            StopReason::EndTurn
        );
        assert_eq!(
            ok(Outcome::ToolFailureBudget { tool: "t".into() }),
            StopReason::EndTurn
        );
        assert_eq!(
            ok(Outcome::DenialSpiral {
                consecutive: 3,
                total: 3
            }),
            StopReason::EndTurn
        );
        assert_eq!(
            stop_reason(&Outcome::Error {
                message: "boom".into()
            })
            .unwrap_err(),
            json!({"kind": "error", "message": "boom"})
        );
    }

    #[test]
    fn text_blocks_join_with_newlines() {
        let (text, images) = prompt_from_blocks(&blocks(json!([
            {"type": "text", "text": "one"},
            {"type": "text", "text": "two"}
        ])))
        .unwrap();
        assert_eq!(text, "one\ntwo");
        assert!(images.is_empty());
    }

    #[test]
    fn a_file_resource_link_becomes_a_decoded_path_reference() {
        let (text, _) = prompt_from_blocks(&blocks(json!([
            {"type": "text", "text": "look at"},
            {"type": "resource_link", "uri": "file:///tmp/my%20dir/a.rs", "name": "a.rs"},
            {"type": "resource_link", "uri": "https://example.com/x", "name": "x"}
        ])))
        .unwrap();
        assert_eq!(text, "look at\n@/tmp/my dir/a.rs\n@https://example.com/x");
    }

    #[test]
    fn an_embedded_text_resource_is_fenced_under_its_reference() {
        let (text, _) = prompt_from_blocks(&blocks(json!([
            {"type": "resource", "resource": {"uri": "file:///w/a.rs", "text": "fn a() {}\n```x```"}}
        ])))
        .unwrap();
        assert_eq!(text, "@/w/a.rs\n````\nfn a() {}\n```x```\n````");
    }

    #[test]
    fn image_blocks_pass_the_wire_checks() {
        let (_, images) = prompt_from_blocks(&blocks(json!([
            {"type": "image", "mimeType": "image/png", "data": "aW1nMQ=="}
        ])))
        .unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");
        let err = prompt_from_blocks(&blocks(json!([
            {"type": "image", "mimeType": "image/tiff", "data": "aW1nMQ=="}
        ])))
        .unwrap_err();
        assert!(err.contains("prompt[0]"), "{err}");
    }

    #[test]
    fn an_empty_prompt_is_refused() {
        assert!(prompt_from_blocks(&[]).is_err());
        let err = prompt_from_blocks(&blocks(json!([{"type": "text", "text": "  "}]))).unwrap_err();
        assert!(err.contains("empty"), "{err}");
    }

    #[test]
    fn audio_and_blob_resources_are_refused() {
        let audio = prompt_from_blocks(&blocks(json!([
            {"type": "audio", "mimeType": "audio/wav", "data": "aW1n"}
        ])))
        .unwrap_err();
        assert!(audio.contains("audio"), "{audio}");
        let blob = prompt_from_blocks(&blocks(json!([
            {"type": "resource", "resource": {"uri": "file:///a.bin", "blob": "aW1n"}}
        ])))
        .unwrap_err();
        assert!(blob.contains("binary"), "{blob}");
    }

    fn plain() -> ClientCaps {
        ClientCaps::default()
    }

    fn opted_in() -> ClientCaps {
        ClientCaps {
            ext: true,
            ..ClientCaps::default()
        }
    }

    fn root() -> PathBuf {
        std::env::temp_dir().join("acp-map-root")
    }

    fn at(rel: &str) -> String {
        root().join(rel).display().to_string()
    }

    fn update_json(out: Option<Outgoing>) -> Value {
        match out {
            Some(Outgoing::Update(u)) => serde_json::to_value(u).unwrap(),
            other => panic!("expected a standard update, got {other:?}"),
        }
    }

    fn start(id: &str, name: &str, input: Value) -> EngineEvent {
        EngineEvent::ToolStart {
            id: id.into(),
            name: name.into(),
            summary: format!("{name} call"),
            input,
        }
    }

    fn done(id: &str, name: &str, ok: bool, output: &str) -> EngineEvent {
        EngineEvent::ToolDone {
            id: id.into(),
            name: name.into(),
            ok,
            lines: 1,
            bytes: output.len() as u64,
            output: output.into(),
            before: None,
        }
    }

    fn wrote(id: &str, before: Option<hotl_tools::Before>) -> EngineEvent {
        EngineEvent::ToolDone {
            id: id.into(),
            name: "write".into(),
            ok: true,
            lines: 1,
            bytes: 5,
            output: "wrote".into(),
            before,
        }
    }

    #[test]
    fn text_delta_is_agent_message_chunk() {
        let mut calls = ToolCalls::rooted(root());
        let event = EngineEvent::TextDelta("hi".into());
        assert_eq!(
            update_json(outgoing(&event, &plain(), &mut calls)),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}})
        );
        assert_eq!(
            update_json(outgoing(
                &EngineEvent::ThinkingDelta("hm".into()),
                &plain(),
                &mut calls
            )),
            json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": "hm"}})
        );
        let ext = update_json(outgoing(&event, &opted_in(), &mut calls));
        assert_eq!(
            ext["_meta"][META_FRAME],
            json!({"type": "text_delta", "text": "hi"})
        );
    }

    #[test]
    fn queued_then_start_is_one_tool_call_then_an_update() {
        let mut calls = ToolCalls::rooted(root());
        let queued = EngineEvent::ToolQueued {
            id: "t1".into(),
            name: "read".into(),
            summary: "read src/a.rs".into(),
            ahead: 1,
        };
        assert_eq!(
            update_json(outgoing(&queued, &plain(), &mut calls)),
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "read src/a.rs",
                   "kind": "read", "status": "pending"})
        );
        let input = json!({"path": "src/a.rs", "offset": 40});
        assert_eq!(
            update_json(outgoing(
                &start("t1", "read", input.clone()),
                &plain(),
                &mut calls
            )),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "title": "read call",
                   "kind": "read", "status": "in_progress", "rawInput": input,
                   "locations": [{"path": at("src/a.rs"), "line": 40}]})
        );
        assert_eq!(
            update_json(outgoing(
                &done("t1", "read", true, "fn a() {}"),
                &plain(),
                &mut calls
            )),
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "t1", "status": "completed",
                   "rawOutput": "fn a() {}"})
        );
        let fresh = update_json(outgoing(
            &start("t2", "bash", json!({"command": "ls"})),
            &plain(),
            &mut calls,
        ));
        assert_eq!(fresh["sessionUpdate"], "tool_call");
        assert_eq!(fresh["status"], "in_progress");
        assert_eq!(fresh["kind"], "execute");
        assert!(fresh.get("locations").is_none(), "{fresh}");
    }

    #[test]
    fn a_settled_call_never_announced_is_a_whole_tool_call() {
        let mut calls = ToolCalls::rooted(root());
        let denied = update_json(outgoing(
            &EngineEvent::ToolDenied {
                id: "t9".into(),
                name: "write".into(),
            },
            &plain(),
            &mut calls,
        ));
        assert_eq!(
            denied,
            json!({"sessionUpdate": "tool_call", "toolCallId": "t9", "title": "write",
                   "kind": "edit", "status": "failed"})
        );
        let answered = update_json(outgoing(
            &done("t8", "bash", true, "by hand"),
            &plain(),
            &mut calls,
        ));
        assert_eq!(answered["sessionUpdate"], "tool_call");
        assert_eq!(answered["status"], "completed");
        assert_eq!(answered["rawOutput"], "by hand");
    }

    #[test]
    fn edit_done_carries_a_diff() {
        let mut calls = ToolCalls::rooted(root());
        let input = json!({"path": "a.rs", "old_string": "let x = 1;", "new_string": "let x = 2;"});
        outgoing(&start("e1", "edit", input), &plain(), &mut calls);
        let edit = update_json(outgoing(
            &done("e1", "edit", true, "edited"),
            &plain(),
            &mut calls,
        ));
        assert_eq!(
            edit["content"],
            json!([{"type": "diff", "path": at("a.rs"), "oldText": "let x = 1;", "newText": "let x = 2;"}])
        );
        let path = std::env::temp_dir()
            .join("acp-map-elsewhere")
            .join("new.rs")
            .display()
            .to_string();
        let write = |id: &str, before, calls: &mut ToolCalls| {
            outgoing(
                &start(
                    id,
                    "write",
                    json!({"path": path, "content": "fn main() {}\n"}),
                ),
                &plain(),
                calls,
            );
            update_json(outgoing(&wrote(id, before), &plain(), calls))
        };
        assert_eq!(
            write("w1", Some(hotl_tools::Before::Absent), &mut calls)["content"],
            json!([{"type": "diff", "path": path, "newText": "fn main() {}\n"}])
        );
        assert_eq!(
            write(
                "w2",
                Some(hotl_tools::Before::Text("fn old() {}\n".into())),
                &mut calls
            )["content"],
            json!([{"type": "diff", "path": path, "oldText": "fn old() {}\n", "newText": "fn main() {}\n"}])
        );
        let unshown = write("w3", Some(hotl_tools::Before::Unshown), &mut calls);
        assert!(unshown.get("content").is_none(), "{unshown}");
        let unknown = write("w4", None, &mut calls);
        assert!(unknown.get("content").is_none(), "{unknown}");
        outgoing(
            &start(
                "e2",
                "edit",
                json!({"path": "a.rs", "old_string": "a", "new_string": "b"}),
            ),
            &plain(),
            &mut calls,
        );
        let failed = update_json(outgoing(
            &done("e2", "edit", false, "no match"),
            &plain(),
            &mut calls,
        ));
        assert!(failed.get("content").is_none(), "{failed}");
        assert_eq!(failed["status"], "failed");
    }

    #[test]
    fn progress_and_child_text_land_on_the_call() {
        let mut calls = ToolCalls::rooted(root());
        outgoing(
            &start("b1", "bash", json!({"command": "make"})),
            &plain(),
            &mut calls,
        );
        let progress = update_json(outgoing(
            &EngineEvent::ToolProgress {
                id: "b1".into(),
                name: "bash".into(),
                tail: "Compiling".into(),
                lines: 3,
                bytes: 30,
            },
            &plain(),
            &mut calls,
        ));
        assert_eq!(
            progress,
            json!({"sessionUpdate": "tool_call_update", "toolCallId": "b1",
                   "content": [{"type": "content", "content": {"type": "text", "text": "Compiling"}}]})
        );
        outgoing(
            &start("s1", "spawn", json!({"prompt": "look"})),
            &plain(),
            &mut calls,
        );
        let text = |t: &str| EngineEvent::ChildText {
            parent_id: "s1".into(),
            text: t.into(),
        };
        outgoing(&text("one "), &plain(), &mut calls);
        let second = update_json(outgoing(&text("two"), &plain(), &mut calls));
        assert_eq!(second["toolCallId"], "s1");
        assert_eq!(second["content"][0]["content"]["text"], "one two");
        let child_tool = EngineEvent::ChildTool {
            parent_id: "s1".into(),
            id: "c1".into(),
            name: "read".into(),
            summary: "read x".into(),
            ok: None,
            tokens: None,
        };
        assert_eq!(outgoing(&child_tool, &plain(), &mut calls), None);
        let ext = update_json(outgoing(&child_tool, &opted_in(), &mut calls));
        assert_eq!(ext["toolCallId"], "s1");
        assert_eq!(ext["_meta"][META_CHILD]["id"], "c1");
    }

    #[test]
    fn a_fallback_is_a_turn_notice_not_a_model_change() {
        let fallback = EngineEvent::FallbackModel {
            model: "anthropic/backup".into(),
        };
        let notices = ClientCaps {
            notices: true,
            ..ClientCaps::default()
        };
        let mut calls = ToolCalls::rooted(root());
        let notice = update_json(outgoing(&fallback, &notices, &mut calls));
        assert_eq!(notice["sessionUpdate"], "notice");
        assert_eq!(notice["severity"], "warning");
        assert!(
            notice["title"]
                .as_str()
                .unwrap()
                .contains("anthropic/backup"),
            "{notice}"
        );
        assert_eq!(outgoing(&fallback, &plain(), &mut calls), None);
        assert_eq!(
            outgoing(&fallback, &opted_in(), &mut calls),
            Some(Outgoing::Ext(crate::wire::update_frame(&fallback).unwrap()))
        );
    }

    #[test]
    fn a_flagged_denial_is_one_tool_call_then_updates() {
        let mut calls = ToolCalls::rooted(root());
        let flagged = update_json(outgoing(
            &EngineEvent::ToolFlagged {
                id: "f1".into(),
                name: "write".into(),
                summary: "write /etc/x".into(),
                why: "outside".into(),
                denied: true,
            },
            &opted_in(),
            &mut calls,
        ));
        assert_eq!(flagged["sessionUpdate"], "tool_call");
        assert_eq!(flagged["status"], "failed");
        let denied = update_json(outgoing(
            &EngineEvent::ToolDenied {
                id: "f1".into(),
                name: "write".into(),
            },
            &opted_in(),
            &mut calls,
        ));
        assert_eq!(denied["sessionUpdate"], "tool_call_update", "{denied}");
        assert_eq!(denied["toolCallId"], "f1");
    }

    #[test]
    fn the_turn_end_forgets_calls_that_never_settled() {
        let mut calls = ToolCalls::rooted(root());
        outgoing(
            &start("b1", "bash", json!({"command": "sleep"})),
            &plain(),
            &mut calls,
        );
        outgoing(&done("b2", "bash", true, "x"), &plain(), &mut calls);
        calls.end_turn();
        let progress = EngineEvent::ToolProgress {
            id: "b1".into(),
            name: "bash".into(),
            tail: "late".into(),
            lines: 1,
            bytes: 4,
        };
        assert_eq!(outgoing(&progress, &plain(), &mut calls), None);
        assert!(calls.open.is_empty() && calls.settled.is_empty());
    }

    #[test]
    fn todos_become_a_plan() {
        let mut calls = ToolCalls::rooted(root());
        let items: Vec<hotl_types::Todo> = serde_json::from_value(json!([
            {"content": "a", "status": "in_progress"},
            {"content": "b", "status": "pending"}
        ]))
        .unwrap();
        assert_eq!(
            update_json(outgoing(
                &EngineEvent::TodosChanged { items },
                &plain(),
                &mut calls
            )),
            json!({"sessionUpdate": "plan", "entries": [
                {"content": "a", "priority": "medium", "status": "in_progress"},
                {"content": "b", "priority": "medium", "status": "pending"}
            ]})
        );
    }

    #[test]
    fn a_title_is_session_info() {
        let mut calls = ToolCalls::rooted(root());
        let titled = EngineEvent::SessionTitled {
            title: Some("Fix the parser".into()),
            usage: Default::default(),
        };
        assert_eq!(
            update_json(outgoing(&titled, &plain(), &mut calls)),
            json!({"sessionUpdate": "session_info_update", "title": "Fix the parser"})
        );
        let untitled = EngineEvent::SessionTitled {
            title: None,
            usage: Default::default(),
        };
        assert_eq!(outgoing(&untitled, &plain(), &mut calls), None);
    }

    fn verdict() -> EngineEvent {
        EngineEvent::GoalVerdict {
            verdict: hotl_engine::GoalVerdictKind::Met,
            reason: "tests pass".into(),
            turns: 2,
            usage: Default::default(),
            evidence: Vec::new(),
        }
    }

    #[test]
    fn goal_verdict_is_none_for_a_plain_client() {
        let mut calls = ToolCalls::rooted(root());
        assert_eq!(outgoing(&verdict(), &plain(), &mut calls), None);
    }

    #[test]
    fn goal_verdict_is_ext_for_an_opted_in_client() {
        let mut calls = ToolCalls::rooted(root());
        match outgoing(&verdict(), &opted_in(), &mut calls) {
            Some(Outgoing::Ext(frame)) => {
                assert_eq!(frame, crate::wire::update_frame(&verdict()).unwrap())
            }
            other => panic!("expected an extension update, got {other:?}"),
        }
    }

    #[test]
    fn retrying_is_a_notice_only_when_offered() {
        let retrying = EngineEvent::Retrying {
            attempt: 2,
            max: 5,
            reason: "overloaded".into(),
            delay_ms: 1_500,
            status: Some(529),
            request_id: None,
            scope: hotl_engine::RetryScope::Sample,
            discarded_partial: false,
        };
        let notices = ClientCaps {
            notices: true,
            ..ClientCaps::default()
        };
        let mut calls = ToolCalls::rooted(root());
        let notice = update_json(outgoing(&retrying, &notices, &mut calls));
        assert_eq!(notice["sessionUpdate"], "notice");
        assert_eq!(notice["severity"], "warning");
        assert!(
            notice["title"].as_str().unwrap().contains("2/5"),
            "{notice}"
        );
        assert_eq!(outgoing(&retrying, &plain(), &mut calls), None);
        assert!(matches!(
            outgoing(&retrying, &opted_in(), &mut calls),
            Some(Outgoing::Ext(_))
        ));
    }

    fn retrying_event(status: Option<u16>, delay_ms: u64) -> EngineEvent {
        EngineEvent::Retrying {
            attempt: 2,
            max: 5,
            reason: "rate limited".into(),
            delay_ms,
            status,
            request_id: None,
            scope: hotl_engine::RetryScope::Sample,
            discarded_partial: false,
        }
    }

    #[test]
    fn a_retry_notice_carries_its_numbers_for_an_opted_in_client() {
        let caps = ClientCaps {
            notices: true,
            ext: true,
            ..ClientCaps::default()
        };
        let mut calls = ToolCalls::rooted(root());
        let notice = update_json(outgoing(
            &retrying_event(Some(429), 2_000),
            &caps,
            &mut calls,
        ));
        assert_eq!(notice["title"], "Retrying (2/5)");
        assert_eq!(
            notice["_meta"][META_RETRY],
            json!({"attempt": 2, "max": 5, "status": 429, "delayMs": 2000, "scope": "sample"})
        );
    }

    #[test]
    fn a_retry_notice_for_a_client_with_notices_but_no_extensions_has_no_meta() {
        let caps = ClientCaps {
            notices: true,
            ..ClientCaps::default()
        };
        let mut calls = ToolCalls::rooted(root());
        let notice = update_json(outgoing(
            &retrying_event(Some(429), 2_000),
            &caps,
            &mut calls,
        ));
        assert!(notice.get("_meta").is_none(), "{notice}");
    }

    #[test]
    fn a_retry_without_a_status_omits_it() {
        let caps = ClientCaps {
            notices: true,
            ext: true,
            ..ClientCaps::default()
        };
        let mut calls = ToolCalls::rooted(root());
        let notice = update_json(outgoing(&retrying_event(None, 0), &caps, &mut calls));
        let meta = &notice["_meta"][META_RETRY];
        assert!(meta.get("status").is_none(), "{meta}");
        assert_eq!(meta["delayMs"], 0);
    }

    #[test]
    fn a_fold_is_one_compaction_from_start_to_end() {
        let caps = ClientCaps {
            compaction: true,
            ..ClientCaps::default()
        };
        let mut calls = ToolCalls::rooted(root());
        let started = update_json(outgoing(
            &EngineEvent::Compacting { items: 9 },
            &caps,
            &mut calls,
        ));
        assert_eq!(started["sessionUpdate"], "compaction_update");
        assert_eq!(started["status"], "in_progress");
        let ended = update_json(outgoing(
            &EngineEvent::Compacted {
                degraded: false,
                focus: None,
                usage: Default::default(),
                estimate: 10,
            },
            &caps,
            &mut calls,
        ));
        assert_eq!(ended["status"], "completed");
        assert_eq!(ended["compactionId"], started["compactionId"]);
        assert_eq!(
            outgoing(&EngineEvent::Compacting { items: 9 }, &plain(), &mut calls),
            None
        );
    }

    fn variant(event: &EngineEvent) -> &'static str {
        match event {
            EngineEvent::TextDelta(_) => "TextDelta",
            EngineEvent::ThinkingDelta(_) => "ThinkingDelta",
            EngineEvent::ToolStart { .. } => "ToolStart",
            EngineEvent::ToolDone { .. } => "ToolDone",
            EngineEvent::GoalEvaluating { .. } => "GoalEvaluating",
            EngineEvent::ToolQueued { .. } => "ToolQueued",
            EngineEvent::Compacting { .. } => "Compacting",
            EngineEvent::ToolProgress { .. } => "ToolProgress",
            EngineEvent::ToolInputProgress { .. } => "ToolInputProgress",
            EngineEvent::ToolDenied { .. } => "ToolDenied",
            EngineEvent::ToolAutoAllowed { .. } => "ToolAutoAllowed",
            EngineEvent::ToolFlagged { .. } => "ToolFlagged",
            EngineEvent::ChildTool { .. } => "ChildTool",
            EngineEvent::ChildText { .. } => "ChildText",
            EngineEvent::Delegation { .. } => "Delegation",
            EngineEvent::CacheWarmed { .. } => "CacheWarmed",
            EngineEvent::Retrying { .. } => "Retrying",
            EngineEvent::FallbackModel { .. } => "FallbackModel",
            EngineEvent::ModelChanged { .. } => "ModelChanged",
            EngineEvent::PromptQueued => "PromptQueued",
            EngineEvent::BudgetNotice { .. } => "BudgetNotice",
            EngineEvent::Compacted { .. } => "Compacted",
            EngineEvent::FoldSkipped { .. } => "FoldSkipped",
            EngineEvent::Cleared { .. } => "Cleared",
            EngineEvent::Ask { .. } => "Ask",
            EngineEvent::Question { .. } => "Question",
            EngineEvent::EgressAsk { .. } => "EgressAsk",
            EngineEvent::TurnStarted { .. } => "TurnStarted",
            EngineEvent::TurnDone { .. } => "TurnDone",
            EngineEvent::TodosChanged { .. } => "TodosChanged",
            EngineEvent::GoalChanged { .. } => "GoalChanged",
            EngineEvent::MemoryChanged { .. } => "MemoryChanged",
            EngineEvent::JobStarted { .. } => "JobStarted",
            EngineEvent::JobDone { .. } => "JobDone",
            EngineEvent::SessionTitled { .. } => "SessionTitled",
            EngineEvent::AsideDone { .. } => "AsideDone",
            EngineEvent::AsideFailed { .. } => "AsideFailed",
            EngineEvent::PlanPresented { .. } => "PlanPresented",
            EngineEvent::GoalVerdict { .. } => "GoalVerdict",
            EngineEvent::LedgerReport(_) => "LedgerReport",
        }
    }

    const VARIANTS: usize = 40;

    fn every_event() -> Vec<EngineEvent> {
        let usage = hotl_types::TokenUsage::default();
        let todo: Vec<hotl_types::Todo> =
            serde_json::from_value(json!([{"content": "a", "status": "pending"}])).unwrap();
        vec![
            EngineEvent::ToolQueued {
                id: "t1".into(),
                name: "edit".into(),
                summary: "edit a.rs".into(),
                ahead: 0,
            },
            EngineEvent::ToolAutoAllowed {
                id: "t1".into(),
                name: "edit".into(),
                rule: "edit*".into(),
            },
            start(
                "t1",
                "edit",
                json!({"path": "a.rs", "old_string": "a", "new_string": "b"}),
            ),
            EngineEvent::ToolInputProgress { calls: 1, bytes: 9 },
            EngineEvent::ToolProgress {
                id: "t1".into(),
                name: "edit".into(),
                tail: "x".into(),
                lines: 1,
                bytes: 1,
            },
            done("t1", "edit", true, "ok"),
            EngineEvent::ToolFlagged {
                id: "t2".into(),
                name: "write".into(),
                summary: "write /etc/x".into(),
                why: "outside".into(),
                denied: true,
            },
            EngineEvent::ToolDenied {
                id: "t3".into(),
                name: "bash".into(),
            },
            start("s1", "spawn", json!({"prompt": "go"})),
            EngineEvent::ChildTool {
                parent_id: "s1".into(),
                id: "c1".into(),
                name: "read".into(),
                summary: "read x".into(),
                ok: Some(true),
                tokens: Some(5),
            },
            EngineEvent::ChildText {
                parent_id: "s1".into(),
                text: "child says".into(),
            },
            EngineEvent::Delegation {
                subagent_runs: 1,
                duplicate_work_paths: 0,
                false_completions: 0,
                usage,
                tool_calls: 1,
            },
            done("s1", "spawn", true, "child done"),
            EngineEvent::TextDelta("hi".into()),
            EngineEvent::ThinkingDelta("hm".into()),
            EngineEvent::GoalEvaluating { turn: 1 },
            EngineEvent::Compacting { items: 3 },
            EngineEvent::Compacted {
                degraded: true,
                focus: Some("tests".into()),
                usage,
                estimate: 5,
            },
            EngineEvent::CacheWarmed {
                usage,
                cost_usd: Some(0.1),
            },
            EngineEvent::Retrying {
                attempt: 1,
                max: 3,
                reason: "429".into(),
                delay_ms: 100,
                status: Some(429),
                request_id: Some("req".into()),
                scope: hotl_engine::RetryScope::Sample,
                discarded_partial: true,
            },
            EngineEvent::FallbackModel { model: "m2".into() },
            EngineEvent::ModelChanged {
                model: "m2".into(),
                previous: "m".into(),
                context_tokens: 1,
                context_window: 2,
            },
            EngineEvent::PromptQueued,
            EngineEvent::BudgetNotice {
                pct: 80,
                used_usd: 8.0,
                cap_usd: 10.0,
            },
            EngineEvent::FoldSkipped {
                reason: "nothing to fold".into(),
            },
            EngineEvent::Cleared { count: 2 },
            EngineEvent::Ask {
                summary: "bash: ls".into(),
                protected_why: None,
                call_id: "t-ask".into(),
                tool: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
                origin: hotl_engine::AskOrigin::Tool,
                reply: tokio::sync::oneshot::channel().0,
            },
            EngineEvent::Question {
                id: "q".into(),
                question: hotl_types::Question {
                    header: "h".into(),
                    prompt: "p".into(),
                    options: Vec::new(),
                    multi: false,
                },
                reply: tokio::sync::oneshot::channel().0,
            },
            EngineEvent::EgressAsk {
                host: "example.com".into(),
                reply: tokio::sync::oneshot::channel().0,
            },
            EngineEvent::TurnStarted { tags: vec![1] },
            EngineEvent::TurnDone {
                outcome: Outcome::Done { text: "x".into() },
                usage,
                mispredictions: 0,
            },
            EngineEvent::TodosChanged {
                items: todo.clone(),
            },
            EngineEvent::GoalChanged {
                condition: Some("ship".into()),
            },
            EngineEvent::MemoryChanged {
                note: "n".into(),
                date: "2026-11-17".into(),
                bytes: 3,
                path: "/m".into(),
            },
            EngineEvent::JobStarted {
                id: Some("t4".into()),
                job: "j1".into(),
                pid: 7,
                command: "sleep 1".into(),
                detached: false,
            },
            EngineEvent::JobDone {
                id: Some("t4".into()),
                job: "j1".into(),
                rc: "0".into(),
                secs: 1,
                stopped_by: None,
            },
            EngineEvent::SessionTitled {
                title: Some("t".into()),
                usage,
            },
            EngineEvent::AsideDone {
                question: "q".into(),
                text: "a".into(),
                usage,
            },
            EngineEvent::AsideFailed {
                question: "q".into(),
                reason: "r".into(),
            },
            EngineEvent::PlanPresented {
                summary: "s".into(),
                nodes: todo,
                path: None,
            },
            verdict(),
            EngineEvent::LedgerReport(hotl_engine::LedgerSummary {
                sample_count: 1,
                overhead_p50_ns: 1,
                overhead_p99_ns: 1,
                phase_deltas: Vec::new(),
                max_rss_bytes: 1,
                samples: Vec::new(),
            }),
        ]
    }

    #[test]
    fn every_event_walks_every_variant() {
        let names: std::collections::BTreeSet<&str> = every_event().iter().map(variant).collect();
        assert_eq!(names.len(), VARIANTS, "{names:?}");
    }

    fn hotl_keys(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(map) => {
                for (k, child) in map {
                    if k.starts_with("hotl.dev/") || k.starts_with("_hotl") {
                        out.push(k.clone());
                    }
                    hotl_keys(child, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|i| hotl_keys(i, out)),
            _ => {}
        }
    }

    #[test]
    fn no_hotl_meta_for_a_plain_client() {
        let every_cap = ClientCaps {
            notices: true,
            compaction: true,
            ..ClientCaps::default()
        };
        for caps in [plain(), every_cap] {
            let mut calls = ToolCalls::rooted(root());
            for event in every_event() {
                match outgoing(&event, &caps, &mut calls) {
                    None => {}
                    Some(Outgoing::Ext(v)) => {
                        panic!("{} went out as an extension: {v}", variant(&event))
                    }
                    Some(Outgoing::Update(u)) => {
                        let v = serde_json::to_value(u).unwrap();
                        let mut keys = Vec::new();
                        hotl_keys(&v, &mut keys);
                        assert!(keys.is_empty(), "{}: {keys:?} in {v}", variant(&event));
                    }
                }
            }
        }
    }

    #[test]
    fn every_frame_reaches_an_opted_in_client_exactly_once() {
        let drained_elsewhere = ["ModelChanged"];
        let mut calls = ToolCalls::rooted(root());
        for event in every_event() {
            let name = variant(&event);
            let frame = crate::wire::update_frame(&event);
            let out = outgoing(&event, &opted_in(), &mut calls);
            if frame.is_none() || drained_elsewhere.contains(&name) {
                assert_eq!(out, None, "{name}");
                continue;
            }
            let carried = match out {
                Some(Outgoing::Ext(v)) => v,
                Some(Outgoing::Update(u)) => {
                    serde_json::to_value(u).unwrap()["_meta"][META_FRAME].clone()
                }
                None => panic!("{name} never reached the client"),
            };
            assert_eq!(Some(carried), frame, "{name}");
        }
    }

    #[test]
    fn tool_kind_covers_every_builtin() {
        let table = [
            ("read", ToolKind::Read),
            ("edit", ToolKind::Edit),
            ("write", ToolKind::Edit),
            ("bash", ToolKind::Execute),
            ("glob", ToolKind::Search),
            ("grep", ToolKind::Search),
            ("recall", ToolKind::Search),
            ("web_fetch", ToolKind::Fetch),
            ("web_search", ToolKind::Fetch),
            ("todo_write", ToolKind::Think),
            ("present_plan", ToolKind::Think),
            ("ask_user", ToolKind::Other),
            ("remember", ToolKind::Other),
            ("report_result", ToolKind::Other),
            ("skill", ToolKind::Other),
            ("spawn", ToolKind::Other),
            ("wait", ToolKind::Other),
            ("send", ToolKind::Other),
            ("resume", ToolKind::Other),
            ("workflow", ToolKind::Other),
            ("mcp", ToolKind::Other),
        ];
        for def in hotl_tools::Registry::builtin().defs() {
            assert!(
                table.iter().any(|(name, _)| *name == def.name),
                "builtin `{}` has no tool kind decision",
                def.name
            );
        }
        for (name, kind) in table {
            assert_eq!(tool_kind(name), kind, "{name}");
        }
    }

    fn asked<'a>(
        call_id: &'a str,
        tool: &'a str,
        input: &'a Value,
        why: Option<&'a str>,
    ) -> AskView<'a> {
        AskView {
            call_id,
            tool,
            input,
            summary: "the summary",
            protected_why: why,
        }
    }

    fn selected(id: &str) -> RequestPermissionOutcome {
        RequestPermissionOutcome::Selected(crate::acp_types::SelectedPermissionOutcome {
            option_id: id.into(),
            meta: None,
        })
    }

    fn reply_meta(v: Value) -> Meta {
        meta_of(META_REPLY, v)
    }

    #[test]
    fn a_tool_id_can_never_pose_as_a_hotl_id() {
        assert_eq!(wire_id("toolu_1"), "toolu_1");
        assert_eq!(wire_id("egress-1"), "tool-egress-1");
        assert_eq!(wire_id("loop-3"), "tool-loop-3");
        assert_eq!(wire_id("tool-egress-1"), "tool-tool-egress-1");
        let caps = ClientCaps {
            ext: true,
            ..ClientCaps::default()
        };
        let mut calls = ToolCalls::default();
        let input = serde_json::json!({"url": "https://example.com"});
        let ask = calls.ask(
            &AskView {
                call_id: "egress-1",
                tool: "web_fetch",
                input: &input,
                summary: "web_fetch https://example.com",
                protected_why: None,
            },
            &caps,
        );
        assert_eq!(ask.tool_call_id, "tool-egress-1");
        assert!(ask.meta.unwrap().get(META_EGRESS).is_none());
        let start = EngineEvent::ToolStart {
            id: "egress-1".into(),
            name: "web_fetch".into(),
            summary: "web_fetch https://example.com".into(),
            input: input.clone(),
        };
        let update = serde_json::to_value(outgoing(&start, &caps, &mut calls).map(|o| match o {
            Outgoing::Update(u) => u,
            Outgoing::Ext(_) => panic!("a standard update"),
        }))
        .unwrap();
        assert_eq!(update["toolCallId"], "tool-egress-1", "{update}");
    }

    #[test]
    fn the_frame_meta_names_a_tool_call_by_its_wire_id() {
        let mut calls = ToolCalls::default();
        let start = EngineEvent::ToolStart {
            id: "loop-1".into(),
            name: "spawn".into(),
            summary: "spawn".into(),
            input: json!({"prompt": "look"}),
        };
        let started = update_json(outgoing(&start, &opted_in(), &mut calls));
        assert_eq!(started["toolCallId"], "tool-loop-1");
        assert_eq!(
            started["_meta"][META_FRAME]["id"], "tool-loop-1",
            "{started}"
        );
        let child_tool = EngineEvent::ChildTool {
            parent_id: "loop-1".into(),
            id: "egress-2".into(),
            name: "read".into(),
            summary: "read x".into(),
            ok: None,
            tokens: None,
        };
        let child = update_json(outgoing(&child_tool, &opted_in(), &mut calls));
        assert_eq!(child["toolCallId"], "tool-loop-1");
        assert_eq!(
            child["_meta"][META_CHILD]["parent_id"], "tool-loop-1",
            "{child}"
        );
        assert_eq!(child["_meta"][META_CHILD]["id"], "egress-2", "{child}");
        let done = EngineEvent::ToolDone {
            id: "loop-1".into(),
            name: "spawn".into(),
            ok: true,
            output: "ok".into(),
            before: None,
            lines: 1,
            bytes: 2,
        };
        let finished = update_json(outgoing(&done, &opted_in(), &mut calls));
        assert_eq!(
            finished["_meta"][META_FRAME]["id"], "tool-loop-1",
            "{finished}"
        );
    }

    #[test]
    fn ask_carries_tool_call_id_kind_and_diff() {
        let mut calls = ToolCalls::rooted(root());
        let input = json!({"path": "a.rs", "old_string": "x", "new_string": "y"});
        let call = calls.ask(&asked("t1", "edit", &input, None), &plain());
        assert_eq!(
            serde_json::to_value(&call).unwrap(),
            json!({
                "toolCallId": "t1",
                "kind": "edit",
                "status": "pending",
                "title": "the summary",
                "content": [{"type": "diff", "path": at("a.rs"), "oldText": "x", "newText": "y"}],
                "locations": [{"path": at("a.rs")}],
                "rawInput": input,
            })
        );
        let ids: Vec<String> = ask_options("the summary")
            .into_iter()
            .map(|o| o.option_id)
            .collect();
        assert_eq!(ids, [OPTION_ALLOW, OPTION_DENY]);
        let after = update_json(outgoing(
            &start("t1", "edit", input.clone()),
            &plain(),
            &mut calls,
        ));
        assert_eq!(after["sessionUpdate"], "tool_call_update", "{after}");
    }

    #[test]
    fn a_write_ask_diffs_against_the_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old.txt"), "before\n").unwrap();
        let mut calls = ToolCalls::rooted(dir.path());
        let content = |rel: &str, calls: &mut ToolCalls| {
            let input = json!({"path": rel, "content": "after\n"});
            let call = calls.ask(&asked(rel, "write", &input, None), &opted_in());
            serde_json::to_value(&call).unwrap()
        };
        let over = content("old.txt", &mut calls);
        assert_eq!(over["content"][0]["oldText"], "before\n");
        assert_eq!(over["content"][0]["newText"], "after\n");
        assert_eq!(
            over["_meta"][META_ASK]["diff"],
            json!([{"op": "del", "text": "before"}, {"op": "add", "text": "after"}])
        );
        let fresh = content("new.txt", &mut calls);
        assert!(fresh["content"][0].get("oldText").is_none(), "{fresh}");
        assert_eq!(fresh["content"][0]["newText"], "after\n");
        #[cfg(unix)]
        {
            let elsewhere = tempfile::tempdir().unwrap();
            std::fs::write(elsewhere.path().join("p.txt"), "private\n").unwrap();
            std::os::unix::fs::symlink(elsewhere.path().join("p.txt"), dir.path().join("link.txt"))
                .unwrap();
            let hidden = content("link.txt", &mut calls);
            assert!(hidden.get("content").is_none(), "{hidden}");
            assert_eq!(hidden["_meta"][META_ASK]["diff"], json!([]));
            assert!(!hidden.to_string().contains("private"));
        }
    }

    #[test]
    fn protected_why_is_meta_for_opted_in_and_title_for_plain() {
        let input = json!({"command": "cat ~/.ssh/id"});
        let call = |caps: &ClientCaps| {
            let mut calls = ToolCalls::rooted(root());
            serde_json::to_value(calls.ask(&asked("t", "bash", &input, Some("a key")), caps))
                .unwrap()
        };
        let plain_call = call(&plain());
        assert_eq!(plain_call["title"], "the summary — a key");
        assert!(plain_call.get("_meta").is_none());
        let ext_call = call(&opted_in());
        assert_eq!(ext_call["title"], "the summary");
        assert_eq!(ext_call["_meta"][META_PROTECTED_WHY], "a key");
        assert_eq!(
            ext_call["_meta"][META_ASK],
            json!({"summary": "the summary", "protectedWhy": "a key", "diff": []})
        );
    }

    #[test]
    fn a_bash_ask_offers_secret_reads_once_they_are_still_denied() {
        let ids = |summary: &str| -> Vec<String> {
            ask_options(summary)
                .into_iter()
                .map(|o| o.option_id)
                .collect()
        };
        assert_eq!(
            ids("bash [sandboxed:seatbelt]: cat ~/.aws/credentials"),
            [OPTION_ALLOW, OPTION_SECRET_READS, OPTION_DENY]
        );
        assert_eq!(
            ids("bash [sandboxed:seatbelt reads:open]: cat x"),
            [OPTION_ALLOW, OPTION_DENY]
        );
        let options = ask_options("bash [sandboxed:seatbelt]: cat x");
        assert_eq!(options[1].name, "Allow, including secret files");
        assert_eq!(options[1].kind, PermissionOptionKind::AllowOnce);
        assert_eq!(options[2].kind, PermissionOptionKind::RejectOnce);
    }

    #[test]
    fn cancelled_outcome_denies() {
        let offered = ask_options("x");
        assert_eq!(
            ask_reply(&RequestPermissionOutcome::Cancelled, None, true, &offered),
            AskReply::Deny { message: None }
        );
    }

    #[test]
    fn unknown_option_denies() {
        let offered = ask_options("edit a.rs");
        for id in ["maybe", OPTION_SECRET_READS, ""] {
            assert_eq!(
                ask_reply(&selected(id), None, true, &offered),
                AskReply::Deny { message: None },
                "{id}"
            );
        }
        assert_eq!(
            ask_reply(&selected("allow"), None, false, &offered),
            AskReply::Allow
        );
    }

    #[test]
    fn respond_meta_ignored_for_plain_client() {
        let offered = ask_options("x");
        let respond = reply_meta(json!({"respond": "use main"}));
        let edited = reply_meta(json!({"input": {"a": 1}}));
        let said_no = reply_meta(json!({"message": "not now"}));
        assert_eq!(
            ask_reply(&selected("allow"), Some(&respond), false, &offered),
            AskReply::Allow
        );
        assert_eq!(
            ask_reply(&selected("allow"), Some(&edited), false, &offered),
            AskReply::Allow
        );
        assert_eq!(
            ask_reply(&selected("deny"), Some(&said_no), false, &offered),
            AskReply::Deny { message: None }
        );
        assert_eq!(
            ask_reply(&selected("allow"), Some(&respond), true, &offered),
            AskReply::Respond {
                content: "use main".into()
            }
        );
        assert_eq!(
            ask_reply(&selected("allow"), Some(&edited), true, &offered),
            AskReply::AllowEdited {
                input: json!({"a": 1})
            }
        );
        assert_eq!(
            ask_reply(&selected("deny"), Some(&said_no), true, &offered),
            AskReply::Deny {
                message: Some("not now".into())
            }
        );
    }

    #[test]
    fn egress_is_a_fetch_permission() {
        assert_eq!(
            serde_json::to_value(egress_tool_call(7, "registry.npmjs.org", &plain())).unwrap(),
            json!({
                "toolCallId": "egress-7",
                "kind": "fetch",
                "status": "pending",
                "title": "Network access to registry.npmjs.org",
                "rawInput": {"host": "registry.npmjs.org"},
            })
        );
        let ext = serde_json::to_value(egress_tool_call(7, "h", &opted_in())).unwrap();
        assert_eq!(ext["_meta"][META_EGRESS], json!({"host": "h"}));
        let ids: Vec<String> = egress_options().into_iter().map(|o| o.option_id).collect();
        assert_eq!(ids, [OPTION_ALLOW, OPTION_DENY]);
        use hotl_tools::net::EgressDecision;
        assert_eq!(egress_decision(&selected("allow")), EgressDecision::Allow);
        assert_eq!(egress_decision(&selected("deny")), EgressDecision::Deny);
        assert_eq!(
            egress_decision(&selected("other")),
            EgressDecision::NoAnswer
        );
        assert_eq!(
            egress_decision(&RequestPermissionOutcome::Cancelled),
            EgressDecision::NoAnswer
        );
    }

    fn answered(value: Value, multi: bool) -> hotl_types::QuestionAnswer {
        let response: CreateElicitationResponse = serde_json::from_value(value).unwrap();
        question_answer(&response, &["MVP".to_string(), "Full".to_string()], multi)
    }

    #[test]
    fn an_accepted_form_maps_to_selected_or_free_text() {
        use hotl_types::QuestionAnswer::{FreeText, NoHuman, Selected};
        let accept = |content: Value| json!({"action": "accept", "content": content});
        assert_eq!(
            answered(accept(json!({"answer": "MVP"})), false),
            Selected(vec!["MVP".into()])
        );
        assert_eq!(
            answered(accept(json!({"answer": ["MVP", "Full", "Nope"]})), true),
            Selected(vec!["MVP".into(), "Full".into()])
        );
        assert_eq!(
            answered(accept(json!({"answer": ["MVP", "Full"]})), false),
            Selected(vec!["MVP".into()])
        );
        assert_eq!(
            answered(accept(json!({"other": "later"})), false),
            FreeText("later".into())
        );
        assert_eq!(
            answered(accept(json!({"answer": "Nope", "other": "later"})), false),
            FreeText("later".into())
        );
        assert_eq!(answered(accept(json!({"answer": "Nope"})), false), NoHuman);
        assert_eq!(answered(accept(json!({"other": "  "})), false), NoHuman);
        assert_eq!(answered(json!({"action": "accept"}), false), NoHuman);
        assert_eq!(answered(json!({"action": "decline"}), false), NoHuman);
        assert_eq!(answered(json!({"action": "cancel"}), false), NoHuman);
    }

    #[test]
    fn a_malformed_elicitation_reply_does_not_parse() {
        assert!(
            serde_json::from_value::<CreateElicitationResponse>(json!({"action": "maybe"}))
                .is_err()
        );
        assert!(serde_json::from_value::<CreateElicitationResponse>(json!({})).is_err());
    }
}
