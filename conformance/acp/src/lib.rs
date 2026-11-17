use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema::{
    AuthenticateResponse, CloseSessionResponse, CreateElicitationRequest, InitializeResponse,
    ListSessionsResponse, LoadSessionResponse, NewSessionResponse, PromptResponse,
    RequestPermissionRequest, ResumeSessionResponse, SessionNotification,
    SetSessionConfigOptionResponse, SetSessionModeResponse,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, LineDirection};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

pub type Wire = Arc<Mutex<Vec<(LineDirection, String)>>>;

fn built(var: &str, name: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(path) => PathBuf::from(path),
        None => Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug")
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX)),
    }
}

pub fn hotl_bin() -> PathBuf {
    built("HOTL_BIN", "hotl")
}

fn fake_bin() -> PathBuf {
    built("FAKE_ANTHROPIC_BIN", "examples/fake_anthropic")
}

struct Fake {
    child: Child,
    url: String,
}

impl Fake {
    fn start(dir: &Path, script: &[Value]) -> Fake {
        let path = dir.join("script.jsonl");
        let lines: Vec<String> = script.iter().map(Value::to_string).collect();
        std::fs::write(&path, lines.join("\n")).unwrap();
        let bin = fake_bin();
        let mut child = Command::new(&bin)
            .arg(&path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| {
                panic!(
                    "{}: {e}. Build it with `cargo build -p hotl-testkit --example fake_anthropic`",
                    bin.display()
                )
            });
        let mut url = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut url)
            .unwrap();
        Fake {
            child,
            url: url.trim().to_string(),
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct World {
    pub work: PathBuf,
    env: Vec<(String, String)>,
    _fake: Fake,
    _dir: tempfile::TempDir,
}

impl World {
    pub fn new(script: &[Value]) -> World {
        World::with_model(script, "claude-conformance")
    }

    pub fn with_model(script: &[Value], model: &str) -> World {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let config = dir.path().join("config");
        let data = dir.path().join("data");
        let work = dir.path().join("work");
        for d in [&home, &config.join("hotl"), &data, &work] {
            std::fs::create_dir_all(d).unwrap();
        }
        let fake = Fake::start(dir.path(), script);
        std::fs::write(
            config.join("hotl/config.toml"),
            format!(
                "[provider]\nmodel = \"{model}\"\nbase_url = \"{}\"\n\n\
                 [permissions]\nmode = \"ask\"\n\n[behavior]\nsandbox = false\n",
                fake.url
            ),
        )
        .unwrap();
        let path = |p: &Path| p.to_string_lossy().to_string();
        let env = vec![
            ("HOME".into(), path(&home)),
            ("XDG_CONFIG_HOME".into(), path(&config)),
            ("XDG_DATA_HOME".into(), path(&data)),
            ("ANTHROPIC_API_KEY".into(), "conformance".into()),
            ("HOTL_ANTHROPIC_BASE_URL".into(), fake.url.clone()),
        ];
        World {
            work,
            env,
            _fake: fake,
            _dir: dir,
        }
    }

    pub fn agent(&self, wire: &Wire) -> AcpAgent {
        let config = AcpAgentConfig::new(hotl_bin())
            .arg("acp")
            .envs(self.env.iter().cloned());
        let wire = wire.clone();
        AcpAgent::new(config).with_debug(move |line, direction| {
            if direction != LineDirection::Stderr {
                wire.lock().unwrap().push((direction, line.to_string()));
            }
        })
    }
}

fn untagged_default(path: &str, key: &str, value: &Value) -> bool {
    let tool_call_default =
        (key == "status" && value == "pending") || (key == "kind" && value == "other");
    (path.contains("/authMethods/") && key == "type" && value == "agent")
        || ((path.ends_with("/update") || path.ends_with("/toolCall")) && tool_call_default)
}

fn kept(sent: &Value, decoded: &Value, path: &str) -> Result<(), String> {
    match (sent, decoded) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in a {
                let here = format!("{path}/{key}");
                match b.get(key) {
                    Some(other) => kept(value, other, &here)?,
                    None if value.is_null() || untagged_default(path, key, value) => {}
                    None => return Err(format!("{here} was dropped")),
                }
            }
            Ok(())
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => a
            .iter()
            .zip(b)
            .enumerate()
            .try_for_each(|(i, (x, y))| kept(x, y, &format!("{path}/{i}"))),
        (Value::Number(a), Value::Number(b)) if a.as_f64() == b.as_f64() => Ok(()),
        (a, b) if a == b => Ok(()),
        (a, b) => Err(format!("{path}: sent {a}, the SDK read {b}")),
    }
}

fn same_through<T: DeserializeOwned + Serialize>(what: &str, value: &Value, line: &str) {
    let typed: T = serde_json::from_value(value.clone())
        .unwrap_or_else(|e| panic!("{what} does not decode: {e}\n{line}"));
    let back = serde_json::to_value(&typed).unwrap();
    if let Err(e) = kept(value, &back, "") {
        panic!("{what} does not survive the SDK types: {e}\n{line}");
    }
}

pub fn check_wire(wire: &Wire) -> usize {
    let lines = wire.lock().unwrap().clone();
    let mut asked: HashMap<String, String> = HashMap::new();
    let mut checked = 0;
    for (direction, line) in &lines {
        let msg: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("not JSON ({direction:?}): {e}\n{line}"));
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id").map(Value::to_string);
        if *direction == LineDirection::Stdin {
            if let (Some(method), Some(id)) = (method, id) {
                asked.insert(id, method.to_string());
            }
            continue;
        }
        assert_eq!(msg["jsonrpc"], "2.0", "{line}");
        assert!(
            !line.contains("hotl.dev/") && !line.contains("_hotl/"),
            "a hotl extension reached a client that did not opt in\n{line}"
        );
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (Some("session/update"), None) => {
                same_through::<SessionNotification>("session/update", &params, line)
            }
            (Some("session/request_permission"), Some(_)) => {
                same_through::<RequestPermissionRequest>("request_permission", &params, line)
            }
            (Some("elicitation/create"), Some(_)) => {
                same_through::<CreateElicitationRequest>("elicitation/create", &params, line)
            }
            (Some(other), _) => {
                panic!("hotl sent a method a v1 client does not know: {other}\n{line}")
            }
            (None, Some(id)) => {
                if let Some(error) = msg.get("error") {
                    assert!(error["code"].is_i64(), "{line}");
                    assert!(error["message"].is_string(), "{line}");
                    checked += 1;
                    continue;
                }
                let result = msg
                    .get("result")
                    .unwrap_or_else(|| panic!("a reply with neither result nor error\n{line}"));
                match asked.get(&id).map(String::as_str) {
                    Some("initialize") => {
                        same_through::<InitializeResponse>("initialize result", result, line)
                    }
                    Some("authenticate") => {
                        same_through::<AuthenticateResponse>("authenticate result", result, line)
                    }
                    Some("session/new") => {
                        same_through::<NewSessionResponse>("session/new result", result, line)
                    }
                    Some("session/load") => {
                        same_through::<LoadSessionResponse>("session/load result", result, line)
                    }
                    Some("session/prompt") => {
                        same_through::<PromptResponse>("session/prompt result", result, line)
                    }
                    Some("session/list") => {
                        same_through::<ListSessionsResponse>("session/list result", result, line)
                    }
                    Some("session/resume") => {
                        same_through::<ResumeSessionResponse>("session/resume result", result, line)
                    }
                    Some("session/close") => {
                        same_through::<CloseSessionResponse>("session/close result", result, line)
                    }
                    Some("session/set_mode") => same_through::<SetSessionModeResponse>(
                        "session/set_mode result",
                        result,
                        line,
                    ),
                    Some("session/set_config_option") => {
                        same_through::<SetSessionConfigOptionResponse>(
                            "session/set_config_option result",
                            result,
                            line,
                        )
                    }
                    other => panic!("a reply to an unknown request ({other:?})\n{line}"),
                }
            }
            (None, None) => panic!("neither a request, a notification nor a reply\n{line}"),
        }
        checked += 1;
    }
    checked
}

pub fn position(wire: &Wire, needle: impl Fn(&Value) -> bool) -> Option<usize> {
    wire.lock()
        .unwrap()
        .iter()
        .filter(|(d, _)| *d == LineDirection::Stdout)
        .position(|(_, line)| serde_json::from_str(line).is_ok_and(|v: Value| needle(&v)))
}

pub fn request_id(wire: &Wire, method: &str) -> Option<Value> {
    wire.lock()
        .unwrap()
        .iter()
        .filter(|(d, _)| *d == LineDirection::Stdin)
        .filter_map(|(_, line)| serde_json::from_str::<Value>(line).ok())
        .find(|v| v["method"] == method)
        .and_then(|v| v.get("id").cloned())
}
