use hotl_types::{Item, SyntheticReason, TodoStatus};
use serde_json::Value;

use crate::acp::{SessionOpen, SessionOpenExtras, WorktreeInfo};
use crate::acp_types::{
    AvailableCommand, AvailableCommandInput, AvailableCommandsUpdate, Content, ContentBlock,
    ContentChunk, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, SessionConfigBoolean,
    SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelect,
    SessionConfigSelectOption, SessionMode, SessionModeState, SessionUpdate, ToolCall,
    ToolCallContent, ToolCallStatus, ToolKind, UnstructuredCommandInput,
};

pub fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" => ToolKind::Read,
        "edit" | "write" | "multi_edit" => ToolKind::Edit,
        "bash" | "bash_output" => ToolKind::Execute,
        "grep" | "glob" | "ls" | "search" => ToolKind::Search,
        "web_fetch" | "web_search" => ToolKind::Fetch,
        "todo_write" | "plan" => ToolKind::Think,
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
        tool_call_id: tool.id,
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
        raw_input: Some(tool.input),
    })
}

fn chunk(text: &str) -> ContentChunk {
    ContentChunk {
        content: ContentBlock::text(text),
    }
}

fn flush_unanswered(open: &mut Vec<OpenTool>, out: &mut Vec<SessionUpdate>) {
    for tool in open.drain(..) {
        out.push(tool_call(tool, ToolCallStatus::Failed, None));
    }
}

pub fn replay_updates(items: &[Item]) -> Vec<SessionUpdate> {
    let mut out = Vec::new();
    let mut open: Vec<OpenTool> = Vec::new();
    for item in items {
        match item {
            Item::User {
                text, synthetic, ..
            } => {
                flush_unanswered(&mut open, &mut out);
                if matches!(synthetic, None | Some(SyntheticReason::Steer)) && !text.is_empty() {
                    out.push(SessionUpdate::UserMessageChunk(chunk(text)));
                }
            }
            Item::Assistant { blocks, .. } => {
                flush_unanswered(&mut open, &mut out);
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(Value::as_str) {
                                out.push(SessionUpdate::AgentMessageChunk(chunk(t)));
                            }
                        }
                        Some("thinking") => {
                            if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                                out.push(SessionUpdate::AgentThoughtChunk(chunk(t)));
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
                            out.push(tool_call(tool, status, Some(&r.content)));
                        }
                        None => out.push(tool_call(tool, ToolCallStatus::Failed, None)),
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
                options: vec![level(&state.model, &state.model)],
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

    fn wire(updates: &[SessionUpdate]) -> Vec<Value> {
        updates
            .iter()
            .map(|u| serde_json::to_value(u).unwrap())
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
            wire(&replay_updates(&items)),
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
        let out = wire(&replay_updates(&items));
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
        let out = wire(&replay_updates(&items));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"]["text"], "also check tests");
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
    fn tool_kinds_follow_the_plan_mapping() {
        assert_eq!(tool_kind("read"), ToolKind::Read);
        assert_eq!(tool_kind("multi_edit"), ToolKind::Edit);
        assert_eq!(tool_kind("bash"), ToolKind::Execute);
        assert_eq!(tool_kind("glob"), ToolKind::Search);
        assert_eq!(tool_kind("web_fetch"), ToolKind::Fetch);
        assert_eq!(tool_kind("todo_write"), ToolKind::Think);
        assert_eq!(tool_kind("mcp__x__y"), ToolKind::Other);
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
            model: "anthropic/m".into(),
            plan: false,
            effort: "default".into(),
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
        assert_eq!(model["options"][0]["value"], "anthropic/m");
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
            },
        ))
        .unwrap();
        assert_eq!(options["sessionUpdate"], "config_option_update");
        assert_eq!(options["configOptions"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn rfc3339_formats_utc_seconds() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(rfc3339(t), "2023-11-14T22:13:20Z");
    }
}
