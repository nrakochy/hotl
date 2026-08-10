//! `hotl bg` — background a session as a **detached socket server** (the ACP
//! solution; no tmux). `hotl bg [prompt]` spawns a `hotl serve` process that
//! outlives your shell and listens on a unix socket, then prints the
//! `hotl attach` command to reach it. Detach/reattach is just connecting and
//! disconnecting the socket; permission asks park until you return.
//!
//! Not tmux, not a nested session — a real background process you attach to.

use std::process::{Command, Stdio};

/// Detach the spawned server from this shell, so it outlives the launcher.
///
/// Unix: its own process group, so a shell's job-control signals stop at us.
/// Windows: `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`, which additionally
/// gives it no console at all — the launcher's console is about to go away and
/// a server holding a handle to it would die with the terminal window.
#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

/// A session id unique to this invocation (the launcher's pid; it exits right
/// after spawning, so the id won't collide with a live server's pid).
pub fn session_id() -> String {
    format!("bg-{}", std::process::id())
}

/// `hotl bg [prompt]`: spawn a detached `hotl serve` and report how to attach.
pub fn bg_main(prompt: Option<&str>, name: Option<&str>) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("hotl bg: cannot find the hotl binary: {e}");
            return 1;
        }
    };
    let id = session_id();
    let log = crate::session_server::run_dir().join(format!("{id}.log"));
    let _ = std::fs::create_dir_all(crate::session_server::run_dir());
    let logfile = std::fs::File::create(&log).ok();

    let mut cmd = Command::new(&exe);
    cmd.arg("serve").arg("--id").arg(&id);
    if let Some(p) = prompt {
        cmd.arg("--prompt").arg(p);
    }
    if let Some(n) = name {
        cmd.arg("--name").arg(n);
    }
    // Detach: no controlling terminal input, output to a log, own process
    // group so the shell's SIGHUP on exit doesn't reach it.
    cmd.stdin(Stdio::null())
        .stdout(
            logfile
                .as_ref()
                .and_then(|f| f.try_clone().ok())
                .map(Stdio::from)
                .unwrap_or_else(Stdio::null),
        )
        .stderr(logfile.map(Stdio::from).unwrap_or_else(Stdio::null));
    detach(&mut cmd);

    match cmd.spawn() {
        Ok(_) => {
            println!("started background session {id}");
            println!("  attach:  hotl attach {id}");
            println!("  list:    hotl attach            (bare)");
            println!("  log:     {}", log.display());
            println!(
                "The session inherits this shell's provider env (HOTL_MODEL + key). \
                 Asks park until you attach and approve them."
            );
            0
        }
        Err(e) => {
            eprintln!("hotl bg: could not start the background session: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_is_prefixed() {
        assert!(session_id().starts_with("bg-"));
    }
}
