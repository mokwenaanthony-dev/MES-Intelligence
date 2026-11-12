//! `edge-agent`: connects one cell's PLC to the Unified Namespace.
//!
//! ```text
//! edge-agent --cell 1000/1000/assy/l1/press3 --plc plc-press3:5020 --mqtt-url mqtt://broker:1883
//! ```
//!
//! | Option | Environment | Default | |
//! |---|---|---|---|
//! | `--cell` | `EDGE_CELL` | (required) | the cell's UNS path |
//! | `--plc` | `EDGE_PLC` | `localhost:5020` | the PLC's Modbus TCP address |
//! | `--unit` | `EDGE_UNIT` | `1` | the PLC's Modbus unit id |
//! | `--mqtt-url` | `MQTT_URL` | `mqtt://localhost:1883` | the broker (credentials allowed, never logged) |
//! | `--state-file` | `EDGE_STATE_FILE` | `edge-state.txt` | remembers the current job across restarts |
//! | `--poll-ms` | `EDGE_POLL_MS` | `500` | how often the PLC is read |
//!
//! The MQTT session is persistent (client id `mes-edge-<cell code>`), and the broker publishes the
//! cell as OFFLINE if the agent dies (last will). Stop it with Ctrl+C or `docker stop`.

use std::path::PathBuf;
use std::time::Duration;

use mes_edge::agent::{self, Agent, Config, ModbusMachine};
use mes_edge::{error, info, mqtt};

fn setting(args: &[String], flag: &str, env: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| std::env::var(env).ok())
        .filter(|value| !value.is_empty())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("edge-agent --cell PATH [--plc HOST:PORT] [--unit N] [--mqtt-url URL] [--state-file FILE] [--poll-ms N]");
        return;
    }
    mes_edge::log::set_level(&std::env::var("LOG_LEVEL").unwrap_or_else(|_| "INFO".into()));
    let Some(cell) = setting(&args, "--cell", "EDGE_CELL") else {
        error!("--cell (or EDGE_CELL) is required, e.g. 1000/1000/assy/l1/press3");
        std::process::exit(2)
    };
    if cell.split('/').count() != 5 || cell.contains(['+', '#']) {
        error!("--cell must be a cell path of five levels without wildcards, got {cell:?}");
        std::process::exit(2)
    }
    let plc = setting(&args, "--plc", "EDGE_PLC").unwrap_or_else(|| "localhost:5020".into());
    let unit: u8 = setting(&args, "--unit", "EDGE_UNIT")
        .and_then(|u| u.parse().ok())
        .unwrap_or(1);
    let url =
        setting(&args, "--mqtt-url", "MQTT_URL").unwrap_or_else(|| "mqtt://localhost:1883".into());
    let state_file = PathBuf::from(
        setting(&args, "--state-file", "EDGE_STATE_FILE")
            .unwrap_or_else(|| "edge-state.txt".into()),
    );
    let poll_ms: u64 = setting(&args, "--poll-ms", "EDGE_POLL_MS")
        .and_then(|p| p.parse().ok())
        .unwrap_or(500);

    let code = cell.rsplit('/').next().unwrap_or("cell").to_string();
    let mut options =
        mqtt::Options::from_url(&url, &format!("mes-edge-{code}")).unwrap_or_else(|e| {
            error!("{e}");
            std::process::exit(2)
        });
    options.clean_session = false; // keep the session: messages for the agent wait while it restarts
    info!(
        "Edge agent for {cell}: PLC {plc} (unit {unit}), broker {}",
        mqtt::redact(&url)
    );

    let agent = Agent::new(Config {
        cell,
        state_file,
        progress_interval: Duration::from_secs(1),
    });
    let mut machine = ModbusMachine::new(&plc, unit);
    agent::run(
        agent,
        options,
        &mut machine,
        Duration::from_millis(poll_ms),
        &|| false,
    );
}
