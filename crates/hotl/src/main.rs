//! hotl — one binary, three capabilities (watch · orchestrate · execute).
//!
//! Phase-1 wiring (merge plan 0002): the dashboard moves behind `hotl watch`;
//! bare `hotl` still launches it (with a note) until the harness's M0 flips
//! the default. `fleet`, `doctor`, `update` are reserved.

mod watch;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("watch") => run_watch(),
        Some("fleet") => {
            eprintln!("hotl fleet (orchestrate) is reserved and not built yet — it arrives after the harness's M4 seams.");
            std::process::exit(2);
        }
        Some("doctor") | Some("update") => {
            eprintln!("`hotl {}` is reserved for the distribution milestone (MD) and not built yet.", args[0]);
            std::process::exit(2);
        }
        Some("--help") | Some("-h") | Some("help") => {
            println!("hotl — human on the loop\n\nUSAGE:\n  hotl watch   tmux agent dashboard\n  hotl         (currently) the dashboard; becomes the agent at harness M0");
        }
        _ => {
            eprintln!("note: the dashboard is moving to `hotl watch`; bare `hotl` becomes the agent at harness M0.");
            run_watch();
        }
    }
}

fn run_watch() {
    if let Err(e) = watch::watch_main() {
        eprintln!("hotl watch: {e}");
        std::process::exit(1);
    }
}
