//! The edge agent's ERP-contract messages, pinned as files (MES 0.2.1, ADR 0007).
//!
//! The agent writes `equipment-state/v1`, a message of the ERP's UNS contract. The Rust crate has no
//! JSON Schema library (standard library only, ADR 0005), so the check is a chain of two links:
//!
//! 1. this test proves the agent's messages have exactly the shape of the files in `tests/golden/`
//!    (same fields, same order, same values, except the ones that change on every run: the
//!    timestamp, `since` and `session`, which only have to be strings);
//! 2. `tests/test_contract_schemas.py` (Python) checks those same files against the ERP's JSON
//!    Schemas pinned in `contract/erp-uns/v1/`.
//!
//! So a change to what the agent writes fails here until the files change, and a file that breaks the
//! contract fails there.

use std::path::PathBuf;
use std::time::Duration;

use mes_edge::agent::{Agent, Config};
use mes_edge::json::{self, Value};
use mes_edge::messages;

/// Fields whose value changes on every run: they must be strings, their text is not compared.
const CHANGING: [&str; 3] = ["timestamp", "since", "session"];

fn golden(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    let text = std::fs::read_to_string(&path).expect("golden file");
    json::parse(&text).expect("golden file is JSON")
}

/// The value with every changing field replaced by a marker (only if it is a string).
fn shape(value: &Value) -> Value {
    match value {
        Value::Object(pairs) => Value::Object(
            pairs
                .iter()
                .map(|(key, v)| {
                    let v = match v {
                        Value::Str(_) if CHANGING.contains(&key.as_str()) => {
                            Value::str("<changes>")
                        }
                        other => shape(other),
                    };
                    (key.clone(), v)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn assert_same_shape(built: &[u8], name: &str) {
    let built = json::parse(std::str::from_utf8(built).unwrap()).unwrap();
    assert_eq!(
        shape(&built).to_json(),
        shape(&golden(name)).to_json(),
        "the agent's message differs from tests/golden/{name}: change the file only if the \
         contract allows it (tests/test_contract_schemas.py checks it)"
    );
}

fn agent() -> Agent {
    let dir = std::env::temp_dir().join(format!("mes-edge-golden-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    Agent::new(Config {
        cell: "1000/1000/assy/l1/press3".into(),
        state_file: dir.join("state.txt"),
        progress_interval: Duration::ZERO,
        heartbeat: Duration::ZERO,
        stall_after: Duration::ZERO,
    })
}

#[test]
fn the_last_will_has_the_pinned_shape() {
    let will = agent().begin_session();
    assert_eq!(will.topic, "1000/1000/assy/l1/press3/state/status");
    assert!(will.retain);
    assert_same_shape(&will.payload, "equipment-state-last-will.json");
}

#[test]
fn state_reports_have_the_pinned_shape() {
    let mut agent = agent();
    agent.begin_session();
    let source = "mes-edge/press3";
    let now = mes_edge::time::now_iso();
    let running = messages::envelope(
        messages::EQUIPMENT_STATE,
        source,
        messages::state_data("RUNNING", Some(("MO00000001", 10)), &now, agent.session()),
    );
    assert_same_shape(&running, "equipment-state-running.json");
    let idle = messages::envelope(
        messages::EQUIPMENT_STATE,
        source,
        messages::state_data("IDLE", None, &now, agent.session()),
    );
    assert_same_shape(&idle, "equipment-state-idle.json");
}
