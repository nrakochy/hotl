//! hotl — one binary, three capabilities (watch · orchestrate · execute).
//!
//! Subcommands:
//!   (none)        the agent REPL (execute — the M0 default flip, merge plan 0002)
//!   -p "prompt"   headless one-shot; asks default-deny (Sec #11); --json for events
//!   watch         the tmux dashboard (the pre-merge `hotl`)
//!   fleet         reserved (orchestrate, M4+)
//!   doctor        reserved (MD)
//!   update        reserved (MD)

mod agent;
mod watch;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("watch") => {
            if let Err(e) = watch::watch_main() {
                eprintln!("hotl watch: {e}");
                std::process::exit(1);
            }
        }
        Some("fleet") => {
            eprintln!("hotl fleet (orchestrate) is reserved and not built yet — it arrives after the harness's M4 seams. See docs/exec-plans/active/0001-harness-build.md.");
            std::process::exit(2);
        }
        Some("doctor") | Some("update") => {
            eprintln!("`hotl {}` is reserved for the distribution milestone (MD) and not built yet.", args[0]);
            std::process::exit(2);
        }
        Some("--help") | Some("-h") | Some("help") => {
            println!(
                "hotl — human on the loop\n\n\
                 USAGE:\n  hotl                 agent REPL (execute)\n  \
                 hotl -p \"prompt\"     headless one-shot (add --json for a JSONL event stream)\n  \
                 hotl watch           tmux agent dashboard (watch)\n  \
                 hotl fleet           reserved (orchestrate)\n\n\
                 ENV:\n  ANTHROPIC_API_KEY    required for the agent\n  \
                 HOTL_MODEL           model override (default {})",
                hotl_provider_anthropic::DEFAULT_MODEL
            );
        }
        _ => {
            // The agent surface. One-shot CLI runs current_thread per the
            // async policy (no pool spinup on the cold-start path).
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            let code = rt.block_on(agent::agent_main(args));
            std::process::exit(code);
        }
    }
}
