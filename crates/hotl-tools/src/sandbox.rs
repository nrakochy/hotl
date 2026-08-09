//! The kernel sandbox floor for `bash` (SECURITY.md layer 3).
//!
//! Write-confinement: the command (and its whole process tree) writes only
//! under the working directory, the temp dir, and /dev.
//!
//! Read-confinement is narrower and newer (plan 0022). Reads are open
//! *except* for two carves: hotl's own config and data dirs (the session
//! token there drives the session socket, which is the permission gate) and
//! the credential class — `~/.ssh`, `~/.aws`, `~/.config/gcloud`, `~/.azure`
//! and the credential dotfiles. The first is never liftable; the second is
//! lifted per-entry by `[sandbox].readable` and per-command by the ask.
//! Everything else — including secrets *inside* the working tree — is still
//! readable, and `[network].egress` is still open by default, so this is not
//! exfiltration-proofing. See `docs/SECURITY.md`.
//!
//! `[network].egress` in config.toml opts into egress confinement (see
//! `net.rs`), which this module enforces at the kernel when asked to.
//!
//! - macOS: Seatbelt via `sandbox-exec` (deprecated by Apple, still the
//!   mechanism its own tooling uses; profile passed inline with parameters).
//! - Linux: Landlock (kernel ≥ 5.13), applied in `pre_exec` after fork.
//! - Anywhere else, or kernels without Landlock: **fail-closed degradation**
//!   to the M0 posture — the command still runs only behind the y/n gate, and
//!   the ask is loudly marked UNSANDBOXED.
//!
//! `HOTL_SANDBOX=off` is the documented escape hatch (marked in the ask).

use std::path::PathBuf;

use crate::net::EgressState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxStatus {
    /// Confinement active; the str names the mechanism.
    Enforced(&'static str),
    /// No floor on this host; reason attached. The y/n gate is the only guard.
    Unavailable(String),
    /// Explicitly disabled via HOTL_SANDBOX=off.
    Disabled,
}

/// Whether the container/orchestrator daemon socket class is reachable from a
/// confined command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnixSockets {
    /// Deny the container/orchestrator daemon socket class (default).
    DenyDaemons,
    /// Everything allowed — `HOTL_UNIX_SOCKETS=open`. Labeled in every ask.
    Open,
}

/// Whether a confined command may drive other applications through Apple
/// Events / LaunchServices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Automation {
    Deny,
    Allow,
}

fn unix_socket_policy() -> UnixSockets {
    if std::env::var("HOTL_UNIX_SOCKETS").is_ok_and(|v| v == "open") {
        UnixSockets::Open
    } else {
        UnixSockets::DenyDaemons
    }
}

fn automation_policy() -> Automation {
    if std::env::var("HOTL_MACOS_AUTOMATION").is_ok_and(|v| v == "allow") {
        Automation::Allow
    } else {
        Automation::Deny
    }
}

/// Whether the credential tier of the read-carve is still denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reads {
    /// The default: `~/.ssh` and the credential class are unreadable.
    Carved,
    /// An operator lifted the whole credential tier via `[sandbox].readable`.
    Open,
}

impl SandboxStatus {
    pub fn label(&self) -> String {
        match self {
            SandboxStatus::Enforced(m) => {
                label_with(m, unix_socket_policy(), automation_policy(), read_posture())
            }
            SandboxStatus::Unavailable(_) => "UNSANDBOXED".to_string(),
            SandboxStatus::Disabled => "UNSANDBOXED(by HOTL_SANDBOX=off)".to_string(),
        }
    }
}

/// A carve was installed (Tier A is present) but its credential tier is
/// empty — the only way that happens is an operator lifting it. An
/// un-inited process has *both* tiers empty and gets no marker: silence is
/// the hardened state, and inventing a marker there would put one on every
/// unit test.
fn read_posture() -> Reads {
    // The probe found the carve unenforceable on this host: say so rather
    // than let the ask imply a confinement that is not there. Per decision 5
    // this labels, it does not revoke bash's allow-rules.
    if read_carve_enforced() == Some(false) {
        return Reads::Open;
    }
    let deny = read_deny();
    if !deny.always.is_empty() && deny.secrets.is_empty() {
        Reads::Open
    } else {
        Reads::Carved
    }
}

/// Pure renderer. Silence is the hardened default; a marker appears only
/// where the operator opted *out* of a denial — the same convention
/// `net::label_suffix` uses (no marker under the default Open egress).
fn label_with(mechanism: &str, unix: UnixSockets, automation: Automation, reads: Reads) -> String {
    let mut out = format!("sandboxed:{mechanism}");
    if matches!(unix, UnixSockets::Open) {
        out.push_str(" unix:open");
    }
    if matches!(automation, Automation::Allow) {
        out.push_str(" automation:allow");
    }
    if matches!(reads, Reads::Open) {
        out.push_str(" reads:open");
    }
    out
}

fn canon(p: PathBuf) -> PathBuf {
    p.canonicalize().unwrap_or(p)
}

/// How far the *tool-level* boundary of the mutating file tools (`write`,
/// `edit`) extends. `[sandbox].file_tools` in config.toml selects the mode
/// from the well-known values `"workspace"` (default) and `"writable"`;
/// unknown values fail closed to `Workspace` at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileToolsMode {
    /// `write`/`edit` stay confined to the workspace root (default).
    #[default]
    Workspace,
    /// `write`/`edit` may also write under the `[sandbox].writable` roots.
    Writable,
}

/// The owner-configured widening of the floor — `[sandbox]` in config.toml,
/// resolved and validated by the config layer before it reaches this module
/// (paths are canonical and never hotl's own config or data dir).
#[derive(Debug, Default)]
pub struct SandboxExtras {
    /// Extra write roots appended to the kernel write set for every
    /// sandboxed spawn.
    pub writable: Vec<PathBuf>,
    /// Whether the mutating file tools also honor `writable`.
    pub file_tools: FileToolsMode,
    /// Absolute, canonical paths sandboxed children may not read (plan 0022).
    pub read_deny: ReadDeny,
}

/// The kernel read-carve, in three tiers.
///
/// The split is the whole design: Tier A is what would let a confined child
/// escalate into hotl itself (the session token drives the socket, which is
/// the permission gate), so it is never liftable. Tier B is the credential
/// class — worth denying by default, but an operator with a real need can
/// lift it per-entry with `[sandbox].readable` or per-command at the ask.
/// Tier C is what the owner's own `[[deny]]` rules imply (plan 0025).
#[derive(Debug, Default, Clone)]
pub struct ReadDeny {
    /// hotl's own config and data dirs. Never liftable, by config or grant.
    pub always: Vec<PathBuf>,
    /// The credential class. Already filtered by `[sandbox].readable`; the
    /// per-command grant drops what remains for one spawn.
    pub secrets: Vec<PathBuf>,
    /// Paths denied by a projected `[[deny]]` rule (plan 0025). Unliftable,
    /// because `Rules::evaluate` makes deny unconditional in-process and a
    /// kernel tier that could be lifted while its in-process twin could not is
    /// exactly the drift 0025 exists to remove.
    ///
    /// Deliberately NOT `always`: that field also drives the file-tool refusal
    /// message (`builtins::hotl_own_dir_reason`) and the read-probe canary
    /// location, neither of which should follow a user's rule — the canary case
    /// would have hotl writing files into a directory the owner denied.
    pub rules: Vec<PathBuf>,
}

impl ReadDeny {
    /// The paths a given spawn must not read. `secret_reads` is the
    /// per-command grant: it drops Tier B and never touches Tier A or C.
    pub(crate) fn for_spawn(&self, secret_reads: bool) -> Vec<PathBuf> {
        let mut out = self.always.clone();
        if !secret_reads {
            out.extend(self.secrets.iter().cloned());
        }
        out.extend(self.rules.iter().cloned());
        out
    }

    pub fn is_empty(&self) -> bool {
        self.always.is_empty() && self.secrets.is_empty() && self.rules.is_empty()
    }
}

/// The `$HOME`-relative credential paths the read-carve denies, absolutized
/// against `home`. Shares `CREDENTIAL_DIRS`/`CREDENTIAL_FILES` with the
/// execute-later *write* classification so the read set and the write set
/// cannot drift. Non-existent entries are kept — the kernel layers treat a
/// missing path as a no-op, and dropping them here would silently un-deny a
/// directory created later in the session.
pub fn credential_read_paths(home: &std::path::Path) -> Vec<PathBuf> {
    crate::CREDENTIAL_DIRS
        .iter()
        .map(|d| home.join(d.trim_end_matches('/')))
        .chain(crate::CREDENTIAL_FILES.iter().map(|f| home.join(f)))
        .collect()
}

static EXTRAS: std::sync::OnceLock<SandboxExtras> = std::sync::OnceLock::new();

/// Install the configured extras, process-wide and set-once (the `net::init`
/// convention — nothing downstream can widen or replace the set afterwards).
///
/// INVARIANT: must run before the first `probe()` call. The probe picks its
/// outside-the-floor target from `write_roots()`, and its memoized verdict
/// must describe the floor the children actually get. An un-inited process
/// falls back to the empty set — narrower than configured, never wider.
pub fn init_extras(extras: SandboxExtras) {
    let _ = EXTRAS.set(extras);
}

pub(crate) fn extra_writable() -> &'static [PathBuf] {
    EXTRAS.get().map(|e| e.writable.as_slice()).unwrap_or(&[])
}

pub(crate) fn file_tools_mode() -> FileToolsMode {
    EXTRAS.get().map(|e| e.file_tools).unwrap_or_default()
}

/// The resolved read-carve. An un-inited process gets the **empty** set —
/// narrower than configured, never wider, matching the `init_extras`
/// invariant above. `read_deny_static` exists so callers can hold a
/// `&'static` without cloning on every spawn.
pub fn read_deny() -> &'static ReadDeny {
    static EMPTY: ReadDeny = ReadDeny {
        always: Vec::new(),
        secrets: Vec::new(),
        rules: Vec::new(),
    };
    EXTRAS.get().map(|e| &e.read_deny).unwrap_or(&EMPTY)
}

tokio::task_local! {
    /// Per-command lift of the Tier B read-deny, scoped by `turn.rs` around
    /// the single `Tool::run` future so it can only ever cover one call
    /// (plan 0022, decision 3). Deliberately out of band: the model writes
    /// the tool input, so a grant that lived there would be self-granted.
    pub static SECRET_READS: bool;
}

/// Whether the current task carries the per-command secret-read grant.
/// Absent scope (every path but an approved bash call) means no grant.
pub(crate) fn secret_reads_granted() -> bool {
    SECRET_READS.try_with(|g| *g).unwrap_or(false)
}

/// The deny set for the spawn happening right now.
pub(crate) fn spawn_read_deny() -> Vec<PathBuf> {
    read_deny().for_spawn(secret_reads_granted())
}

/// The complete kernel write set — the single source of truth, consumed by
/// the Seatbelt profile, the Landlock ruleset, and `probe_dir` alike, so the
/// enforcement and its confinement proof can never disagree.
pub(crate) fn write_roots() -> Vec<PathBuf> {
    write_roots_with(extra_writable())
}

/// Public so the config layer can validate a read-deny entry against the
/// write grants: Landlock resolves against the *closest* matching rule, so a
/// `from_all` write grant on an ancestor re-opens the read it is meant to
/// deny (plan 0022, watch-out 7).
pub fn write_roots_with(extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = vec![
        canon(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
        canon(std::env::temp_dir()),
        PathBuf::from("/private/tmp"),
        PathBuf::from("/dev"),
    ];
    roots.extend(extra.iter().cloned());
    roots
}

/// Hard ceiling on the smoke test. A wedged `sandbox-exec` must never wedge
/// startup; on expiry the child is killed and the verdict is Unavailable
/// (fail-closed).
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Probe what this host can enforce.
///
/// INVARIANT: `Enforced(m)` is returned only after a child spawned under `m`
/// *failed* to create a file outside the confinement on this host — the
/// mechanism's presence on disk is never sufficient (D-9). Enforced by
/// `tests/sandbox_probe.rs::probe_refuses_a_mechanism_that_does_not_confine`.
///
/// Memoized: exactly one smoke-test spawn per process. `builtins.rs`,
/// `doctor.rs`, `shell_hooks.rs` and `agent.rs` all call this directly.
pub fn probe() -> SandboxStatus {
    static STATUS: std::sync::OnceLock<SandboxStatus> = std::sync::OnceLock::new();
    STATUS.get_or_init(probe_uncached).clone()
}

fn probe_uncached() -> SandboxStatus {
    if std::env::var("HOTL_SANDBOX").is_ok_and(|v| v == "off") {
        return SandboxStatus::Disabled;
    }
    let mechanism = match mechanism_available() {
        Ok(m) => m,
        Err(reason) => return SandboxStatus::Unavailable(reason),
    };
    match verify_confinement_with(mechanism, |script| {
        build_command(
            script,
            &SandboxStatus::Enforced(mechanism),
            &EgressState::Open,
        )
    }) {
        Ok(()) => SandboxStatus::Enforced(mechanism),
        Err(reason) => SandboxStatus::Unavailable(reason),
    }
}

/// Is the mechanism *present*? (The old `probe` stopped here — that is D-9.)
fn mechanism_available() -> Result<&'static str, String> {
    #[cfg(target_os = "macos")]
    {
        if std::path::Path::new("/usr/bin/sandbox-exec").exists() {
            return Ok("seatbelt");
        }
        return Err("sandbox-exec not found".into());
    }
    #[cfg(target_os = "linux")]
    {
        return linux_mechanism();
    }
    #[allow(unreachable_code)]
    Err("no sandbox mechanism for this OS".into())
}

/// The highest Landlock ABI this kernel actually honors. `HardRequirement`
/// makes an unsupported level an error rather than a silent downgrade, so the
/// first level that builds is the truth. `None` = no Landlock at all.
///
/// (`landlock`'s own current-ABI query is private in 0.4.5 — the ladder is the
/// supported public route. The list is bounded by the ABI levels that crate
/// version knows about; a newer kernel simply reports the highest it names.)
#[cfg(target_os = "linux")]
fn landlock_abi() -> Option<landlock::ABI> {
    use landlock::{Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, ABI};
    static ABI_LEVEL: std::sync::OnceLock<Option<ABI>> = std::sync::OnceLock::new();
    *ABI_LEVEL.get_or_init(|| {
        for abi in [
            ABI::V7,
            ABI::V6,
            ABI::V5,
            ABI::V4,
            ABI::V3,
            ABI::V2,
            ABI::V1,
        ] {
            let ok = Ruleset::default()
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(AccessFs::from_all(abi))
                .and_then(|r| r.create());
            if ok.is_ok() {
                return Some(abi);
            }
        }
        None
    })
}

/// Which Landlock posture this kernel earns. ABI v3 (Linux 6.2) is the first
/// level carrying the truncate right; below it the floor is genuinely partial
/// and is never certified silently.
#[cfg(target_os = "linux")]
fn linux_mechanism() -> Result<&'static str, String> {
    let Some(abi) = landlock_abi() else {
        return Err("landlock unavailable: kernel has no Landlock support".into());
    };
    if (abi as i32) >= 3 {
        return Ok("landlock");
    }
    if std::env::var("HOTL_SANDBOX").is_ok_and(|v| v == "best-effort") {
        return Ok("landlock(partial)");
    }
    Err(format!(
        "landlock ABI v{} (kernel < 6.2): the truncate right does not exist here, so a \
         `truncate(2)` by path outside the workspace is unconfined. Set \
         HOTL_SANDBOX=best-effort to accept the partial floor (every ask is labeled \
         `sandboxed:landlock(partial)`), or upgrade the kernel.",
        abi as i32
    ))
}

/// A directory we can really write to that is **outside** everything the
/// *write* floor re-allows (`write_roots()`: cwd, `TMPDIR`, `/private/tmp`, `/dev`,
/// and any configured `[sandbox].writable` entry). Returning it is a
/// positive control: the parent creates and deletes a file here first, so a
/// later "the file does not exist" assertion cannot pass merely because the
/// path was unwritable anyway (the vacuous-negative trap).
pub fn probe_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("HOTL_SANDBOX_PROBE_DIR") {
        return writable(&canon(PathBuf::from(dir)));
    }
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from("/var/tmp")];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home));
    }
    probe_dir_from(&write_roots(), candidates)
}

/// The pure search: first candidate that is outside every `allowed` root and
/// really writable. `allowed` must be canonical; candidates are canonicalized
/// here.
fn probe_dir_from(allowed: &[PathBuf], candidates: Vec<PathBuf>) -> Result<PathBuf, String> {
    let mut last = "no candidate directory outside the confinement".to_string();
    for cand in candidates {
        let cand = canon(cand);
        if allowed.iter().any(|a| cand.starts_with(a)) {
            continue; // inside the write set — proves nothing
        }
        match writable(&cand) {
            Ok(dir) => return Ok(dir),
            Err(e) => last = e,
        }
    }
    Err(format!(
        "cannot verify the sandbox: {last}. Set HOTL_SANDBOX_PROBE_DIR to a writable \
         directory outside the working directory, TMPDIR, and any [sandbox].writable entry."
    ))
}

fn writable(dir: &std::path::Path) -> Result<PathBuf, String> {
    let probe = dir.join(format!("hotl-sbx-writable-{}", std::process::id()));
    std::fs::write(&probe, b"x").map_err(|e| format!("{} is not writable: {e}", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(dir.to_path_buf())
}

/// Single-quote a path for `sh -c`. `None` when the path cannot be quoted
/// safely (a NUL or a non-UTF-8 component) — the caller then falls through to
/// a fail-closed verdict rather than building a dubious shell word.
fn shell_single_quote(p: &std::path::Path) -> Option<String> {
    let s = p.to_str()?;
    if s.contains('\0') {
        return None;
    }
    Some(format!("'{}'", s.replace('\'', r"'\''")))
}

/// Whether the plan-0022 read-carve was proven enforced by `probe()`.
/// `None` before the probe has run, or on a host where there was nothing to
/// prove (an empty Tier A). `hotl doctor` prints it and the ask label marks
/// `reads:open` when it is `Some(false)`.
pub fn read_carve_enforced() -> Option<bool> {
    READ_CARVE_ENFORCED.get().copied()
}

static READ_CARVE_ENFORCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// The exit-code base the probe script uses. Anything outside `BASE..BASE+4`
/// means the child never reached our `exit` — a `sandbox-exec` that refused
/// the profile, a killed process — and is reported as *unverifiable* rather
/// than silently read as an escape.
const PROBE_EXIT_BASE: i32 = 10;

/// Spawn a child under `build` that tries to (1) create a file outside the
/// write confinement and (2) read a canary inside the read-carve, and certify
/// the mechanism only if the write failed **and** left nothing on disk.
/// `build` is injected so the negative case is testable on any host, with no
/// sandbox required.
///
/// One spawn, not two: `probe_is_memoized_so_it_costs_one_spawn_per_process`
/// pins the cost at one child per process.
///
/// **A failed read-carve does not demote the verdict** (plan 0022, task 7).
/// Write-confinement is the older, load-bearing claim that gates `bash`
/// allow-rules; demoting on a host that cannot narrow reads would revoke
/// auto-approval for a restriction nobody opted into — the regression
/// decision 5 rejects. The host is labeled `reads:open` instead.
pub fn verify_confinement_with(
    mechanism: &str,
    build: impl Fn(&str) -> tokio::process::Command,
) -> Result<(), String> {
    let dir = probe_dir()?;
    let stamp = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let target = dir.join(format!("hotl-sbx-probe-{stamp}"));
    let _ = std::fs::remove_file(&target);
    let quoted = shell_single_quote(&target)
        .ok_or_else(|| format!("cannot verify {mechanism}: unquotable probe path"))?;

    let canary = read_probe_canary(&stamp);
    let read_script = match canary.as_ref().and_then(|c| shell_single_quote(c)) {
        Some(q) => format!("cat {q} >/dev/null 2>&1 && r=1"),
        None => "true".to_string(),
    };
    // The write runs in a subshell so a shell that treats a failed redirect
    // as fatal kills only the subshell — otherwise a working sandbox would
    // look unverifiable. Exit code carries both bits: +2 write escaped,
    // +1 read escaped.
    let script = format!(
        "w=0; r=0; ( echo hotl-probe > {quoted} ) 2>/dev/null && w=1; {read_script}; \
         exit $(( {PROBE_EXIT_BASE} + w * 2 + r ))"
    );
    let outcome = run_bounded(build(&script));
    let leaked = target.exists();
    if leaked {
        let _ = std::fs::remove_file(&target);
    }
    // Delete the canary on *every* path, including the escape path — the same
    // rule `tests/sandbox_probe_leak.rs` enforces for the write probe.
    if let Some(c) = &canary {
        let _ = std::fs::remove_file(c);
    }
    let code = match outcome {
        Ok(code) => code,
        Err(e) => return Err(format!("`{mechanism}` could not be verified: {e}")),
    };
    let Some(bits) = code
        .checked_sub(PROBE_EXIT_BASE)
        .filter(|b| (0..4).contains(b))
    else {
        return Err(format!(
            "`{mechanism}` could not be verified: the probe child exited {code} without \
             reaching its own `exit` — the profile/ruleset was probably rejected"
        ));
    };
    if canary.is_some() {
        let _ = READ_CARVE_ENFORCED.set(bits & 1 == 0);
    }
    if leaked || bits & 2 != 0 {
        return Err(format!(
            "`{mechanism}` did not confine a write to {} — the profile/ruleset is not \
             being applied on this host, so bash cannot be auto-approved",
            target.display()
        ));
    }
    Ok(())
}

/// Plant a canary inside Tier A and prove the *parent* can read it back.
/// Without that positive control a "the child could not read it" result
/// proves nothing — the file might simply be unreadable to everyone.
/// `None` means there was nothing to test (no carve, or no Tier A dir that
/// exists and round-trips), and the read half is skipped rather than faked.
fn read_probe_canary(stamp: &str) -> Option<PathBuf> {
    for dir in &read_deny().always {
        let path = dir.join(format!("hotl-sbx-read-probe-{stamp}"));
        if std::fs::write(&path, b"hotl-read-probe").is_err() {
            continue;
        }
        if std::fs::read(&path).is_ok_and(|b| b == b"hotl-read-probe") {
            return Some(path);
        }
        let _ = std::fs::remove_file(&path);
    }
    None
}

/// Run to completion under `PROBE_TIMEOUT` using the **std** command inside
/// the tokio one: `probe()` is sync and is called from contexts with and
/// without a tokio runtime (`doctor.rs` vs `builtins.rs`), so it must not
/// depend on a reactor. `as_std_mut` keeps any `pre_exec` closure the Landlock
/// path installed. Returns Ok(exit code); a signal death reports -1, which is
/// outside the probe's own code range and so reads as unverifiable.
fn run_bounded(mut cmd: tokio::process::Command) -> std::io::Result<i32> {
    let std_cmd = cmd.as_std_mut();
    std_cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = std_cmd.spawn()?;
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(status) => return Ok(status.code().unwrap_or(-1)),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("probe did not exit within {PROBE_TIMEOUT:?}"),
                ));
            }
            None => std::thread::sleep(std::time::Duration::from_millis(5)),
        }
    }
}

/// Build the command for `sh -c <command>` under the active floor and the
/// resolved egress state. With `EgressState::Open` the result is byte-identical
/// to the pre-egress behavior.
pub fn build_command(
    command: &str,
    status: &SandboxStatus,
    egress: &EgressState,
) -> tokio::process::Command {
    let cmd = match status {
        SandboxStatus::Enforced("seatbelt") => seatbelt_command(command, egress),
        #[cfg(target_os = "linux")]
        SandboxStatus::Enforced("landlock") | SandboxStatus::Enforced("landlock(partial)") => {
            landlock_command(command, egress)
        }
        _ => {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(command);
            cmd
        }
    };
    apply_child_env(cmd, egress)
}

/// Put a spawned tool child in its own session (Vuln 2): with no controlling
/// terminal it cannot `ioctl(TIOCSTI)` keystrokes into hotl's approval prompt
/// or paint spoofed UI onto `/dev/tty`. `setsid` also makes the child a
/// process-group leader with pgid == pid, preserving the `kill(-pid)` group
/// reap that `.process_group(0)` used to provide (see `builtins::kill_group`).
/// Stacks after any Landlock `pre_exec` already installed by the builders.
pub fn detach_session(cmd: &mut tokio::process::Command) {
    // SAFETY: `setsid` is async-signal-safe and touches no shared state; it is
    // the only work done between fork and exec here.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Build a direct `program arg...` invocation (no shell) under the active
/// floor and the resolved egress state — the argv-safe sibling of
/// `build_command`, for callers (like `grep`) that must never splice a
/// model-authored value into a shell string. Same sandbox floor, same proxy
/// env wiring; only the exec shape differs (`program`+`args` vs `sh -c`).
pub fn build_argv(
    program: &str,
    args: &[String],
    status: &SandboxStatus,
    egress: &EgressState,
) -> tokio::process::Command {
    let cmd = match status {
        SandboxStatus::Enforced("seatbelt") => seatbelt_argv(program, args, egress),
        #[cfg(target_os = "linux")]
        SandboxStatus::Enforced("landlock") | SandboxStatus::Enforced("landlock(partial)") => {
            landlock_argv(program, args, egress)
        }
        _ => {
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args);
            cmd
        }
    };
    apply_child_env(cmd, egress)
}

/// Credentials the *harness* holds. A child process — a model-authored bash
/// command, `rg`, a diagnostic, an owner hook — has no legitimate need for
/// any of them, and an auto-approved `bash` can otherwise read them with
/// `env`. Deliberately narrow: the Masker's KEY/TOKEN/SECRET heuristic
/// (hotl-store/src/lib.rs) would also strip GITHUB_TOKEN /
/// CARGO_REGISTRY_TOKEN / NPM_TOKEN and silently break `gh`, `cargo publish`
/// and `npm publish`. `HOTL_SCRUB_ENV_STRICT=1` opts into that heuristic.
const PROVIDER_CREDENTIAL_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_ADMIN_KEY",
    "OPENAI_API_KEY",
    "HOTL_API_KEY_HELPER",
    "HOTL_SCRUB_ENV",
    "HOTL_SCRUB_ENV_STRICT",
];

/// Same markers as `hotl_store::Masker` — duplicated rather than depended on
/// (hotl-tools does not link hotl-store). R7 owns consolidating the sanitizer
/// copies; this list joins that queue.
const SECRET_NAME_MARKERS: [&str; 7] = [
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "AUTH",
];

fn names_to_scrub(extra: Option<&str>, strict: bool) -> Vec<String> {
    let mut names: Vec<String> = PROVIDER_CREDENTIAL_VARS
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(extra) = extra {
        names.extend(
            extra
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from),
        );
    }
    if strict {
        for (name, value) in std::env::vars() {
            let upper = name.to_uppercase();
            if value.len() >= 8 && SECRET_NAME_MARKERS.iter().any(|m| upper.contains(m)) {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The one place a child's environment is shaped. Both `build_command` and
/// `build_argv` end here, so there is no second spawn path to forget.
///
/// INVARIANT: no provider credential is present in any child's environment.
/// Enforced by `provider_credentials_never_reach_the_child`.
fn apply_child_env(
    mut cmd: tokio::process::Command,
    egress: &EgressState,
) -> tokio::process::Command {
    for name in names_to_scrub(
        std::env::var("HOTL_SCRUB_ENV").ok().as_deref(),
        std::env::var("HOTL_SCRUB_ENV_STRICT").is_ok_and(|v| v == "1"),
    ) {
        cmd.env_remove(name);
    }
    apply_proxy_env(cmd, egress)
}

/// Allowlist mode: cooperating clients (curl, git, pip, cargo — anything
/// honoring the proxy env) route through the filtering proxy on loopback;
/// non-cooperating clients hit the kernel loopback-only wall and fail closed.
/// For Off/Open, no env — the kernel does all the work (or nothing changes).
fn apply_proxy_env(
    mut cmd: tokio::process::Command,
    egress: &EgressState,
) -> tokio::process::Command {
    if let EgressState::Proxy(port) = egress {
        // The credential rides the standard proxy URL, which curl, git, pip
        // and cargo all forward as `Proxy-Authorization`.
        let proxy = match crate::net::proxy_user_info() {
            Some(userinfo) => format!("http://{userinfo}@127.0.0.1:{port}"),
            None => format!("http://127.0.0.1:{port}"),
        };
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
            "ALL_PROXY",
        ] {
            cmd.env(key, &proxy);
        }
        for key in ["NO_PROXY", "no_proxy"] {
            cmd.env(key, "localhost,127.0.0.1,::1");
        }
    }
    cmd
}

/// The Seatbelt profile, pure (unit-tested against drift). Write-deny by
/// default with the working tree, temp, /dev, and any configured
/// `[sandbox].writable` roots re-allowed; read-deny for the plan-0022 carve
/// (`file-read-data` only, so `stat` and therefore `ls -la` keep working);
/// when `confine_network`, deny all
/// network and re-allow unix-domain sockets and loopback (the mDNSResponder
/// unix socket means DNS resolution still works — documented in SECURITY.md
/// as a resolution, not exfil-confinement, limit).
///
/// The extras ride `-D EXTRA_i=` parameters like CWD/TMP do — a literal path
/// spliced into the profile text would need Seatbelt-string escaping and is a
/// quoting hazard; a parameter is opaque to the profile language.
/// Directories under the workspace whose writes are execute-later hazards the
/// bash sandbox denies at the kernel (Vuln 4): git hooks (persistence), CI
/// workflows (RCE with secrets on the next push), cargo config (rustc-wrapper /
/// target runners), and the two agent-config dirs. Kept deliberately narrow —
/// developer-facing generated files (Makefile, package.json, build.rs) and
/// `.git/config` are *not* here, so ordinary build/git flows are never
/// hard-denied; those rely on the permission-layer escalation
/// (`bash_protected_write_reason`).
///
/// **macOS only, and not for want of trying** (plan 0022 task 4 retired this
/// slot's old "the deferred Linux work" marker). SBPL has real deny rules;
/// Landlock has none, and its rights union across ancestors, so denying
/// `<cwd>/.git/hooks` means granting no write on `<cwd>` or `<cwd>/.git`.
/// Measured on 6.8: that does deny the hook, and also denies creating any
/// new file at the top of the workspace and `.git/index.lock` — it breaks
/// ordinary work, so it is not shipped. Linux keeps the permission-layer
/// escalation alone here; `docs/SECURITY.md` states the gap.
#[cfg(target_os = "macos")]
const PROTECTED_SUBPATHS: &[&str] = &[
    ".git/hooks",
    ".github/workflows",
    ".cargo",
    ".hotl",
    ".claude",
];

#[cfg(target_os = "macos")]
fn seatbelt_profile(
    confine_network: bool,
    unix_sockets: UnixSockets,
    automation: Automation,
    extra_count: usize,
    noread_count: usize,
) -> String {
    let mut profile = String::from(
        r#"(version 1)
(allow default)
(deny file-write*)
(allow file-write*
  (subpath (param "CWD"))
  (subpath (param "TMP"))
  (subpath "/private/tmp")
  (subpath "/dev")"#,
    );
    for i in 0..extra_count {
        profile.push_str(&format!("\n  (subpath (param \"EXTRA_{i}\"))"));
    }
    profile.push_str(")\n");
    // Vuln 4: carve the execute-later dirs back out of the cwd write grant.
    // Placed after the allow (SBPL is last-match-wins) and before any network
    // clause, which is a different operation class. Each PROT_i is the absolute
    // `<cwd>/<subpath>` passed as a `-D` param by `seatbelt_base_with`.
    profile.push_str("(deny file-write*");
    for i in 0..PROTECTED_SUBPATHS.len() {
        profile.push_str(&format!("\n  (subpath (param \"PROT_{i}\"))"));
    }
    profile.push_str(")\n");
    // Plan 0022: the read-carve. `(allow default)` at the top is what grants
    // reads today, so these denies must follow it — SBPL is last-match-wins.
    // `file-read-data`, *not* `file-read*`: the latter also denies
    // file-read-metadata, which breaks `ls -la ~` (it stats every child).
    // With data alone, `stat` succeeds while contents and directory listing
    // do not — the path exists, its contents do not. Each NOREAD_i is an
    // absolute path passed as a `-D` param by `seatbelt_base_with`.
    //
    // The `> 0` guard keeps the profile byte-identical to the pre-0022 text
    // when nothing is carved, which the clause tests rely on.
    if noread_count > 0 {
        profile.push_str("(deny file-read-data");
        for i in 0..noread_count {
            profile.push_str(&format!("\n  (subpath (param \"NOREAD_{i}\"))"));
        }
        profile.push_str(")\n");
    }
    if confine_network {
        profile.push_str(
            r#"(deny network*)
(allow network* (local unix) (remote unix))
(allow network-outbound (remote ip "localhost:*"))
(allow network-inbound (local ip "localhost:*"))
"#,
        );
    }
    // `(allow default)` above leaves Apple Events open, so an
    // `osascript -e 'tell app "Terminal" to do script "…"'` would execute its
    // payload in a process that is *not* a descendant of this sandbox — an
    // escape with no disk write. Denying the event send is the surgical
    // control: a blanket `(deny mach-lookup)` would need a long,
    // release-fragile allowlist for cfprefsd/notifyd/launchd, and denying the
    // AppleEvents/LaunchServices brokers by name was measured to convert a
    // fast refusal into an indefinite hang (see the plan's execution
    // findings), which is a worse outcome than the refusal it replaces.
    //
    // INVARIANT (partially verified — see specs/exec-plans/active/
    // 0019-remediation-sandbox.md, execution findings): no Apple Event leaves
    // a confined child unless the operator set HOTL_MACOS_AUTOMATION=allow.
    // The positive direction — that plain AppleScript still runs — is enforced
    // by `seatbelt_denies_apple_events_without_breaking_plain_applescript`.
    // The negative direction is NOT covered by a test: no cross-application
    // Apple Event is observable on the author's host either way (TCC gates the
    // reachable targets, and the one that works unsandboxed is already refused
    // by the pre-existing profile), so any such assertion would pass before
    // the change too. Re-verify on a Mac with an Automation TCC grant.
    if matches!(automation, Automation::Deny) {
        profile.push_str("(deny appleevent-send)\n");
    }
    // Kept at the *end* of the profile so no later clause can widen it back. A
    // unix socket connect is a network operation, not a file write, so the
    // file-write floor above does not touch it — and `/var/run/docker.sock` is
    // a root-equivalent escape (mount the host root, write anywhere).
    // Not denied here: ssh-agent/gpg-agent (capability-limited, and denying
    // them breaks `git push` over SSH) — see docs/SECURITY.md.
    // INVARIANT: the daemon socket class is unreachable unless the operator
    // set HOTL_UNIX_SOCKETS=open. Enforced by
    // `seatbelt_blocks_a_connect_to_a_denied_daemon_socket`.
    if matches!(unix_sockets, UnixSockets::DenyDaemons) {
        profile.push_str(
            "(deny network-outbound (regex #\"(docker|podman|containerd|crio|dockershim)[^/]*\\.sock$\"))\n\
             (deny network-outbound (regex #\"^/(private/)?(var/)?run/(docker|containerd|crio)/\"))\n",
        );
    }
    profile
}

/// The `sandbox-exec` invocation up to (not including) the program to run —
/// shared by the `sh -c` and direct-argv exec shapes.
#[cfg(target_os = "macos")]
fn seatbelt_base(egress: &EgressState) -> tokio::process::Command {
    seatbelt_base_with(egress, extra_writable(), &spawn_read_deny())
}

/// The extras-explicit seam — behavioral tests drive configured write roots
/// and read denials through here without touching the process-global
/// `EXTRAS` or the `SECRET_READS` task-local.
#[cfg(target_os = "macos")]
fn seatbelt_base_with(
    egress: &EgressState,
    extras: &[PathBuf],
    noread: &[PathBuf],
) -> tokio::process::Command {
    let confine_network = matches!(egress, EgressState::Off | EgressState::Proxy(_));
    let cwd = canon(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let tmp = canon(std::env::temp_dir());
    let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
    cmd.arg("-p")
        .arg(seatbelt_profile(
            confine_network,
            unix_socket_policy(),
            automation_policy(),
            extras.len(),
            noread.len(),
        ))
        .arg("-D")
        .arg(format!("CWD={}", cwd.display()))
        .arg("-D")
        .arg(format!("TMP={}", tmp.display()));
    // Vuln 4: the absolute execute-later dirs the profile denies write to. Byte-
    // exact OsString concat so a non-UTF-8 cwd reaches sandbox-exec faithfully.
    for (i, sub) in PROTECTED_SUBPATHS.iter().enumerate() {
        let mut d = std::ffi::OsString::from(format!("PROT_{i}="));
        d.push(cwd.join(sub).as_os_str());
        cmd.arg("-D").arg(d);
    }
    for (i, p) in extras.iter().enumerate() {
        // OsString concatenation, not format!: a non-UTF-8 path must reach
        // sandbox-exec byte-exact, never lossily rendered.
        let mut d = std::ffi::OsString::from(format!("EXTRA_{i}="));
        d.push(p.as_os_str());
        cmd.arg("-D").arg(d);
    }
    // Plan 0022: the read-carve paths. Same byte-exact OsString concat.
    for (i, p) in noread.iter().enumerate() {
        let mut d = std::ffi::OsString::from(format!("NOREAD_{i}="));
        d.push(p.as_os_str());
        cmd.arg("-D").arg(d);
    }
    cmd
}

#[cfg(target_os = "macos")]
fn seatbelt_command(command: &str, egress: &EgressState) -> tokio::process::Command {
    let mut cmd = seatbelt_base(egress);
    cmd.arg("sh").arg("-c").arg(command);
    cmd
}

#[cfg(target_os = "macos")]
fn seatbelt_argv(program: &str, args: &[String], egress: &EgressState) -> tokio::process::Command {
    let mut cmd = seatbelt_base(egress);
    cmd.arg(program).args(args);
    cmd
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
fn seatbelt_command(command: &str, _egress: &EgressState) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
fn seatbelt_argv(program: &str, args: &[String], _egress: &EgressState) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    cmd
}

/// Can this kernel enforce Landlock **net** rules (ABI v4, kernel ≥ 6.7)?
/// Handled as a `HardRequirement` so a pre-6.7 kernel errors out here instead
/// of silently skipping net enforcement — the caller degrades to the loud
/// `Unenforced` posture, never to open-and-quiet.
#[cfg(target_os = "linux")]
pub(crate) fn landlock_net_supported() -> Result<(), String> {
    use std::sync::OnceLock;

    use landlock::{AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr};

    static SUPPORT: OnceLock<Result<(), String>> = OnceLock::new();
    SUPPORT
        .get_or_init(|| {
            match Ruleset::default()
                .set_compatibility(CompatLevel::HardRequirement)
                .handle_access(AccessNet::ConnectTcp)
            {
                Ok(r) => match r.create() {
                    Ok(_) => Ok(()),
                    Err(e) => Err(format!("landlock net unavailable: {e}")),
                },
                Err(e) => Err(format!("landlock net needs kernel ≥ 6.7: {e}")),
            }
        })
        .clone()
}

/// Build the fully-populated ruleset **in the parent**: `pre_exec` runs
/// between fork and exec in a multithreaded process, where allocation
/// (malloc lock) and other non-async-signal-safe work can deadlock, so
/// everything that allocates happens here, before the spawn.
///
/// The fs part stays best-effort at ABI::V2 as before. The net part
/// (Off/Proxy egress) is a **hard requirement**: `ConnectTcp` only — zero
/// allowed ports for Off, exactly the proxy port for Proxy. Two honest Linux
/// limits, by Landlock's design: net rules are **TCP-only** (UDP — including
/// DNS and DNS-tunnel exfiltration — is not confined) and **port-scoped, not
/// address-scoped** (the proxy port number is connectable on any host, and
/// Off blocks loopback TCP too; unix-domain sockets are untouched either
/// way). Shared by `landlock_command` and `landlock_argv`, via
/// `apply_landlock_with`.
///
/// Landlock is allowlist-only and its rights **union across ancestors** — the
/// kernel walks up from the accessed path and allows as soon as any ancestor
/// rule grants the right. A nested rule therefore cannot narrow a broader
/// one (measured on 6.8: `/` = from_read plus `~/.ssh` = Execute still reads
/// `~/.ssh/id_rsa`). The only way to deny a read is to *not* grant it on any
/// ancestor, which is what this descent does:
///
/// - no deny entry at or under `dir` → grant the whole subtree, stop;
/// - otherwise → grant `ReadDir` on `dir` alone and recurse into every child
///   that is not itself denied.
///
/// The ancestor `ReadDir` is required, not cosmetic: without it no rule
/// covers `~` and `ls ~` fails. Symlinked children are skipped — `PathFd`
/// follows links, so granting through one would re-open a denied target by
/// its real path; the target is granted through its own real path anyway.
///
/// Documented asymmetry (measured, not assumed): the ancestor's `ReadDir` is
/// hierarchical, so filenames *inside* a denied directory stay listable on
/// Linux while macOS `file-read-data` hides them. Contents are denied on
/// both. `SECURITY.md` says so rather than papering over it.
///
/// A `PathFd::new` failure denies that subtree rather than skipping it — the
/// fail-closed direction, but an opaque `EACCES` deep inside a grandchild, so
/// each one is recorded for `read_carve_failures()` to surface.
#[cfg(target_os = "linux")]
fn grant_read_except(
    mut ruleset: landlock::RulesetCreated,
    dir: &std::path::Path,
    deny: &[PathBuf],
    abi: landlock::ABI,
    failures: &mut Vec<String>,
) -> Option<landlock::RulesetCreated> {
    use landlock::{AccessFs, PathBeneath, PathFd, RulesetCreatedAttr};

    if deny.iter().any(|d| d == dir) {
        return Some(ruleset); // grant nothing: this is the carve
    }
    if !deny.iter().any(|d| d.starts_with(dir)) {
        return match PathFd::new(dir) {
            Ok(fd) => ruleset
                .add_rule(PathBeneath::new(fd, AccessFs::from_read(abi)))
                .ok(),
            Err(e) => {
                failures.push(format!("{}: {e}", dir.display()));
                Some(ruleset)
            }
        };
    }
    match PathFd::new(dir) {
        Ok(fd) => {
            ruleset = ruleset
                .add_rule(PathBeneath::new(fd, AccessFs::ReadDir))
                .ok()?
        }
        Err(e) => {
            failures.push(format!("{}: {e}", dir.display()));
            return Some(ruleset);
        }
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            failures.push(format!("{}: {e}", dir.display()));
            return Some(ruleset);
        }
    };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_symlink()).unwrap_or(true) {
            continue;
        }
        ruleset = grant_read_except(ruleset, &entry.path(), deny, abi, failures)?;
    }
    Some(ruleset)
}

/// Paths the read-carve descent could not open, so their subtree is denied
/// without anyone having asked. Populated once, on the first sandboxed spawn
/// of the process (the ruleset is memoized); `hotl doctor` prints it.
pub fn read_carve_failures() -> &'static [String] {
    #[cfg(target_os = "linux")]
    {
        return READ_CARVE_FAILURES.get().map(Vec::as_slice).unwrap_or(&[]);
    }
    #[cfg(not(target_os = "linux"))]
    &[]
}

#[cfg(target_os = "linux")]
static READ_CARVE_FAILURES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// `extras` and `noread` are explicit seams — behavioral tests drive
/// configured write roots and read denials through here without touching the
/// process-global `EXTRAS` or the `SECRET_READS` task-local.
#[cfg(target_os = "linux")]
fn build_landlock_ruleset_with(
    egress: &EgressState,
    extras: &[PathBuf],
    noread: &[PathBuf],
    failures: &mut Vec<String>,
) -> Option<std::os::unix::io::OwnedFd> {
    use landlock::{
        Access, AccessFs, AccessNet, CompatLevel, Compatible, NetPort, PathFd, Ruleset,
        RulesetAttr, RulesetCreatedAttr, ABI,
    };

    // ABI v3 (Linux 6.2) is the first level that carries
    // LANDLOCK_ACCESS_FS_TRUNCATE. Landlock only restricts rights present in
    // the *handled* mask, so pinning v2 here left `truncate(2)` by path
    // unconfined on every kernel, not merely on old ones — a raw
    // `truncate(2)` (not `truncate -s 0`, which opens O_WRONLY first and is
    // already denied by WriteFile) could zero any file on the host from an
    // auto-approved bash. BestEffort still degrades gracefully below v3 —
    // `probe()` is what reports the degradation instead of hiding it.
    // INVARIANT: the handled mask includes Truncate wherever the kernel offers
    // it. Enforced by `landlock_confines_truncate_by_path`.
    let abi = ABI::V3;
    let cwd = canon(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let tmp = canon(std::env::temp_dir());
    let confine_network = matches!(egress, EgressState::Off | EgressState::Proxy(_));
    let mut attr = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .ok()?;
    if confine_network {
        // HardRequirement: on a kernel without the net ABI this fails,
        // build_landlock_ruleset_with returns None, and the child refuses to
        // exec (fail-closed) rather than run with open egress.
        attr = attr
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessNet::ConnectTcp)
            .ok()?
            .set_compatibility(CompatLevel::BestEffort);
    }
    let mut ruleset = attr.create().ok()?;
    // Read + execute everywhere, minus the plan-0022 carve. With an empty
    // deny set this is exactly the single `/` from_read rule it replaced —
    // no descent, no readdir, no extra rules.
    ruleset = grant_read_except(ruleset, std::path::Path::new("/"), noread, abi, failures)?;
    // Full access under cwd, tmp, /dev, and any configured extra root. A
    // since-deleted extra fails `PathFd::new` and is skipped — writes there
    // simply stay denied (fail closed), matching the /dev handling.
    //
    // No `PROTECTED_SUBPATHS` twin here: carving `.git/hooks` back out of a
    // cwd grant would mean granting no write on cwd itself (rights union
    // across ancestors), which measurably denies creating any new file at the
    // top of the workspace. See the const's doc comment.
    let base = [cwd.as_path(), tmp.as_path(), std::path::Path::new("/dev")];
    for p in base
        .iter()
        .copied()
        .chain(extras.iter().map(PathBuf::as_path))
    {
        if let Ok(fd) = PathFd::new(p) {
            ruleset = ruleset
                .add_rule(landlock::PathBeneath::new(fd, AccessFs::from_all(abi)))
                .ok()?;
        }
    }
    // Proxy mode: the single connectable TCP port. Off adds no ports.
    if let EgressState::Proxy(port) = egress {
        ruleset = ruleset
            .add_rule(
                NetPort::new(*port, AccessNet::ConnectTcp)
                    .set_compatibility(CompatLevel::HardRequirement),
            )
            .ok()?;
    }
    // Extract the ruleset fd; None when the kernel can't enforce it.
    ruleset.into()
}

/// The ruleset is memoized per spawn shape, not rebuilt per spawn: cwd,
/// `TMPDIR`, the extras and the deny set are all fixed after startup, and a
/// `Proxy(port)`'s port is fixed once the proxy starts. Without this the
/// plan-0022 descent would re-`readdir` `$HOME` and re-add a rule per entry
/// on every `bash` call.
///
/// The per-command secret-read grant is part of the key: a lifted ruleset
/// must never be handed to the *next* command, which did not take the grant.
#[cfg(target_os = "linux")]
fn cached_ruleset(egress: &EgressState) -> Option<std::sync::Arc<std::os::unix::io::OwnedFd>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    type Key = (Option<u16>, bool, bool);
    static CACHE: OnceLock<Mutex<HashMap<Key, Option<Arc<std::os::unix::io::OwnedFd>>>>> =
        OnceLock::new();
    let granted = secret_reads_granted();
    // Everything the built ruleset depends on. `Unenforced` shares `Open`'s
    // key because it confines no network either — the loud marker and the
    // allow-rule revocation are `net.rs`'s job, not the ruleset's.
    let key: Key = match egress {
        EgressState::Proxy(p) => (Some(*p), true, granted),
        EgressState::Off => (None, true, granted),
        EgressState::Open | EgressState::Unenforced(_) => (None, false, granted),
    };
    let cache = CACHE.get_or_init(Default::default);
    let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .entry(key)
        .or_insert_with(|| {
            let mut failures = Vec::new();
            let fd = build_landlock_ruleset_with(
                egress,
                extra_writable(),
                &spawn_read_deny(),
                &mut failures,
            );
            if !failures.is_empty() {
                let _ = READ_CARVE_FAILURES.set(failures);
            }
            fd.map(Arc::new)
        })
        .clone()
}

#[cfg(target_os = "linux")]
fn apply_landlock(cmd: tokio::process::Command, egress: &EgressState) -> tokio::process::Command {
    attach_ruleset(cmd, cached_ruleset(egress))
}

#[cfg(target_os = "linux")]
// The behavioral-test seam: production spawns go through the memoized
// `cached_ruleset`, which is what makes the descent affordable per spawn.
#[allow(dead_code)]
fn apply_landlock_with(
    cmd: tokio::process::Command,
    egress: &EgressState,
    extras: &[PathBuf],
    noread: &[PathBuf],
) -> tokio::process::Command {
    let mut failures = Vec::new();
    let fd = build_landlock_ruleset_with(egress, extras, noread, &mut failures);
    attach_ruleset(cmd, fd.map(std::sync::Arc::new))
}

#[cfg(target_os = "linux")]
fn attach_ruleset(
    mut cmd: tokio::process::Command,
    ruleset_fd: Option<std::sync::Arc<std::os::unix::io::OwnedFd>>,
) -> tokio::process::Command {
    use std::os::unix::io::AsRawFd;

    // The fd is captured by the closure, so it stays open across every spawn
    // of this Command (pre_exec runs after fork, before exec — a parent-owned
    // fd is still open in the child there). Fail-closed: with no usable fd the
    // child refuses to exec rather than run unconfined.
    let apply = move || {
        // Async-signal-safe only from here: raw syscalls, no allocation.
        let Some(fd) = ruleset_fd.as_ref().map(|f| f.as_raw_fd()) else {
            return Err(std::io::Error::from_raw_os_error(libc::ENOSYS));
        };
        // SAFETY: plain syscalls with no memory handed to the kernel beyond
        // the fd and integer flags.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, fd, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    };
    // SAFETY: `apply` performs only async-signal-safe operations (see above).
    unsafe {
        cmd.pre_exec(apply);
    }
    cmd
}

#[cfg(target_os = "linux")]
fn landlock_command(command: &str, egress: &EgressState) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command);
    apply_landlock(cmd, egress)
}

#[cfg(target_os = "linux")]
fn landlock_argv(program: &str, args: &[String], egress: &EgressState) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    apply_landlock(cmd, egress)
}

#[cfg(test)]
mod env_tests {
    use super::*;

    /// The proxy URL a child sees. Since Task 9 it carries the session
    /// credential as userinfo (curl/git/pip/cargo forward that as
    /// `Proxy-Authorization`), so this asserts the *shape* — right authority,
    /// and a non-empty credential when auth is on — rather than re-deriving
    /// the token, which would make the assertion a tautology.
    fn assert_proxy_url(envs: &[(String, Option<String>)], key: &str, port: u16) {
        let value = envs
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("{key} must be set"));
        let authority = format!("@127.0.0.1:{port}");
        match crate::net::proxy_user_info() {
            Some(_) => {
                assert!(
                    value.ends_with(&authority),
                    "{key} must point at the proxy: {value}"
                );
                let userinfo = value
                    .strip_prefix("http://")
                    .and_then(|r| r.strip_suffix(&authority))
                    .unwrap_or_default();
                let secret = userinfo.strip_prefix("hotl:").unwrap_or_default();
                assert!(
                    !secret.is_empty(),
                    "{key} must carry a non-empty session credential: {value}"
                );
            }
            None => assert_eq!(value, format!("http://127.0.0.1:{port}")),
        }
    }

    #[test]
    fn proxy_state_injects_proxy_env_and_off_does_not() {
        // Env injection is OS-independent (it rides the Command itself).
        let status = SandboxStatus::Unavailable("test".into());
        let cmd = build_command("true", &status, &EgressState::Proxy(9123));
        let envs: Vec<_> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
            "ALL_PROXY",
        ] {
            assert_proxy_url(&envs, key, 9123);
        }
        for key in ["NO_PROXY", "no_proxy"] {
            assert!(
                envs.contains(&(key.to_string(), Some("localhost,127.0.0.1,::1".to_string()))),
                "{key} must exempt loopback"
            );
        }
        // Off and Open *set* nothing (kernel-only / unchanged behavior). The
        // environment is not untouched, though: `apply_child_env` removes the
        // provider credentials on every path — pinned by
        // `provider_credentials_never_reach_the_child` below.
        for egress in [EgressState::Off, EgressState::Open] {
            let cmd = build_command("true", &status, &egress);
            assert!(
                cmd.as_std().get_envs().all(|(_, v)| v.is_none()),
                "{egress:?} must only remove variables, never set one"
            );
        }
    }

    #[test]
    fn provider_credentials_never_reach_the_child() {
        let status = SandboxStatus::Unavailable("test".into());
        for egress in [
            EgressState::Open,
            EgressState::Off,
            EgressState::Proxy(9123),
        ] {
            let cmd = build_command("true", &status, &egress);
            let envs: Vec<(String, Option<String>)> = cmd
                .as_std()
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect();
            for name in [
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "OPENAI_API_KEY",
                "HOTL_API_KEY_HELPER",
            ] {
                assert!(
                    envs.contains(&(name.to_string(), None)),
                    "{name} must be removed from the child env under {egress:?}"
                );
            }
            // A credential a child legitimately needs is left alone by default.
            assert!(
                !envs.iter().any(|(k, _)| k == "GITHUB_TOKEN"),
                "the default scrub must not strip GITHUB_TOKEN (gh/git would break)"
            );
        }
        // The same chokepoint covers the argv shape.
        let argv = build_argv("rg", &["--files".to_string()], &status, &EgressState::Open);
        assert!(argv
            .as_std()
            .get_envs()
            .any(|(k, v)| k == "ANTHROPIC_API_KEY" && v.is_none()));
    }

    #[test]
    fn strict_mode_extends_the_scrub_and_explicit_names_add_to_it() {
        assert!(names_to_scrub(Some("MY_THING,OTHER"), false)
            .iter()
            .any(|n| n == "MY_THING"));
        assert!(names_to_scrub(None, false)
            .iter()
            .any(|n| n == "ANTHROPIC_API_KEY"));
        // Strict adds every secret-named var *present in this process*.
        std::env::set_var("HOTL_TEST_SCRUB_TOKEN", "abcdefghij");
        assert!(names_to_scrub(None, true)
            .iter()
            .any(|n| n == "HOTL_TEST_SCRUB_TOKEN"));
        assert!(!names_to_scrub(None, false)
            .iter()
            .any(|n| n == "HOTL_TEST_SCRUB_TOKEN"));
        std::env::remove_var("HOTL_TEST_SCRUB_TOKEN");
    }

    #[test]
    fn build_argv_execs_program_and_args_directly_and_still_wires_proxy_env() {
        // No sandbox floor on this "host": the argv falls through to a plain
        // `Command::new(program)` — no `sh -c` wrapping, ever.
        let status = SandboxStatus::Unavailable("test".into());
        let args = vec!["--line-number".to_string(), "needle".to_string()];
        let cmd = build_argv("rg", &args, &status, &EgressState::Open);
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "rg");
        let got_args: Vec<_> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(got_args, args);

        // Proxy env wiring is shared with build_command.
        let proxied = build_argv("rg", &args, &status, &EgressState::Proxy(9123));
        let envs: Vec<_> = proxied
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_proxy_url(&envs, "HTTP_PROXY", 9123);
    }
}

#[cfg(test)]
mod write_set_tests {
    use super::*;

    #[test]
    fn write_roots_with_appends_extras_after_the_base_set() {
        let extra = PathBuf::from("/somewhere/cache");
        let base = write_roots_with(&[]);
        let roots = write_roots_with(std::slice::from_ref(&extra));
        assert_eq!(roots.len(), base.len() + 1);
        assert_eq!(roots[..base.len()], base[..]);
        assert_eq!(roots.last(), Some(&extra));
        // The base set is exactly today's floor.
        assert!(base.contains(&PathBuf::from("/dev")));
        assert!(base.contains(&PathBuf::from("/private/tmp")));
    }

    /// Plan 0022 decision 3. The grant lifts Tier B and only Tier B, and it
    /// exists only inside the task-local scope `turn.rs` wraps around the
    /// single `Tool::run` future — the next call in the same turn re-enters
    /// with no scope at all and is denied again.
    #[tokio::test]
    async fn the_secret_read_grant_is_scoped_and_never_lifts_tier_a() {
        let deny = ReadDeny {
            always: vec![PathBuf::from("/data/hotl")],
            secrets: vec![PathBuf::from("/home/u/.ssh")],
            rules: Vec::new(),
        };
        assert_eq!(
            deny.for_spawn(false),
            vec![PathBuf::from("/data/hotl"), PathBuf::from("/home/u/.ssh")]
        );
        // Granted: Tier B drops, Tier A does not.
        assert_eq!(deny.for_spawn(true), vec![PathBuf::from("/data/hotl")]);

        // And the grant really is the task-local, not a flag anyone can set:
        // outside a scope it reads false, which is what an un-granted call —
        // and every hook, diagnostic and `grep` spawn — sees.
        assert!(!secret_reads_granted());
        SECRET_READS
            .scope(true, async {
                assert!(secret_reads_granted());
            })
            .await;
        assert!(
            !secret_reads_granted(),
            "the grant must not outlive its call"
        );
    }

    /// Plan 0025 decision 6. The grant lifts Tier B and only Tier B — a
    /// projected `[[deny]]` rule is unliftable, because `Rules::evaluate` makes
    /// deny unconditional in-process.
    #[test]
    fn the_secret_read_grant_does_not_lift_a_projected_rule() {
        let deny = ReadDeny {
            always: vec![PathBuf::from("/data/hotl")],
            secrets: vec![PathBuf::from("/home/u/.ssh")],
            rules: vec![PathBuf::from("/Volumes/secrets")],
        };
        for granted in [false, true] {
            assert!(
                deny.for_spawn(granted)
                    .contains(&PathBuf::from("/Volumes/secrets")),
                "the grant must not lift Tier C (secret_reads = {granted})"
            );
        }
        assert_eq!(
            deny.for_spawn(true),
            vec![
                PathBuf::from("/data/hotl"),
                PathBuf::from("/Volumes/secrets")
            ]
        );
    }

    #[test]
    fn probe_dir_from_skips_candidates_inside_any_allowed_root() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = vec![canon(dir.path().to_path_buf())];
        let inside = dir.path().join("sub");
        std::fs::create_dir_all(&inside).unwrap();
        // The only candidate sits inside an allowed root — writing there
        // proves nothing, so the search must fail and name the escape hatch.
        let err = probe_dir_from(&allowed, vec![inside]).unwrap_err();
        assert!(err.contains("HOTL_SANDBOX_PROBE_DIR"), "{err}");
    }

    #[test]
    fn probe_dir_from_accepts_a_writable_candidate_outside_the_set() {
        let allowed_dir = tempfile::tempdir().unwrap();
        let candidate = tempfile::tempdir().unwrap();
        let got = probe_dir_from(
            &[canon(allowed_dir.path().to_path_buf())],
            vec![candidate.path().to_path_buf()],
        )
        .unwrap();
        assert_eq!(got, canon(candidate.path().to_path_buf()));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn seatbelt_denies_protected_subpaths_under_cwd() {
        // Vuln 4: a sandboxed bash cannot write the execute-later dirs even by
        // an obscured path the static check missed, while ordinary in-cwd
        // writes stay allowed. Driven against a tempdir CWD so no repo is
        // touched and no process-global cwd is changed.
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = canon(dir.path().to_path_buf());
        let profile = seatbelt_profile(false, unix_socket_policy(), automation_policy(), 0, 0);
        let run = |cmd: &str| {
            let mut c = std::process::Command::new("/usr/bin/sandbox-exec");
            // Run in the tempdir so the child's real cwd matches the CWD param
            // (production always keeps them equal — build_command derives CWD
            // from current_dir()).
            c.current_dir(&cwd);
            c.arg("-p")
                .arg(&profile)
                .arg("-D")
                .arg(format!("CWD={}", cwd.display()))
                .arg("-D")
                .arg(format!("TMP={}", canon(std::env::temp_dir()).display()));
            for (i, sub) in PROTECTED_SUBPATHS.iter().enumerate() {
                c.arg("-D")
                    .arg(format!("PROT_{i}={}", cwd.join(sub).display()));
            }
            c.arg("sh").arg("-c").arg(cmd);
            c.output().expect("spawn sandbox-exec")
        };

        // Ordinary in-cwd write: allowed.
        let ok = run("mkdir -p src && printf x > src/ok");
        assert!(
            ok.status.success(),
            "cwd write should be allowed: {}",
            String::from_utf8_lossy(&ok.stderr)
        );

        // A git hook: denied at the kernel.
        let hook = run("mkdir -p .git/hooks && printf x > .git/hooks/pre-commit");
        assert!(
            !hook.status.success(),
            "writing a git hook must be denied by the floor"
        );
        assert!(
            !cwd.join(".git/hooks/pre-commit").exists(),
            "the hook must not have been written"
        );

        // A CI workflow: also denied.
        let wf = run("mkdir -p .github/workflows && printf x > .github/workflows/ci.yml");
        assert!(!wf.status.success(), "writing a CI workflow must be denied");
    }

    #[test]
    fn detach_session_starts_a_new_session_leader() {
        // Vuln 2: a detached tool child leads its own session (sid == pid) and
        // no longer shares hotl's session, so it has no route to hotl's
        // controlling terminal — no TIOCSTI keystroke injection, no /dev/tty
        // spoof. This is also what keeps kill_group's kill(-pid) working.
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("2");
        detach_session(&mut cmd);
        let mut child = cmd.as_std_mut().spawn().expect("spawn sleep");
        let pid = child.id() as i32;
        // pre_exec runs in the child after fork; poll until setsid has landed.
        let mut child_sid = -1;
        for _ in 0..100 {
            child_sid = unsafe { libc::getsid(pid) };
            if child_sid == pid {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let our_sid = unsafe { libc::getsid(0) };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(child_sid, pid, "child is not a session leader");
        assert_ne!(child_sid, our_sid, "child still shares hotl's session");
    }

    async fn run(cmd: &str) -> std::process::Output {
        run_with(cmd, &EgressState::Open).await
    }

    async fn run_with(cmd: &str, egress: &EgressState) -> std::process::Output {
        let status = probe();
        assert!(
            matches!(status, SandboxStatus::Enforced("seatbelt")),
            "no seatbelt on this mac?"
        );
        build_command(cmd, &status, egress)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .expect("spawn")
    }

    /// Replaces `seatbelt_profile_strings_do_not_drift`, which asserted a
    /// literal equals itself — a pure change detector proving nothing about
    /// enforcement. This asserts the *properties* the profile must have; the
    /// behavioral tests below and `seatbelt_confines_writes` prove enforcement.
    #[test]
    fn seatbelt_profile_carries_every_required_clause() {
        let open = seatbelt_profile(false, UnixSockets::DenyDaemons, Automation::Deny, 0, 0);
        for clause in [
            "(deny file-write*)",
            "(subpath (param \"CWD\"))",
            "(subpath (param \"TMP\"))",
            "(deny appleevent-send)",
            "docker",
        ] {
            assert!(open.contains(clause), "profile lost `{clause}`:\n{open}");
        }
        // No configured extras and no carve → byte-identical to the pre-0022
        // profile shape.
        assert!(!open.contains("EXTRA"), "no EXTRA params without extras");
        assert!(!open.contains("NOREAD"), "no NOREAD params without a carve");
        assert!(
            !open.contains("file-read"),
            "reads untouched without a carve"
        );
        // Both opt-outs really opt out.
        assert!(
            !seatbelt_profile(false, UnixSockets::DenyDaemons, Automation::Allow, 0, 0)
                .contains("appleevent-send")
        );
        let confined = seatbelt_profile(true, UnixSockets::DenyDaemons, Automation::Deny, 0, 0);
        for clause in [
            "(deny network*)",
            "(allow network* (local unix) (remote unix))",
        ] {
            assert!(
                confined.contains(clause),
                "confined profile lost `{clause}`"
            );
        }
        // Network confinement is additive over the open profile's file clauses.
        assert!(confined.contains("(deny file-write*)"));
    }

    #[test]
    fn seatbelt_profile_re_allows_each_configured_extra_via_params() {
        let p = seatbelt_profile(true, UnixSockets::DenyDaemons, Automation::Deny, 2, 0);
        for clause in [
            "(subpath (param \"EXTRA_0\"))",
            "(subpath (param \"EXTRA_1\"))",
        ] {
            assert!(p.contains(clause), "profile lost `{clause}`:\n{p}");
        }
        // The extras join the file-write re-allow block, never the tail: the
        // container-daemon denies stay terminal so nothing widens them back.
        let deny = p.find("(deny network-outbound").expect("daemon deny");
        assert!(p.find("EXTRA_1").unwrap() < deny);
    }

    /// Plan 0022. Two properties the read-carve block must have: it denies
    /// *data* only (metadata stays readable, or `ls -la ~` breaks), and it
    /// sits after the write clauses and before the terminal socket denies, so
    /// nothing later widens it back.
    #[test]
    fn seatbelt_read_deny_precedes_the_terminal_socket_denies() {
        let p = seatbelt_profile(true, UnixSockets::DenyDaemons, Automation::Deny, 0, 2);
        for clause in [
            "(deny file-read-data",
            "(subpath (param \"NOREAD_0\"))",
            "(subpath (param \"NOREAD_1\"))",
        ] {
            assert!(p.contains(clause), "profile lost `{clause}`:\n{p}");
        }
        // Data, never metadata: `file-read*` would deny file-read-metadata
        // too, and `ls -la ~` stats every child.
        assert!(
            !p.contains("(deny file-read*"),
            "denying metadata breaks `ls -la`:\n{p}"
        );
        let read = p.find("(deny file-read-data").expect("read deny");
        let write = p.find("(deny file-write*)").expect("write deny");
        let socket = p.find("(deny network-outbound").expect("daemon deny");
        assert!(write < read, "the read carve must follow the write floor");
        assert!(
            read < socket,
            "the socket denies must stay terminal (nothing widens them back)"
        );
    }

    /// The behavioral half: a carved path really is unreadable, its metadata
    /// really is not, and a sibling outside the carve reads fine. Without
    /// that last positive control the test would pass on a broken profile
    /// that denied everything.
    #[tokio::test]
    async fn seatbelt_denies_reading_a_carved_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let carved = canon(dir.path().to_path_buf()).join("secrets");
        let open = canon(dir.path().to_path_buf()).join("ordinary");
        std::fs::create_dir_all(&carved).unwrap();
        std::fs::create_dir_all(&open).unwrap();
        std::fs::write(carved.join("id_probe"), b"canary").unwrap();
        std::fs::write(open.join("id_probe"), b"canary").unwrap();

        let run = |script: String, noread: Vec<PathBuf>| async move {
            let mut cmd = seatbelt_base_with(&EgressState::Open, &[], &noread);
            cmd.arg("sh").arg("-c").arg(script);
            cmd.stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await
                .expect("spawn")
        };
        let cat = |p: &std::path::Path| format!("cat {}", p.display());

        // Positive control 1: with nothing carved, the canary reads.
        let control = run(cat(&carved.join("id_probe")), vec![]).await;
        assert!(
            control.status.success(),
            "the canary must be readable without a carve: {}",
            String::from_utf8_lossy(&control.stderr)
        );

        let carve = vec![carved.clone()];
        // The carve denies the contents...
        let denied = run(cat(&carved.join("id_probe")), carve.clone()).await;
        assert!(!denied.status.success(), "a carved path must not read");
        assert!(!String::from_utf8_lossy(&denied.stdout).contains("canary"));
        // ...but not its metadata: `stat` is what `ls -la` needs.
        let stat = run(
            format!("stat {} > /dev/null", carved.join("id_probe").display()),
            carve.clone(),
        )
        .await;
        assert!(
            stat.status.success(),
            "metadata must stay readable (`ls -la` stats every child): {}",
            String::from_utf8_lossy(&stat.stderr)
        );
        // Positive control 2: a sibling outside the carve is untouched.
        let sibling = run(cat(&open.join("id_probe")), carve).await;
        assert!(
            sibling.status.success(),
            "a path outside the carve must still read: {}",
            String::from_utf8_lossy(&sibling.stderr)
        );
    }

    #[tokio::test]
    async fn seatbelt_allows_writes_under_a_configured_extra_dir() {
        // The extra must live *outside* the default floor or the test is
        // vacuous (a tempdir sits under TMPDIR, which is already writable) —
        // so carve it out of the probe dir. Driven through the
        // `seatbelt_base_with` seam, never the process-global EXTRAS.
        let base = probe_dir().expect("a dir outside the floor");
        let extra = base.join(format!("hotl-sbx-extra-{}", std::process::id()));
        std::fs::create_dir_all(&extra).unwrap();
        let target = extra.join("cache-file");
        let script = format!("echo x > {}", target.display());

        // Control: without the extra configured, the floor denies the write.
        let denied = run(&script).await;
        assert!(
            !denied.status.success(),
            "control write outside the floor must be denied"
        );

        // Same write with the extra configured: allowed.
        let mut cmd = seatbelt_base_with(&EgressState::Open, std::slice::from_ref(&extra), &[]);
        cmd.arg("sh").arg("-c").arg(&script);
        let ok = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .expect("spawn");
        let content = std::fs::read_to_string(&target).unwrap_or_default();
        std::fs::remove_dir_all(&extra).ok();
        assert!(
            ok.status.success(),
            "write under a configured extra must be allowed: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
        assert_eq!(content.trim(), "x");
    }

    #[test]
    fn seatbelt_denies_the_container_daemon_socket_class_in_both_network_modes() {
        for confined in [false, true] {
            let p = seatbelt_profile(confined, UnixSockets::DenyDaemons, Automation::Deny, 0, 0);
            // The first *deny* on network-outbound: the `(allow
            // network-outbound (remote ip …))` of the confined profile also
            // contains the operation name, so match on the deny itself.
            let deny = p
                .find("(deny network-outbound")
                .expect("a unix-socket deny clause");
            // The clause is a regex, so `docker` and `.sock` are not adjacent
            // in the profile text — assert both halves of the alternation.
            assert!(
                p[deny..].contains("docker") && p[deny..].contains(".sock"),
                "the docker daemon socket class must be denied:\n{p}"
            );
            if confined {
                // Keep the denies terminal: they are appended after the
                // re-allow so no later clause can widen them back.
                assert!(
                    deny > p.find("(allow network* (local unix)").unwrap(),
                    "the daemon deny must follow the unix re-allow"
                );
            }
        }
        // The opt-out really opts out.
        assert!(
            !seatbelt_profile(false, UnixSockets::Open, Automation::Deny, 0, 0).contains("docker")
        );
    }

    #[tokio::test]
    async fn seatbelt_blocks_a_connect_to_a_denied_daemon_socket() {
        // A real unix socket at a denied path, in a temp dir we control, so the
        // test proves the *deny*, not the absence of Docker.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("docker.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::thread::spawn(move || {
            let _ = listener.accept();
        });
        // Positive control: a *differently named* socket in the same dir connects.
        let allowed = dir.path().join("ok.sock");
        let l2 = std::os::unix::net::UnixListener::bind(&allowed).unwrap();
        std::thread::spawn(move || {
            let _ = l2.accept();
        });

        let connect = |p: &std::path::Path| format!("nc -U {} < /dev/null", p.display());
        let denied = run(&connect(&sock)).await;
        let ok = run(&connect(&allowed)).await;
        assert!(
            ok.status.success(),
            "the control socket must still connect: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
        assert!(
            !denied.status.success(),
            "a *.docker.sock connect must be denied"
        );
    }

    /// The honest half of the AppleEvents denial.
    ///
    /// What this proves: `(deny appleevent-send)` does not break plain
    /// AppleScript. That matters — the clause is worthless if it makes
    /// `osascript` unusable, and a blanket `(deny mach-lookup)` would.
    ///
    /// What it deliberately does *not* claim: that a cross-application Apple
    /// Event is newly blocked. On this host no such event is observable either
    /// way — `Terminal`/`Finder` hang identically with and without any sandbox
    /// (TCC gates them), and `System Events`, the one target that works
    /// unsandboxed, is already refused (-10004) by the *existing* profile. An
    /// assertion that the event fails would therefore pass before the change
    /// as well: a vacuous test. See the plan's execution findings.
    #[tokio::test]
    async fn seatbelt_denies_apple_events_without_breaking_plain_applescript() {
        if !std::path::Path::new("/usr/bin/osascript").exists() {
            panic!("osascript is part of the base system; its absence is the bug, not a skip");
        }
        let ok = run("osascript -e 'return 1 + 1'").await;
        assert!(
            ok.status.success(),
            "osascript itself must still run under the deny: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
        assert!(String::from_utf8_lossy(&ok.stdout).contains('2'));
    }

    #[test]
    fn label_marks_only_the_opted_out_state() {
        assert_eq!(
            SandboxStatus::Enforced("seatbelt").label(),
            "sandboxed:seatbelt"
        );
        // With the opt-out active the marker rides every ask; here assert the
        // pure renderer.
        assert_eq!(
            label_with(
                "seatbelt",
                UnixSockets::Open,
                Automation::Allow,
                Reads::Open
            ),
            "sandboxed:seatbelt unix:open automation:allow reads:open"
        );
        // The default carve is silent — plan 0022 does not add a marker to
        // every ask on a hardened host.
        assert_eq!(
            label_with(
                "seatbelt",
                UnixSockets::DenyDaemons,
                Automation::Deny,
                Reads::Carved
            ),
            "sandboxed:seatbelt"
        );
        // An un-inited process has no carve at all and still says nothing.
        assert_eq!(read_posture(), Reads::Carved);
    }

    #[tokio::test]
    async fn seatbelt_egress_off_confines_to_loopback() {
        // A loopback listener the confined command may reach.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let _ = listener.accept();
        });
        // Loopback connect: allowed under egress Off.
        let ok = run_with(&format!("nc -z -G 2 127.0.0.1 {port}"), &EgressState::Off).await;
        assert!(
            ok.status.success(),
            "loopback connect should be allowed under egress off: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
        // Outbound non-loopback connect: must not succeed. (On an offline
        // machine it also fails — that is the safe direction to assert.)
        let denied = run_with("nc -z -G 2 1.1.1.1 443", &EgressState::Off).await;
        assert!(
            !denied.status.success(),
            "non-loopback connect must fail under egress off"
        );
    }

    #[tokio::test]
    async fn seatbelt_confines_writes() {
        // Write inside cwd: allowed.
        let inside = format!(
            "touch ./.hotl-sbx-ok-{} && rm ./.hotl-sbx-ok-{}",
            std::process::id(),
            std::process::id()
        );
        let ok = run(&inside).await;
        assert!(
            ok.status.success(),
            "cwd write should be allowed: {}",
            String::from_utf8_lossy(&ok.stderr)
        );

        // Write outside (HOME): denied by the floor.
        let home = std::env::var("HOME").expect("HOME");
        let target = format!("{home}/.hotl-sbx-denied-{}", std::process::id());
        let outside = format!("touch {target}");
        let denied = run(&outside).await;
        let leaked = std::path::Path::new(&target).exists();
        if leaked {
            std::fs::remove_file(&target).ok();
        }
        assert!(
            !denied.status.success(),
            "write outside cwd must fail under the floor"
        );
        assert!(!leaked, "file must not exist outside the sandbox");

        // Reads outside cwd stay allowed *except* where plan 0022 carves
        // them. This process installed no extras, so the carve is empty and
        // `$HOME` is wholly readable; `seatbelt_denies_reading_a_carved_path`
        // owns the negative, and `tests/sandbox_read_carve.rs` owns it
        // through the process-global path.
        let read = run(&format!("ls {home} > /dev/null")).await;
        assert!(
            read.status.success(),
            "with an empty carve, reads outside cwd stay allowed"
        );
        assert!(
            read_deny().is_empty(),
            "this process must not have installed a carve"
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    async fn run(cmd: &str) -> std::process::Output {
        let status = probe();
        assert!(
            matches!(status, SandboxStatus::Enforced(_)),
            "no landlock here? {status:?}"
        );
        build_command(cmd, &status, &EgressState::Open)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .expect("spawn")
    }

    /// `seatbelt_confines_writes` had no Linux twin. This is it.
    #[tokio::test]
    async fn landlock_confines_writes() {
        let dir = probe_dir().expect("a probe dir outside the confinement");
        let target = dir.join(format!("hotl-ll-write-{}", std::process::id()));
        let _ = std::fs::remove_file(&target);

        // Inside cwd: allowed.
        let ok = run(&format!(
            "touch ./.hotl-ll-ok-{0} && rm ./.hotl-ll-ok-{0}",
            std::process::id()
        ))
        .await;
        assert!(
            ok.status.success(),
            "cwd write must be allowed: {}",
            String::from_utf8_lossy(&ok.stderr)
        );

        // Outside: denied, and nothing lands.
        let denied = run(&format!("echo x > {}", target.display())).await;
        let leaked = target.exists();
        if leaked {
            let _ = std::fs::remove_file(&target);
        }
        assert!(
            !denied.status.success(),
            "write outside cwd must fail under landlock"
        );
        assert!(!leaked, "file must not exist outside the sandbox");

        // Reads outside cwd stay allowed *except* where plan 0022 carves
        // them. This process installed no extras, so the carve is empty;
        // `landlock_denies_reading_a_carved_path` owns the negative.
        assert!(run("ls / > /dev/null").await.status.success());
        assert!(
            read_deny().is_empty(),
            "this process must not have installed a carve"
        );
    }

    /// Plan 0022. Landlock's rights union across ancestors, so a denial is
    /// expressed by *not* granting read on any ancestor — the
    /// `grant_read_except` descent. Driven through the `apply_landlock_with`
    /// seam, never the process-global EXTRAS.
    #[tokio::test]
    async fn landlock_denies_reading_a_carved_path() {
        let base = probe_dir().expect("a dir outside the floor");
        let root = base.join(format!("hotl-ll-carve-{}", std::process::id()));
        let carved = root.join("secret");
        let open = root.join("ordinary");
        std::fs::create_dir_all(&carved).unwrap();
        std::fs::create_dir_all(&open).unwrap();
        std::fs::write(carved.join("id_probe"), b"canary").unwrap();
        std::fs::write(open.join("id_probe"), b"canary").unwrap();

        let run_carved = |script: String, noread: Vec<PathBuf>| async move {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-c").arg(script);
            apply_landlock_with(cmd, &EgressState::Open, &[], &noread)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
                .await
                .expect("spawn")
        };
        let cat = |p: &std::path::Path| format!("cat {}", p.display());

        // Positive control: with nothing carved, the canary reads. Without
        // this the negative would pass on a host where nothing is readable.
        let control = run_carved(cat(&carved.join("id_probe")), vec![]).await;
        assert!(
            control.status.success(),
            "the canary must read without a carve: {}",
            String::from_utf8_lossy(&control.stderr)
        );

        let carve = vec![carved.clone()];
        let denied = run_carved(cat(&carved.join("id_probe")), carve.clone()).await;
        let sibling = run_carved(cat(&open.join("id_probe")), carve).await;
        std::fs::remove_dir_all(&root).ok();
        assert!(!denied.status.success(), "a carved path must not read");
        assert!(!String::from_utf8_lossy(&denied.stdout).contains("canary"));
        assert!(
            sibling.status.success(),
            "a path outside the carve must still read: {}",
            String::from_utf8_lossy(&sibling.stderr)
        );
    }

    /// The failure this test exists to catch is a missing ancestor `ReadDir`
    /// rule, which would break every user's shell in one commit: no rule
    /// covers `~` itself, so `ls ~` fails.
    #[tokio::test]
    async fn landlock_narrowing_keeps_ancestor_listing_working() {
        let base = probe_dir().expect("a dir outside the floor");
        let root = base.join(format!("hotl-ll-listing-{}", std::process::id()));
        let carved = root.join(".ssh");
        std::fs::create_dir_all(&carved).unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        std::fs::write(carved.join("id_probe"), b"canary").unwrap();

        let run_carved = |script: String| {
            let noread = vec![carved.clone()];
            async move {
                let mut cmd = tokio::process::Command::new("sh");
                cmd.arg("-c").arg(script);
                apply_landlock_with(cmd, &EgressState::Open, &[], &noread)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .output()
                    .await
                    .expect("spawn")
            }
        };
        let list = run_carved(format!("ls {} > /dev/null", root.display())).await;
        let long = run_carved(format!("ls -la {} > /dev/null", root.display())).await;
        let read = run_carved(format!("cat {}", carved.join("id_probe").display())).await;
        std::fs::remove_dir_all(&root).ok();
        assert!(
            list.status.success(),
            "listing the carve's parent must work: {}",
            String::from_utf8_lossy(&list.stderr)
        );
        assert!(
            long.status.success(),
            "`ls -la` must work — it stats every child: {}",
            String::from_utf8_lossy(&long.stderr)
        );
        assert!(!read.status.success(), "the carved file must not read");
    }

    /// The descent readdirs `$HOME` and adds a rule per entry, so it must not
    /// run per spawn. Two spawns through the global path share one fd.
    #[test]
    fn landlock_ruleset_is_built_once_per_process() {
        use std::os::unix::io::AsRawFd;
        let first = cached_ruleset(&EgressState::Open);
        let second = cached_ruleset(&EgressState::Open);
        match (first, second) {
            (Some(a), Some(b)) => assert_eq!(
                a.as_raw_fd(),
                b.as_raw_fd(),
                "the ruleset must be memoized, not rebuilt per spawn"
            ),
            (None, None) => assert!(
                landlock_abi().is_none(),
                "a Landlock host must produce a ruleset"
            ),
            (a, b) => panic!(
                "memoization is not stable: {:?}/{:?}",
                a.is_some(),
                b.is_some()
            ),
        }
    }

    /// The Linux twin of `seatbelt_allows_writes_under_a_configured_extra_dir`.
    #[tokio::test]
    async fn landlock_allows_writes_under_a_configured_extra_dir() {
        // The extra must live *outside* the default floor or the test is
        // vacuous (a tempdir sits under TMPDIR, which is already writable) —
        // so carve it out of the probe dir. Driven through the
        // `apply_landlock_with` seam, never the process-global EXTRAS.
        let base = probe_dir().expect("a dir outside the floor");
        let extra = base.join(format!("hotl-ll-extra-{}", std::process::id()));
        std::fs::create_dir_all(&extra).unwrap();
        let target = extra.join("cache-file");
        let script = format!("echo x > {}", target.display());

        // Control: without the extra configured, the floor denies the write.
        let denied = run(&script).await;
        assert!(
            !denied.status.success(),
            "control write outside the floor must be denied"
        );

        // Same write with the extra configured: allowed.
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(&script);
        let ok = apply_landlock_with(cmd, &EgressState::Open, std::slice::from_ref(&extra), &[])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .expect("spawn");
        let content = std::fs::read_to_string(&target).unwrap_or_default();
        std::fs::remove_dir_all(&extra).ok();
        assert!(
            ok.status.success(),
            "write under a configured extra must be allowed: {}",
            String::from_utf8_lossy(&ok.stderr)
        );
        assert_eq!(content.trim(), "x");
    }

    /// A shell word that calls `truncate(2)` **by path** on `victim`.
    ///
    /// The demonstrator has to reach the raw syscall. Neither `truncate -s 0`
    /// (GNU coreutils `open`s the file `O_WRONLY` first) nor `> file` (open
    /// with `O_CREAT|O_TRUNC`) will do: `AccessFs::WriteFile` already denies
    /// both at ABI v1, so they are denied whether or not the truncate right is
    /// handled and prove nothing either way. Measured against the v2 ruleset:
    /// coreutils and the redirect are denied with EACCES while a raw
    /// `truncate(2)` zeroes the file. `perl` first, `python3` as the fallback;
    /// with neither the test fails loudly rather than skipping (a skip here
    /// would be indistinguishable from enforcement).
    fn truncate_by_path(victim: &std::path::Path) -> String {
        let p = victim.display();
        if std::path::Path::new("/usr/bin/perl").exists() {
            return format!("perl -e 'truncate($ARGV[0], 0) or die $!' {p}");
        }
        format!("python3 -c \"import os,sys; os.truncate(sys.argv[1], 0)\" {p}")
    }

    /// The verified hole: `truncate(2)` by path is a *v3* right and the
    /// ruleset only handled v2, so this succeeded on every kernel.
    #[tokio::test]
    async fn landlock_confines_truncate_by_path() {
        if !matches!(probe(), SandboxStatus::Enforced("landlock")) {
            // A `landlock(partial)` host genuinely cannot enforce this; the
            // label says so. Assert *that*, rather than skipping silently.
            assert!(
                landlock_abi().is_some_and(|a| (a as i32) < 3),
                "v3+ host must certify fully"
            );
            return;
        }
        let dir = probe_dir().expect("a probe dir");
        let victim = dir.join(format!("hotl-ll-trunc-{}", std::process::id()));
        std::fs::write(&victim, b"important").unwrap();
        let out = run(&truncate_by_path(&victim)).await;
        let size = std::fs::metadata(&victim).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_file(&victim);
        assert!(
            !out.status.success(),
            "truncate outside cwd must be denied: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(size, 9, "the file outside the workspace must be untouched");
    }

    #[test]
    fn abi_below_v3_is_not_silently_certified() {
        // Whatever this kernel is, the verdict and the ABI must agree.
        match (probe(), landlock_abi()) {
            (SandboxStatus::Enforced("landlock"), Some(abi)) => assert!(abi as i32 >= 3),
            (SandboxStatus::Enforced("landlock(partial)"), Some(abi)) => {
                assert!((abi as i32) < 3);
                assert_eq!(std::env::var("HOTL_SANDBOX").as_deref(), Ok("best-effort"));
            }
            (SandboxStatus::Unavailable(reason), Some(abi)) => {
                assert!((abi as i32) < 3 && reason.contains("truncate"), "{reason}");
            }
            (status, abi) => panic!("inconsistent: {status:?} / {abi:?}"),
        }
    }
}
