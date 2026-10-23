//! `hotl schema json-stream|acp` (0076): print a published wire schema. The
//! same bytes `site/public/schema/*.schema.json` hold — the drift test in
//! `tests/wire_schema.rs` keeps the two equal.

/// The schema text for `which`, pretty-printed with a trailing newline.
pub fn render(which: &str) -> Option<String> {
    let v = match which {
        "json-stream" => crate::wire::json_stream_schema(),
        "acp" => crate::acp::acp_schema(),
        _ => return None,
    };
    let mut s = serde_json::to_string_pretty(&sorted(v)).expect("a schema always serializes");
    s.push('\n');
    Some(s)
}

pub fn schema_main(args: &[String]) -> i32 {
    match args.first().and_then(|w| render(w)) {
        Some(text) => {
            print!("{text}");
            0
        }
        None => {
            eprintln!("usage: hotl schema json-stream|acp");
            2
        }
    }
}

/// Keys in sorted order at every level: `serde_json`'s map keeps insertion
/// order in the default build (`preserve_order`) and sorts without it, so the
/// published bytes are pinned to one order regardless of features.
fn sorted(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            serde_json::Value::Object(entries.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sorted).collect())
        }
        other => other,
    }
}
