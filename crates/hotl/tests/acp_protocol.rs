//! Golden ACP protocol scenario: drive the real server over an in-process
//! duplex stream with a scripted-provider session (no child process).

use std::sync::Arc;

use hotl_engine::{spawn_session, EngineConfig, SessionDeps};
use hotl_platform::SystemClock;
use hotl_provider::ScriptedProvider;
use hotl_store::{Masker, SessionLog};
use hotl_tools::{rules::Rules, Registry};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

// The server module lives in the binary crate; pull it in directly. Some
// items are only exercised by the real factory in the binary, not this test.
#[path = "../src/images.rs"]
#[allow(dead_code)]
mod images;

#[path = "../src/acp.rs"]
#[allow(dead_code)]
mod acp;

// `acp.rs` renders its frames through the shared renderer; pull that in too,
// so `crate::wire` resolves the same way it does in the binary.
#[path = "../src/wire.rs"]
#[allow(dead_code)]
mod wire;

/// A session whose scripted model calls bash (a gated tool → a permission
/// ask) then replies with text.
fn scripted_factory() -> acp::SessionFactory {
    scripted_factory_with_mode("ask")
}

/// What `serve` advertises when a test does not care about the values.
fn server_info() -> acp::ServerInfo {
    acp::ServerInfo {
        skills: Vec::new(),
        workflows: Vec::new(),
        workflows_report: || json!({"runs": []}),
        default_mode: "ask".into(),
        default_plan: false,
        context_window: 200_000,
        stream_idle_secs: 90,
        model_resolver: None,
        // Uncatalogued deliberately: matches `scripted_factory`'s session log
        // model ("m"), and keeps `cost_usd` absent for scenarios that don't
        // test pricing.
        model: "m".into(),
    }
}

fn scripted_factory_with_mode(mode: &'static str) -> acp::SessionFactory {
    scripted_factory_recording(mode, None)
}

/// `scripted_factory_with_mode`, optionally recording the `session_id` of each
/// `SessionSpec::Load` it is handed. That id is the only way to tell a resume
/// through the store id from one through the connection's `acp-N` handle —
/// a factory that ignores the spec (as the plain scripted one does) cannot.
fn scripted_factory_recording(
    mode: &'static str,
    loads: Option<Arc<std::sync::Mutex<Vec<String>>>>,
) -> acp::SessionFactory {
    Box::new(move |spec| {
        // Echo the requested name back, as the real factory does — resolving
        // the open's name is the factory's job, not the protocol layer's.
        let name = match spec {
            acp::SessionSpec::New { name } => name,
            acp::SessionSpec::Load { name, session_id } => {
                if let Some(loads) = &loads {
                    loads.lock().unwrap().push(session_id);
                }
                name
            }
            acp::SessionSpec::Fork {
                name,
                session_id,
                keep,
            } => {
                if let Some(loads) = &loads {
                    loads
                        .lock()
                        .unwrap()
                        .push(format!("fork:{session_id}:{keep:?}"));
                }
                name
            }
        };
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_call("t1", "bash", json!({"command": "echo hi"})),
            ScriptedProvider::text_reply("all done via acp"),
        ]));
        Ok(acp::SessionOpen {
            handle: spawn_session(SessionDeps {
                concurrency: Default::default(),
                provider,
                registry: Arc::new(Registry::builtin()),
                rules: Arc::new(Rules::default()),
                sandbox_enforced: false,
                clock: Arc::new(SystemClock),
                log,
                system: "sys".into(),
                cwd: std::env::temp_dir(),
                hooks: None,
                initial_items: Vec::new(),
                initial_todos: Vec::new(),
                initial_decisions: Vec::new(),
                plan_files: None,
                initial_goal: None,
                config: EngineConfig {
                    max_turns: 6,
                    ..Default::default()
                },
            }),
            name,
            mode: mode.to_string(),
            plan: false,
            // A resolved session default, so the handshake test can pin that
            // it rides the open reply (0030 Task 8).
            default_effort: Some("xhigh".into()),
            effort_schedule: None,
            goal: None,
            todos: Vec::new(),
            effort: None,
            previous_model: None,
            session_id,
            warnings: Vec::new(),
            title: None,
            model: None,
        })
    })
}

/// A factory whose opened session is already mid-turn — its projection ends on
/// an unanswered user item, exactly what `needs_continuation` fires on — and
/// whose provider has two distinguishable replies queued. Which reply the
/// client sees first says whether the open auto-continued.
fn interrupted_factory(seen: Arc<std::sync::Mutex<Vec<String>>>) -> acp::SessionFactory {
    Box::new(move |spec| {
        match &spec {
            acp::SessionSpec::New { .. } => seen.lock().unwrap().push("new".into()),
            acp::SessionSpec::Load { session_id, .. } => {
                seen.lock().unwrap().push(format!("load:{session_id}"))
            }
            acp::SessionSpec::Fork {
                session_id, keep, ..
            } => seen
                .lock()
                .unwrap()
                .push(format!("fork:{session_id}:{keep:?}")),
        }
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        Ok(acp::SessionOpen {
            handle: spawn_session(SessionDeps {
                concurrency: Default::default(),
                provider: Arc::new(ScriptedProvider::new(vec![
                    ScriptedProvider::text_reply("FIRST"),
                    ScriptedProvider::text_reply("SECOND"),
                ])),
                registry: Arc::new(Registry::builtin()),
                rules: Arc::new(Rules::default()),
                sandbox_enforced: false,
                clock: Arc::new(SystemClock),
                log,
                system: "sys".into(),
                cwd: std::env::temp_dir(),
                hooks: None,
                initial_items: vec![hotl_types::Item::User {
                    text: "the parent's last, unanswered prompt".into(),
                    synthetic: None,
                    images: Vec::new(),
                }],
                initial_todos: Vec::new(),
                initial_decisions: Vec::new(),
                plan_files: None,
                initial_goal: None,
                config: EngineConfig {
                    max_turns: 6,
                    ..Default::default()
                },
            }),
            name: None,
            mode: "auto".to_string(),
            plan: false,
            default_effort: None,
            effort_schedule: None,
            goal: None,
            todos: Vec::new(),
            effort: None,
            previous_model: None,
            session_id,
            warnings: Vec::new(),
            title: None,
            model: None,
        })
    })
}

/// D-A9: a resume of an interrupted session finishes the interrupted turn; a
/// **fork** of the same session must not. The user forked to send the
/// conversation somewhere else, and auto-continuing would spend a full-price
/// sample re-answering the parent's stale prompt before the phase instruction
/// ever arrives.
#[tokio::test]
async fn a_fork_never_auto_continues_but_a_resume_still_does() {
    // Resume: the reply arrives with no prompt from the client at all.
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    tokio::spawn(acp::serve(
        sread,
        swrite,
        interrupted_factory(seen.clone()),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"S1"}}),
    )
    .await;
    let text = next_matching(&mut lines, |m| {
        m["params"]["update"]["type"] == "text_delta"
    })
    .await;
    assert_eq!(
        text["params"]["update"]["text"], "FIRST",
        "a resumed interrupted turn picks itself back up"
    );
    assert_eq!(seen.lock().unwrap().clone(), ["load:S1"]);

    // Fork: the same session, the same interrupted projection — but the first
    // reply the client ever sees is the answer to its *own* prompt.
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    tokio::spawn(acp::serve(
        sread,
        swrite,
        interrupted_factory(seen.clone()),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/load",
               "params":{"sessionId":"S1","fork":true,"keepTurns":2}}),
    )
    .await;
    let opened = next_matching(&mut lines, |m| m["id"] == json!(2)).await;
    assert!(opened["result"]["sessionId"].as_str().is_some(), "{opened}");
    assert_eq!(
        seen.lock().unwrap().clone(),
        ["fork:S1:Turns(2)"],
        "`fork: true` plus `keepTurns` reaches the factory as a Fork spec"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt",
               "params":{"text":"Entering phase: Plan."}}),
    )
    .await;
    let text = next_matching(&mut lines, |m| {
        m["params"]["update"]["type"] == "text_delta"
    })
    .await;
    assert_eq!(
        text["params"]["update"]["text"], "FIRST",
        "the fork spent no sample before the client's first prompt — an \
         auto-continue would have consumed FIRST and left SECOND here"
    );
}

/// The two keep coordinates are alternatives, not a pair, and neither means
/// anything without `fork: true`.
#[tokio::test]
async fn keep_params_are_rejected_without_a_fork_and_against_each_other() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/load",
               "params":{"sessionId":"S1","keepItems":4}}),
    )
    .await;
    let err = next_matching(&mut lines, |m| m["id"] == json!(2)).await;
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("fork"),
        "{err}"
    );

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/load",
               "params":{"sessionId":"S1","fork":true,"keepItems":4,"keepTurns":2}}),
    )
    .await;
    let err = next_matching(&mut lines, |m| m["id"] == json!(3)).await;
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("mutually exclusive"),
        "{err}"
    );
}

/// A factory whose opened session carries inherited state (0040): seed items
/// in the engine's Head, a todo list, the given effort tri-state, and a
/// lineage model that differs from the running one — everything the open
/// reply must surface so the status strip is true at open (§5.7).
fn inherited_state_factory(effort: Option<Option<String>>) -> acp::SessionFactory {
    Box::new(move |_spec| {
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        let todos = vec![hotl_types::Todo {
            content: "wire the gate".into(),
            status: hotl_types::TodoStatus::Pending,
            active_form: None,
            ..Default::default()
        }];
        Ok(acp::SessionOpen {
            handle: spawn_session(SessionDeps {
                concurrency: Default::default(),
                provider: Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
                    "ok",
                )])),
                registry: Arc::new(Registry::builtin()),
                rules: Arc::new(Rules::default()),
                sandbox_enforced: false,
                clock: Arc::new(SystemClock),
                log,
                system: "sys".into(),
                cwd: std::env::temp_dir(),
                hooks: None,
                initial_items: vec![hotl_types::Item::User {
                    text: "an inherited turn's worth of context".into(),
                    synthetic: None,
                    images: Vec::new(),
                }],
                initial_todos: todos.clone(),
                initial_decisions: Vec::new(),
                plan_files: None,
                initial_goal: None,
                config: EngineConfig {
                    max_turns: 6,
                    ..Default::default()
                },
            }),
            name: None,
            mode: "ask".into(),
            plan: false,
            default_effort: Some("xhigh".into()),
            effort_schedule: None,
            goal: None,
            todos,
            effort: effort.clone(),
            previous_model: Some("old-m".into()),
            session_id,
            warnings: Vec::new(),
            title: None,
            model: None,
        })
    })
}

/// §5.7 at open: the reply carries the engine's at-open context estimate, the
/// inherited todo list, and the inherited effort — before any turn runs. A
/// session without them says so honestly: `todos: []`, and the `effort` key
/// *absent* (never-set ≠ cleared; the client falls back to `defaultEffort`).
#[tokio::test]
async fn the_open_reply_seeds_context_todos_and_effort() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        inherited_state_factory(Some(Some("max".into()))),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    let r = &opened["result"];
    assert!(
        r["contextTokens"].as_u64().unwrap_or(0) > 0,
        "seed items, system prompt and tool schemas occupy real tokens: {opened}"
    );
    assert_eq!(r["todos"][0]["content"], "wire the gate", "{opened}");
    assert_eq!(r["todos"][0]["status"], "pending", "{opened}");
    assert_eq!(
        r["effort"], "max",
        "inherited-set rides as the level itself"
    );

    // The base factory inherits nothing: `effort` absent, `todos` empty.
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    let r = &opened["result"];
    assert!(
        r.get("effort").is_none(),
        "never-set must stay absent, not null — collapsing the tri-state \
         re-creates the effort lie: {opened}"
    );
    assert_eq!(r["todos"], json!([]), "{opened}");
}

/// The inner half of the tri-state: an inherited *clear* rides as `null`, so
/// the client knows to drop its `defaultEffort` seed rather than fall back.
#[tokio::test]
async fn a_cleared_inherited_effort_rides_as_null() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        inherited_state_factory(Some(None)),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    assert_eq!(
        opened["result"].get("effort"),
        Some(&Value::Null),
        "cleared ≠ never-set: {opened}"
    );
}

/// Resume re-derives the model from config; when the lineage log's model
/// differs, the reply names it — a silent switch under an inherited
/// transcript is a §5.7 lie. Same model (or a fresh session): key absent.
/// 0071 T4: a restored title rides the open reply (absent when none), and a
/// title the engine mints is broadcast as `session_titled`.
#[tokio::test]
async fn the_open_reply_carries_the_title_and_session_titled_is_broadcast() {
    let factory: acp::SessionFactory = Box::new(move |spec| {
        let title = match &spec {
            acp::SessionSpec::Load { .. } => Some("Fix the auth flow".to_string()),
            _ => None,
        };
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::text_reply("answer"),
            ScriptedProvider::text_reply("Minted title"),
        ]));
        Ok(acp::SessionOpen {
            handle: spawn_session(SessionDeps {
                concurrency: Default::default(),
                provider,
                registry: Arc::new(Registry::builtin()),
                rules: Arc::new(Rules::default()),
                sandbox_enforced: false,
                clock: Arc::new(SystemClock),
                log,
                system: "sys".into(),
                cwd: std::env::temp_dir(),
                hooks: None,
                initial_items: Vec::new(),
                initial_todos: Vec::new(),
                initial_decisions: Vec::new(),
                plan_files: None,
                initial_goal: None,
                config: EngineConfig {
                    auto_title: title.is_none(),
                    ..EngineConfig::default()
                },
            }),
            name: None,
            mode: "ask".into(),
            plan: false,
            default_effort: None,
            effort_schedule: None,
            goal: None,
            todos: Vec::new(),
            effort: None,
            previous_model: None,
            session_id,
            warnings: Vec::new(),
            title,
            model: None,
        })
    });
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, factory, server_info(), None));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"S1"}}),
    )
    .await;
    let open = next(&mut lines).await;
    assert_eq!(open["result"]["title"], "Fix the auth flow");
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new"}),
    )
    .await;
    let open = next(&mut lines).await;
    assert!(open["result"].get("title").is_none(), "{open}");
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"text":"go"}}),
    )
    .await;
    let titled = next_matching(&mut lines, |m| {
        m["params"]["update"]["type"] == "session_titled"
    })
    .await;
    assert_eq!(titled["params"]["update"]["title"], "Minted title");
    assert_eq!(titled["params"]["update"]["usage"]["input_tokens"], 10);
}

#[tokio::test]
async fn the_open_reply_names_the_previous_model() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        inherited_state_factory(None),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    assert_eq!(opened["result"]["previousModel"], "old-m", "{opened}");

    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    assert!(
        opened["result"].get("previousModel").is_none(),
        "no switch, no key: {opened}"
    );
}

async fn send(w: &mut (impl AsyncWriteExt + Unpin), v: Value) {
    let mut line = v.to_string();
    line.push('\n');
    w.write_all(line.as_bytes()).await.unwrap();
    w.flush().await.unwrap();
}

/// `initialize` advertises the roster so a front end can resolve
/// `/<skill>` without walking the config dirs itself.
#[tokio::test]
async fn initialize_advertises_skill_names_and_descriptions() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    let skills = vec![
        acp::SkillInfo {
            name: "brainstorming".into(),
            description: "turn an idea into a design".into(),
        },
        acp::SkillInfo {
            name: "acme:deploy".into(),
            description: String::new(),
        },
    ];
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        acp::ServerInfo {
            skills,
            workflows: vec![acp::WorkflowInfo {
                name: "review-changes".into(),
                description: "review then verify".into(),
            }],
            ..server_info()
        },
        None,
    ));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let init = next(&mut lines).await;
    assert_eq!(
        init["result"]["skills"],
        json!([
            {"name": "brainstorming", "description": "turn an idea into a design"},
            {"name": "acme:deploy", "description": ""},
        ])
    );
    // 0044: saved recipes ride beside skills, same shape, so `/<recipe>`
    // completes and dispatches from the handshake alone.
    assert_eq!(
        init["result"]["workflows"],
        json!([{"name": "review-changes", "description": "review then verify"}])
    );
}

/// `session/workflows` (0044): the `session/context` shape — ack on the id,
/// `workflows_report` as a broadcast, and an error with no session open.
#[tokio::test]
async fn session_workflows_returns_an_ack_and_broadcasts_a_report() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/workflows"}),
    )
    .await;
    let err = next(&mut lines).await;
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("requires an open session"),
        "{err}"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":{}}),
    )
    .await;
    next_matching(&mut lines, |m| m["id"] == json!(3)).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/workflows"}),
    )
    .await;
    let ack = next_matching(&mut lines, |m| m["id"] == json!(4)).await;
    assert_eq!(ack["result"], json!({"ok": true}));
    let note = next_matching(&mut lines, |m| {
        m["params"]["update"]["type"] == "workflows_report"
    })
    .await;
    assert!(
        note["params"]["update"]["runs"].is_array(),
        "runs is the list, empty in a fresh process: {note}"
    );
}

/// `session/context` (plan 0028): a thin ack on the id, the report itself as
/// a broadcast. The reply carries no payload on purpose — a correlated one
/// would need a third id queue in the TUI client's `translate`.
#[tokio::test]
async fn session_context_returns_an_ack_and_broadcasts_a_report() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}),
    )
    .await;
    next(&mut lines).await;

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/context","params":{}}),
    )
    .await;
    let ack = next_matching(&mut lines, |m| m["id"] == 3).await;
    assert_eq!(ack["result"], json!({"ok": true}));

    let report = next_matching(&mut lines, |m| {
        m["params"]["update"]["type"] == "context_report"
    })
    .await;
    let update = &report["params"]["update"];
    assert_eq!(report["params"]["schemaVersion"], 1);
    assert_eq!(update["window"], 200_000);
    let rows = update["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 12, "every row is emitted, zeros included");
    // The system prompt is the one row a brand-new session cannot have at 0.
    let system = rows
        .iter()
        .find(|r| r["kind"] == "system_prompt")
        .expect("system_prompt row");
    assert!(system["tokens"].as_u64().expect("u64") > 0);
}

/// 0071 T5: `session/aside` acks thinly; the engine broadcasts the answer.
#[tokio::test]
async fn session_aside_acks_then_broadcasts_the_answer() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    let factory: acp::SessionFactory = Box::new(move |_spec| {
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        let provider = Arc::new(ScriptedProvider::new(vec![ScriptedProvider::text_reply(
            "it returns a Result",
        )]));
        Ok(acp::SessionOpen {
            handle: spawn_session(SessionDeps {
                concurrency: Default::default(),
                provider,
                registry: Arc::new(Registry::builtin()),
                rules: Arc::new(Rules::default()),
                sandbox_enforced: false,
                clock: Arc::new(SystemClock),
                log,
                system: "sys".into(),
                cwd: std::env::temp_dir(),
                hooks: None,
                initial_items: Vec::new(),
                initial_todos: Vec::new(),
                initial_decisions: Vec::new(),
                plan_files: None,
                initial_goal: None,
                config: EngineConfig::default(),
            }),
            name: None,
            mode: "ask".into(),
            plan: false,
            default_effort: None,
            effort_schedule: None,
            goal: None,
            todos: Vec::new(),
            effort: None,
            previous_model: None,
            session_id,
            warnings: Vec::new(),
            title: None,
            model: None,
        })
    });
    tokio::spawn(acp::serve(sread, swrite, factory, server_info(), None));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"session/aside","params":{"question":"x"}}),
    )
    .await;
    assert!(
        next(&mut lines).await["error"].is_object(),
        "no session yet"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/aside","params":{"question":"  "}}),
    )
    .await;
    assert!(
        next(&mut lines).await["error"].is_object(),
        "an empty question"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":5,"method":"session/aside","params":{"question":"what does it return?"}}),
    )
    .await;
    let ack = next(&mut lines).await;
    assert_eq!(ack["result"]["ok"], true);
    let answer = next_matching(&mut lines, |m| {
        m["params"]["update"]["type"] == "aside_result"
    })
    .await;
    assert_eq!(
        answer["params"]["update"]["question"],
        "what does it return?"
    );
    assert_eq!(answer["params"]["update"]["text"], "it returns a Result");
    assert_eq!(answer["params"]["update"]["usage"]["input_tokens"], 10);
}

/// 0066: `session/fold` acks whether it ran now; a fresh session has
/// nothing to fold, which comes back as a `fold_skipped` update.
#[tokio::test]
async fn session_fold_at_idle_acks_now_and_relays_the_outcome() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/fold","params":{}}),
    )
    .await;
    let ack = next_matching(&mut lines, |m| m["id"] == 3).await;
    assert_eq!(ack["result"]["queued"], false, "{ack}");
    let skipped = next_matching(&mut lines, |m| {
        m["params"]["update"]["type"] == "fold_skipped"
    })
    .await;
    assert!(!skipped["params"]["update"]["reason"]
        .as_str()
        .unwrap()
        .is_empty());
    assert_eq!(
        skipped["params"]["schemaVersion"],
        acp::UPDATE_SCHEMA_VERSION
    );
}

#[tokio::test]
async fn session_fold_without_a_session_is_an_error() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"session/fold","params":{}}),
    )
    .await;
    let err = next(&mut lines).await;
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("session"));
}

#[tokio::test]
async fn session_fold_rejects_a_bad_focus() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/fold","params":{"focus":"x".repeat(4001)}}),
    )
    .await;
    let err = next_matching(&mut lines, |m| m["id"] == 3).await;
    assert!(
        err["error"]["message"].as_str().unwrap().contains("focus"),
        "{err}"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/fold","params":{"focus":7}}),
    )
    .await;
    let err = next_matching(&mut lines, |m| m["id"] == 4).await;
    assert!(err["error"].is_object(), "{err}");
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":5,"method":"session/fold","params":{"focus":"   "}}),
    )
    .await;
    let ack = next_matching(&mut lines, |m| m["id"] == 5).await;
    assert_eq!(ack["result"]["queued"], false, "blank = no focus: {ack}");
}

#[tokio::test]
async fn session_context_without_a_session_is_an_error() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/context","params":{}}),
    )
    .await;
    let reply = next_matching(&mut lines, |m| m["id"] == 2).await;
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("requires an open session"),
        "{reply}"
    );
}

/// The wire tags are `snake_case` and stay that way: they are a protocol
/// contract, and the TUI's display labels are a separate table.
#[tokio::test]
async fn the_context_report_kinds_are_snake_case() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{}}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/context","params":{}}),
    )
    .await;
    let report = next_matching(&mut lines, |m| {
        m["params"]["update"]["type"] == "context_report"
    })
    .await;
    for row in report["params"]["update"]["rows"].as_array().expect("rows") {
        let kind = row["kind"].as_str().expect("kind is a string");
        assert!(
            !kind.is_empty() && kind.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "kind `{kind}` is not snake_case"
        );
    }
}

#[tokio::test]
async fn initialize_new_prompt_permission_and_result() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    // 1. initialize → carries the stable schema version.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let init = next(&mut lines).await;
    assert_eq!(init["result"]["schemaVersion"], acp::UPDATE_SCHEMA_VERSION);

    // 2. session/new → a session id.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let new = next(&mut lines).await;
    let session_id = new["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    // 3. session/prompt → streams updates, requests permission, resolves.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"text":"go"}}),
    )
    .await;

    let mut saw_tool_start = false;
    let mut prompt_result: Option<Value> = None;
    // Read frames until the prompt (id 3) result arrives.
    while prompt_result.is_none() {
        let msg = next(&mut lines).await;
        match msg.get("method").and_then(Value::as_str) {
            Some("session/request_permission") => {
                // The bash call is gated → the server asks us. Approve it.
                assert_eq!(msg["params"]["sessionId"], session_id);
                let rid = msg["id"].clone();
                send(
                    &mut cwrite,
                    json!({"jsonrpc":"2.0","id":rid,"result":{"allow":true}}),
                )
                .await;
            }
            Some("session/update") => {
                assert_eq!(msg["params"]["schemaVersion"], acp::UPDATE_SCHEMA_VERSION);
                if msg["params"]["update"]["type"] == "tool_start" {
                    saw_tool_start = true;
                }
            }
            _ if msg.get("id") == Some(&json!(3)) => prompt_result = Some(msg),
            _ => {}
        }
    }

    let result = prompt_result.unwrap();
    assert_eq!(result["result"]["outcome"]["kind"], "done");
    assert_eq!(result["result"]["outcome"]["text"], "all done via acp");
    assert_eq!(
        result["result"]["schemaVersion"],
        acp::UPDATE_SCHEMA_VERSION
    );
    assert!(
        result["result"].get("usage").is_some(),
        "usage rides in the stable result"
    );
    assert!(saw_tool_start, "tool status streamed as an update");

    // 4. unknown method → JSON-RPC error, no crash.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":9,"method":"bogus/method"}),
    )
    .await;
    let err = read_until_id(&mut lines, 9).await;
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown method"));
}

/// Two prompts in flight: the engine queues the second, and each prompt
/// request is answered by its own turn's outcome, in order.
#[tokio::test]
async fn overlapping_prompts_resolve_in_order() {
    let factory: acp::SessionFactory = Box::new(|_spec| {
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::text_reply("first turn"),
            ScriptedProvider::text_reply("second turn"),
        ]));
        Ok(acp::SessionOpen {
            handle: spawn_session(SessionDeps {
                concurrency: Default::default(),
                provider,
                registry: Arc::new(Registry::builtin()),
                rules: Arc::new(Rules::default()),
                sandbox_enforced: false,
                clock: Arc::new(SystemClock),
                log,
                system: "sys".into(),
                cwd: std::env::temp_dir(),
                hooks: None,
                initial_items: Vec::new(),
                initial_todos: Vec::new(),
                initial_decisions: Vec::new(),
                plan_files: None,
                initial_goal: None,
                config: EngineConfig {
                    max_turns: 6,
                    ..Default::default()
                },
            }),
            name: None,
            mode: "ask".into(),
            plan: false,
            default_effort: None,
            effort_schedule: None,
            goal: None,
            todos: Vec::new(),
            effort: None,
            previous_model: None,
            session_id,
            warnings: Vec::new(),
            title: None,
            model: None,
        })
    });
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, factory, server_info(), None));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"session/new"}),
    )
    .await;
    read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"text":"a"}}),
    )
    .await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"text":"b"}}),
    )
    .await;

    let first = read_until_id(&mut lines, 2).await;
    assert_eq!(first["result"]["outcome"]["text"], "first turn");
    let second = read_until_id(&mut lines, 3).await;
    assert_eq!(second["result"]["outcome"]["text"], "second turn");
}

/// Replacing the session (session/new while one exists) aborts the old drain
/// and clears its parked state — the new session works end to end, and the
/// old in-flight prompt is never answered with the new session's outcome.
#[tokio::test]
async fn replacing_a_session_clears_parked_state() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"session/new"}),
    )
    .await;
    let first = read_until_id(&mut lines, 1).await;
    let first_sid = first["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();

    // Prompt; wait for the gated bash call's permission request — leave it parked.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"text":"go"}}),
    )
    .await;
    loop {
        let msg = next(&mut lines).await;
        if msg.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            break;
        }
    }

    // Replace the session while the ask is parked.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new"}),
    )
    .await;
    let second = read_until_id(&mut lines, 3).await;
    let second_sid = second["result"]["sessionId"]
        .as_str()
        .expect("session id")
        .to_string();
    assert_ne!(first_sid, second_sid);

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"text":"again"}}),
    )
    .await;
    let result = loop {
        let msg = next(&mut lines).await;
        assert_ne!(
            msg.get("id"),
            Some(&json!(2)),
            "stale prompt answered: {msg}"
        );
        if msg.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            assert_eq!(msg["params"]["sessionId"], second_sid);
            let rid = msg["id"].clone();
            send(
                &mut cwrite,
                json!({"jsonrpc":"2.0","id":rid,"result":{"allow":true}}),
            )
            .await;
        } else if msg.get("id") == Some(&json!(4)) {
            break msg;
        }
    };
    assert_eq!(result["result"]["outcome"]["kind"], "done");
    assert_eq!(result["result"]["outcome"]["text"], "all done via acp");
}

/// `session/steer` queues mid-turn feedback: acknowledged `{queued:true}`
/// with a session, an error without one.
#[tokio::test]
async fn steer_is_acknowledged_and_reaches_engine() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    // Steering with NO session is an error naming the missing session.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"session/steer","params":{"text":"go left"}}),
    )
    .await;
    let err = read_until_id(&mut lines, 1).await;
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("session"));

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    read_until_id(&mut lines, 2).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/steer","params":{"text":"go left"}}),
    )
    .await;
    let ack = read_until_id(&mut lines, 3).await;
    assert_eq!(ack["result"], json!({"queued": true}));

    // Missing params.text is an error too.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/steer"}),
    )
    .await;
    let err = read_until_id(&mut lines, 4).await;
    assert!(err["error"]["message"].as_str().unwrap().contains("text"));
}

/// Images ride `session/prompt`/`session/steer` params and are validated at
/// the wire — a poisoned payload is rejected before anything is committed —
/// and the open result advertises `images: true` for feature detection.
#[tokio::test]
async fn prompt_images_are_validated_at_the_wire() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    // A text-only script: no tool call, so the accepted prompt's turn
    // completes without this test also having to answer a permission ask.
    let factory: acp::SessionFactory = Box::new(move |spec| {
        let name = match spec {
            acp::SessionSpec::New { name } => name,
            acp::SessionSpec::Load { name, .. } | acp::SessionSpec::Fork { name, .. } => name,
        };
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        Ok(acp::SessionOpen {
            handle: spawn_session(SessionDeps {
                concurrency: Default::default(),
                provider: Arc::new(ScriptedProvider::new(vec![
                    ScriptedProvider::text_reply("saw it"),
                    ScriptedProvider::text_reply("saw that too"),
                ])),
                registry: Arc::new(Registry::builtin()),
                rules: Arc::new(Rules::default()),
                sandbox_enforced: false,
                clock: Arc::new(SystemClock),
                log,
                system: "sys".into(),
                cwd: std::env::temp_dir(),
                hooks: None,
                initial_items: Vec::new(),
                initial_todos: Vec::new(),
                initial_decisions: Vec::new(),
                plan_files: None,
                initial_goal: None,
                config: EngineConfig::default(),
            }),
            name,
            mode: "ask".into(),
            plan: false,
            default_effort: None,
            effort_schedule: None,
            goal: None,
            todos: Vec::new(),
            effort: None,
            previous_model: None,
            session_id,
            warnings: Vec::new(),
            title: None,
            model: None,
        })
    });
    tokio::spawn(acp::serve(sread, swrite, factory, server_info(), None));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"session/new"}),
    )
    .await;
    let opened = read_until_id(&mut lines, 1).await;
    assert_eq!(opened["result"]["images"], json!(true));

    // Bad base64 is rejected with a reason, before the log ever sees it.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{
            "text":"look: [Image #1]",
            "images":[{"media_type":"image/png","data":"not base64!!"}]}}),
    )
    .await;
    let err = read_until_id(&mut lines, 2).await;
    assert!(
        err["error"]["message"].as_str().unwrap().contains("base64"),
        "{err}"
    );

    // A valid image is accepted; the turn runs to completion.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{
            "text":"look: [Image #1]",
            "images":[{"media_type":"image/png","data":"aW1nMQ=="}]}}),
    )
    .await;
    let done = read_until_id(&mut lines, 3).await;
    assert!(done.get("result").is_some(), "{done}");

    // Steer takes the same shape and the same validation.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/steer","params":{
            "text":"and this",
            "images":[{"media_type":"image/bmp","data":"aW1n"}]}}),
    )
    .await;
    let err = read_until_id(&mut lines, 4).await;
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("media_type"),
        "{err}"
    );
}

async fn read_until_id(
    lines: &mut tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>,
    id: u64,
) -> Value {
    loop {
        let m = next(lines).await;
        if m.get("id") == Some(&json!(id)) {
            return m;
        }
    }
}

/// session/new carries a name back; session/rename acks and re-renames.
#[tokio::test]
async fn named_open_and_rename() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;

    // rename before a session exists → error.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/rename","params":{"name":"x"}}),
    )
    .await;
    assert!(
        next(&mut lines).await["error"].is_object(),
        "no session yet"
    );

    // open with a name (surrounding whitespace normalizes away).
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":{"name":"  fix-auth  "}}),
    )
    .await;
    let open = next(&mut lines).await;
    assert_eq!(open["result"]["name"], "fix-auth");

    // invalid rename → error; valid rename → ok.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/rename","params":{"name":"   "}}),
    )
    .await;
    assert!(
        next(&mut lines).await["error"].is_object(),
        "blank name rejected"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":5,"method":"session/rename","params":{"name":"better-name"}}),
    )
    .await;
    assert_eq!(next(&mut lines).await["result"]["ok"], true);
}

/// `ask_user` round-trips through `session/request_question`: the client
/// sees the header/prompt/options, answers with a selection, and the tool
/// result (fed back to the scripted model) carries the selected label. Also
/// covers the SECURITY invariant end to end: the question never touches
/// `session/request_permission` — only a plain `session/prompt` result.
#[tokio::test]
async fn ask_user_round_trip_via_session_request_question() {
    let factory: acp::SessionFactory = Box::new(|_spec| {
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        let (cmd_tx, cmd_rx) = hotl_engine::session_channel();
        let (event_tx, event_rx) = hotl_engine::event_channel();
        let mut registry = Registry::builtin();
        let notifications = hotl_engine::hooks::NotificationDrain::new();
        registry.register(Box::new(hotl_tools::AskUserTool::new(
            hotl_engine::question_sink(
                cmd_tx.downgrade(),
                event_tx.downgrade(),
                None,
                notifications.clone(),
            ),
        )));
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_call(
                "t1",
                "ask_user",
                json!({
                    "header": "Scope", "prompt": "How far?",
                    "options": [{"label": "MVP"}, {"label": "Full"}]
                }),
            ),
            ScriptedProvider::text_reply("all done via acp"),
        ]));
        Ok(acp::SessionOpen {
            handle: hotl_engine::spawn_session_with_channels(
                SessionDeps {
                    concurrency: Default::default(),
                    provider,
                    registry: Arc::new(registry),
                    rules: Arc::new(Rules::default()),
                    sandbox_enforced: false,
                    clock: Arc::new(SystemClock),
                    log,
                    system: "sys".into(),
                    cwd: std::env::temp_dir(),
                    hooks: None,
                    initial_items: Vec::new(),
                    initial_todos: Vec::new(),
                    initial_decisions: Vec::new(),
                    plan_files: None,
                    initial_goal: None,
                    config: EngineConfig {
                        max_turns: 6,
                        ..Default::default()
                    },
                },
                cmd_tx,
                cmd_rx,
                event_tx,
                event_rx,
                notifications,
            ),
            name: None,
            mode: "ask".into(),
            plan: false,
            default_effort: None,
            effort_schedule: None,
            goal: None,
            todos: Vec::new(),
            effort: None,
            previous_model: None,
            session_id,
            warnings: Vec::new(),
            title: None,
            model: None,
        })
    });
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, factory, server_info(), None));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"session/new"}),
    )
    .await;
    let session_id = read_until_id(&mut lines, 1)
        .await
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .expect("session id")
        .to_string();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"text":"go"}}),
    )
    .await;

    let result = loop {
        let msg = next(&mut lines).await;
        assert_ne!(
            msg.get("method").and_then(Value::as_str),
            Some("session/request_permission"),
            "ask_user must never route through the permission gate: {msg}"
        );
        if msg.get("method").and_then(Value::as_str) == Some("session/request_question") {
            assert_eq!(msg["params"]["sessionId"], session_id);
            assert_eq!(msg["params"]["header"], "Scope");
            assert_eq!(msg["params"]["prompt"], "How far?");
            assert_eq!(
                msg["params"]["options"],
                json!([{"label": "MVP"}, {"label": "Full"}])
            );
            let rid = msg["id"].clone();
            send(
                &mut cwrite,
                json!({"jsonrpc":"2.0","id":rid,"result":{"selected":["MVP"]}}),
            )
            .await;
        } else if msg.get("id") == Some(&json!(2)) {
            break msg;
        }
    };
    assert_eq!(result["result"]["outcome"]["kind"], "done");
    assert_eq!(result["result"]["outcome"]["text"], "all done via acp");
}

/// `session/set_mode` acks and switches the mode; an invalid mode errors
/// naming the valid ones. Mirrors `named_open_and_rename`.
#[tokio::test]
async fn set_mode_acks_and_rejects_unknown_modes() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;

    // set_mode before a session exists → error.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/set_mode","params":{"mode":"plan"}}),
    )
    .await;
    assert!(
        next(&mut lines).await["error"].is_object(),
        "no session yet"
    );

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new"}),
    )
    .await;
    next(&mut lines).await;

    // invalid mode → error naming the valid ones.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/set_mode","params":{"mode":"yolo"}}),
    )
    .await;
    let err = next(&mut lines).await;
    assert!(err["error"].is_object(), "invalid mode rejected");
    let message = err["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("ask"), "names valid modes: {message}");

    // valid mode → ok.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":5,"method":"session/set_mode","params":{"mode":"plan"}}),
    )
    .await;
    assert_eq!(next(&mut lines).await["result"]["ok"], true);
}

/// The permission mode is server-side truth. A client that renders a badge
/// must never have to guess it — the evaluation's §5.7 bug was a UI that
/// showed "ask" while the session ran "auto".
#[tokio::test]
async fn the_session_reports_its_effective_mode() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory_with_mode("auto"),
        acp::ServerInfo {
            skills: Vec::new(),
            workflows: Vec::new(),
            workflows_report: || json!({"runs": []}),
            default_mode: "auto".into(),
            default_plan: false,
            context_window: 1_000_000,
            stream_idle_secs: 90,
            model_resolver: None,
            model: "m".into(),
        },
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let hello = read_until_id(&mut lines, 1).await;
    assert_eq!(hello["result"]["defaultMode"], "auto");
    assert_eq!(hello["result"]["contextWindow"], 1_000_000);

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    assert_eq!(
        opened["result"]["mode"], "auto",
        "session/new must report the mode"
    );
    assert_eq!(
        opened["result"]["defaultEffort"], "xhigh",
        "session/new must report the resolved session default effort"
    );

    // A mode change is broadcast, not just acked — any attached surface updates.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_mode","params":{"mode":"dontask"}}),
    )
    .await;
    let mut saw_notification = false;
    for _ in 0..8 {
        let m = next(&mut lines).await;
        if m["method"] == "session/update" && m["params"]["update"]["type"] == "mode_changed" {
            assert_eq!(m["params"]["update"]["mode"], "dontask");
            saw_notification = true;
            break;
        }
        if m["id"] == json!(3) {
            assert_eq!(
                m["result"]["mode"], "dontask",
                "the ack carries the effective mode"
            );
        }
    }
    assert!(saw_notification, "set_mode must broadcast mode_changed");
}

/// Plan is its own axis on the wire: `session/set_plan` acks and broadcasts
/// `plan_changed`, and a client still sending the pre-split `set_mode {"mode":
/// "plan"}` is routed to it rather than told its mode word is invalid.
#[tokio::test]
async fn set_plan_is_its_own_axis_and_the_legacy_plan_mode_word_still_works() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let hello = read_until_id(&mut lines, 1).await;
    assert_eq!(hello["result"]["defaultPlan"], false);
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    assert_eq!(opened["result"]["plan"], false, "session/new reports plan");

    // The dedicated method.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_plan","params":{"plan":true}}),
    )
    .await;
    let mut saw = false;
    for _ in 0..8 {
        let m = next(&mut lines).await;
        if m["method"] == "session/update" && m["params"]["update"]["type"] == "plan_changed" {
            assert_eq!(m["params"]["update"]["plan"], true);
            saw = true;
            break;
        }
        if m["id"] == json!(3) {
            assert_eq!(m["result"]["plan"], true, "the ack carries the plan state");
        }
    }
    assert!(saw, "set_plan must broadcast plan_changed");

    // The legacy spelling lands on the same axis, not on an error.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/set_mode","params":{"mode":"plan"}}),
    )
    .await;
    let mut saw_legacy = false;
    for _ in 0..8 {
        let m = next(&mut lines).await;
        if m["id"] == json!(4) {
            assert!(
                m.get("error").is_none(),
                "a pre-split client must not get an error: {m}"
            );
            assert_eq!(m["result"]["plan"], true);
        }
        if m["method"] == "session/update" && m["params"]["update"]["type"] == "plan_changed" {
            saw_legacy = true;
            break;
        }
    }
    assert!(
        saw_legacy,
        "legacy set_mode(plan) must broadcast plan_changed"
    );
}

/// `session/set_effort` acks with the effective level, broadcasts
/// `effort_changed`, round-trips the cleared case as JSON `null`, and rejects
/// a rung it does not carry.
/// 0067: a resolver that never touches a key — `provider/model` becomes a
/// same-dialect switch; `openai/…` a cross-dialect one with a scripted
/// provider; `nope/…` the startup error text.
fn test_resolver() -> acp::ModelResolver {
    Arc::new(|raw: &str| {
        let raw = raw.trim();
        let (provider, model) = match raw.split_once('/') {
            Some((p, m)) => (p.to_string(), m.to_string()),
            None => ("anthropic".to_string(), raw.to_string()),
        };
        let provider_obj: Option<Arc<dyn hotl_provider::Provider>> = match provider.as_str() {
            "anthropic" => None,
            "openai" => Some(Arc::new(ScriptedProvider::new(vec![
                ScriptedProvider::text_reply("from openai"),
            ]))),
            other => {
                return Err(format!(
                    "unknown provider `{other}`. Supported: anthropic/<model>, openai/<model>, \
                     openai-responses/<model>"
                ))
            }
        };
        Ok((
            hotl_engine::ModelSwitch {
                spec: format!("{provider}/{model}"),
                model,
                provider: provider_obj,
                profile: hotl_engine::ModelProfile {
                    context_window: 200_000,
                    max_tokens: 8_000,
                    token_profile: hotl_context::TokenProfile::CONSERVATIVE,
                },
            },
            Vec::new(),
        ))
    })
}

fn server_info_with_resolver() -> acp::ServerInfo {
    acp::ServerInfo {
        model_resolver: Some(test_resolver()),
        ..server_info()
    }
}

#[tokio::test]
async fn set_model_acks_applied_and_the_engine_broadcasts_model_changed() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info_with_resolver(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let _ = read_until_id(&mut lines, 2).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_model","params":{"model":"anthropic/other"}}),
    )
    .await;
    // Applied at once: the engine's broadcast and the ack arrive in either
    // order, so collect both.
    let (mut ack, mut changed) = (None, None);
    for _ in 0..8 {
        if ack.is_some() && changed.is_some() {
            break;
        }
        let m = next(&mut lines).await;
        if m["id"] == json!(3) {
            ack = Some(m);
        } else if m["params"]["update"]["type"] == "model_changed" {
            changed = Some(m);
        }
    }
    let ack = ack.expect("the ack");
    assert_eq!(ack["result"]["applied"], true, "{ack}");
    assert_eq!(ack["result"]["model"], "anthropic/other");
    let changed = changed.expect("the broadcast");
    assert_eq!(changed["params"]["update"]["model"], "anthropic/other");
    assert_eq!(changed["params"]["update"]["context_window"], 200_000);
    assert!(changed["params"]["update"]["context_tokens"].is_number());
}

#[tokio::test]
async fn set_model_during_a_turn_acks_queued_then_broadcasts_at_the_boundary() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info_with_resolver(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let _ = read_until_id(&mut lines, 2).await;
    // The scripted turn asks for permission and waits: a turn is running.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"text":"go"}}),
    )
    .await;
    let ask = next_matching(&mut lines, |m| m["method"] == "session/request_permission").await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/set_model","params":{"model":"anthropic/other"}}),
    )
    .await;
    let ack = read_until_id(&mut lines, 4).await;
    assert_eq!(ack["result"]["applied"], false, "parked: {ack}");
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id": ask["id"],"result":{"allow": true}}),
    )
    .await;
    let changed = next_matching(&mut lines, |m| {
        m["params"]["update"]["type"] == "model_changed"
    })
    .await;
    assert_eq!(changed["params"]["update"]["model"], "anthropic/other");
}

#[tokio::test]
async fn set_model_rejects_an_unknown_provider_and_changes_nothing() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info_with_resolver(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let _ = read_until_id(&mut lines, 2).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_model","params":{"model":"nope/x"}}),
    )
    .await;
    let err = read_until_id(&mut lines, 3).await;
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown provider"),
        "{err}"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/set_model","params":{}}),
    )
    .await;
    let err = read_until_id(&mut lines, 4).await;
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("params.model"),
        "{err}"
    );
}

#[tokio::test]
async fn set_model_without_a_resolver_is_an_error_not_a_silent_no_op() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let _ = read_until_id(&mut lines, 2).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_model","params":{"model":"anthropic/other"}}),
    )
    .await;
    let err = read_until_id(&mut lines, 3).await;
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not supported"),
        "{err}"
    );
}

/// 0067: the open reply's `model` is the model the session runs — a
/// resumed switch when the factory restored one, else the process default.
#[tokio::test]
async fn a_resumed_session_runs_its_last_model_set() {
    let factory: acp::SessionFactory = Box::new(move |spec| {
        let switched = matches!(spec, acp::SessionSpec::Load { .. });
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        Ok(acp::SessionOpen {
            handle: spawn_session(SessionDeps {
                concurrency: Default::default(),
                provider: Arc::new(ScriptedProvider::new(vec![])),
                registry: Arc::new(Registry::builtin()),
                rules: Arc::new(Rules::default()),
                sandbox_enforced: false,
                clock: Arc::new(SystemClock),
                log,
                system: "sys".into(),
                cwd: std::env::temp_dir(),
                hooks: None,
                initial_items: Vec::new(),
                initial_todos: Vec::new(),
                initial_decisions: Vec::new(),
                plan_files: None,
                initial_goal: None,
                config: EngineConfig::default(),
            }),
            name: None,
            mode: "ask".into(),
            plan: false,
            default_effort: None,
            effort_schedule: None,
            goal: None,
            todos: Vec::new(),
            effort: None,
            previous_model: None,
            session_id,
            warnings: Vec::new(),
            title: None,
            model: switched.then(|| "openai-responses/gpt-5.6".to_string()),
        })
    });
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, factory, server_info(), None));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"S1"}}),
    )
    .await;
    let open = read_until_id(&mut lines, 2).await;
    assert_eq!(
        open["result"]["model"], "openai-responses/gpt-5.6",
        "{open}"
    );
    assert!(open["result"].get("previousModel").is_none());
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new"}),
    )
    .await;
    let open = read_until_id(&mut lines, 3).await;
    assert_eq!(open["result"]["model"], server_info().model, "{open}");
}

#[tokio::test]
async fn set_effort_acks_broadcasts_and_rejects_unknown_levels() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let _ = read_until_id(&mut lines, 2).await;

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_effort","params":{"effort":"xhigh"}}),
    )
    .await;
    let mut saw = false;
    for _ in 0..8 {
        let m = next(&mut lines).await;
        if m["method"] == "session/update" && m["params"]["update"]["type"] == "effort_changed" {
            assert_eq!(m["params"]["update"]["effort"], "xhigh");
            saw = true;
            break;
        }
        if m["id"] == json!(3) {
            assert_eq!(m["result"]["effort"], "xhigh", "the ack carries the level");
        }
    }
    assert!(saw, "set_effort must broadcast effort_changed");

    // Clearing is its own act, and reaches the wire as an explicit null.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/set_effort","params":{"effort":"default"}}),
    )
    .await;
    let mut saw_clear = false;
    for _ in 0..8 {
        let m = next(&mut lines).await;
        if m["method"] == "session/update" && m["params"]["update"]["type"] == "effort_changed" {
            assert_eq!(m["params"]["update"]["effort"], Value::Null);
            saw_clear = true;
            break;
        }
        if m["id"] == json!(4) {
            assert_eq!(m["result"]["effort"], Value::Null);
        }
    }
    assert!(
        saw_clear,
        "clearing must broadcast effort_changed with null"
    );

    // `ultra` is another harness's orchestration opt-in, not a rung here.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":5,"method":"session/set_effort","params":{"effort":"ultra"}}),
    )
    .await;
    let m = read_until_id(&mut lines, 5).await;
    assert!(m.get("error").is_some(), "an unknown rung must error: {m}");
}

/// `session/set_goal` acks with the normalized goal; the change reaches every
/// attached surface as `goal_changed` through the drain's catch-all (the
/// engine's own `GoalChanged` event — there is no handler-side notify to
/// drift from it). `null` clears, and a non-string / overlong param errors.
#[tokio::test]
async fn set_goal_acks_relays_the_engine_broadcast_and_rejects_invalid_params() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    assert_eq!(
        opened["result"]["goal"],
        Value::Null,
        "a fresh session opens with no goal"
    );

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_goal",
               "params":{"goal":"  all tests pass  "}}),
    )
    .await;
    let mut saw = false;
    for _ in 0..8 {
        let m = next(&mut lines).await;
        if m["method"] == "session/update" && m["params"]["update"]["type"] == "goal_changed" {
            assert_eq!(m["params"]["update"]["goal"], "all tests pass");
            saw = true;
            break;
        }
        if m["id"] == json!(3) {
            assert_eq!(
                m["result"]["goal"], "all tests pass",
                "the ack carries the normalized goal"
            );
        }
    }
    assert!(saw, "a set goal must reach the wire as goal_changed");

    // `null` clears, and the engine's clear is broadcast the same way.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/set_goal","params":{"goal":null}}),
    )
    .await;
    let mut saw_clear = false;
    for _ in 0..8 {
        let m = next(&mut lines).await;
        if m["method"] == "session/update" && m["params"]["update"]["type"] == "goal_changed" {
            assert_eq!(m["params"]["update"]["goal"], Value::Null);
            saw_clear = true;
            break;
        }
        if m["id"] == json!(4) {
            assert_eq!(m["result"]["goal"], Value::Null);
        }
    }
    assert!(saw_clear, "clearing must broadcast goal_changed with null");

    // Overlong and non-string params are usage errors, not silent trims.
    let overlong = "x".repeat(4001);
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":5,"method":"session/set_goal","params":{"goal":overlong}}),
    )
    .await;
    let m = read_until_id(&mut lines, 5).await;
    assert!(m.get("error").is_some(), "overlong must error: {m}");
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":6,"method":"session/set_goal","params":{"goal":42}}),
    )
    .await;
    let m = read_until_id(&mut lines, 6).await;
    assert!(m.get("error").is_some(), "a non-string must error: {m}");
}

/// The open reply's `goal` key is the resume handshake: a factory restoring a
/// goal on `session/load` reaches the client synchronously, and a fork's open
/// carries `null` (forks never inherit the goal — 0034 decision 6).
#[tokio::test]
async fn the_open_reply_carries_the_resumed_goal_and_a_forks_is_null() {
    // A factory shaped like the real one: Load restores the chain's goal,
    // Fork (and New) never carry one.
    let factory: acp::SessionFactory = Box::new(move |spec| {
        let goal = match &spec {
            acp::SessionSpec::Load { .. } => Some("all tests pass".to_string()),
            acp::SessionSpec::New { .. } | acp::SessionSpec::Fork { .. } => None,
        };
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        Ok(acp::SessionOpen {
            handle: spawn_session(SessionDeps {
                concurrency: Default::default(),
                provider: Arc::new(ScriptedProvider::new(vec![])),
                registry: Arc::new(Registry::builtin()),
                rules: Arc::new(Rules::default()),
                sandbox_enforced: false,
                clock: Arc::new(SystemClock),
                log,
                system: "sys".into(),
                cwd: std::env::temp_dir(),
                hooks: None,
                initial_items: vec![hotl_types::Item::Assistant {
                    blocks: vec![json!({"type":"text","text":"done"})],
                    model: None,
                }],
                initial_todos: Vec::new(),
                initial_decisions: Vec::new(),
                plan_files: None,
                initial_goal: goal.clone(),
                config: EngineConfig::default(),
            }),
            name: None,
            mode: "ask".to_string(),
            plan: false,
            default_effort: None,
            effort_schedule: None,
            goal,
            todos: Vec::new(),
            effort: None,
            previous_model: None,
            session_id,
            warnings: Vec::new(),
            title: None,
            model: None,
        })
    });
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, factory, server_info(), None));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/load","params":{"sessionId":"S1"}}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    assert_eq!(
        opened["result"]["goal"], "all tests pass",
        "resume must hand the restored goal to the client synchronously"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/load",
               "params":{"sessionId":"S1","fork":true}}),
    )
    .await;
    let forked = read_until_id(&mut lines, 3).await;
    assert_eq!(
        forked["result"]["goal"],
        Value::Null,
        "a fork never inherits the goal"
    );
}

async fn next(lines: &mut tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>) -> Value {
    let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .expect("acp frame timeout")
        .expect("io")
        .expect("eof");
    serde_json::from_str(&line).expect("valid json frame")
}

/// Read frames until `want` says one is the interesting one, or give up. The
/// reload path interleaves an ack, a broadcast and whatever the replacement
/// session's drain emits, and no test should depend on that order.
async fn next_matching(
    lines: &mut tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>,
    want: impl Fn(&Value) -> bool,
) -> Value {
    for _ in 0..12 {
        let m = next(lines).await;
        if want(&m) {
            return m;
        }
    }
    panic!("no matching frame in 12 tries");
}

/// A rebuild that succeeds, handing back a second scripted factory plus the
/// `ServerInfo` a freshly-read config would have produced. `loads` records the
/// `session_id` the replacement factory is asked to resume.
fn ok_reload(
    info: acp::ServerInfo,
    warnings: Vec<String>,
    loads: Option<Arc<std::sync::Mutex<Vec<String>>>>,
) -> acp::Reload {
    Box::new(move || {
        let (info, warnings, loads) = (info.clone(), warnings.clone(), loads.clone());
        Box::pin(async move { Ok((scripted_factory_recording("plan", loads), info, warnings)) })
    })
}

fn failing_reload(reason: &'static str) -> acp::Reload {
    Box::new(move || Box::pin(async move { Err(reason.to_string()) }))
}

/// `/reload`'s engine half: the factory and the advertised roster are replaced
/// and the live session is re-opened onto them, then every attached surface is
/// told — the ack alone would leave a second client rendering a stale badge.
#[tokio::test]
async fn reload_config_swaps_the_engine_and_broadcasts_the_new_truth() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    let reloaded = acp::ServerInfo {
        skills: vec![acp::SkillInfo {
            name: "run".into(),
            description: "launch the app".into(),
        }],
        workflows: vec![acp::WorkflowInfo {
            name: "review-changes".into(),
            description: "review then verify".into(),
        }],
        workflows_report: || json!({"runs": []}),
        default_mode: "auto".into(),
        default_plan: false,
        context_window: 900_000,
        stream_idle_secs: 90,
        model_resolver: None,
        model: "m2".into(),
    };
    // What `SessionSpec::Load` the replacement factory is handed, so this test
    // can prove the resume names the *store* session id.
    let loads: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        Some(ok_reload(
            reloaded,
            vec!["[network] egress unenforced".into()],
            Some(loads.clone()),
        )),
    ));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let opened = next(&mut lines).await;
    let first_session = opened["result"]["sessionId"].clone();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/reload_config"}),
    )
    .await;

    let ack = next_matching(&mut lines, |m| m["id"] == json!(3)).await;
    assert_eq!(ack["result"]["ok"], json!(true), "{ack}");
    assert_eq!(
        ack["result"]["model"], "m2",
        "the ack carries the new model"
    );
    assert_eq!(ack["result"]["contextWindow"], 900_000);
    assert_ne!(
        ack["result"]["sessionId"], first_session,
        "the session was re-opened, so it is a new link in the log chain"
    );

    // REGRESSION: the resume must name the **store** session id, not this
    // connection's `acp-N` handle. Resuming through the handle looks fine here
    // — a scripted factory ignores the id — but against the real factory it
    // fails `replay_chain` and the re-open dies with "could not load session
    // acp-1", leaving the connection with no session at all.
    let loaded = loads.lock().unwrap().clone();
    assert_eq!(loaded.len(), 1, "the reload re-opens exactly once");
    assert!(
        !loaded[0].starts_with("acp-"),
        "reload resumed through the connection handle `{}` instead of the store id",
        loaded[0]
    );

    let note = next_matching(&mut lines, |m| {
        m["params"]["update"]["type"] == "config_reloaded"
    })
    .await;
    let u = &note["params"]["update"];
    assert_eq!(u["model"], "m2");
    assert_eq!(u["mode"], "plan", "the re-opened session's own mode");
    assert_eq!(u["context_window"], 900_000);
    assert_eq!(
        u["skills"],
        json!([{"name": "run", "description": "launch the app"}]),
        "the client re-seeds its `/`-completion from this"
    );
    assert_eq!(
        u["workflows"],
        json!([{"name": "review-changes", "description": "review then verify"}]),
        "and its workflow roster (0044)"
    );
    assert_eq!(u["warnings"], json!(["[network] egress unenforced"]));

    // The swap is real, not just advertised: `initialize` now answers with
    // the reloaded roster too.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"initialize"}),
    )
    .await;
    let init = next_matching(&mut lines, |m| m["id"] == json!(4)).await;
    assert_eq!(init["result"]["contextWindow"], 900_000);
    assert_eq!(init["result"]["defaultMode"], "auto");
}

/// A `config.toml` with a typo must not cost you the session: the running
/// engine is left exactly as it was, and both the ack and the broadcast say so.
#[tokio::test]
async fn a_failed_reload_keeps_the_running_session_answering() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        Some(failing_reload("TOML parse error at line 3")),
    ));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new"}),
    )
    .await;
    let opened = next(&mut lines).await;
    let session_id = opened["result"]["sessionId"].clone();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/reload_config"}),
    )
    .await;
    let note = next_matching(&mut lines, |m| {
        m["params"]["update"]["type"] == "config_reload_failed"
    })
    .await;
    assert!(note["params"]["update"]["reason"]
        .as_str()
        .unwrap()
        .contains("TOML parse error"));
    let err = next_matching(&mut lines, |m| m["id"] == json!(3)).await;
    assert!(err["error"]["message"]
        .as_str()
        .unwrap()
        .contains("config reload failed"));

    // Still the same session, and still alive: the scripted turn reaches its
    // gated `bash` call, which only the original session's drain can raise.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/prompt","params":{"text":"go"}}),
    )
    .await;
    let ask = next_matching(&mut lines, |m| m["method"] == "session/request_permission").await;
    assert_eq!(
        ask["params"]["sessionId"], session_id,
        "the pre-reload session is the one still running"
    );
}

/// `serve` (no hook) is the `hotl acp`-over-stdio shape a host may embed
/// without granting a config rebuild. It must say so rather than silently
/// no-op — a client that read "ok" would show a reload that never happened.
#[tokio::test]
async fn reload_config_without_a_hook_is_an_error_not_a_silent_no_op() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));

    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"session/reload_config"}),
    )
    .await;
    let m = next(&mut lines).await;
    assert!(
        m["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not supported"),
        "{m}"
    );
}
