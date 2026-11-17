use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::schema::{
    AuthenticateRequest, CancelNotification, ContentBlock, InitializeRequest, LoadSessionRequest,
    NewSessionRequest, PermissionOptionKind, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCallStatus,
};
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{AcpAgent, Agent, Client};
use hotl_acp_conformance::{check_wire, position, request_id, Wire, World};
use serde_json::{json, Value};

type Seen = Arc<Mutex<Vec<SessionNotification>>>;
type Asked = Arc<Mutex<Vec<RequestPermissionRequest>>>;

#[derive(Default)]
struct Log {
    wire: Wire,
    seen: Seen,
    asked: Asked,
}

async fn run<R>(
    agent: AcpAgent,
    log: &Log,
    main: impl AsyncFnOnce(ConnectionTo<Agent>) -> Result<R, agent_client_protocol::Error>,
) -> R {
    let (seen, asked) = (log.seen.clone(), log.asked.clone());
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
