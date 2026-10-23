//! 0077 T6: a real process speaking ACP, driven by the real `AcpChild` under
//! the real sandbox probe. The agent is a POSIX-sh fixture written at test
//! time (the `agent_plugins_conformance.rs` pattern), so nothing here needs
//! an installed agent or a credential.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use hotl_engine::{EngineEvent, Outcome};
use hotl_tools::net::EgressState;
use hotl_tools::rules::Rules;
use hotl_tools::sandbox::SandboxStatus;

use crate::acp_child;

use acp_child::{AcpChild, AcpSpawn};

/// The fake agent. `$1` is a log it appends hotl's permission answers to,
/// `$2` an outside path it tries to write (or `skip`). On a prompt it writes
/// `inside.txt` in its cwd, starts a `sleep` it records the pid of, announces
/// one `execute` call and asks permission for it with all four options; on
/// the answer it settles the call as hotl decided and ends with a fenced
/// report. A prompt saying `hang` is never answered.
const AGENT: &str = r#"#!/bin/sh
log="$1"; outside="$2"; prompt_id=""
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"protocolVersion\":1,\"agentCapabilities\":{}}}" ;;
    *'"method":"session/new"'*)
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"sessionId\":\"s1\"}}" ;;
    *'"method":"session/prompt"'*)
      prompt_id=$id
      echo inside > "$PWD/inside.txt"
      if [ "$outside" != skip ]; then
        if echo outside > "$outside" 2>/dev/null; then echo "outside-written" >> "$log"; else echo "outside-refused" >> "$log"; fi
      fi
      sleep 300 &
      echo "sleep-pid $!" >> "$log"
      case "$line" in *hang*) continue ;; esac
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"running echo. "}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"echo hi","kind":"execute","status":"pending","rawInput":{"command":"echo hi"}}}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":900,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"t1","title":"echo hi","kind":"execute","rawInput":{"command":"echo hi"}},"options":[{"optionId":"aa","name":"Always","kind":"allow_always"},{"optionId":"a1","name":"Once","kind":"allow_once"},{"optionId":"r1","name":"No","kind":"reject_once"},{"optionId":"ra","name":"Never","kind":"reject_always"}]}}' ;;
    *'"id":900'*)
      echo "answer $line" >> "$log"
      case "$line" in *'"optionId":"a1"'*) status=completed ;; *) status=failed ;; esac
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s1\",\"update\":{\"sessionUpdate\":\"tool_call_update\",\"toolCallId\":\"t1\",\"status\":\"$status\"}}}"
      printf '%s\n' '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done\n```json\n{\"outcome\": \"completed\", \"summary\": \"ran echo\", \"citations\": [\"a.rs:1\"]}\n```\n"}}}}'
      printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":$prompt_id,\"result\":{\"stopReason\":\"end_turn\"}}" ;;
    *'"method":"session/cancel"'*) exit 0 ;;
  esac
done
"#;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    script: PathBuf,
    log: PathBuf,
    report: PathBuf,
}

fn fixture() -> Fixture {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(dir.path()).unwrap();
    let root = base.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let script = base.join("fake-acp-agent");
    std::fs::write(&script, AGENT).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    Fixture {
        log: base.join("agent.log"),
        report: base.join("scratch").join("RESPONSE.json"),
        _dir: dir,
        root,
        script,
    }
}

fn enforced() -> Option<SandboxStatus> {
    match hotl_tools::sandbox::probe() {
        s @ SandboxStatus::Enforced(_) => Some(s),
        _ => None,
    }
}

fn spec(f: &Fixture, rules: Rules, outside: &str) -> AcpSpawn {
    let sandbox = hotl_tools::sandbox::probe();
    AcpSpawn {
        program: f.script.to_string_lossy().into_owned(),
        args: vec![f.log.to_string_lossy().into_owned(), outside.to_string()],
        root: f.root.clone(),
        state_dirs: Vec::new(),
        timeout: Duration::from_secs(30),
        mode: None,
        rules: Arc::new(rules.with_mode(hotl_tools::rules::PermissionMode::DontAsk)),
        hooks: None,
        sandbox_enforced: matches!(sandbox, SandboxStatus::Enforced(_)),
        sandbox,
        egress: Some(EgressState::Open),
        report_path: Some(f.report.clone()),
        grace: Duration::from_millis(500),
    }
}

async fn turn(child: &mut AcpChild) -> Vec<EngineEvent> {
    let mut out = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(30), child.events().recv())
            .await
            .expect("turn timed out")
            .expect("events closed");
        let done = matches!(ev, EngineEvent::TurnDone { .. });
        out.push(ev);
        if done {
            return out;
        }
    }
}

fn log(f: &Fixture) -> String {
    std::fs::read_to_string(&f.log).unwrap_or_default()
}

/// A process is gone when it is missing or a zombie (an orphan's reaper may
/// not have run yet; it no longer runs either way).
fn alive(pid: &str) -> bool {
    let proc_stat = PathBuf::from(format!("/proc/{pid}/stat"));
    if Path::new("/proc/self/stat").exists() {
        return std::fs::read_to_string(proc_stat)
            .ok()
            .and_then(|s| s.rsplit(')').next().map(|r| r.trim().to_string()))
            .is_some_and(|r| !r.starts_with('Z'));
    }
    // The darwin nix sandbox has no `ps`; `kill 0` cannot see a zombie, but
    // the caller polls until the reaper runs.
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", pid])
        .output()
    else {
        return unsafe { libc::kill(pid.parse().unwrap(), 0) } == 0;
    };
    let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
    !stat.is_empty() && !stat.starts_with('Z')
}

#[tokio::test]
async fn a_real_process_agent_completes_the_handshake_prompt_and_report() {
    let f = fixture();
    let mut child = AcpChild::spawn(spec(&f, Rules::default(), "skip"))
        .await
        .expect("handshake");
    assert!(child.pid().is_some());
    child.prompt("list the functions".into());
    let events = turn(&mut child).await;
    match events.last() {
        Some(EngineEvent::TurnDone {
            outcome: Outcome::Done { text },
            usage,
            ..
        }) => {
            assert!(text.contains("running echo"), "{text}");
            assert_eq!(*usage, Default::default());
        }
        other => panic!("{other:?}"),
    }
    let v = hotl_tools::report_tool::read_response(&f.report).expect("RESPONSE.json");
    assert_eq!(v["outcome"], "completed");
    assert_eq!(v["summary"], "ran echo");
    assert!(events.iter().any(
        |e| matches!(e, EngineEvent::ToolDone { name, ok: true, .. } if name == "report_result")
    ));
    child.finish(Duration::from_millis(500)).await;
}

#[tokio::test]
async fn the_permission_reply_is_allow_once_only_when_a_rule_matches() {
    // No rule: rejected once, and the band shows the denial.
    let f = fixture();
    let mut child = AcpChild::spawn(spec(&f, Rules::default(), "skip"))
        .await
        .unwrap();
    child.prompt("go".into());
    let events = turn(&mut child).await;
    assert!(log(&f).contains(r#""optionId":"r1""#), "{}", log(&f));
    assert!(events.iter().any(
        |e| matches!(e, EngineEvent::ToolDenied { id, name } if id == "t1" && name == "bash")
    ));
    child.finish(Duration::from_millis(500)).await;

    // An allow rule: allowed once — where the floor backs bash allow rules.
    let f = fixture();
    let rules = Rules::from_toml("[[allow]]\ntool = \"bash\"\nprefix = \"echo \"\n").unwrap();
    let mut child = AcpChild::spawn(spec(&f, rules, "skip")).await.unwrap();
    child.prompt("go".into());
    let events = turn(&mut child).await;
    let answer = log(&f);
    assert!(
        !answer.contains("\"aa\"") && !answer.contains("\"ra\""),
        "{answer}"
    );
    match enforced() {
        Some(_) => {
            assert!(answer.contains(r#""optionId":"a1""#), "{answer}");
            assert!(events
                .iter()
                .any(|e| matches!(e, EngineEvent::ToolDone { id, ok: true, .. } if id == "t1")));
        }
        // Without the floor a bash allow rule does not auto-approve.
        None => assert!(answer.contains(r#""optionId":"r1""#), "{answer}"),
    }
    child.finish(Duration::from_millis(500)).await;
}

#[tokio::test]
async fn the_fake_agent_runs_in_the_child_root_under_the_floor() {
    let f = fixture();
    // Outside every write root: not the cwd, not a temp dir, not the root.
    let outside = enforced().and_then(|_| {
        use hotl_platform::KnownPaths as _;
        hotl_platform::KNOWN_PATHS.home().map(|h| {
            h.join(format!(".hotl-acp-floor-test-{}", std::process::id()))
                .to_string_lossy()
                .into_owned()
        })
    });
    let mut child = AcpChild::spawn(spec(
        &f,
        Rules::default(),
        outside.as_deref().unwrap_or("skip"),
    ))
    .await
    .unwrap();
    child.prompt("go".into());
    turn(&mut child).await;
    assert_eq!(
        std::fs::read_to_string(f.root.join("inside.txt")).unwrap(),
        "inside\n",
        "the agent's cwd is the child root"
    );
    if let Some(path) = outside {
        let refused = log(&f).contains("outside-refused");
        let _ = std::fs::remove_file(&path);
        assert!(refused, "the floor let the agent write {path}: {}", log(&f));
    }
    child.finish(Duration::from_millis(500)).await;
}

#[tokio::test]
async fn killing_the_parent_token_reaps_the_agent_process_group() {
    let f = fixture();
    let mut child = AcpChild::spawn(spec(&f, Rules::default(), "skip"))
        .await
        .unwrap();
    child.prompt("hang".into());
    // The agent's own child, in its process group.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let pid = loop {
        if let Some(p) = log(&f)
            .lines()
            .find_map(|l| l.strip_prefix("sleep-pid ").map(str::to_string))
        {
            break p;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the agent never started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(alive(&pid));
    child.interrupt();
    let events = turn(&mut child).await;
    assert!(matches!(
        events.last(),
        Some(EngineEvent::TurnDone {
            outcome: Outcome::Cancelled,
            ..
        })
    ));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while alive(&pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "the agent's process group outlived the cancel"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
