//! `hotl bg` — background a session as a **detached socket server** (the ACP
//! solution; no tmux). `hotl bg [prompt]` spawns a `hotl serve` process that
//! outlives your shell and listens on a unix socket, then prints the
//! `hotl attach` command to reach it. Detach/reattach is just connecting and
//! disconnecting the socket; permission asks park until you return.
//!
//! Not tmux, not a nested session — a real background process you attach to.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// A session id unique to this invocation (the launcher's pid; it exits right
/// after spawning, so the id won't collide with a live server's pid).
pub fn session_id() -> String {
    format!("bg-{}", std::process::id())
}

/// `hotl bg [prompt]`: spawn a detached `hotl serve` and report how to attach.
pub fn bg_main(prompt: Option<&str>) -> i32 {
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
        .stderr(logfile.map(Stdio::from).unwrap_or_else(Stdio::null))
        .process_group(0);

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
