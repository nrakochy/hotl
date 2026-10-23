//! Terminal-level smoke scenarios (0074): the real `hotl` binary under a
//! private tmux, pointed at the testkit's scripted Anthropic server. Each
//! scenario is a script of keys and the rows it expects; a frame golden only
//! where the strip is at rest. Skips with a reason where tmux is absent or
//! cannot start — unless `HOTL_TMUX_SMOKE=required`, the Linux `harness` CI
//! leg, where a skip is a failure.
//!
//! Serialized under both runners: nextest's `tmux` test group (one at a
//! time, `.config/nextest.toml`) and this binary's own mutex for `cargo
//! test`. Private servers would not collide; timing assertions want the CPU.

#![cfg(unix)]

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use hotl_testkit::fake_anthropic::{self as fake, FakeAnthropic, Script};
use hotl_testkit::tmux::{normalize_rows, Availability, Launch, Masks, TmuxSession};

static SMOKE: Mutex<()> = Mutex::new(());

/// The strip row at 80×24 — the same index `tui_e2e` draws it at.
const STRIP: usize = 19;
const TRANSCRIPT: Range<usize> = 0..STRIP;

struct SmokeConfig {
    cols: u16,
    rows: u16,
    mode: &'static str,
    vim_mode: bool,
    extra_toml: &'static str,
    editor: Option<String>,
}

impl Default for SmokeConfig {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            mode: "bypass",
            vim_mode: false,
            extra_toml: "",
            editor: None,
        }
    }
}

struct Smoke {
    _dir: tempfile::TempDir,
    fake: FakeAnthropic,
    tmux: TmuxSession,
    work: PathBuf,
    secret: String,
    masks: Masks,
}

impl Drop for Smoke {
    fn drop(&mut self) {
        // The wire check every scenario ends with: nothing the binary sent
        // carries the planted secret. Skipped while unwinding — a second
        // panic would abort with no message.
        if !std::thread::panicking() {
            for body in self.fake.bodies() {
                assert!(
                    !body.contains(&self.secret),
                    "the planted secret reached the wire: {body}"
                );
            }
        }
    }
}

fn masks() -> Masks {
    Masks {
        at_rest: hotl_tui::anim::at_rest(),
        frames: hotl_tui::view::WORKING_FRAMES
            .iter()
            .map(|s| s.to_string())
            .collect(),
        wave_glyphs: "⠀⠁⠂⠃⠄⠅⠆⠇⡀⡁⡂⡃⡄⡅⡆⡇⠈⠉⠊⠋⠌⠍⠎⠏⡈⡉⡊⡋⡌⡍⡎⡏⠐⠑⠒⠓⠔⠕⠖⠗⡐⡑⡒⡓⡔⡕⡖⡗⠘⠙⠚⠛⠜⠝⠞⠟⡘⡙⡚⡛⡜⡝⡞⡟⠠⠡⠢⠣⠤⠥⠦⠧⡠⡡⡢⡣⡤⡥⡦⡧⠨⠩⠪⠫⠬⠭⠮⠯⡨⡩⡪⡫⡬⡭⡮⡯⠰⠱⠲⠳⠴⠵⠶⠷⡰⡱⡲⡳⡴⡵⡶⡷⠸⠹⠺⠻⠼⠽⠾⠿⡸⡹⡺⡻⡼⡽⡾⡿⢀⢁⢂⢃⢄⢅⢆⢇⣀⣁⣂⣃⣄⣅⣆⣇⢈⢉⢊⢋⢌⢍⢎⢏⣈⣉⣊⣋⣌⣍⣎⣏⢐⢑⢒⢓⢔⢕⢖⢗⣐⣑⣒⣓⣔⣕⣖⣗⢘⢙⢚⢛⢜⢝⢞⢟⣘⣙⣚⣛⣜⣝⣞⣟⢠⢡⢢⢣⢤⢥⢦⢧⣠⣡⣢⣣⣤⣥⣦⣧⢨⢩⢪⢫⢬⢭⢮⢯⣨⣩⣪⣫⣬⣭⣮⣯⢰⢱⢲⢳⢴⢵⢶⢷⣰⣱⣲⣳⣴⣵⣶⣷⢸⢹⢺⢻⢼⢽⢾⢿⣸⣹⣺⣻⣼⣽⣾⣿"
            .chars()
            .collect(),
    }
}

/// `None` = skipped (the reason is printed). Holds the serial lock for the
/// scenario's life through the returned guard's `_serial`.
async fn launch(
    cfg: SmokeConfig,
    script: Script,
) -> Option<(Smoke, std::sync::MutexGuard<'static, ()>)> {
    // The fake binds before the lock: the only await in here, and a loopback
    // listener needs no serialization. Everything tmux is behind the lock.
    let fake = FakeAnthropic::start(script).await;
    let serial = SMOKE.lock().unwrap_or_else(|e| e.into_inner());
    match TmuxSession::availability() {
        Availability::Available => {}
        other => {
            if std::env::var("HOTL_TMUX_SMOKE").as_deref() == Ok("required") {
                panic!("HOTL_TMUX_SMOKE=required but tmux is {other:?}");
            }
            eprintln!("skipped: tmux {other:?}");
            return None;
        }
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let cfg_dir = dir.path().join("cfg");
    let data = dir.path().join("data");
    let work = dir.path().join("work");
    for d in [&home, &cfg_dir.join("hotl"), &data, &work] {
        std::fs::create_dir_all(d).unwrap();
    }
    let _ = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&work)
        .output();
    std::fs::write(
        cfg_dir.join("hotl/config.toml"),
        format!(
            "[provider]\nmodel = \"claude-smoke\"\nbase_url = \"{}\"\n\n[permissions]\nmode = \"{}\"\n\n[behavior]\nsandbox = false\nvim_mode = {}\nmouse = false\n{}\n",
            fake.base_url(),
            cfg.mode,
            cfg.vim_mode,
            cfg.extra_toml
        ),
    )
    .unwrap();
    let secret = format!("PLANTED-{}-{}", std::process::id(), fastrand_u64());
    let mut env = vec![
        ("HOME".to_string(), home.to_string_lossy().to_string()),
        (
            "XDG_CONFIG_HOME".to_string(),
            cfg_dir.to_string_lossy().to_string(),
        ),
        (
            "XDG_DATA_HOME".to_string(),
            data.to_string_lossy().to_string(),
        ),
        ("ANTHROPIC_API_KEY".to_string(), "x".to_string()),
        ("HOTL_SMOKE_PLANTED_SECRET".to_string(), secret.clone()),
        ("TERM".to_string(), "xterm-256color".to_string()),
    ];
    if let Some(editor) = &cfg.editor {
        env.push(("EDITOR".to_string(), editor.clone()));
    }
    // The TUI draws on stdout; startup warnings go to stderr, which would
    // land in the pane and corrupt every frame. A file keeps them readable.
    let stderr = dir.path().join("hotl.stderr");
    let tmux = TmuxSession::start(&Launch {
        cols: cfg.cols,
        rows: cfg.rows,
        cwd: work.clone(),
        env,
        cmd: vec![
            "sh".into(),
            "-c".into(),
            format!(
                "exec '{}' -n smoke 2>'{}'",
                env!("CARGO_BIN_EXE_hotl"),
                stderr.display()
            ),
        ],
    })
    .expect("tmux session");
    let smoke = Smoke {
        _dir: dir,
        fake,
        tmux,
        work,
        secret,
        masks: masks(),
    };
    // The deadline absorbs a debug binary's startup.
    let at_rest = smoke.masks.at_rest.clone();
    // At 80×24 the strip is the readiness signal; at other sizes (160×40
    // draws no strip row above the box under tmux — tracker row) the box is.
    let standard = cfg.cols == 80 && cfg.rows == 24;
    smoke.tmux.wait_for(
        "the console to come up",
        |rows| {
            if standard {
                // `starting…` sits on the resting wave until the session opens.
                strip_of(rows).is_some_and(|r| r.starts_with(&at_rest) && !r.contains("starting…"))
            } else {
                rows.iter().any(|r| r.starts_with("  ┌"))
            }
        },
        Duration::from_secs(20),
    );
    Some((smoke, serial))
}

/// The strip is the row just above the composer box, at any terminal size.
fn strip_of(rows: &[String]) -> Option<&String> {
    let boxed = rows.iter().rposition(|r| r.starts_with("  ┌"))?;
    rows.get(boxed.checked_sub(1)?)
}

fn fastrand_u64() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(7)
}

impl Smoke {
    fn normalized(&self) -> Vec<String> {
        normalize_rows(&self.tmux.capture(), TRANSCRIPT, &self.masks)
    }

    fn wait_strip(&self, what: &str, needle: &str, within: Duration) -> Vec<String> {
        self.tmux.wait_for(
            what,
            |rows| strip_of(rows).is_some_and(|r| r.contains(needle)),
            within,
        )
    }

    fn wait_any_row(&self, what: &str, needle: &str, within: Duration) -> Vec<String> {
        self.tmux
            .wait_for(what, |rows| rows.iter().any(|r| r.contains(needle)), within)
    }

    fn wait_at_rest(&self, within: Duration) -> Vec<String> {
        let at_rest = self.masks.at_rest.clone();
        self.tmux.wait_for(
            "the strip back at rest",
            |rows| strip_of(rows).is_some_and(|r| r.starts_with(&at_rest)),
            within,
        )
    }

    fn file(&self, rel: &str) -> PathBuf {
        self.work.join(rel)
    }
}

fn s(secs: u64) -> Duration {
    Duration::from_secs(secs)
}

// --- Task 5: scaffold + first golden ------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_session_boots_to_the_resting_strip_golden() {
    let Some((smoke, _serial)) = launch(SmokeConfig::default(), Script::default()).await else {
        return;
    };
    let rows = smoke.normalized();
    assert_eq!(rows.len(), 24, "{rows:#?}");
    assert!(
        rows[STRIP].starts_with(&smoke.masks.at_rest),
        "{}",
        rows[STRIP]
    );
    assert!(rows[STRIP].contains("claude-smoke"), "{}", rows[STRIP]);
    // The composer box is drawn below the strip and nothing has been typed;
    // the hint line closes the frame.
    assert!(rows[STRIP + 1].starts_with("  ┌"), "{rows:#?}");
    assert!(rows[STRIP + 3].starts_with("  └"), "{rows:#?}");
    assert!(rows[23].starts_with("? help · / commands"), "{rows:#?}");
    assert!(
        !rows.iter().any(|r| r.contains("esc interrupt")),
        "{rows:#?}"
    );
    assert_eq!(smoke.fake.request_count(), 0, "boot sends nothing");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_smoke_sends_its_prompt_to_the_fake_and_reads_the_reply() {
    let Some((smoke, _serial)) = launch(
        SmokeConfig::default(),
        Script::queue(vec![fake::text("all done via tmux").into()]),
    )
    .await
    else {
        return;
    };
    smoke.tmux.type_line("go");
    smoke.wait_any_row("the reply", "all done via tmux", s(15));
    smoke.wait_at_rest(s(10));
    let reqs = smoke.fake.requests();
    assert_eq!(reqs.len(), 1, "{reqs:#?}");
    assert_eq!(reqs[0]["model"], "claude-smoke");
    // The prompt rides as a user text block beside the orientation items.
    let messages = reqs[0]["messages"].as_array().expect("messages");
    assert!(
        messages.iter().all(|m| m["role"] == "user"),
        "{messages:#?}"
    );
    assert!(
        messages.iter().any(|m| m["content"][0]["text"] == "go"),
        "{messages:#?}"
    );
}

// --- Task 6: strip liveness (tracker 184, 87) ---------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_strip_reads_preparing_while_a_long_tool_input_streams_and_never_quiet() {
    let big = "x".repeat(12 * 1024);
    let Some((smoke, _serial)) = launch(
        SmokeConfig::default(),
        Script::queue(vec![
            fake::tool_use_streamed(
                "t1",
                "write",
                serde_json::json!({"path": "big.txt", "content": big}),
                1024,
                s(1),
            )
            .into(),
            fake::text("wrote it").into(),
        ]),
    )
    .await
    else {
        return;
    };
    smoke.tmux.type_line("write the big file");
    smoke.wait_strip("preparing on the strip", "preparing 1 call", s(10));
    // Past QUIET_AFTER (10 s): never `quiet`, and the byte count never drops.
    let deadline = std::time::Instant::now() + s(13);
    let mut last_kb = 0.0f64;
    while std::time::Instant::now() < deadline {
        let rows = smoke.tmux.capture();
        let strip = &rows[STRIP];
        assert!(!strip.contains("quiet"), "{strip}");
        if let Some(kb) = parse_kb(strip) {
            assert!(kb + 0.01 >= last_kb, "bytes went down: {strip}");
            last_kb = kb;
        }
        if !strip.contains("preparing") {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    // The card names big.txt while the input still streams; the follow-up
    // reply is only sampled after the write's result.
    smoke.wait_any_row("the reply after the write", "wrote it", s(20));
    let rows = smoke.wait_at_rest(s(20));
    let on_disk = std::fs::read_to_string(smoke.file("big.txt"))
        .unwrap_or_else(|e| panic!("big.txt on disk: {e}\n{}", rows.join("\n")));
    assert_eq!(on_disk.len(), 12 * 1024);
}

/// `preparing 1 call · 4.2 KB · 3s` → 4.2 (KB); `512 B` → 0.5.
fn parse_kb(strip: &str) -> Option<f64> {
    let i = strip.find("preparing")?;
    let after = &strip[i..];
    let mut parts = after.split('·').map(str::trim);
    parts.next()?;
    let bytes = parts.next()?;
    let (num, unit) = bytes.split_once(' ')?;
    let n: f64 = num.parse().ok()?;
    Some(match unit {
        "B" => n / 1024.0,
        "KB" => n,
        "MB" => n * 1024.0,
        _ => return None,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_card_wanders_and_settles_to_the_hand_off_glyph() {
    let Some((smoke, _serial)) = launch(
        SmokeConfig::default(),
        Script::queue(vec![
            fake::tool_use(
                "t1",
                "bash",
                serde_json::json!({"command": "sleep 2; echo hi"}),
            )
            .into(),
            fake::text("said hi").into(),
        ]),
    )
    .await
    else {
        return;
    };
    smoke.tmux.type_line("say hi slowly");
    let frames = smoke.masks.frames.clone();
    let working_row = |r: &str| {
        let t = r.trim_start();
        frames.iter().any(|f| t.starts_with(f.as_str()))
    };
    // An unsandboxed bash asks even in bypass; answer it, then watch the spine.
    let deadline = std::time::Instant::now() + s(15);
    let mut first: Option<String> = None;
    while std::time::Instant::now() < deadline {
        let rows = smoke.tmux.capture();
        if rows[STRIP].contains("waiting on you") {
            smoke.tmux.send_keys(&["y"]);
        }
        if let Some(r) = rows[..STRIP].iter().find(|r| working_row(r)) {
            first = Some(r.clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let first = first.expect("a working card within 15 s");
    std::thread::sleep(Duration::from_millis(300));
    if let Some(later) = smoke.tmux.capture()[..STRIP]
        .iter()
        .find(|r| working_row(r))
    {
        let glyph = |r: &str| r.trim_start().chars().next();
        assert_ne!(
            glyph(&first),
            glyph(later),
            "the spine did not move: {first} / {later}"
        );
    }
    smoke.wait_any_row("the settled card", "→", s(20));
    smoke.wait_at_rest(s(10));
}

/// Answer any ask that shows up while waiting for `needle` on any row.
fn wait_allowing(smoke: &Smoke, what: &str, needle: &str, within: Duration) -> Vec<String> {
    let deadline = std::time::Instant::now() + within;
    loop {
        let rows = smoke.tmux.capture();
        if rows.iter().any(|r| r.contains(needle)) {
            return rows;
        }
        if rows[STRIP].contains("waiting on you") {
            smoke.tmux.send_keys(&["y"]);
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "timed out waiting for {what}; last frame:\n{}",
                rows.join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// --- Task 7: agent band (tracker 94) ----------------------------------------------

fn three_children() -> Script {
    let spawn = |id: &str, task: &str| {
        fake::tool_use(
            id,
            "spawn",
            serde_json::json!({"agent_type": "explore", "task": task}),
        )
    };
    let report = |text: &str| {
        fake::tool_use(
            "r1",
            "report_result",
            serde_json::json!({"outcome": "completed", "summary": text, "files_touched": [], "commits": [], "citations": []}),
        )
    };
    Script::queue(vec![
        fake::batch(vec![
            spawn("t1", "child-A look around"),
            spawn("t2", "child-B look around"),
            spawn("t3", "child-C look around"),
        ])
        .into(),
        fake::text("three children done").into(),
    ])
    .route("child-A look around", report("A done").after(s(3)))
    .route("child-B look around", report("B done").after(s(6)))
    .route("child-C look around", report("C done").after(s(9)))
    // Each child samples once more after its report; routes are consumed in
    // order, so the second match per child lands here, not on the parent.
    .route("child-A look around", fake::text("A finished"))
    .route("child-B look around", fake::text("B finished"))
    .route("child-C look around", fake::text("C finished"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_band_lists_running_spawns_and_the_esc_ladder_walks_back() {
    let Some((smoke, _serial)) = launch(SmokeConfig::default(), three_children()).await else {
        return;
    };
    smoke.tmux.type_line("go");
    // (a) the band: `● main` plus a row per running child.
    let rows = wait_allowing(&smoke, "the agent band", "● main", s(15));
    assert!(rows.iter().any(|r| r.contains("● main")), "{rows:#?}");
    // (b) highlight the first spawn while others still run; Enter swaps to its
    // stream, Escape walks back without interrupting.
    smoke.tmux.send_keys(&["Down"]);
    smoke.tmux.send_keys(&["Escape"]);
    // (c) rows vanish as children settle; the band is gone after the last.
    smoke.tmux.wait_for(
        "the band gone",
        |rows| !rows.iter().any(|r| r.contains("● main")),
        s(30),
    );
    smoke.wait_any_row("the parent's answer", "three children done", s(20));
    smoke.wait_at_rest(s(10));
    let rows = smoke.tmux.capture();
    assert!(!rows[STRIP].contains("esc interrupt"), "{}", rows[STRIP]);
    // (c) Up recalls the last prompt into the composer once the band is gone.
    smoke.tmux.send_keys(&["Up"]);
    smoke.tmux.wait_for(
        "the recalled prompt",
        |rows| rows[STRIP + 2].contains("go"),
        s(5),
    );
    assert!(
        smoke.fake.request_count() >= 5,
        "parent ×2 + three children"
    );
}

// --- Task 8: composer and modals (tracker 107) ------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn composer_chords_edit_the_line() {
    for vim in [false, true] {
        let Some((smoke, _serial)) = launch(
            SmokeConfig {
                vim_mode: vim,
                ..SmokeConfig::default()
            },
            Script::default(),
        )
        .await
        else {
            return;
        };
        if vim {
            smoke.tmux.send_keys(&["i"]);
        }
        smoke.tmux.type_text("the quick brwon fox");
        smoke.wait_any_row("the typed line", "the quick brwon fox", s(5));
        // Back over `n fox`: the cursor sits after `brwo`; two backspaces
        // and `ow` make `brown`.
        for _ in 0..5 {
            smoke.tmux.send_keys(&["Left"]);
        }
        smoke.tmux.send_keys(&["BSpace", "BSpace"]);
        smoke.tmux.type_text("ow");
        smoke.wait_any_row("the fixed word", "the quick brown fox", s(5));
        smoke.tmux.send_keys(&["C-e", "C-w"]);
        smoke.tmux.wait_for(
            "ctrl-w took the last word",
            |rows| {
                rows.iter().any(|r| r.contains("the quick brown"))
                    && !rows.iter().any(|r| r.contains("brown fox"))
            },
            s(5),
        );
        smoke.tmux.send_keys(&["C-a", "C-k"]);
        smoke.tmux.wait_for(
            "ctrl-k cleared the line",
            |rows| !rows.iter().any(|r| r.contains("the quick")),
            s(5),
        );
        assert_eq!(
            smoke.fake.request_count(),
            0,
            "vim={vim}: editing sends nothing"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_g_round_trips_the_composer_through_editor() {
    let dir = tempfile::tempdir().unwrap();
    let editor = dir.path().join("editor.sh");
    std::fs::write(&editor, "#!/bin/sh\nprintf ' edited' >> \"$1\"\n").unwrap();
    let mut perms = std::fs::metadata(&editor).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&editor, perms).unwrap();
    let Some((smoke, _serial)) = launch(
        SmokeConfig {
            editor: Some(editor.to_string_lossy().to_string()),
            ..SmokeConfig::default()
        },
        Script::default(),
    )
    .await
    else {
        return;
    };
    smoke.tmux.type_text("draft");
    smoke.wait_any_row("the draft", "draft", s(5));
    smoke.tmux.send_keys(&["C-g"]);
    smoke.wait_any_row("the editor's append", "draft edited", s(10));
    assert_eq!(smoke.fake.request_count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slash_completion_and_help_name_the_right_things() {
    let Some((smoke, _serial)) = launch(SmokeConfig::default(), Script::default()).await else {
        return;
    };
    smoke.tmux.type_text("/eff");
    smoke.tmux.send_keys(&["Tab"]);
    smoke.wait_any_row("the completed command", "/effort", s(5));
    smoke.tmux.send_keys(&["C-a", "C-k"]);
    smoke.tmux.type_text("?");
    let rows = smoke.tmux.wait_for(
        "the help box",
        |rows| rows.iter().any(|r| r.contains("/goal")),
        s(5),
    );
    let all = rows.join("\n");
    assert!(all.contains("/workflows"), "{all}");
    assert!(all.contains("ctrl-g"), "{all}");
    smoke.tmux.send_keys(&["Escape"]);
    smoke.tmux.wait_for(
        "the help box closed",
        |rows| !rows.iter().any(|r| r.contains("/goal")),
        s(5),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_user_and_permission_modals_look_different() {
    let Some((smoke, _serial)) = launch(
        SmokeConfig {
            mode: "ask",
            ..SmokeConfig::default()
        },
        Script::queue(vec![
            fake::tool_use(
                "t1",
                "ask_user",
                serde_json::json!({"header": "Scope", "prompt": "How far?", "options": [{"label": "MVP"}, {"label": "Full"}, {"label": "Later"}]}),
            )
            .into(),
            fake::tool_use("t2", "bash", serde_json::json!({"command": "echo hi"})).into(),
            fake::text("all done").into(),
        ]),
    )
    .await
    else {
        return;
    };
    smoke.tmux.type_line("decide");
    let question = smoke.wait_any_row("the question modal", "How far?", s(15));
    assert!(
        question.iter().any(|r| r.contains("Scope")),
        "{question:#?}"
    );
    smoke.tmux.send_keys(&["Down", "Down", "Enter"]);
    let ask = smoke.tmux.wait_for(
        "the permission modal",
        |rows| rows.iter().any(|r| r.contains("echo hi")) && rows[STRIP].contains("waiting on you"),
        s(15),
    );
    assert_ne!(question, ask, "the two modals must look different");
    smoke.tmux.send_keys(&["y"]);
    smoke.wait_any_row("the answer", "all done", s(15));
    let reqs = smoke.fake.requests();
    assert_eq!(reqs.len(), 3);
    assert!(
        reqs[1].to_string().contains("Later"),
        "the third option: {}",
        reqs[1]
    );
}

// --- Task 9: layout (tracker 113) --------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_shows_a_stop_glyph_and_no_interrupt_hint() {
    let Some((smoke, _serial)) = launch(
        SmokeConfig {
            mode: "ask",
            ..SmokeConfig::default()
        },
        Script::queue(vec![
            fake::tool_use("t1", "bash", serde_json::json!({"command": "echo no"})).into(),
            fake::text("denied, so be it").into(),
        ]),
    )
    .await
    else {
        return;
    };
    smoke.tmux.type_line("try");
    smoke.wait_any_row("the ask", "echo no", s(15));
    smoke.tmux.send_keys(&["n"]);
    smoke.wait_any_row("the deny-reason field", "deny reason", s(5));
    smoke.tmux.send_keys(&["Enter"]);
    smoke.wait_any_row("the denied card", "⊘", s(15));
    smoke.wait_at_rest(s(10));
    let rows = smoke.tmux.capture();
    assert!(
        !rows.iter().any(|r| r.contains("esc interrupt")),
        "{rows:#?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_strip_stays_put_when_the_first_answer_overflows() {
    let long: String = (1..=40)
        .map(|i| format!("line {i} of a long answer"))
        .collect::<Vec<_>>()
        .join("\n");
    let Some((smoke, _serial)) = launch(
        SmokeConfig::default(),
        Script::queue(vec![fake::text("short").into(), fake::text(&long).into()]),
    )
    .await
    else {
        return;
    };
    smoke.tmux.type_line("first");
    smoke.wait_any_row("the short answer", "● short", s(15));
    smoke.wait_at_rest(s(10));
    smoke.tmux.type_line("second");
    smoke.wait_any_row("the long answer's tail", "line 40 of a long answer", s(20));
    let rows = smoke.wait_at_rest(s(10));
    assert!(rows[STRIP].starts_with(&smoke.masks.at_rest), "{rows:#?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_answer_wraps_inside_the_measure_on_160_columns() {
    let answer = "word ".repeat(80);
    let Some((smoke, _serial)) = launch(
        SmokeConfig {
            cols: 160,
            rows: 40,
            ..SmokeConfig::default()
        },
        Script::queue(vec![fake::text(answer.trim()).into()]),
    )
    .await
    else {
        return;
    };
    smoke.tmux.type_line("talk");
    let rows = smoke.tmux.wait_for(
        "the wrapped answer",
        |rows| rows.iter().filter(|r| r.contains("word word")).count() >= 2,
        s(15),
    );
    for r in rows.iter().filter(|r| r.contains("word word")) {
        let last = r.trim_end().chars().count();
        assert!(
            last <= 116,
            "a prose row runs past the measure ({last} cols): {r}"
        );
    }
}
