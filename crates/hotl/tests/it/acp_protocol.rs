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

use crate::{acp, acp_types};

/// A session whose scripted model calls bash (a gated tool → a permission
/// ask) then replies with text.
fn scripted_factory() -> acp::SessionFactory {
    scripted_factory_with_mode("ask")
}

fn new_params() -> Value {
    json!({"cwd": std::env::temp_dir(), "mcpServers": []})
}

fn init_params() -> Value {
    json!({"protocolVersion": 1, "clientCapabilities": {"_meta": {"hotl.dev/extensions": 1}}})
}

fn open_of(reply: &Value) -> &Value {
    &reply["result"]["_meta"]["hotl.dev/open"]
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
        key_available: true,
        sessions_dir: None,
        fallback_models: Vec::new(),
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
            acp::SessionSpec::New { name, .. } => name,
            acp::SessionSpec::Load {
                name, session_id, ..
            } => {
                if let Some(loads) = &loads {
                    loads.lock().unwrap().push(session_id);
                }
                name
            }
            acp::SessionSpec::Fork {
                name,
                session_id,
                keep,
                ..
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
            history: Vec::new(),
            memory_mode: None,
            worktree: None,
            lease: None,
            color: None,
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
            history: Vec::new(),
            memory_mode: None,
            worktree: None,
            lease: None,
            color: None,
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"S1","cwd":std::env::temp_dir()}}),
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"_hotl/session/fork",
               "params":{"sessionId":"S1","keepTurns":2}}),
    )
    .await;
    let opened = next_matching(&mut lines, |m| m["id"] == json!(2)).await;
    assert!(opened["result"]["sessionId"].as_str().is_some(), "{opened}");
    assert_eq!(
        seen.lock().unwrap().clone(),
        ["fork:S1:Turns(2)"],
        "_hotl/session/fork with `keepTurns` reaches the factory as a Fork spec"
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
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
        json!({"jsonrpc":"2.0","id":3,"method":"_hotl/session/fork",
               "params":{"sessionId":"S1","keepItems":4,"keepTurns":2}}),
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
            history: Vec::new(),
            memory_mode: None,
            worktree: None,
            lease: None,
            color: Some("pink".into()),
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    let r = open_of(&opened);
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
    assert_eq!(r["color"], "pink", "a resumed /color rides the open reply");

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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    let r = open_of(&opened);
    assert!(
        r.get("effort").is_none(),
        "never-set must stay absent, not null — collapsing the tri-state \
         re-creates the effort lie: {opened}"
    );
    assert_eq!(r["todos"], json!([]), "{opened}");
    assert!(r.get("color").is_none(), "no color: key absent: {opened}");
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    assert_eq!(
        open_of(&opened).get("effort"),
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
            history: Vec::new(),
            memory_mode: None,
            worktree: None,
            lease: None,
            color: None,
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"S1","cwd":std::env::temp_dir()}}),
    )
    .await;
    let open = next(&mut lines).await;
    assert_eq!(open_of(&open)["title"], "Fix the auth flow");
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":new_params()}),
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    assert_eq!(open_of(&opened)["previousModel"], "old-m", "{opened}");

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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"_meta":{"hotl.dev/extensions":1}}}}),
    )
    .await;
    let init = next(&mut lines).await;
    assert_eq!(
        init["result"]["agentCapabilities"]["_meta"]["hotl.dev/init"]["skills"],
        json!([
            {"name": "brainstorming", "description": "turn an idea into a design"},
            {"name": "acme:deploy", "description": ""},
        ])
    );
    // 0044: saved recipes ride beside skills, same shape, so `/<recipe>`
    // completes and dispatches from the handshake alone.
    assert_eq!(
        init["result"]["agentCapabilities"]["_meta"]["hotl.dev/init"]["workflows"],
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
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
        json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":new_params()}),
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
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
            history: Vec::new(),
            memory_mode: None,
            worktree: None,
            lease: None,
            color: None,
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
        json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":new_params()}),
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
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

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
    )
    .await;
    assert_eq!(next(&mut lines).await["result"]["protocolVersion"], 1);

    // 2. session/new → a session id.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
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
            history: Vec::new(),
            memory_mode: None,
            worktree: None,
            lease: None,
            color: None,
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
        json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":new_params()}),
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
        json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":new_params()}),
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
        json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":new_params()}),
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
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
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
            acp::SessionSpec::New { name, .. } => name,
            acp::SessionSpec::Load { name, .. } | acp::SessionSpec::Fork { name, .. } => name,
        };
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        Ok(acp::SessionOpen {
            history: Vec::new(),
            memory_mode: None,
            worktree: None,
            lease: None,
            color: None,
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
        json!({"jsonrpc":"2.0","id":100,"method":"initialize","params":init_params()}),
    )
    .await;
    read_until_id(&mut lines, 100).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":new_params()}),
    )
    .await;
    let opened = read_until_id(&mut lines, 1).await;
    assert_eq!(open_of(&opened)["images"], json!(true));

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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
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
        json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":{"cwd": std::env::temp_dir(), "mcpServers": [], "_meta": {"hotl.dev/open": {"name":"  fix-auth  "}}}}),
    )
    .await;
    let open = next(&mut lines).await;
    assert_eq!(open_of(&open)["name"], "fix-auth");

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

#[tokio::test]
async fn set_color_acks_a_known_name_or_null_and_refuses_the_rest() {
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/set_color","params":{"color":"red"}}),
    )
    .await;
    assert!(
        read_until_id(&mut lines, 2).await["error"].is_object(),
        "no session yet"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":new_params()}),
    )
    .await;
    read_until_id(&mut lines, 3).await;
    for (id, color) in [(4, json!("Pink")), (5, json!(null))] {
        send(
            &mut cwrite,
            json!({"jsonrpc":"2.0","id":id,"method":"session/set_color","params":{"color":color}}),
        )
        .await;
        let r = read_until_id(&mut lines, id).await;
        assert_eq!(r["result"]["ok"], true, "{color}: {r}");
    }
    for (id, params) in [
        (6, json!({"color":"magenta"})),
        (7, json!({"color":"default"})),
        (8, json!({"color":3})),
        (9, json!({})),
    ] {
        send(
            &mut cwrite,
            json!({"jsonrpc":"2.0","id":id,"method":"session/set_color","params":params}),
        )
        .await;
        let r = read_until_id(&mut lines, id).await;
        assert!(r["error"].is_object(), "{params}: {r}");
    }
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
            history: Vec::new(),
            memory_mode: None,
            worktree: None,
            lease: None,
            color: None,
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
        json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":new_params()}),
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

#[tokio::test]
async fn set_mode_emits_current_mode_update() {
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    assert_eq!(opened["result"]["modes"]["currentModeId"], "ask");
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_mode","params":{"sessionId":"s","modeId":"dontask"}}),
    )
    .await;
    let reply = read_until_id(&mut lines, 3).await;
    assert_eq!(reply["result"], json!({}));
    let update = next_standard(&mut lines, "current_mode_update").await;
    assert_eq!(update["params"]["update"]["currentModeId"], "dontask");
}

#[tokio::test]
async fn set_mode_rejects_unknown_modes_and_the_old_mode_param() {
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 2).await;
    for (id, params) in [
        (3, json!({"modeId": "yolo"})),
        (4, json!({"mode": "ask"})),
        (5, json!({"modeId": "plan"})),
    ] {
        send(
            &mut cwrite,
            json!({"jsonrpc":"2.0","id":id,"method":"session/set_mode","params":params}),
        )
        .await;
        let err = read_until_id(&mut lines, id).await;
        assert_eq!(err["error"]["code"], -32602, "{err}");
    }
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":6,"method":"session/set_mode","params":{"mode":"ask"}}),
    )
    .await;
    let err = read_until_id(&mut lines, 6).await;
    assert!(err["error"]["message"].as_str().unwrap().contains("modeId"));
}

#[tokio::test]
async fn the_old_set_calls_are_gone() {
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 2).await;
    for (id, method) in [
        (3, "session/set_model"),
        (4, "session/set_effort"),
        (5, "session/set_plan"),
    ] {
        send(
            &mut cwrite,
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":{}}),
        )
        .await;
        let err = read_until_id(&mut lines, id).await;
        assert_eq!(err["error"]["code"], -32601, "{method}: {err}");
    }
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
            key_available: true,
            sessions_dir: None,
            fallback_models: Vec::new(),
            model: "m".into(),
        },
        None,
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"_meta":{"hotl.dev/extensions":1}}}}),
    )
    .await;
    let hello = read_until_id(&mut lines, 1).await;
    assert_eq!(
        hello["result"]["agentCapabilities"]["_meta"]["hotl.dev/init"]["defaultMode"],
        "auto"
    );
    assert_eq!(
        hello["result"]["agentCapabilities"]["_meta"]["hotl.dev/init"]["contextWindow"],
        1_000_000
    );

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    assert_eq!(
        opened["result"]["modes"]["currentModeId"], "auto",
        "session/new must report the mode"
    );
    assert_eq!(
        open_of(&opened)["defaultEffort"],
        "xhigh",
        "session/new must report the resolved session default effort"
    );

    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_mode","params":{"modeId":"dontask"}}),
    )
    .await;
    let m = next_standard(&mut lines, "current_mode_update").await;
    assert_eq!(m["params"]["update"]["currentModeId"], "dontask");
}

#[tokio::test]
async fn plan_is_a_boolean_option() {
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    let options = &opened["result"]["configOptions"];
    let plan = options
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "plan")
        .unwrap();
    assert_eq!(plan["type"], "boolean");
    assert_eq!(plan["currentValue"], false);
    assert_eq!(open_of(&opened)["plan"], false);
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_config_option",
               "params":{"sessionId":"s","configId":"plan","type":"boolean","value":true}}),
    )
    .await;
    let reply = read_until_id(&mut lines, 3).await;
    let plan = reply["result"]["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "plan")
        .unwrap();
    assert_eq!(plan["currentValue"], true);
    let update = next_standard(&mut lines, "config_option_update").await;
    assert_eq!(
        update["params"]["update"]["configOptions"],
        reply["result"]["configOptions"]
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/set_config_option",
               "params":{"sessionId":"s","configId":"plan","value":"yes"}}),
    )
    .await;
    let err = read_until_id(&mut lines, 4).await;
    assert_eq!(err["error"]["code"], -32602, "{err}");
}

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
        key_available: true,
        sessions_dir: None,
        fallback_models: Vec::new(),
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 2).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_config_option","params":{"configId":"model","value":"anthropic/other"}}),
    )
    .await;
    let (mut ack, mut changed) = (None, None);
    while ack.is_none() || changed.is_none() {
        let m = next_raw(&mut lines).await;
        if m["id"] == json!(3) {
            ack = Some(m);
        } else if m["params"]["update"]["sessionUpdate"] == "config_option_update" {
            changed = Some(m);
        }
    }
    let (ack, changed) = (ack.unwrap(), changed.unwrap());
    let meta = &ack["result"]["_meta"]["hotl.dev/model"];
    assert_eq!(meta["applied"], true, "{ack}");
    assert_eq!(meta["model"], "anthropic/other");
    let change = &changed["params"]["update"]["_meta"]["hotl.dev/model"];
    assert_eq!(change["model"], "anthropic/other");
    assert_eq!(change["contextWindow"], 200_000);
    assert!(change["contextTokens"].is_number());
    assert_eq!(
        changed["params"]["update"]["configOptions"][0]["currentValue"],
        "anthropic/other"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/set_config_option","params":{"configId":"plan","value":true}}),
    )
    .await;
    loop {
        let m = next_raw(&mut lines).await;
        assert_ne!(m["params"]["update"]["type"], "model_changed", "{m}");
        assert_ne!(
            m["params"]["update"]["sessionUpdate"], "config_option_update",
            "one update per model change: {m}"
        );
        if m["id"] == json!(4) {
            break;
        }
    }
}

#[tokio::test]
async fn the_model_option_lists_the_configured_models() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    let info = acp::ServerInfo {
        fallback_models: vec!["anthropic/x".into(), "m".into()],
        ..server_info()
    };
    tokio::spawn(acp::serve(sread, swrite, scripted_factory(), info, None));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    let model = &opened["result"]["configOptions"][0];
    assert_eq!(model["currentValue"], "m");
    let values: Vec<&str> = model["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["value"].as_str().unwrap())
        .collect();
    assert_eq!(values, ["m", "anthropic/x"]);
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
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
        json!({"jsonrpc":"2.0","id":4,"method":"session/set_config_option","params":{"configId":"model","value":"anthropic/other"}}),
    )
    .await;
    let ack = read_until_id(&mut lines, 4).await;
    assert_eq!(
        ack["result"]["_meta"]["hotl.dev/model"]["applied"], false,
        "parked: {ack}"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id": ask["id"],"result":{"allow": true}}),
    )
    .await;
    let changed = next_standard(&mut lines, "config_option_update").await;
    assert_eq!(
        changed["params"]["update"]["_meta"]["hotl.dev/model"]["model"],
        "anthropic/other"
    );
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 2).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_config_option","params":{"configId":"model","value":"nope/x"}}),
    )
    .await;
    let err = read_until_id(&mut lines, 3).await;
    assert_eq!(err["error"]["code"], -32602);
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown provider"),
        "{err}"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/set_config_option","params":{"configId":"model"}}),
    )
    .await;
    let err = read_until_id(&mut lines, 4).await;
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("params.value"),
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 2).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_config_option","params":{"configId":"model","value":"anthropic/other"}}),
    )
    .await;
    let err = read_until_id(&mut lines, 3).await;
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot change"),
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
            history: Vec::new(),
            memory_mode: None,
            worktree: None,
            lease: None,
            color: None,
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"S1","cwd":std::env::temp_dir()}}),
    )
    .await;
    let open = read_until_id(&mut lines, 2).await;
    assert_eq!(
        open_of(&open)["model"],
        "openai-responses/gpt-5.6",
        "{open}"
    );
    assert!(open["result"].get("previousModel").is_none());
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/new","params":new_params()}),
    )
    .await;
    let open = read_until_id(&mut lines, 3).await;
    assert_eq!(open_of(&open)["model"], server_info().model, "{open}");
}

#[tokio::test]
async fn set_config_option_effort_sets_clears_and_rejects_unknown_levels() {
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    let effort = opened["result"]["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["id"] == "effort")
        .unwrap();
    assert_eq!(effort["category"], "thought_level");
    assert_eq!(effort["currentValue"], "xhigh");
    for (id, value) in [(3, "max"), (4, "default")] {
        send(
            &mut cwrite,
            json!({"jsonrpc":"2.0","id":id,"method":"session/set_config_option",
                   "params":{"sessionId":"s","configId":"effort","value":value}}),
        )
        .await;
        let reply = read_until_id(&mut lines, id).await;
        let effort = reply["result"]["configOptions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["id"] == "effort")
            .unwrap();
        assert_eq!(effort["currentValue"], value, "{reply}");
    }
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":5,"method":"session/set_config_option",
               "params":{"sessionId":"s","configId":"effort","value":"ultra"}}),
    )
    .await;
    let m = read_until_id(&mut lines, 5).await;
    assert_eq!(
        m["error"]["code"], -32602,
        "an unknown rung must error: {m}"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":6,"method":"session/set_config_option",
               "params":{"sessionId":"s","configId":"colour","value":"red"}}),
    )
    .await;
    let m = read_until_id(&mut lines, 6).await;
    assert_eq!(
        m["error"]["code"], -32602,
        "an unknown option must error: {m}"
    );
}

#[tokio::test]
async fn set_config_option_model_switches_and_returns_all_options() {
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 2).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/set_config_option",
               "params":{"sessionId":"s","configId":"model","value":"anthropic/other"}}),
    )
    .await;
    let reply = read_until_id(&mut lines, 3).await;
    let ids: Vec<&str> = reply["result"]["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["model", "effort", "plan"]);
    assert_eq!(
        reply["result"]["configOptions"][0]["currentValue"],
        "anthropic/other"
    );
    assert_eq!(reply["result"]["_meta"]["hotl.dev/model"]["applied"], true);
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    assert_eq!(
        open_of(&opened)["goal"],
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
            history: Vec::new(),
            memory_mode: None,
            worktree: None,
            lease: None,
            color: None,
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/resume","params":{"sessionId":"S1","cwd":std::env::temp_dir()}}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    assert_eq!(
        open_of(&opened)["goal"],
        "all tests pass",
        "resume must hand the restored goal to the client synchronously"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"_hotl/session/fork",
               "params":{"sessionId":"S1"}}),
    )
    .await;
    let forked = read_until_id(&mut lines, 3).await;
    assert_eq!(
        open_of(&forked)["goal"],
        Value::Null,
        "a fork never inherits the goal"
    );
}

async fn next(lines: &mut tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>) -> Value {
    loop {
        let m = next_raw(lines).await;
        if m["params"]["update"].get("sessionUpdate").is_none() {
            return m;
        }
    }
}

async fn next_raw(
    lines: &mut tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>,
) -> Value {
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

async fn next_standard(
    lines: &mut tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>,
    kind: &str,
) -> Value {
    for _ in 0..12 {
        let m = next_raw(lines).await;
        if m["params"]["update"]["sessionUpdate"] == kind {
            return m;
        }
    }
    panic!("no {kind} frame in 12 tries");
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
        key_available: true,
        sessions_dir: None,
        fallback_models: Vec::new(),
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
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
    assert_eq!(
        ack["result"]["sessionId"], first_session,
        "the client keeps the session id it knows across a reload"
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
        json!({"jsonrpc":"2.0","id":4,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"_meta":{"hotl.dev/extensions":1}}}}),
    )
    .await;
    let init = next_matching(&mut lines, |m| m["id"] == json!(4)).await;
    assert_eq!(
        init["result"]["agentCapabilities"]["_meta"]["hotl.dev/init"]["contextWindow"],
        900_000
    );
    assert_eq!(
        init["result"]["agentCapabilities"]["_meta"]["hotl.dev/init"]["defaultMode"],
        "auto"
    );
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
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    next(&mut lines).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
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

/// A session with the memory dial set (0069): the `remember` tool is
/// registered the way the binary does under `read-write`, and the project
/// file lives under a tempdir that outlives the factory.
fn memory_factory(
    mode: hotl_engine::MemoryMode,
    mem_dir: std::path::PathBuf,
    script: Vec<Vec<Result<hotl_provider::StreamEvent, hotl_provider::ProviderError>>>,
) -> acp::SessionFactory {
    Box::new(move |_spec| {
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        let provider = Arc::new(ScriptedProvider::new(script.clone()));
        let (cmd_tx, cmd_rx) = hotl_engine::session_channel();
        let (event_tx, event_rx) = hotl_engine::event_channel();
        let mut registry = Registry::builtin();
        if mode == hotl_engine::MemoryMode::ReadWrite {
            registry.register(Box::new(hotl_tools::RememberTool::new(
                hotl_engine::memory_sink(cmd_tx.downgrade()),
            )));
        }
        let handle = hotl_engine::spawn_session_with_channels(
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
                    memory_files: Some(hotl_engine::MemoryFiles {
                        project: "proj".into(),
                        dir: mem_dir.clone(),
                        mode,
                    }),
                    ..Default::default()
                },
            },
            cmd_tx,
            cmd_rx,
            event_tx,
            event_rx,
            hotl_engine::hooks::NotificationDrain::new(),
        );
        Ok(acp::SessionOpen {
            history: Vec::new(),
            handle,
            name: None,
            mode: "ask".into(),
            plan: false,
            default_effort: None,
            effort_schedule: None,
            goal: None,
            todos: Vec::new(),
            effort: None,
            previous_model: None,
            memory_mode: Some(mode.as_str().to_string()),
            worktree: None,
            lease: None,
            color: None,
            session_id,
            warnings: Vec::new(),
            title: None,
            model: None,
        })
    })
}

async fn open_memory_session(
    factory: acp::SessionFactory,
) -> (
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    Value,
) {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, factory, server_info(), None));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    let _ = read_until_id(&mut lines, 1).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    (cwrite, lines, opened)
}

#[tokio::test]
async fn remember_asks_via_request_permission_then_acks_and_broadcasts_memory_changed() {
    let dir = tempfile::tempdir().unwrap();
    let mem = dir.path().join("memory/proj");
    let (mut cwrite, mut lines, opened) = open_memory_session(memory_factory(
        hotl_engine::MemoryMode::ReadWrite,
        mem.clone(),
        vec![],
    ))
    .await;
    assert_eq!(open_of(&opened)["memoryMode"], "read-write");
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/remember",
               "params":{"note":"  tests need\nnextest  "}}),
    )
    .await;
    let mut asked = false;
    let mut acked = false;
    let mut changed = false;
    for _ in 0..12 {
        if acked && changed {
            break;
        }
        let m = next(&mut lines).await;
        match m.get("method").and_then(Value::as_str) {
            Some("session/request_permission") => {
                let summary = m["params"]["summary"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                assert!(
                    summary.contains("remember → project memory: \"tests need nextest\""),
                    "{summary}"
                );
                asked = true;
                let rid = m["id"].clone();
                send(
                    &mut cwrite,
                    json!({"jsonrpc":"2.0","id":rid,"result":{"allow":true}}),
                )
                .await;
            }
            Some("session/update") if m["params"]["update"]["type"] == "memory_changed" => {
                assert_eq!(m["params"]["update"]["note"], "tests need nextest");
                assert!(m["params"]["update"]["bytes"].as_u64().unwrap() > 0);
                changed = true;
            }
            _ if m["id"] == json!(3) => {
                assert_eq!(m["result"]["ok"], true, "{m}");
                acked = true;
            }
            _ => {}
        }
    }
    assert!(
        asked && acked && changed,
        "asked={asked} acked={acked} changed={changed}"
    );
    let file = std::fs::read_to_string(mem.join("MEMORY.md")).unwrap();
    assert!(file.ends_with(": tests need nextest\n"), "{file}");
}

#[tokio::test]
async fn remember_bare_returns_a_memory_report() {
    let dir = tempfile::tempdir().unwrap();
    let mem = dir.path().join("memory/proj");
    std::fs::create_dir_all(&mem).unwrap();
    std::fs::write(
        mem.join("MEMORY.md"),
        "## Notes\n- 2026-10-01: one\n- 2026-10-02: two\n",
    )
    .unwrap();
    let (mut cwrite, mut lines, opened) = open_memory_session(memory_factory(
        hotl_engine::MemoryMode::ReadOnly,
        mem.clone(),
        vec![],
    ))
    .await;
    assert_eq!(open_of(&opened)["memoryMode"], "read-only");
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/remember","params":{}}),
    )
    .await;
    let mut report = None;
    for _ in 0..6 {
        let m = next(&mut lines).await;
        if m["method"] == "session/update" && m["params"]["update"]["type"] == "memory_report" {
            report = Some(m["params"]["update"].clone());
            break;
        }
    }
    let r = report.expect("a memory_report notification");
    assert_eq!(r["mode"], "read-only");
    assert_eq!(r["cap"], hotl_context::MEMORY_BUDGET_BYTES);
    assert_eq!(r["bytes"], 45);
    assert_eq!(r["entries"].as_array().unwrap().len(), 2);
    assert_eq!(r["entries"][1]["note"], "two");
    assert!(r["path"].as_str().unwrap().ends_with("MEMORY.md"));
}

#[tokio::test]
async fn remember_rejects_an_empty_or_overlong_note() {
    let dir = tempfile::tempdir().unwrap();
    let (mut cwrite, mut lines, _) = open_memory_session(memory_factory(
        hotl_engine::MemoryMode::ReadWrite,
        dir.path().join("memory/proj"),
        vec![],
    ))
    .await;
    // Whitespace-only reads as absent → a report, not an error.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/remember","params":{"note":"   "}}),
    )
    .await;
    let ack = read_until_id(&mut lines, 3).await;
    assert_eq!(ack["result"]["ok"], true, "{ack}");
    let long = "x".repeat(hotl_types::MEMORY_NOTE_MAX_CHARS + 1);
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/remember","params":{"note":long}}),
    )
    .await;
    let err = read_until_id(&mut lines, 4).await;
    let text = err["error"]["message"].as_str().unwrap_or_default();
    assert!(text.contains("1–2000 characters"), "{err}");
    assert!(!dir.path().join("memory/proj/MEMORY.md").exists());
}

#[tokio::test]
async fn remember_in_read_only_mode_errors_without_a_permission_request() {
    let dir = tempfile::tempdir().unwrap();
    let (mut cwrite, mut lines, _) = open_memory_session(memory_factory(
        hotl_engine::MemoryMode::ReadOnly,
        dir.path().join("memory/proj"),
        vec![],
    ))
    .await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/remember","params":{"note":"x"}}),
    )
    .await;
    let mut err = None;
    for _ in 0..6 {
        let m = next(&mut lines).await;
        assert_ne!(
            m.get("method").and_then(Value::as_str),
            Some("session/request_permission"),
            "read-only must not ask"
        );
        if m["id"] == json!(3) {
            err = Some(m);
            break;
        }
    }
    let err = err.expect("an error reply");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("memory is read-only ([memory] mode)"),
        "{err}"
    );
}

/// A session whose scripted model starts one background job (0064).
#[cfg(unix)]
fn jobs_factory(command: &str) -> acp::SessionFactory {
    let command = command.to_string();
    Box::new(move |_spec| {
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_call(
                "t1",
                "bash",
                json!({"command": command, "background": true}),
            ),
            ScriptedProvider::text_reply("started it"),
            ScriptedProvider::text_reply("saw it end"),
        ]));
        Ok(acp::SessionOpen {
            history: Vec::new(),
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
            memory_mode: None,
            worktree: None,
            lease: None,
            color: None,
            session_id,
            warnings: Vec::new(),
            title: None,
            model: None,
        })
    })
}

/// Drive a prompt to its result, allowing every permission ask; returns the
/// `job_started` frame seen on the way.
#[cfg(unix)]
async fn prompt_and_collect_job(
    cwrite: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    id: u64,
) -> Value {
    send(
        cwrite,
        json!({"jsonrpc":"2.0","id":id,"method":"session/prompt","params":{"text":"go"}}),
    )
    .await;
    let mut started = None;
    loop {
        let m = next(lines).await;
        match m.get("method").and_then(Value::as_str) {
            Some("session/request_permission") => {
                let rid = m["id"].clone();
                send(
                    cwrite,
                    json!({"jsonrpc":"2.0","id":rid,"result":{"allow":true}}),
                )
                .await;
            }
            Some("session/update") if m["params"]["update"]["type"] == "job_started" => {
                started = Some(m["params"]["update"].clone());
            }
            _ if m["id"] == json!(id) => break,
            _ => {}
        }
    }
    started.expect("a job_started frame")
}

#[cfg(unix)]
#[tokio::test]
async fn session_jobs_reports_running_and_finished_jobs() {
    let (mut cwrite, mut lines, _) = open_memory_session(jobs_factory("sleep 60")).await;
    let started = prompt_and_collect_job(&mut cwrite, &mut lines, 3).await;
    assert_eq!(started["id"], "t1", "the starting call's id");
    let job = started["job"].as_str().unwrap().to_string();
    assert_eq!(job.len(), 6);
    assert_eq!(started["detached"], false);
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/jobs","params":{}}),
    )
    .await;
    let mut report = None;
    for _ in 0..8 {
        let m = next(&mut lines).await;
        if m["method"] == "session/update" && m["params"]["update"]["type"] == "jobs_report" {
            report = Some(m["params"]["update"].clone());
            break;
        }
    }
    let r = report.expect("a jobs_report");
    let rows = r["jobs"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], job);
    assert_eq!(rows[0]["state"], "running");
    assert_eq!(rows[0]["rc"], json!(null));
    assert_eq!(rows[0]["command"], "sleep 60");
    assert!(rows[0]["pid"].as_u64().unwrap() > 0);
    // Stop it; the report then shows it done with the kill recorded.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":5,"method":"session/jobs/stop","params":{"job": job}}),
    )
    .await;
    let ack = read_until_id(&mut lines, 5).await;
    assert_eq!(ack["result"]["stopped"], true, "{ack}");
    let mut done = None;
    for _ in 0..40 {
        let m = next(&mut lines).await;
        if m["method"] == "session/update" && m["params"]["update"]["type"] == "job_done" {
            done = Some(m["params"]["update"].clone());
            break;
        }
    }
    let d = done.expect("a job_done frame");
    assert_eq!(d["job"], job);
    assert_eq!(d["rc"], "killed:SIGKILL");
    assert_eq!(d["stopped_by"], "/jobs");
}

#[cfg(unix)]
#[tokio::test]
async fn session_jobs_stop_kills_by_id_and_unknown_ids_say_so() {
    let (mut cwrite, mut lines, _) = open_memory_session(jobs_factory("sleep 60")).await;
    let started = prompt_and_collect_job(&mut cwrite, &mut lines, 3).await;
    let job = started["job"].as_str().unwrap().to_string();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/jobs/stop","params":{"job":"nope00"}}),
    )
    .await;
    let ack = read_until_id(&mut lines, 4).await;
    assert_eq!(ack["result"]["stopped"], false, "{ack}");
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":5,"method":"session/jobs/stop","params":{}}),
    )
    .await;
    let err = read_until_id(&mut lines, 5).await;
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("params.job"),
        "{err}"
    );
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":6,"method":"session/jobs/stop","params":{"job": job}}),
    )
    .await;
    let ack = read_until_id(&mut lines, 6).await;
    assert_eq!(ack["result"]["stopped"], true, "{ack}");
    // Twice is false: the job is no longer running.
    for _ in 0..40 {
        let m = next(&mut lines).await;
        if m["method"] == "session/update" && m["params"]["update"]["type"] == "job_done" {
            break;
        }
    }
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":7,"method":"session/jobs/stop","params":{"job": job}}),
    )
    .await;
    let ack = read_until_id(&mut lines, 7).await;
    assert_eq!(ack["result"]["stopped"], false, "{ack}");
}

/// Registers a fake running sub-agent with the session's registry (0072), so
/// `session/children` has something to report without a real child builder.
struct RegisterChild;

impl hotl_tools::Tool for RegisterChild {
    fn name(&self) -> &'static str {
        "register_child"
    }
    fn description(&self) -> &str {
        "starts a fake background child"
    }
    fn schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn permission(&self, _input: &Value) -> hotl_tools::Permission {
        hotl_tools::Permission::None
    }
    fn read_only(&self) -> bool {
        true
    }
    fn run<'a>(
        &'a self,
        _input: Value,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> futures_util::future::BoxFuture<'a, hotl_tools::ToolOutcome> {
        Box::pin(async move {
            let Some(children) = hotl_tools::children::registry() else {
                return hotl_tools::ToolOutcome::err("no registry");
            };
            let (steer, _) = tokio::sync::mpsc::channel(1);
            let (resume, _) = tokio::sync::mpsc::channel(1);
            let entry = children
                .register(hotl_tools::children::ChildSpec {
                    call_id: hotl_tools::current_call_id(),
                    agent_type: "explore".into(),
                    summary: "look around".into(),
                    background: true,
                    isolated: false,
                    read_only: true,
                    started_ms: 1_000,
                    steer,
                    resume,
                })
                .unwrap();
            hotl_tools::ToolOutcome::ok(format!("started {}", entry.id))
        })
    }
}

/// A session whose scripted model starts one fake child, then finishes a
/// second one as parked.
fn children_factory() -> acp::SessionFactory {
    Box::new(move |_spec| {
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        let provider = Arc::new(ScriptedProvider::new(vec![
            ScriptedProvider::tool_call("t1", "register_child", json!({})),
            ScriptedProvider::text_reply("started it"),
        ]));
        let mut registry = Registry::builtin();
        registry.register(Box::new(RegisterChild));
        Ok(acp::SessionOpen {
            history: Vec::new(),
            handle: spawn_session(SessionDeps {
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
            memory_mode: None,
            worktree: None,
            lease: None,
            color: None,
            session_id,
            warnings: Vec::new(),
            title: None,
            model: None,
        })
    })
}

async fn children_report(
    cwrite: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    id: u64,
) -> Value {
    send(
        cwrite,
        json!({"jsonrpc":"2.0","id":id,"method":"session/children","params":{}}),
    )
    .await;
    for _ in 0..12 {
        let m = next(lines).await;
        if m["method"] == "session/update" && m["params"]["update"]["type"] == "children_report" {
            return m["params"]["update"].clone();
        }
    }
    panic!("no children_report");
}

#[tokio::test]
async fn session_children_reports_running_and_closed() {
    let (mut cwrite, mut lines, _) = open_memory_session(children_factory()).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"text":"go"}}),
    )
    .await;
    read_until_id(&mut lines, 3).await;
    let r = children_report(&mut cwrite, &mut lines, 4).await;
    let rows = r["children"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{r}");
    let row = &rows[0];
    assert_eq!(row["id"].as_str().unwrap().len(), 6);
    assert_eq!(row["call_id"], "t1", "the spawn call's id");
    assert_eq!(row["agent_type"], "explore");
    assert_eq!(row["summary"], "look around");
    assert_eq!(row["state"], "running");
    assert_eq!(row["ok"], json!(null));
    assert_eq!(row["background"], true);
    assert_eq!(row["isolated"], false);
    assert!(row["secs"].as_u64().is_some());
    let child = row["id"].as_str().unwrap().to_string();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":5,"method":"session/children/stop","params":{"child": child}}),
    )
    .await;
    let ack = read_until_id(&mut lines, 5).await;
    assert_eq!(ack["result"]["stopped"], true, "{ack}");
    let r = children_report(&mut cwrite, &mut lines, 6).await;
    assert_eq!(r["children"][0]["state"], "closed", "{r}");
}

#[tokio::test]
async fn session_children_stop_interrupts_by_id_and_unknown_ids_say_so() {
    let (mut cwrite, mut lines, _) = open_memory_session(children_factory()).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"text":"go"}}),
    )
    .await;
    read_until_id(&mut lines, 3).await;
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":4,"method":"session/children/stop","params":{"child":"nope00"}}),
    )
    .await;
    let ack = read_until_id(&mut lines, 4).await;
    assert_eq!(ack["result"]["stopped"], false, "{ack}");
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":5,"method":"session/children/stop","params":{}}),
    )
    .await;
    let err = read_until_id(&mut lines, 5).await;
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("params.child"),
        "{err}"
    );
    let r = children_report(&mut cwrite, &mut lines, 6).await;
    let child = r["children"][0]["id"].as_str().unwrap().to_string();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":7,"method":"session/children/stop","params":{"child": child}}),
    )
    .await;
    assert_eq!(
        read_until_id(&mut lines, 7).await["result"]["stopped"],
        true
    );
    // Twice is false: closed is not live.
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":8,"method":"session/children/stop","params":{"child": child}}),
    )
    .await;
    assert_eq!(
        read_until_id(&mut lines, 8).await["result"]["stopped"],
        false
    );
}

async fn initialize_with(params: Value) -> Value {
    initialize_with_info(params, server_info()).await
}

async fn initialize_with_info(params: Value, info: acp::ServerInfo) -> Value {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, scripted_factory(), info, None));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":params}),
    )
    .await;
    next(&mut lines).await["result"].clone()
}

#[tokio::test]
async fn initialize_answers_v1() {
    let r = initialize_with(json!({"protocolVersion": 1})).await;
    assert_eq!(r["protocolVersion"], json!(1));
    assert_eq!(r["agentInfo"]["name"], "hotl");
    assert_eq!(r["agentInfo"]["title"], "hotl");
    assert_eq!(r["agentInfo"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        r["authMethods"],
        json!([{
            "id": "hotl-key",
            "name": "API key from hotl config",
            "description": "Uses the key hotl setup stored, or the provider's env var",
            "type": "agent"
        }])
    );
    let caps = &r["agentCapabilities"];
    assert_eq!(caps["loadSession"], true);
    assert_eq!(
        caps["promptCapabilities"],
        json!({"image": true, "audio": false, "embeddedContext": true})
    );
    assert_eq!(caps["mcpCapabilities"], json!({"http": true, "sse": false}));
    assert_eq!(
        caps["sessionCapabilities"],
        json!({"list": {}, "resume": {}, "close": {}})
    );
    assert!(caps.get("_meta").is_none());
    assert!(r.get("skills").is_none());
}

#[tokio::test]
async fn initialize_answers_1_to_a_future_version() {
    let r = initialize_with(json!({"protocolVersion": 7})).await;
    assert_eq!(r["protocolVersion"], json!(1));
}

#[tokio::test]
async fn initialize_answers_1_to_an_old_client() {
    let r = initialize_with(json!({"protocolVersion": "0.1"})).await;
    assert_eq!(r["protocolVersion"], json!(1));
    let r = initialize_with(Value::Null).await;
    assert_eq!(r["protocolVersion"], json!(1));
}

#[tokio::test]
async fn initialize_meta_only_for_opted_in_clients() {
    let r = initialize_with(json!({
        "protocolVersion": 1,
        "clientCapabilities": {"_meta": {"hotl.dev/extensions": 1}}
    }))
    .await;
    let init = &r["agentCapabilities"]["_meta"]["hotl.dev/init"];
    assert!(init["skills"].is_array());
    assert!(init["workflows"].is_array());
    assert_eq!(init["defaultMode"], "ask");
    assert_eq!(init["defaultPlan"], false);
    assert!(init["contextWindow"].is_u64());
    assert!(init["streamIdleSecs"].is_u64());
    assert_eq!(init["acpSchema"], acp::ACP_SCHEMA_CHECKED);
    assert_eq!(
        init["extensions"].as_array().unwrap().len(),
        acp::EXTENSION_METHODS.len()
    );
    let r = initialize_with(json!({
        "protocolVersion": 1,
        "clientCapabilities": {"_meta": {"hotl.dev/extensions": 2}}
    }))
    .await;
    assert!(r["agentCapabilities"].get("_meta").is_none());
}

#[test]
fn client_caps_read_what_the_client_announced() {
    let req: acp_types::InitializeRequest = serde_json::from_value(json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "_meta": {"hotl.dev/extensions": 1},
            "session": {"notices": {}, "compaction": {}},
            "elicitation": {"form": {}},
            "auth": {"terminal": true}
        }
    }))
    .unwrap();
    assert_eq!(
        acp::client_caps_of(&req),
        acp::ClientCaps {
            ext: true,
            notices: true,
            compaction: true,
            elicitation_form: true,
            auth_terminal: true
        }
    );
    assert_eq!(
        acp::client_caps_of(&Default::default()),
        acp::ClientCaps::default()
    );
}

/// Every method the handler matches is advertised, and nothing else: the
/// match arms are read from the source, so a new arm without a roster entry
/// fails here. Each advertised handler method also answers without
/// "unknown method".
#[tokio::test]
async fn every_handled_method_is_advertised_as_an_extension() {
    let src = std::fs::read_to_string(
        std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("src/acp.rs"),
    )
    .unwrap();
    let mut handled: Vec<String> = Vec::new();
    for line in src.lines() {
        let t = line.trim_start();
        if !(t.ends_with("=> {") || t.ends_with("=>")) {
            continue;
        }
        let mut rest = t;
        while let Some(i) = rest.find("\"session/") {
            let tail = &rest[i + 1..];
            let end = tail.find('"').unwrap();
            handled.push(tail[..end].to_string());
            rest = &tail[end..];
        }
    }
    for m in [
        "session/request_permission",
        "session/request_question",
        "session/request_egress",
    ] {
        assert!(src.contains(&format!("\"method\": \"{m}\"")), "{m} is sent");
        handled.push(m.to_string());
    }
    handled.sort();
    let mut advertised: Vec<String> = acp::EXTENSION_METHODS
        .iter()
        .map(|m| m.to_string())
        .collect();
    advertised.sort();
    assert_eq!(handled, advertised);

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
    let mut id = 100;
    for m in acp::EXTENSION_METHODS
        .iter()
        .filter(|m| !m.starts_with("session/request_"))
    {
        // No session yet: most answer "requires an open session" — anything
        // but "unknown method". `new`/`load` would open one; skip them here.
        if matches!(*m, "session/new" | "session/load" | "session/prompt") {
            continue;
        }
        id += 1;
        send(&mut cwrite, json!({"jsonrpc":"2.0","id":id,"method":m})).await;
        let r = read_until_id(&mut lines, id).await;
        let msg = r["error"]["message"].as_str().unwrap_or("");
        assert!(!msg.contains("unknown method"), "{m}: {r}");
    }
}

async fn error_code_for(request: Value) -> i64 {
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
    send(&mut cwrite, request).await;
    let reply = read_until_id(&mut lines, 1).await;
    reply["error"]["code"].as_i64().unwrap()
}

#[tokio::test]
async fn unknown_method_is_32601() {
    let code = error_code_for(json!({"jsonrpc":"2.0","id":1,"method":"session/frobnicate"})).await;
    assert_eq!(code, -32601);
}

#[tokio::test]
async fn missing_session_id_is_32602() {
    let code =
        error_code_for(json!({"jsonrpc":"2.0","id":1,"method":"session/load","params":{}})).await;
    assert_eq!(code, -32602);
}

#[tokio::test]
async fn a_request_with_no_open_session_is_32002() {
    let code =
        error_code_for(json!({"jsonrpc":"2.0","id":1,"method":"session/fold","params":{}})).await;
    assert_eq!(code, -32002);
}

#[test]
fn ext_names_carry_the_hotl_prefix() {
    assert_eq!(crate::acp_ext::ext("session/steer"), "_hotl/session/steer");
    assert_eq!(crate::acp_ext::META_EXT, "hotl.dev/extensions");
}

#[tokio::test]
async fn auth_methods_list_agent_method() {
    let r = initialize_with(json!({"protocolVersion": 1})).await;
    let methods = r["authMethods"].as_array().unwrap();
    assert_eq!(methods.len(), 1);
    assert_eq!(methods[0]["id"], "hotl-key");
    assert_eq!(methods[0]["type"], "agent");
}

#[tokio::test]
async fn terminal_method_only_when_client_offers_it() {
    let r = initialize_with(json!({
        "protocolVersion": 1,
        "clientCapabilities": {"auth": {"terminal": true}}
    }))
    .await;
    let methods = r["authMethods"].as_array().unwrap();
    assert_eq!(methods.len(), 2);
    assert_eq!(
        methods[1],
        json!({"id": "hotl-setup", "name": "Run hotl setup", "type": "terminal", "args": ["setup"]})
    );
}

#[tokio::test]
async fn initialize_with_a_string_version_keeps_the_opt_in() {
    for version in [json!("0.1"), json!(70000)] {
        let r = initialize_with(json!({
            "protocolVersion": version,
            "clientCapabilities": {"_meta": {"hotl.dev/extensions": 1}}
        }))
        .await;
        assert_eq!(r["protocolVersion"], json!(1));
        assert!(r["agentCapabilities"]["_meta"]["hotl.dev/init"].is_object());
    }
}

async fn rpc_with_info(info: acp::ServerInfo, calls: Vec<Value>) -> Vec<Value> {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, scripted_factory(), info, None));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    let mut out = Vec::new();
    for (i, call) in calls.into_iter().enumerate() {
        let mut call = call;
        call["jsonrpc"] = json!("2.0");
        call["id"] = json!(i + 1);
        send(&mut cwrite, call).await;
        out.push(read_until_id(&mut lines, i as u64 + 1).await);
    }
    out
}

fn no_key_info() -> acp::ServerInfo {
    acp::ServerInfo {
        key_available: false,
        sessions_dir: None,
        fallback_models: Vec::new(),
        ..server_info()
    }
}

#[tokio::test]
async fn session_new_without_a_key_is_auth_required() {
    let r = rpc_with_info(
        no_key_info(),
        vec![
            json!({"method":"initialize","params":{"protocolVersion":1}}),
            json!({"method":"session/new","params":new_params()}),
            json!({"method":"session/load","params":{"sessionId":"01ABC","cwd":std::env::temp_dir()}}),
            json!({"method":"session/resume","params":{"sessionId":"01ABC","cwd":std::env::temp_dir()}}),
            json!({"method":"_hotl/session/fork","params":{"sessionId":"01ABC"}}),
        ],
    )
    .await;
    for reply in &r[1..] {
        assert_eq!(reply["error"]["code"], -32000, "{reply}");
        assert_eq!(reply["error"]["message"], "no API key — run `hotl setup`");
    }
}

#[tokio::test]
async fn authenticate_checks_the_key() {
    let ok = rpc_with_info(
        server_info(),
        vec![json!({"method":"authenticate","params":{"methodId":"hotl-key"}})],
    )
    .await;
    assert_eq!(ok[0]["result"], json!({}));
    let none = rpc_with_info(
        no_key_info(),
        vec![json!({"method":"authenticate","params":{"methodId":"hotl-key"}})],
    )
    .await;
    assert_eq!(none[0]["error"]["code"], -32000);
}

#[tokio::test]
async fn authenticate_rejects_unknown_and_terminal_methods() {
    let r = rpc_with_info(
        server_info(),
        vec![
            json!({"method":"authenticate","params":{"methodId":"nope"}}),
            json!({"method":"authenticate","params":{"methodId":"hotl-setup"}}),
            json!({"method":"authenticate","params":{}}),
        ],
    )
    .await;
    for reply in &r {
        assert_eq!(reply["error"]["code"], -32602, "{reply}");
    }
}

fn hook_to(info_after: fn() -> acp::ServerInfo) -> acp::Reload {
    Box::new(move || {
        Box::pin(async move { Ok((scripted_factory(), info_after(), Vec::<String>::new())) })
    })
}

async fn rpc_with_hook(info: acp::ServerInfo, hook: acp::Reload, calls: Vec<Value>) -> Vec<Value> {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        info,
        Some(hook),
    ));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    let mut out = Vec::new();
    for (i, call) in calls.into_iter().enumerate() {
        let mut call = call;
        call["jsonrpc"] = json!("2.0");
        call["id"] = json!(i + 1);
        send(&mut cwrite, call).await;
        out.push(read_until_id(&mut lines, i as u64 + 1).await);
    }
    out
}

#[tokio::test]
async fn reload_with_a_live_session_after_the_key_vanishes_is_auth_required() {
    let r = rpc_with_hook(
        server_info(),
        hook_to(no_key_info),
        vec![
            json!({"method":"initialize","params":{"protocolVersion":1}}),
            json!({"method":"session/new","params":new_params()}),
            json!({"method":"session/reload_config","params":{}}),
        ],
    )
    .await;
    assert!(r[1]["result"]["sessionId"].is_string(), "{}", r[1]);
    assert_eq!(r[2]["error"]["code"], -32000, "{}", r[2]);
}

#[tokio::test]
async fn authenticate_reloads_so_a_fresh_key_is_picked_up() {
    let r = rpc_with_hook(
        no_key_info(),
        hook_to(server_info),
        vec![
            json!({"method":"initialize","params":{"protocolVersion":1}}),
            json!({"method":"session/new","params":new_params()}),
            json!({"method":"authenticate","params":{"methodId":"hotl-key"}}),
            json!({"method":"session/new","params":new_params()}),
        ],
    )
    .await;
    assert_eq!(r[1]["error"]["code"], -32000);
    assert_eq!(r[2]["result"], json!({}), "{}", r[2]);
    assert!(r[3]["result"]["sessionId"].is_string(), "{}", r[3]);
}

fn history_factory(
    history: Vec<hotl_types::Item>,
    replies: Vec<Vec<Result<hotl_provider::StreamEvent, hotl_provider::ProviderError>>>,
) -> acp::SessionFactory {
    let replies = Arc::new(std::sync::Mutex::new(Some(replies)));
    Box::new(move |spec| {
        let loaded = matches!(spec, acp::SessionSpec::Load { .. });
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        let items = if loaded { history.clone() } else { Vec::new() };
        let turns = replies.lock().unwrap().take().unwrap_or_default();
        Ok(acp::SessionOpen {
            history: items.clone(),
            memory_mode: None,
            worktree: None,
            lease: None,
            color: None,
            handle: spawn_session(SessionDeps {
                concurrency: Default::default(),
                provider: Arc::new(ScriptedProvider::new(turns)),
                registry: Arc::new(Registry::builtin()),
                rules: Arc::new(Rules::default()),
                sandbox_enforced: false,
                clock: Arc::new(SystemClock),
                log,
                system: "sys".into(),
                cwd: std::env::temp_dir(),
                hooks: None,
                initial_items: items,
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
    })
}

fn finished_turn() -> Vec<hotl_types::Item> {
    vec![
        hotl_types::Item::User {
            text: "list the files".into(),
            synthetic: None,
            images: Vec::new(),
        },
        hotl_types::Item::Assistant {
            blocks: vec![
                json!({"type":"text","text":"Looking."}),
                json!({"type":"tool_use","id":"t1","name":"bash","input":{"command":"ls"}}),
            ],
            model: None,
        },
        hotl_types::Item::ToolResults {
            results: vec![hotl_types::ToolResultItem {
                tool_use_id: "t1".into(),
                content: "a.txt".into(),
                is_error: false,
            }],
        },
        hotl_types::Item::Assistant {
            blocks: vec![json!({"type":"text","text":"One file."})],
            model: None,
        },
    ]
}

async fn connect(
    factory: acp::SessionFactory,
    info: acp::ServerInfo,
) -> (
    impl AsyncWriteExt + Unpin,
    tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>,
) {
    let (client, server) = tokio::io::duplex(256 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(sread, swrite, factory, info, None));
    let (cread, mut cwrite) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut cwrite,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":init_params()}),
    )
    .await;
    read_until_id(&mut lines, 1).await;
    (cwrite, lines)
}

async fn frames_through_reply(
    lines: &mut tokio::io::Lines<BufReader<impl tokio::io::AsyncRead + Unpin>>,
    id: u64,
) -> (Vec<Value>, Value) {
    let mut before = Vec::new();
    loop {
        let m = next_raw(lines).await;
        if m["id"] == json!(id) {
            return (before, m);
        }
        before.push(m);
    }
}

fn session_updates(frames: &[Value]) -> Vec<&Value> {
    frames
        .iter()
        .filter(|m| m["method"] == "session/update")
        .map(|m| &m["params"]["update"])
        .filter(|u| u.get("sessionUpdate").is_some())
        .collect()
}

#[tokio::test]
async fn new_requires_absolute_cwd() {
    let (mut w, mut lines) = connect(scripted_factory(), server_info()).await;
    let missing = std::env::temp_dir().join("hotl-no-such-dir-0081");
    let bad = [
        json!({"cwd": "rel", "mcpServers": []}),
        json!({"cwd": missing, "mcpServers": []}),
        json!({"mcpServers": []}),
        json!({"cwd": std::env::temp_dir(), "mcpServers": "x"}),
    ];
    for (i, params) in bad.into_iter().enumerate() {
        let id = 10 + i as u64;
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":id,"method":"session/new","params":params}),
        )
        .await;
        let r = read_until_id(&mut lines, id).await;
        assert_eq!(r["error"]["code"], -32602, "{r}");
    }
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":20,"method":"session/new","params":{
            "cwd": std::env::temp_dir(),
            "mcpServers": [{"name": "fs", "command": "/bin/echo", "args": [], "env": []}]
        }}),
    )
    .await;
    let r = read_until_id(&mut lines, 20).await;
    assert!(r["result"]["sessionId"].is_string(), "{r}");
    assert_eq!(r["result"]["modes"]["currentModeId"], "ask", "{r}");
    assert_eq!(
        r["result"]["modes"]["availableModes"][1],
        json!({"id": "dontask", "name": "Don't ask", "description": "Deny anything that would ask"})
    );
}

#[tokio::test]
async fn a_plain_client_gets_no_hotl_meta_on_open() {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (sread, swrite) = tokio::io::split(server);
    tokio::spawn(acp::serve(
        sread,
        swrite,
        scripted_factory(),
        server_info(),
        None,
    ));
    let (cread, mut w) = tokio::io::split(client);
    let mut lines = BufReader::new(cread).lines();
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}),
    )
    .await;
    read_until_id(&mut lines, 1).await;
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let r = read_until_id(&mut lines, 2).await;
    assert!(r["result"].get("_meta").is_none(), "{r}");
    assert!(r["result"]["modes"].is_object(), "{r}");
}

#[tokio::test]
async fn load_replays_before_it_replies() {
    let (mut w, mut lines) = connect(history_factory(finished_turn(), vec![]), server_info()).await;
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":2,"method":"session/load",
               "params":{"sessionId":"S1","cwd":std::env::temp_dir(),"mcpServers":[]}}),
    )
    .await;
    let (before, reply) = frames_through_reply(&mut lines, 2).await;
    assert!(reply["result"].get("sessionId").is_none(), "{reply}");
    assert_eq!(reply["result"]["modes"]["currentModeId"], "ask");
    for m in &before {
        assert_eq!(m["params"]["sessionId"], "S1", "{m}");
    }
    let kinds: Vec<&str> = session_updates(&before)
        .iter()
        .map(|u| u["sessionUpdate"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "user_message_chunk",
            "agent_message_chunk",
            "tool_call",
            "agent_message_chunk"
        ]
    );
    let tool = session_updates(&before)[2];
    assert_eq!(tool["toolCallId"], "t1");
    assert_eq!(tool["status"], "completed");
    assert_eq!(tool["kind"], "execute");
    assert_eq!(tool["content"][0]["content"]["text"], "a.txt");
}

#[tokio::test]
async fn load_of_a_cut_off_tool_marks_it_failed_then_continues() {
    let cut_off = vec![
        hotl_types::Item::User {
            text: "edit it".into(),
            synthetic: None,
            images: Vec::new(),
        },
        hotl_types::Item::Assistant {
            blocks: vec![json!({"type":"tool_use","id":"t7","name":"edit","input":{}})],
            model: None,
        },
    ];
    let (mut w, mut lines) = connect(
        history_factory(cut_off, vec![ScriptedProvider::text_reply("CONTINUED")]),
        server_info(),
    )
    .await;
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":2,"method":"session/load",
               "params":{"sessionId":"S1","cwd":std::env::temp_dir(),"mcpServers":[]}}),
    )
    .await;
    let (before, _reply) = frames_through_reply(&mut lines, 2).await;
    assert!(
        !before
            .iter()
            .any(|m| m["params"]["update"]["type"] == "text_delta"),
        "nothing continues before the reply: {before:?}"
    );
    let updates = session_updates(&before);
    let last = updates.last().expect("replayed updates");
    assert_eq!(last["sessionUpdate"], "tool_call");
    assert_eq!(last["toolCallId"], "t7");
    assert_eq!(last["status"], "failed");
    let text = next_matching(&mut lines, |m| {
        m["params"]["update"]["type"] == "text_delta"
    })
    .await;
    assert_eq!(text["params"]["update"]["text"], "CONTINUED");
}

#[tokio::test]
async fn resume_does_not_replay() {
    let (mut w, mut lines) = connect(history_factory(finished_turn(), vec![]), server_info()).await;
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":2,"method":"session/resume",
               "params":{"sessionId":"S1","cwd":std::env::temp_dir()}}),
    )
    .await;
    let (before, reply) = frames_through_reply(&mut lines, 2).await;
    assert!(before.is_empty(), "{before:?}");
    assert_eq!(reply["result"]["modes"]["currentModeId"], "ask", "{reply}");
    assert!(open_of(&reply).is_object(), "{reply}");
}

#[tokio::test]
async fn load_rejects_the_old_fork_flag() {
    let (mut w, mut lines) = connect(scripted_factory(), server_info()).await;
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":2,"method":"session/load",
               "params":{"sessionId":"S1","cwd":std::env::temp_dir(),"fork":true}}),
    )
    .await;
    let r = read_until_id(&mut lines, 2).await;
    assert_eq!(r["error"]["code"], -32602, "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("_hotl/session/fork"),
        "{r}"
    );
}

#[tokio::test]
async fn open_updates_follow_the_reply_in_order() {
    let info = acp::ServerInfo {
        skills: vec![acp::SkillInfo {
            name: "run".into(),
            description: "launch the app".into(),
        }],
        workflows: vec![acp::WorkflowInfo {
            name: "review".into(),
            description: "review then verify".into(),
        }],
        ..server_info()
    };
    let (mut w, mut lines) = connect(inherited_state_factory(None), info).await;
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":2,"method":"session/resume",
               "params":{"sessionId":"S1","cwd":std::env::temp_dir()}}),
    )
    .await;
    read_until_id(&mut lines, 2).await;
    let mut seen = Vec::new();
    while seen.len() < 3 {
        let m = next_raw(&mut lines).await;
        if m["params"]["update"].get("sessionUpdate").is_some() {
            seen.push(m["params"]["update"].clone());
        }
    }
    assert_eq!(seen[0]["sessionUpdate"], "available_commands_update");
    assert_eq!(
        seen[0]["availableCommands"],
        json!([
            {"name": "run", "description": "launch the app", "input": {"hint": "optional arguments"}},
            {"name": "review", "description": "review then verify", "input": {"hint": "optional arguments"}}
        ])
    );
    assert_eq!(seen[1]["sessionUpdate"], "plan");
    assert_eq!(seen[1]["entries"][0]["content"], "wire the gate");
    assert_eq!(seen[2]["sessionUpdate"], "usage_update");
    assert!(seen[2]["used"].as_u64().unwrap() > 0);
    assert_eq!(seen[2]["size"], 200_000);
}

fn seed_sessions(dir: &std::path::Path, cwd: &str, count: usize, first_secs: u64) -> Vec<String> {
    let mut ids = Vec::new();
    for i in 0..count {
        let path = {
            let mut log = SessionLog::create(dir, "m", None, Masker::empty(), 0).unwrap();
            log.append(
                &hotl_types::EntryPayload::CwdSet {
                    path: cwd.to_string(),
                },
                1,
            )
            .unwrap();
            ids.push(log.session_id.clone());
            log.path().to_path_buf()
        };
        let when =
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(first_secs + i as u64 * 10);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
    ids
}

#[tokio::test]
async fn list_pages_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let a = seed_sessions(dir.path(), "/work/a", 52, 1_700_000_000);
    let b = seed_sessions(dir.path(), "/work/b", 1, 1_800_000_000);
    SessionLog::create(dir.path(), "m", None, Masker::empty(), 0).unwrap();
    let info = acp::ServerInfo {
        sessions_dir: Some(dir.path().to_path_buf()),
        fallback_models: Vec::new(),
        ..server_info()
    };
    let (mut w, mut lines) = connect(scripted_factory(), info).await;
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":2,"method":"session/list","params":{"cwd":"/work/a"}}),
    )
    .await;
    let page = read_until_id(&mut lines, 2).await;
    let sessions = page["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 50, "{page}");
    assert_eq!(sessions[0]["sessionId"], a[51].as_str());
    assert_eq!(sessions[0]["cwd"], "/work/a");
    assert_eq!(sessions[0]["updatedAt"], "2023-11-14T22:21:50Z");
    assert_eq!(sessions[49]["sessionId"], a[2].as_str());
    let cursor = page["result"]["nextCursor"].as_str().unwrap().to_string();
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":3,"method":"session/list",
               "params":{"cwd":"/work/a","cursor":cursor}}),
    )
    .await;
    let rest = read_until_id(&mut lines, 3).await;
    let ids: Vec<&str> = rest["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["sessionId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [a[1].as_str(), a[0].as_str()]);
    assert!(rest["result"].get("nextCursor").is_none(), "{rest}");

    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":4,"method":"session/list","params":{}}),
    )
    .await;
    let all = read_until_id(&mut lines, 4).await;
    assert_eq!(all["result"]["sessions"][0]["sessionId"], b[0].as_str());
    assert_eq!(all["result"]["sessions"].as_array().unwrap().len(), 50);

    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":5,"method":"session/list","params":{"cursor":"nope"}}),
    )
    .await;
    let bad = read_until_id(&mut lines, 5).await;
    assert_eq!(bad["error"]["code"], -32602, "{bad}");
}

#[tokio::test]
async fn close_is_idempotent() {
    let (mut w, mut lines) = connect(scripted_factory(), server_info()).await;
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":new_params()}),
    )
    .await;
    let opened = read_until_id(&mut lines, 2).await;
    let sid = opened["result"]["sessionId"].clone();
    for (id, target) in [(3, sid.clone()), (4, sid), (5, json!("no-such-session"))] {
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":id,"method":"session/close","params":{"sessionId":target}}),
        )
        .await;
        let r = read_until_id(&mut lines, id).await;
        assert_eq!(r["result"], json!({}), "{r}");
    }
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":6,"method":"session/prompt","params":{"text":"hi"}}),
    )
    .await;
    let r = read_until_id(&mut lines, 6).await;
    assert_eq!(
        r["error"]["code"], -32002,
        "the closed session is gone: {r}"
    );
}

#[tokio::test]
async fn fork_returns_a_new_session_and_does_not_continue() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (mut w, mut lines) = connect(interrupted_factory(seen.clone()), server_info()).await;
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":2,"method":"_hotl/session/fork",
               "params":{"sessionId":"S1","cwd":std::env::temp_dir()}}),
    )
    .await;
    let (before, reply) = frames_through_reply(&mut lines, 2).await;
    assert!(before.is_empty(), "a fork replays nothing: {before:?}");
    let sid = reply["result"]["sessionId"].as_str().unwrap();
    assert_ne!(sid, "S1");
    assert_eq!(reply["result"]["modes"]["currentModeId"], "auto");
    assert_eq!(seen.lock().unwrap().clone(), ["fork:S1:All"]);
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"text":"go"}}),
    )
    .await;
    let text = next_matching(&mut lines, |m| {
        m["params"]["update"]["type"] == "text_delta"
    })
    .await;
    assert_eq!(text["params"]["update"]["text"], "FIRST");
    assert_eq!(text["params"]["sessionId"], sid);
}

fn store_factory(
    dir: std::path::PathBuf,
    loads: Arc<std::sync::Mutex<Vec<String>>>,
) -> acp::SessionFactory {
    Box::new(move |spec| {
        let (history, replay) = match &spec {
            acp::SessionSpec::Load {
                session_id, replay, ..
            } => {
                loads.lock().unwrap().push(session_id.clone());
                (
                    hotl_store::replay_chain(&dir, session_id)
                        .map_err(|e| e.to_string())?
                        .items,
                    *replay,
                )
            }
            acp::SessionSpec::Fork { session_id, .. } => {
                loads.lock().unwrap().push(format!("fork:{session_id}"));
                (Vec::new(), false)
            }
            acp::SessionSpec::New { .. } => (Vec::new(), false),
        };
        let log = SessionLog::create_ephemeral("m", None, Masker::empty(), 0).expect("log");
        let session_id = log.session_id.clone();
        Ok(acp::SessionOpen {
            history: if replay { history } else { Vec::new() },
            memory_mode: None,
            worktree: None,
            lease: None,
            color: None,
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
            model: None,
        })
    })
}

fn logged_turn(
    dir: &std::path::Path,
    parent: Option<&str>,
    fork: bool,
    said: &str,
    secs: u64,
) -> String {
    let path = {
        let mut log = SessionLog::create(
            dir,
            "m",
            parent.map(|p| hotl_store::ParentRef {
                session_id: p.to_string(),
                tip_entry_id: None,
            }),
            Masker::empty(),
            0,
        )
        .unwrap();
        if fork {
            log.append(&hotl_types::EntryPayload::BranchMove { keep_items: 0 }, 1)
                .unwrap();
        }
        log.append(
            &hotl_types::EntryPayload::CwdSet {
                path: "/work/chain".into(),
            },
            1,
        )
        .unwrap();
        log.append(
            &hotl_types::EntryPayload::Item {
                item: hotl_types::Item::User {
                    text: said.into(),
                    synthetic: None,
                    images: Vec::new(),
                },
            },
            2,
        )
        .unwrap();
        log.path().to_path_buf()
    };
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
        .unwrap();
    path.file_stem().unwrap().to_str().unwrap().to_string()
}

#[tokio::test]
async fn load_of_a_resumed_id_sees_its_newest_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let s1 = logged_turn(dir.path(), None, false, "first ask", 1_700_000_000);
    let c1 = logged_turn(dir.path(), Some(&s1), false, "second ask", 1_700_000_100);
    let fork = logged_turn(dir.path(), Some(&c1), true, "forked ask", 1_700_000_200);
    let loads = Arc::new(std::sync::Mutex::new(Vec::new()));
    let info = acp::ServerInfo {
        sessions_dir: Some(dir.path().to_path_buf()),
        fallback_models: Vec::new(),
        ..server_info()
    };
    let (mut w, mut lines) =
        connect(store_factory(dir.path().to_path_buf(), loads.clone()), info).await;
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":2,"method":"session/load",
               "params":{"sessionId":s1,"cwd":std::env::temp_dir(),"mcpServers":[]}}),
    )
    .await;
    let (before, reply) = frames_through_reply(&mut lines, 2).await;
    assert!(reply.get("result").is_some(), "{reply}");
    let said: Vec<&str> = session_updates(&before)
        .iter()
        .filter(|u| u["sessionUpdate"] == "user_message_chunk")
        .map(|u| u["content"]["text"].as_str().unwrap())
        .collect();
    assert_eq!(said, ["first ask", "second ask"]);
    assert_eq!(before[0]["params"]["sessionId"], s1.as_str());
    assert_eq!(loads.lock().unwrap().clone(), vec![c1.clone()]);

    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":3,"method":"_hotl/session/fork",
               "params":{"sessionId":s1,"cwd":std::env::temp_dir()}}),
    )
    .await;
    read_until_id(&mut lines, 3).await;
    assert_eq!(loads.lock().unwrap()[1], format!("fork:{c1}"));

    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":4,"method":"session/list","params":{"cwd":"/work/chain"}}),
    )
    .await;
    let listed = read_until_id(&mut lines, 4).await;
    let ids: Vec<&str> = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["sessionId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [fork.as_str(), c1.as_str()], "one row per chain");
}

#[tokio::test]
async fn opening_an_unknown_stored_id_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let info = acp::ServerInfo {
        sessions_dir: Some(dir.path().to_path_buf()),
        fallback_models: Vec::new(),
        ..server_info()
    };
    let (mut w, mut lines) = connect(scripted_factory(), info).await;
    let calls = [
        (
            "session/load",
            json!({"sessionId":"01NOPE","cwd":std::env::temp_dir()}),
        ),
        (
            "session/resume",
            json!({"sessionId":"01NOPE","cwd":std::env::temp_dir()}),
        ),
        ("_hotl/session/fork", json!({"sessionId":"../x"})),
    ];
    for (i, (method, params)) in calls.into_iter().enumerate() {
        let id = 10 + i as u64;
        send(
            &mut w,
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )
        .await;
        let r = read_until_id(&mut lines, id).await;
        assert_eq!(r["error"]["code"], -32002, "{method}: {r}");
    }
}

#[tokio::test]
async fn list_matches_a_cwd_by_its_real_path() {
    let dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let real = dunce::canonicalize(work.path()).unwrap();
    let path = {
        let mut log = SessionLog::create(dir.path(), "m", None, Masker::empty(), 0).unwrap();
        log.append(
            &hotl_types::EntryPayload::CwdSet {
                path: real.display().to_string(),
            },
            1,
        )
        .unwrap();
        log.session_id.clone()
    };
    let info = acp::ServerInfo {
        sessions_dir: Some(dir.path().to_path_buf()),
        fallback_models: Vec::new(),
        ..server_info()
    };
    let (mut w, mut lines) = connect(scripted_factory(), info).await;
    let spelled = work.path().join(".");
    send(
        &mut w,
        json!({"jsonrpc":"2.0","id":2,"method":"session/list","params":{"cwd":spelled}}),
    )
    .await;
    let listed = read_until_id(&mut lines, 2).await;
    assert_eq!(
        listed["result"]["sessions"][0]["sessionId"],
        path.as_str(),
        "{listed}"
    );
}
