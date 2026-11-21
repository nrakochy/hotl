use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    AuthenticateRequest, CancelNotification, CloseSessionRequest, ContentBlock,
    CreateElicitationRequest, CreateElicitationResponse, InitializeRequest, ListSessionsRequest,
    LoadSessionRequest, NewSessionRequest, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    ResumeSessionRequest, SelectedPermissionOutcome, SessionNotification, SessionUpdate,
    SetSessionConfigOptionRequest, SetSessionModeRequest, StopReason, TextContent, ToolCallStatus,
};
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{AcpAgent, Agent, Client};
use hotl_acp_conformance::{check_wire, position, request_id, Wire, World};
use serde_json::{json, Value};

type Seen = Arc<Mutex<Vec<SessionNotification>>>;
type Asked = Arc<Mutex<Vec<RequestPermissionRequest>>>;
type Elicited = Arc<Mutex<Vec<Value>>>;

#[derive(Default)]
struct Log {
    wire: Wire,
    seen: Seen,
    asked: Asked,
    elicited: Elicited,
}

async fn run<R>(
    agent: AcpAgent,
    log: &Log,
    main: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<R, agent_client_protocol::Error>,
) -> R {
    let (seen, asked, elicited) = (log.seen.clone(), log.asked.clone(), log.elicited.clone());
    let client = Client
        .v1()
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                seen.lock().unwrap().push(n);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |r: RequestPermissionRequest, responder, _cx| {
                let allow = r
                    .options
                    .iter()
                    .find(|o| o.kind == PermissionOptionKind::AllowOnce)
                    .map(|o| o.option_id.clone());
                asked.lock().unwrap().push(r);
                responder.respond(RequestPermissionResponse::new(match allow {
                    Some(id) => {
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id))
                    }
                    None => RequestPermissionOutcome::Cancelled,
                }))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |r: CreateElicitationRequest, responder, _cx| {
                let request = serde_json::to_value(&r).unwrap();
                let reply = match request.pointer("/requestedSchema/properties/answer/enum/0") {
                    Some(label) => json!({"action": "accept", "content": {"answer": label}}),
                    None => json!({"action": "decline"}),
                };
                elicited.lock().unwrap().push(request);
                responder
                    .respond(serde_json::from_value::<CreateElicitationResponse>(reply).unwrap())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, main);
    tokio::time::timeout(Duration::from_secs(600), client)
        .await
        .expect("the run timed out")
        .expect("the connection failed")
}

fn text(t: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text(TextContent::new(t))]
}

fn chunks(log: &Log, user: bool) -> String {
    log.seen
        .lock()
        .unwrap()
        .iter()
        .filter_map(|n| match (&n.update, user) {
            (SessionUpdate::UserMessageChunk(c), true)
            | (SessionUpdate::AgentMessageChunk(c), false) => match &c.content {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn sdk_client_initializes_and_authenticates() {
    let world = World::new(&[]);
    let log = Log::default();
    let init = run(
        world.agent(&log.wire),
        &log,
        async |cx: ConnectionTo<Agent>| {
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let id = init.auth_methods.first().map(|m| m.id().clone()).unwrap();
            cx.send_request(AuthenticateRequest::new(id))
                .block_task()
                .await?;
            Ok(init)
        },
    )
    .await;
    assert_eq!(init.protocol_version, ProtocolVersion::V1);
    assert!(init.agent_capabilities.load_session);
    assert!(init
        .auth_methods
        .iter()
        .any(|m| m.id().to_string() == "hotl-key"));
    assert_eq!(check_wire(&log.wire), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn sdk_client_runs_a_prompt_to_end_turn() {
    let world = World::new(&[json!({"text": "hello from the fake"})]);
    let log = Log::default();
    let work = world.work.clone();
    let reply = run(
        world.agent(&log.wire),
        &log,
        async |cx: ConnectionTo<Agent>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(work))
                .block_task()
                .await?;
            cx.send_request(PromptRequest::new(session.session_id, text("say hello")))
                .block_task()
                .await
        },
    )
    .await;
    assert_eq!(reply.stop_reason, StopReason::EndTurn);
    assert_eq!(chunks(&log, false), "hello from the fake");
    assert!(check_wire(&log.wire) >= 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn sdk_client_answers_a_permission_and_the_tool_runs() {
    let world = World::new(&[
        json!({"tool_use": {"id": "t1", "name": "write", "input": {"path": "out.txt", "content": "written"}}}),
        json!({"text": "done"}),
    ]);
    let log = Log::default();
    let work = world.work.clone();
    let reply = run(
        world.agent(&log.wire),
        &log,
        async |cx: ConnectionTo<Agent>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(work))
                .block_task()
                .await?;
            cx.send_request(PromptRequest::new(session.session_id, text("write it")))
                .block_task()
                .await
        },
    )
    .await;
    assert_eq!(reply.stop_reason, StopReason::EndTurn);
    let asked = log.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "one permission request");
    let call = asked[0].tool_call.tool_call_id.clone();
    assert_eq!(
        std::fs::read_to_string(world.work.join("out.txt")).unwrap(),
        "written"
    );
    assert!(log.seen.lock().unwrap().iter().any(|n| matches!(
        &n.update,
        SessionUpdate::ToolCallUpdate(u)
            if u.tool_call_id == call && u.fields.status == Some(ToolCallStatus::Completed)
    )));
    assert!(check_wire(&log.wire) >= 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn sdk_client_cancel_ends_with_cancelled() {
    let world = World::new(&[json!({"text": "too late", "after_ms": 120000})]);
    let log = Log::default();
    let work = world.work.clone();
    let reply = run(
        world.agent(&log.wire),
        &log,
        async |cx: ConnectionTo<Agent>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(work))
                .block_task()
                .await?;
            let id = session.session_id;
            let prompt = cx
                .send_request(PromptRequest::new(id.clone(), text("take your time")))
                .block_task();
            let cancel = async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                cx.send_notification(CancelNotification::new(id))
            };
            let (reply, sent) = tokio::join!(
                tokio::time::timeout(Duration::from_secs(60), prompt),
                cancel
            );
            sent?;
            Ok(reply.expect("cancel did not end the turn"))
        },
    )
    .await
    .expect("the prompt failed");
    assert_eq!(reply.stop_reason, StopReason::Cancelled);
    assert_eq!(chunks(&log, false), "");
    assert!(check_wire(&log.wire) >= 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn sdk_client_load_replays_history() {
    let world = World::new(&[json!({"text": "first answer"})]);
    let first = Log::default();
    let work = world.work.clone();
    let session_id = run(
        world.agent(&first.wire),
        &first,
        async |cx: ConnectionTo<Agent>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(work))
                .block_task()
                .await?;
            let reply = cx
                .send_request(PromptRequest::new(
                    session.session_id.clone(),
                    text("remember me"),
                ))
                .block_task()
                .await?;
            assert_eq!(reply.stop_reason, StopReason::EndTurn);
            Ok(session.session_id)
        },
    )
    .await;
    assert!(check_wire(&first.wire) >= 4);

    let second = Log::default();
    let work = world.work.clone();
    let sid = session_id.clone();
    run(
        world.agent(&second.wire),
        &second,
        async |cx: ConnectionTo<Agent>| {
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert!(init.agent_capabilities.load_session);
            cx.send_request(LoadSessionRequest::new(sid, work))
                .block_task()
                .await
        },
    )
    .await;
    assert_eq!(chunks(&second, true), "remember me");
    assert_eq!(chunks(&second, false), "first answer");
    let is_chunk =
        |v: &Value, kind: &str| v.pointer("/params/update/sessionUpdate") == Some(&json!(kind));
    let replayed = position(&second.wire, |v| is_chunk(v, "agent_message_chunk")).unwrap();
    let load = request_id(&second.wire, "session/load");
    let answered = position(&second.wire, |v| {
        v.get("result").is_some() && v.get("id") == load.as_ref()
    })
    .unwrap();
    assert!(
        replayed < answered,
        "session/load must replay the history before it answers"
    );
    assert!(check_wire(&second.wire) >= 4);
}

fn typed<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn sdk_client_with_every_capability_reads_every_update_hotl_sends() {
    let world = World::with_model(
        &[
            json!({"http_error": 529, "kind": "overloaded_error"}),
            json!({"tool_use": {"id": "t1", "name": "todo_write", "input": {"todos": [
                {"id": "n1", "content": "read the code", "status": "in_progress", "active_form": "reading"},
                {"id": "n2", "content": "write the fix", "status": "pending", "active_form": "writing"}
            ]}}}),
            json!({"tool_use": {"id": "t2", "name": "ask_user", "input": {
                "header": "Scope", "prompt": "How far?",
                "options": [{"label": "small"}, {"label": "large"}]
            }}}),
            json!({"text": "all set", "usage": {"input": 2000, "output": 300}}),
        ],
        "claude-sonnet-4-6",
    );
    let log = Log::default();
    let work = world.work.clone();
    let reply = run(
        world.agent(&log.wire),
        &log,
        async |cx: ConnectionTo<Agent>| {
            cx.send_request(typed::<InitializeRequest>(json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "session": {"notices": {}, "compaction": {}},
                    "elicitation": {"form": {}}
                }
            })))
            .block_task()
            .await?;
            let session = cx
                .send_request(NewSessionRequest::new(work.clone()))
                .block_task()
                .await?;
            let id = session.session_id.clone();
            let modes = serde_json::to_value(&session.modes).unwrap();
            let other_mode = modes["availableModes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].clone())
                .find(|m| *m != modes["currentModeId"])
                .unwrap();
            cx.send_request(typed::<SetSessionModeRequest>(
                json!({"sessionId": id, "modeId": other_mode}),
            ))
            .block_task()
            .await?;
            cx.send_request(typed::<SetSessionConfigOptionRequest>(
                json!({"sessionId": id, "configId": "effort", "value": "high"}),
            ))
            .block_task()
            .await?;
            let reply = cx
                .send_request(PromptRequest::new(id.clone(), text("plan it")))
                .block_task()
                .await?;
            cx.send_request(typed::<ListSessionsRequest>(json!({})))
                .block_task()
                .await?;
            cx.send_request(ResumeSessionRequest::new(id.clone(), work.clone()))
                .block_task()
                .await?;
            cx.send_request(CloseSessionRequest::new(id))
                .block_task()
                .await?;
            Ok(reply)
        },
    )
    .await;
    assert_eq!(reply.stop_reason, StopReason::EndTurn);
    let seen = log.seen.lock().unwrap().clone();
    let has = |want: fn(&SessionUpdate) -> bool| seen.iter().any(|n| want(&n.update));
    assert!(has(|u| matches!(u, SessionUpdate::Notice(_))), "a notice");
    assert!(has(|u| matches!(u, SessionUpdate::Plan(_))), "a plan");
    assert!(
        has(|u| matches!(u, SessionUpdate::CurrentModeUpdate(_))),
        "a mode update"
    );
    assert!(
        has(|u| matches!(u, SessionUpdate::ConfigOptionUpdate(_))),
        "a config option update"
    );
    assert!(
        has(|u| matches!(u, SessionUpdate::UsageUpdate(u) if u.cost.is_some())),
        "a priced usage update"
    );
    assert_eq!(log.elicited.lock().unwrap().len(), 1, "one form question");
    assert!(check_wire(&log.wire) >= 10);
}

#[tokio::test(flavor = "multi_thread")]
async fn sdk_client_with_compaction_reads_a_fold_as_compaction_updates() {
    let world = World::with_model(
        &[
            json!({"tool_use": {"id": "t1", "name": "todo_write", "input": {"todos": [
                {"id": "n1", "content": "read the code", "status": "in_progress", "active_form": "reading"}
            ]}}, "usage": {"input": 5_000_000, "output": 10}}),
            json!({"text": "the digest of what happened"}),
            json!({"text": "folded and done"}),
        ],
        "claude-sonnet-4-6",
    );
    let log = Log::default();
    let work = world.work.clone();
    let reply = run(
        world.agent(&log.wire),
        &log,
        async |cx: ConnectionTo<Agent>| {
            cx.send_request(typed::<InitializeRequest>(json!({
                "protocolVersion": 1,
                "clientCapabilities": {"session": {"compaction": {}}}
            })))
            .block_task()
            .await?;
            let session = cx
                .send_request(NewSessionRequest::new(work.clone()))
                .block_task()
                .await?;
            cx.send_request(PromptRequest::new(session.session_id, text("fold it")))
                .block_task()
                .await
        },
    )
    .await;
    assert_eq!(reply.stop_reason, StopReason::EndTurn);
    let folds: Vec<Value> = log
        .seen
        .lock()
        .unwrap()
        .iter()
        .filter(|n| matches!(n.update, SessionUpdate::CompactionUpdate(_)))
        .map(|n| serde_json::to_value(&n.update).unwrap())
        .collect();
    assert_eq!(folds.len(), 2, "{folds:?}");
    assert_eq!(folds[0]["status"], "in_progress", "{folds:?}");
    assert_eq!(folds[1]["status"], "completed", "{folds:?}");
    assert_eq!(folds[0]["compactionId"], folds[1]["compactionId"]);
    assert!(check_wire(&log.wire) >= 4);
}
