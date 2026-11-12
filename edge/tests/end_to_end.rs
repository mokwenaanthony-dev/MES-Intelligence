//! End to end: broker, simulated PLC over Modbus TCP and the edge agent, as separate network peers.
//!
//! The scenario plays the MES's part: it puts a job on `<cell>/schedule/current-job` (retained),
//! waits for the agent to report the job COMPLETE on `<cell>/state/job`, then clears the job and
//! checks the machine goes back to idle. It runs against the in-process test broker always, and
//! against a real Mosquitto when `MQTT_URL` is set (CI starts one).

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mes_edge::agent::{self, Agent, Config, ModbusMachine};
use mes_edge::json::{self, Value};
use mes_edge::mqtt::{self, Client, Options};
use mes_edge::plc::{Plc, State};

/// A topic root no other run uses, so parallel tests and old retained messages never interfere.
fn unique_cell() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("e{}x{nanos}/1000/assy/l1/press3", std::process::id())
}

fn options(url: &str, client_id: &str) -> Options {
    let mut options = Options::from_url(url, client_id).unwrap();
    options.keep_alive = Duration::from_secs(10);
    options
}

/// The `data` object of a received message.
fn data(message: &mqtt::Message) -> Value {
    json::parse(std::str::from_utf8(&message.payload).unwrap())
        .unwrap()
        .get("data")
        .unwrap()
        .clone()
}

fn field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Wait for a message on `topic` whose data passes `wanted`, up to 15 seconds.
fn wait_for(observer: &mut Client, topic: &str, wanted: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Some(message) = observer.poll(Duration::from_millis(200)).unwrap() {
            if message.topic == topic && !message.payload.is_empty() && wanted(&data(&message)) {
                return data(&message);
            }
        }
    }
    panic!("no matching message on {topic}");
}

fn scenario(url: &str) {
    let cell = unique_cell();
    let dir =
        std::env::temp_dir().join(format!("mes-edge-e2e-{}", cell.split('/').next().unwrap()));
    std::fs::create_dir_all(&dir).unwrap();

    // The simulated PLC on a free port, its clock fast: 10 parts per 10 ms tick.
    let plc = Arc::new(Mutex::new(Plc::new(1_000, 4)));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let plc_address = listener.local_addr().unwrap().to_string();
    let served = Arc::clone(&plc);
    std::thread::spawn(move || mes_edge::modbus::serve(listener, served));
    let clock = Arc::clone(&plc);
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(10));
        clock.lock().unwrap().advance(10_000);
    });

    // An observer (subscribed first, so it sees everything) and the MES's job, retained.
    let mut observer = Client::connect(&options(url, &format!("observer-{}", cell.len()))).unwrap();
    observer.subscribe(&format!("{cell}/state/#")).unwrap();
    let mut mes = Client::connect(&options(url, "mes-test")).unwrap();
    let job = r#"{"schema":"job/v1","event_id":null,"timestamp":"t","source":"mes-intelligence",
        "data":{"job_id":"mo00000001-0010","order":"MO00000001","operation_number":10,"quantity":"30.000","unit":"EA"}}"#;
    mes.publish(
        &format!("{cell}/schedule/current-job"),
        job.as_bytes(),
        true,
    )
    .unwrap();

    // The agent, in its own thread, until `stop` is set.
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let agent_cell = cell.clone();
    let agent_url = url.to_string();
    let agent_thread = std::thread::spawn(move || {
        let agent = Agent::new(Config {
            cell: agent_cell,
            state_file: dir.join("state.txt"),
            progress_interval: Duration::from_millis(100),
        });
        let mut machine = ModbusMachine::new(&plc_address, 1);
        agent::run(
            agent,
            options(&agent_url, "edge-under-test"),
            &mut machine,
            Duration::from_millis(50),
            &|| stopped.load(Ordering::Relaxed),
        );
    });

    // The agent starts the machine and reports the job complete: 30 good parts, every 4th scrapped.
    let complete = wait_for(&mut observer, &format!("{cell}/state/job"), |d| {
        field(d, "status") == "COMPLETE"
    });
    assert_eq!(field(&complete, "job_id"), "mo00000001-0010");
    assert_eq!(field(&complete, "good_quantity"), "30");
    assert_eq!(field(&complete, "scrap_quantity"), "9"); // parts 4, 8 ... 36 of 39
    assert_eq!(field(&complete, "run_hours"), "0.011"); // 39 parts x 1 s = 39 s
    assert_eq!(plc.lock().unwrap().state(), State::Complete);

    // The MES clears the job: the agent resets the machine.
    mes.publish(&format!("{cell}/schedule/current-job"), b"", true)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while plc.lock().unwrap().state() != State::Idle {
        assert!(Instant::now() < deadline, "the machine was not reset");
        std::thread::sleep(Duration::from_millis(20));
    }

    // A clean stop: DISCONNECT, so the broker does not publish the OFFLINE will.
    stop.store(true, Ordering::Relaxed);
    agent_thread.join().unwrap();
    let late = observer.poll(Duration::from_millis(500)).unwrap();
    assert!(
        late.as_ref()
            .is_none_or(|m| !String::from_utf8_lossy(&m.payload).contains("OFFLINE")),
        "the last will was published after a clean stop"
    );

    // Tidy the broker: retained messages of this run are cleared.
    for topic in ["state/job", "state/status", "schedule/current-job"] {
        mes.publish(&format!("{cell}/{topic}"), b"", true).unwrap();
    }
}

#[test]
fn edge_agent_against_the_test_broker() {
    let broker = common::TestBroker::start();
    scenario(&broker.url);
}

#[test]
fn edge_agent_against_mosquitto() {
    match std::env::var("MQTT_URL") {
        Ok(url) if !url.is_empty() => scenario(&url),
        _ => eprintln!("MQTT_URL not set: skipped (CI runs it against Mosquitto)"),
    }
}

/// The last will: if a client vanishes without DISCONNECT, the broker publishes its will.
fn last_will(url: &str) {
    let topic = format!("{}/state/status", unique_cell());
    let mut observer = Client::connect(&options(url, "will-observer")).unwrap();
    observer.subscribe(&topic).unwrap();
    let mut doomed = options(url, "will-doomed");
    doomed.will = Some(mqtt::Will {
        topic: topic.clone(),
        payload: b"OFFLINE".to_vec(),
        retain: true,
    });
    let client = Client::connect(&doomed).unwrap();
    drop(client); // the TCP connection closes without DISCONNECT, as when the agent crashes
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(Instant::now() < deadline, "the will was not published");
        if let Some(message) = observer.poll(Duration::from_millis(200)).unwrap() {
            assert_eq!(
                (message.topic.as_str(), message.payload.as_slice()),
                (topic.as_str(), &b"OFFLINE"[..])
            );
            break;
        }
    }
    let mut tidy = Client::connect(&options(url, "will-tidy")).unwrap();
    tidy.publish(&topic, b"", true).unwrap();
}

#[test]
fn last_will_against_the_test_broker() {
    let broker = common::TestBroker::start();
    last_will(&broker.url);
    assert_eq!(broker.retained("nothing"), None);
}

#[test]
fn last_will_against_mosquitto() {
    match std::env::var("MQTT_URL") {
        Ok(url) if !url.is_empty() => last_will(&url),
        _ => eprintln!("MQTT_URL not set: skipped (CI runs it against Mosquitto)"),
    }
}
