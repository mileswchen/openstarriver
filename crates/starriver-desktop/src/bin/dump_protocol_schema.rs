//! Regenerate `desktop/electron/protocol.schema.json` (the schema source for
//! FPM P1 code generation).
//!
//! Usage: `cargo run -p starriver-desktop --bin dump_protocol_schema`
//! Once committed, `protocol_catalog::tests::committed_protocol_schema_is_up_to_date`
//! guards against drift.

fn main() {
    let json = starriver_desktop::protocol_catalog::protocol_schema_json();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../desktop/electron/protocol.schema.json");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, json).expect("write protocol.schema.json");
    println!("wrote {}", path.display());
}
