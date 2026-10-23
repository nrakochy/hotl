//! `hotl features [--md]` (0076): the feature-stage table. `--md` prints the
//! markdown the docs page embeds.

use crate::features::{markdown, FEATURES};

pub fn features_main(args: &[String]) -> i32 {
    if args.iter().any(|a| a == "--md") {
        print!("{}", markdown());
        return 0;
    }
    if let Some(bad) = args.first() {
        eprintln!("hotl features: unknown argument `{bad}` (try --md)");
        return 2;
    }
    for f in FEATURES {
        println!(
            "{:<12} {:<44} {:<7} {} — {}",
            f.stage.as_str(),
            f.switch.label(),
            f.since,
            f.name,
            f.summary
        );
    }
    0
}
