//! The edge agent: one cell's link between the MES (through MQTT) and its PLC (through Modbus).
//!
//! It works like a control loop, the way Kubernetes controllers do. It keeps two pictures and makes
//! the second match the first, over and over:
//!
//! * **wanted**: the job the MES has put on `<cell>/schedule/current-job` (retained), or none;
//! * **actual**: what the PLC reports in its registers.
//!
//! Every tick it reads the PLC, starts or resets the machine when the two pictures differ, and
//! publishes what changed: the cell state (`equipment-state/v1`) and the job's progress
//! (`job-progress/v1`), both retained. Because it compares state rather than reacting to single
//! messages, the agent is **idempotent** by construction:
//!
//! * the same job message twice: wanted is unchanged, nothing happens;
//! * the agent restarts mid-job: the state file says which job it started, the PLC is still
//!   running it, so the agent carries on instead of starting again (the counters are kept);
//! * the agent restarts after finishing: the state file says the job is complete, so it reports
//!   the same totals again instead of making the parts twice;
//! * the PLC or the broker disappears: the agent says OFFLINE (or the broker publishes its last
//!   will) and picks up where it was when they come back.
//!
//! The agent never decides what to make. The MES (Level 3) dispatches; the agent (Level 2) executes
//! and reports. The MES turns a COMPLETE report into the ERP's operation-performance event.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::messages::{self, Job, Progress};
use crate::mqtt;
use crate::plc::{self, Reading, State};
use crate::{info, warning};

/// Where the agent publishes (the MQTT client in production, a recorder in tests).
pub trait Bus {
    /// Publish at QoS 1 and wait until the broker has it.
    fn publish(&mut self, topic: &str, payload: &[u8], retain: bool) -> io::Result<()>;
}

impl Bus for mqtt::Client {
    fn publish(&mut self, topic: &str, payload: &[u8], retain: bool) -> io::Result<()> {
        mqtt::Client::publish(self, topic, payload, retain)
    }
}

/// The machine the agent drives (the PLC over Modbus in production, an in-memory PLC in tests).
pub trait Machine {
    /// Read the machine's state and counters.
    fn read(&mut self) -> io::Result<Reading>;
    /// Start a job of `target` good parts (resets the counters).
    fn start(&mut self, target: u32) -> io::Result<()>;
    /// Stop and go back to idle.
    fn reset(&mut self) -> io::Result<()>;
}

/// The PLC over Modbus TCP, connecting (again) whenever it is needed.
pub struct ModbusMachine {
    address: String,
    unit: u8,
    client: Option<crate::modbus::Client>,
}

impl ModbusMachine {
    /// A PLC at `address` (e.g. `plc-press3:5020`), Modbus unit `unit`.
    pub fn new(address: &str, unit: u8) -> ModbusMachine {
        ModbusMachine {
            address: address.to_string(),
            unit,
            client: None,
        }
    }

    /// Run `action` on a connected client; on failure drop the connection so the next call reconnects.
    fn with_client<T>(
        &mut self,
        action: impl FnOnce(&mut crate::modbus::Client) -> io::Result<T>,
    ) -> io::Result<T> {
        if self.client.is_none() {
            self.client = Some(crate::modbus::Client::connect(
                &self.address,
                self.unit,
                Duration::from_secs(2),
            )?);
            info!("Connected to PLC at {}", self.address);
        }
        let result = action(self.client.as_mut().expect("connected above"));
        if result.is_err() {
            self.client = None;
        }
        result
    }
}

impl Machine for ModbusMachine {
    fn read(&mut self) -> io::Result<Reading> {
        let registers = self.with_client(|c| c.read_holding(plc::STATE, plc::READING_LENGTH))?;
        Reading::from_registers(&registers)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "wrong number of registers"))
    }

    fn start(&mut self, target: u32) -> io::Result<()> {
        let [high, low] = plc::split(target);
        // Command and target in one request, so the PLC never sees one without the other.
        self.with_client(|c| c.write_multiple(plc::COMMAND, &[plc::START, high, low]))
    }

    fn reset(&mut self) -> io::Result<()> {
        self.with_client(|c| c.write_multiple(plc::COMMAND, &[plc::RESET]))
    }
}

/// Settings of one agent.
#[derive(Clone, Debug)]
pub struct Config {
    /// The cell's UNS path, e.g. `1000/1000/assy/l1/press3`.
    pub cell: String,
    /// File that remembers the last job started or finished, so a restart is harmless.
    pub state_file: PathBuf,
    /// Least time between two RUNNING progress messages (COMPLETE is always sent at once).
    pub progress_interval: Duration,
}

impl Config {
    /// The `source` field of the agent's messages, e.g. `mes-edge/press3`.
    pub fn source(&self) -> String {
        format!(
            "mes-edge/{}",
            self.cell.rsplit('/').next().unwrap_or(&self.cell)
        )
    }
}

/// The agent's memory between ticks.
pub struct Agent {
    config: Config,
    /// The job the MES wants (from the last current-job message).
    wanted: Option<Job>,
    /// The job the PLC is working on (or finished), as last saved in the state file.
    saved: Option<Progress>,
    /// The last cell state published: (state, order, operation).
    last_state: Option<(String, Option<String>, Option<i64>)>,
    since: String,
    /// The last progress published, and when.
    last_progress: Option<Progress>,
    last_progress_at: Option<Instant>,
    /// When the broker connection was (re)made, and whether the MES's current-job topic has spoken
    /// since: an empty topic sends nothing, so "no job" is only believed after [`NO_JOB_GRACE`].
    connected_at: Instant,
    heard_from_mes: bool,
}

/// How long after connecting the agent waits for a retained current-job message before it treats
/// silence as "no job". Without it, a reconnect could reset a running machine because the retained
/// job had not arrived yet.
pub const NO_JOB_GRACE: Duration = Duration::from_secs(5);

/// Why a turn of the loop failed: the PLC (logged, retried next tick) or the broker (reconnect).
#[derive(Debug)]
enum Failure {
    Plc(io::Error),
    Bus(io::Error),
}

/// The equipment-state name for a PLC state (ERP contract v1: RUNNING, IDLE, SETUP, DOWN, OFFLINE).
pub fn state_name(state: State) -> &'static str {
    match state {
        State::Running => "RUNNING",
        State::Idle | State::Complete => "IDLE", // a finished job waits for the MES: the cell is idle
        State::Down => "DOWN",
    }
}

impl Agent {
    /// A new agent; reads the state file if there is one.
    pub fn new(config: Config) -> Agent {
        let saved = std::fs::read_to_string(&config.state_file)
            .ok()
            .and_then(|text| Progress::from_lines(&text));
        if let Some(progress) = &saved {
            info!(
                "State file: job {} was {}",
                progress.job_id, progress.status
            );
        }
        Agent {
            config,
            wanted: None,
            saved,
            last_state: None,
            since: crate::time::now_iso(),
            last_progress: None,
            last_progress_at: None,
            connected_at: Instant::now(),
            heard_from_mes: false,
        }
    }

    /// The topic the agent must subscribe to.
    pub fn subscription(&self) -> String {
        messages::current_job_topic(&self.config.cell)
    }

    /// The message the broker should publish if the agent dies: the cell is OFFLINE.
    pub fn last_will(&self) -> mqtt::Will {
        mqtt::Will {
            topic: messages::state_topic(&self.config.cell),
            payload: messages::envelope(
                messages::EQUIPMENT_STATE,
                &self.config.source(),
                messages::state_data("OFFLINE", None, &crate::time::now_iso()),
            ),
            retain: true,
        }
    }

    /// After a (re)connection: publish everything again, the broker may have lost it.
    pub fn connected(&mut self) {
        self.last_state = None;
        self.last_progress = None;
        self.connected_at = Instant::now();
        self.heard_from_mes = false;
    }

    /// Take a message from the broker. Only the current-job topic matters; anything else is ignored.
    pub fn on_message(&mut self, message: &mqtt::Message) {
        if message.topic != self.subscription() {
            return;
        }
        self.heard_from_mes = true;
        match messages::parse_job(&message.payload) {
            Ok(job) => {
                if job != self.wanted {
                    match &job {
                        Some(job) => info!("MES wants job {} ({} parts)", job.job_id, job.target),
                        None => info!("MES cleared the current job"),
                    }
                    self.wanted = job;
                }
            }
            Err(reason) => warning!("Ignored current-job message: {reason}"),
        }
    }

    fn save(&mut self, progress: Progress) {
        // Write a temporary file and rename it over the old one: a crash mid-write never leaves
        // half a file, because a rename within one directory is atomic.
        let temporary = self.config.state_file.with_extension("tmp");
        let written = std::fs::write(&temporary, progress.to_lines())
            .and_then(|()| std::fs::rename(&temporary, &self.config.state_file));
        if let Err(e) = written {
            warning!(
                "Could not save the state file {}: {e}",
                self.config.state_file.display()
            );
        }
        self.saved = Some(progress);
    }

    fn publish_state(
        &mut self,
        bus: &mut impl Bus,
        state: &str,
        job: Option<(&str, i64)>,
    ) -> io::Result<()> {
        let key = (
            state.to_string(),
            job.map(|j| j.0.to_string()),
            job.map(|j| j.1),
        );
        if self.last_state.as_ref() == Some(&key) {
            return Ok(()); // report by exception: only changes are published
        }
        if self.last_state.as_ref().map(|s| &s.0) != Some(&key.0) {
            self.since = crate::time::now_iso();
        }
        let payload = messages::envelope(
            messages::EQUIPMENT_STATE,
            &self.config.source(),
            messages::state_data(state, job, &self.since),
        );
        bus.publish(&messages::state_topic(&self.config.cell), &payload, true)?;
        info!("Cell state {state}");
        self.last_state = Some(key);
        Ok(())
    }

    fn publish_progress(&mut self, bus: &mut impl Bus, progress: &Progress) -> io::Result<()> {
        let payload = messages::envelope(
            messages::JOB_PROGRESS,
            &self.config.source(),
            progress.to_data(),
        );
        bus.publish(
            &messages::job_progress_topic(&self.config.cell),
            &payload,
            true,
        )?;
        self.last_progress = Some(progress.clone());
        self.last_progress_at = Some(Instant::now());
        Ok(())
    }

    /// One turn of the loop: read the PLC, make it match the MES's wish, publish what changed.
    /// Errors from the bus are returned (the caller reconnects); PLC trouble is reported as OFFLINE.
    pub fn tick(&mut self, bus: &mut impl Bus, machine: &mut impl Machine) -> io::Result<()> {
        let reading = match machine.read() {
            Ok(reading) => reading,
            Err(e) => {
                if self.last_state.as_ref().map(|s| s.0.as_str()) != Some("OFFLINE") {
                    warning!("PLC unreachable: {e}");
                }
                return self.publish_state(bus, "OFFLINE", None);
            }
        };
        match self.reconcile(bus, machine, reading) {
            Ok(()) => Ok(()),
            Err(Failure::Bus(e)) => Err(e), // let the caller reconnect
            Err(Failure::Plc(e)) => {
                warning!("Could not drive the PLC: {e}"); // try again next tick
                Ok(())
            }
        }
    }

    fn reconcile(
        &mut self,
        bus: &mut impl Bus,
        machine: &mut impl Machine,
        mut reading: Reading,
    ) -> Result<(), Failure> {
        let saved_job = self.saved.as_ref().map(|p| p.job_id.clone());
        match self.wanted.clone() {
            None => {
                // No job wanted. A finished or abandoned job is cleared off the machine, but only once
                // we are sure the MES really has none (see NO_JOB_GRACE).
                let sure = self.heard_from_mes || self.connected_at.elapsed() >= NO_JOB_GRACE;
                if sure && reading.state != State::Idle && saved_job.is_some() {
                    machine.reset().map_err(Failure::Plc)?;
                    info!("Machine reset: the MES has no job for this cell");
                    reading = machine.read().map_err(Failure::Plc)?;
                }
                self.publish_state(bus, state_name(reading.state), None)
                    .map_err(Failure::Bus)?;
            }
            Some(job) => {
                let ours = saved_job.as_deref() == Some(job.job_id.as_str());
                let complete = ours && self.saved.as_ref().is_some_and(|p| p.status == "COMPLETE");
                let running_here =
                    ours && matches!(reading.state, State::Running | State::Complete);
                if !complete && !running_here {
                    // A new job, or ours but the PLC lost it (restarted): start it.
                    machine.start(job.target).map_err(Failure::Plc)?;
                    info!(
                        "Started job {} on the machine: {} good parts",
                        job.job_id, job.target
                    );
                    self.save(Progress {
                        job_id: job.job_id.clone(),
                        order: job.order.clone(),
                        operation_number: job.operation_number,
                        status: "RUNNING".into(),
                        good: 0,
                        scrap: 0,
                        run_seconds: 0,
                    });
                    reading = machine.read().map_err(Failure::Plc)?;
                }
                self.report(bus, &job, reading).map_err(Failure::Bus)?;
            }
        }
        Ok(())
    }

    /// Publish the cell state and the job's progress for the wanted job.
    fn report(&mut self, bus: &mut impl Bus, job: &Job, reading: Reading) -> io::Result<()> {
        let saved = self
            .saved
            .clone()
            .expect("a wanted job is always saved before it is reported");
        if saved.status == "COMPLETE" {
            // Finished earlier (maybe before a restart): the saved totals are the answer, whatever
            // the PLC shows now.
            self.publish_state(bus, state_name(reading.state), None)?;
            if self.last_progress.as_ref() != Some(&saved) {
                info!(
                    "Job {} complete: {} good, {} scrap (reported again)",
                    saved.job_id, saved.good, saved.scrap
                );
                self.publish_progress(bus, &saved)?;
            }
            return Ok(());
        }
        let job_ref = Some((job.order.as_str(), job.operation_number));
        self.publish_state(
            bus,
            state_name(reading.state),
            job_ref.filter(|_| reading.state == State::Running),
        )?;
        let complete = reading.state == State::Complete;
        let progress = Progress {
            status: if complete { "COMPLETE" } else { "RUNNING" }.into(),
            good: reading.good,
            scrap: reading.scrap,
            run_seconds: reading.run_seconds,
            ..saved
        };
        if complete {
            info!(
                "Job {} complete: {} good, {} scrap",
                progress.job_id, progress.good, progress.scrap
            );
            self.save(progress.clone()); // remember first: a restart must not make the parts again
            self.publish_progress(bus, &progress)?;
        } else if self.last_progress.as_ref() != Some(&progress)
            && self
                .last_progress_at
                .is_none_or(|at| at.elapsed() >= self.config.progress_interval)
        {
            self.publish_progress(bus, &progress)?;
        }
        Ok(())
    }
}

/// Run the agent until `stop()` is true: connect, subscribe, then tick every `poll` interval;
/// reconnect with back-off (1, 2, 4 ... 30 s) whenever the broker connection fails.
pub fn run(
    mut agent: Agent,
    options: mqtt::Options,
    machine: &mut impl Machine,
    poll: Duration,
    stop: &dyn Fn() -> bool,
) {
    let mut options = options;
    options.will = Some(agent.last_will());
    let mut backoff = Duration::from_secs(1);
    while !stop() {
        let mut client = match mqtt::Client::connect(&options) {
            Ok(client) => client,
            Err(e) => {
                warning!(
                    "Broker {}:{} unreachable ({e}); retrying in {}s",
                    options.host,
                    options.port,
                    backoff.as_secs()
                );
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        info!(
            "Connected to broker {}:{} as {}",
            options.host, options.port, options.client_id
        );
        agent.connected();
        let result = (|| -> io::Result<()> {
            client.subscribe(&agent.subscription())?;
            while !stop() {
                if let Some(message) = client.poll(poll)? {
                    agent.on_message(&message);
                }
                agent.tick(&mut client, machine)?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                let _ = client.disconnect(); // stopping: a clean goodbye, so the will is not published
                info!("Stopped");
            }
            Err(e) => warning!("Broker connection lost: {e}; reconnecting"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modbus::Registers;
    use crate::plc::Plc;

    /// Records what the agent publishes.
    #[derive(Default)]
    struct Recorder {
        published: Vec<(String, String)>,
    }

    impl Bus for Recorder {
        fn publish(&mut self, topic: &str, payload: &[u8], retain: bool) -> io::Result<()> {
            assert!(retain, "the agent only publishes retained state");
            self.published.push((
                topic.to_string(),
                String::from_utf8(payload.to_vec()).unwrap(),
            ));
            Ok(())
        }
    }

    impl Recorder {
        fn take(&mut self) -> Vec<(String, String)> {
            std::mem::take(&mut self.published)
        }
    }

    /// The simulated PLC in memory, driven through its registers like the real one; can be unplugged.
    struct Local {
        plc: Plc,
        starts: u32,
        unplugged: bool,
    }

    impl Machine for Local {
        fn read(&mut self) -> io::Result<Reading> {
            if self.unplugged {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    "unplugged",
                ));
            }
            Ok(
                Reading::from_registers(&self.plc.read(plc::STATE, plc::READING_LENGTH).unwrap())
                    .unwrap(),
            )
        }
        fn start(&mut self, target: u32) -> io::Result<()> {
            self.starts += 1;
            let [high, low] = plc::split(target);
            assert!(self.plc.write(plc::COMMAND, &[plc::START, high, low]));
            Ok(())
        }
        fn reset(&mut self) -> io::Result<()> {
            assert!(self.plc.write(plc::COMMAND, &[plc::RESET]));
            Ok(())
        }
    }

    const CELL: &str = "1000/1000/assy/l1/press3";

    fn job_message(job_id: &str, quantity: &str) -> mqtt::Message {
        let payload = format!(
            r#"{{"schema":"job/v1","event_id":null,"timestamp":"t","source":"mes-intelligence","data":{{"job_id":"{job_id}","order":"MO00000001","operation_number":10,"quantity":"{quantity}"}}}}"#
        );
        mqtt::Message {
            topic: format!("{CELL}/schedule/current-job"),
            payload: payload.into_bytes(),
            retain: true,
        }
    }

    fn cleared() -> mqtt::Message {
        mqtt::Message {
            topic: format!("{CELL}/schedule/current-job"),
            payload: vec![],
            retain: true,
        }
    }

    fn agent(dir: &std::path::Path) -> Agent {
        Agent::new(Config {
            cell: CELL.into(),
            state_file: dir.join("edge-state.txt"),
            progress_interval: Duration::ZERO,
        })
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mes-edge-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn data_of(published: &[(String, String)], topic_end: &str) -> Vec<crate::json::Value> {
        published
            .iter()
            .filter(|(topic, _)| topic.ends_with(topic_end))
            .map(|(_, payload)| {
                crate::json::parse(payload)
                    .unwrap()
                    .get("data")
                    .unwrap()
                    .clone()
            })
            .collect()
    }

    fn text(value: &crate::json::Value, key: &str) -> String {
        value
            .get(key)
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_json())
            })
            .unwrap_or_default()
    }

    #[test]
    fn runs_a_job_from_dispatch_to_complete() {
        let dir = temp_dir("run");
        let mut agent = agent(&dir);
        let (mut bus, mut machine) = (
            Recorder::default(),
            Local {
                plc: Plc::new(378_000, 0),
                starts: 0,
                unplugged: false,
            },
        );

        agent.tick(&mut bus, &mut machine).unwrap(); // nothing wanted yet: the idle cell is announced
        let states = data_of(&bus.take(), "/state/status");
        assert_eq!(text(&states[0], "state"), "IDLE");

        agent.on_message(&job_message("mo00000001-0010", "100.000"));
        agent.tick(&mut bus, &mut machine).unwrap();
        assert_eq!(machine.starts, 1);
        let published = bus.take();
        assert_eq!(
            text(&data_of(&published, "/state/status")[0], "state"),
            "RUNNING"
        );
        assert_eq!(
            text(&data_of(&published, "/state/status")[0], "order"),
            "MO00000001"
        );
        assert_eq!(
            text(&data_of(&published, "/state/job")[0], "status"),
            "RUNNING"
        );

        machine.plc.advance(378_000 * 50);
        agent.tick(&mut bus, &mut machine).unwrap();
        let progress = data_of(&bus.take(), "/state/job");
        assert_eq!(
            (
                text(&progress[0], "good_quantity"),
                text(&progress[0], "run_hours")
            ),
            ("50".into(), "5.250".into())
        );

        // The same job message again (QoS 1 redelivery, or the retained copy after a reconnect): no new start.
        agent.on_message(&job_message("mo00000001-0010", "100.000"));
        agent.tick(&mut bus, &mut machine).unwrap();
        assert_eq!(machine.starts, 1);
        assert!(
            bus.take().is_empty(),
            "nothing changed, so nothing is published"
        );

        machine.plc.advance(378_000 * 60);
        agent.tick(&mut bus, &mut machine).unwrap();
        let published = bus.take();
        let progress = data_of(&published, "/state/job");
        assert_eq!(text(&progress[0], "status"), "COMPLETE");
        assert_eq!(
            (
                text(&progress[0], "good_quantity"),
                text(&progress[0], "run_hours")
            ),
            ("100".into(), "10.500".into())
        );
        assert_eq!(
            text(&data_of(&published, "/state/status")[0], "state"),
            "IDLE"
        );
        agent.tick(&mut bus, &mut machine).unwrap();
        assert!(bus.take().is_empty(), "COMPLETE is published once");

        agent.on_message(&cleared()); // the MES took the result and cleared the job
        agent.tick(&mut bus, &mut machine).unwrap();
        assert_eq!(machine.plc.state(), State::Idle);
        assert!(bus.take().is_empty(), "the cell was already reported idle");
    }

    #[test]
    fn a_restart_never_makes_the_parts_twice() {
        let dir = temp_dir("restart");
        let mut machine = Local {
            plc: Plc::new(1_000, 0),
            starts: 0,
            unplugged: false,
        };
        let mut bus = Recorder::default();

        // First life: start the job, get half way.
        let mut first = agent(&dir);
        first.on_message(&job_message("mo00000001-0010", "10"));
        first.tick(&mut bus, &mut machine).unwrap();
        machine.plc.advance(5_000);
        drop(first);

        // Second life, mid-job: the retained job arrives again; the PLC is still on it, so carry on.
        let mut second = agent(&dir);
        second.on_message(&job_message("mo00000001-0010", "10"));
        second.tick(&mut bus, &mut machine).unwrap();
        assert_eq!(machine.starts, 1, "resumed, not restarted");
        machine.plc.advance(60_000);
        second.tick(&mut bus, &mut machine).unwrap();
        drop(second);
        assert_eq!(
            text(&data_of(&bus.take(), "/state/job").pop().unwrap(), "status"),
            "COMPLETE"
        );

        // Third life after completion, with the PLC power-cycled (counters gone): report the saved totals.
        machine.plc = Plc::new(1_000, 0);
        let mut third = agent(&dir);
        third.on_message(&job_message("mo00000001-0010", "10"));
        third.tick(&mut bus, &mut machine).unwrap();
        assert_eq!(machine.starts, 1, "a finished job is never started again");
        let again = data_of(&bus.take(), "/state/job");
        assert_eq!(
            (text(&again[0], "status"), text(&again[0], "good_quantity")),
            ("COMPLETE".into(), "10".into())
        );

        // A new job is started as usual.
        third.on_message(&job_message("mo00000001-0020", "3"));
        third.tick(&mut bus, &mut machine).unwrap();
        assert_eq!(machine.starts, 2);
    }

    #[test]
    fn plc_lost_mid_job_is_restarted_and_offline_is_reported() {
        let dir = temp_dir("offline");
        let mut agent = agent(&dir);
        let mut machine = Local {
            plc: Plc::new(1_000, 0),
            starts: 0,
            unplugged: false,
        };
        let mut bus = Recorder::default();
        agent.on_message(&job_message("mo00000001-0010", "5"));
        agent.tick(&mut bus, &mut machine).unwrap();
        bus.take();

        machine.unplugged = true;
        agent.tick(&mut bus, &mut machine).unwrap();
        agent.tick(&mut bus, &mut machine).unwrap();
        let states = data_of(&bus.take(), "/state/status");
        assert_eq!(states.len(), 1, "OFFLINE is published once");
        assert_eq!(text(&states[0], "state"), "OFFLINE");

        // It comes back after a power cycle, idle: the job is started again.
        machine.plc = Plc::new(1_000, 0);
        machine.unplugged = false;
        agent.tick(&mut bus, &mut machine).unwrap();
        assert_eq!(machine.starts, 2);
        assert_eq!(
            text(&data_of(&bus.take(), "/state/status")[0], "state"),
            "RUNNING"
        );
    }

    #[test]
    fn ignores_other_topics_and_bad_messages() {
        let dir = temp_dir("bad");
        let mut agent = agent(&dir);
        agent.on_message(&mqtt::Message {
            topic: "x/y".into(),
            payload: b"{}".to_vec(),
            retain: false,
        });
        agent.on_message(&mqtt::Message {
            topic: format!("{CELL}/schedule/current-job"),
            payload: b"{\"schema\":\"job/v1\"".to_vec(),
            retain: true,
        });
        assert!(agent.wanted.is_none());
        let will = agent.last_will();
        assert_eq!(will.topic, format!("{CELL}/state/status"));
        assert!(String::from_utf8(will.payload)
            .unwrap()
            .contains("\"OFFLINE\""));
        assert_eq!(agent.config.source(), "mes-edge/press3");
    }
}
