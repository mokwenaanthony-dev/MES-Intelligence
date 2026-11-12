//! `plc-sim`: a simulated PLC on Modbus TCP, so the edge agent has a machine to talk to.
//!
//! ```text
//! plc-sim --listen 0.0.0.0:5020 --cycle-seconds 378 --speed 3600 --scrap-every 0
//! ```
//!
//! * `--cycle-seconds`: machine time per part (378 s makes 100 parts in 10.5 hours, as planned in the
//!   ERP's demo routing for PRESS3);
//! * `--speed`: machine seconds per real second (3600 = an hour a second, as the Python simulator);
//! * `--scrap-every`: every n-th part is scrap (0 = none).
//!
//! The register map is in `src/plc.rs`. Every option can also come from the environment
//! (`PLC_LISTEN`, `PLC_CYCLE_SECONDS`, `PLC_SPEED`, `PLC_SCRAP_EVERY`, `LOG_LEVEL`).

use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mes_edge::plc::Plc;
use mes_edge::{error, info, modbus};

/// Command-line option, else environment variable, else default.
fn setting(args: &[String], flag: &str, env: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| std::env::var(env).ok())
        .unwrap_or_else(|| default.to_string())
}

fn number<T: std::str::FromStr>(text: &str, name: &str) -> T {
    text.parse().unwrap_or_else(|_| {
        error!("{name} must be a number, got {text:?}");
        std::process::exit(2)
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("plc-sim [--listen ADDR] [--cycle-seconds N] [--speed N] [--scrap-every N]");
        return;
    }
    mes_edge::log::set_level(&std::env::var("LOG_LEVEL").unwrap_or_else(|_| "INFO".into()));
    let listen = setting(&args, "--listen", "PLC_LISTEN", "0.0.0.0:5020");
    let cycle_seconds: f64 = number(
        &setting(&args, "--cycle-seconds", "PLC_CYCLE_SECONDS", "378"),
        "--cycle-seconds",
    );
    let speed: f64 = number(&setting(&args, "--speed", "PLC_SPEED", "3600"), "--speed");
    let scrap_every: u32 = number(
        &setting(&args, "--scrap-every", "PLC_SCRAP_EVERY", "0"),
        "--scrap-every",
    );

    let plc = Arc::new(Mutex::new(Plc::new(
        (cycle_seconds * 1_000.0) as u64,
        scrap_every,
    )));

    // The machine's clock: every 50 ms of real time, `speed` times as much machine time passes.
    let clock = Arc::clone(&plc);
    std::thread::Builder::new()
        .name("machine".into())
        .spawn(move || {
            let mut last = Instant::now();
            let mut carry = 0.0f64; // fractions of a machine millisecond, so nothing is lost to rounding
            loop {
                std::thread::sleep(Duration::from_millis(50));
                let now = Instant::now();
                let machine_ms = (now - last).as_secs_f64() * 1_000.0 * speed + carry;
                last = now;
                carry = machine_ms.fract();
                clock
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .advance(machine_ms as u64);
            }
        })
        .expect("start the machine clock");

    let listener = TcpListener::bind(&listen).unwrap_or_else(|e| {
        error!("Cannot listen on {listen}: {e}");
        std::process::exit(1)
    });
    info!("Simulated PLC on {listen}: {cycle_seconds} s per part, speed x{speed}, scrap every {scrap_every}");
    modbus::serve(listener, plc);
}
