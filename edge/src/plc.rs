//! The simulated PLC: a machine that makes parts, behind Modbus registers.
//!
//! A real PLC runs the machine and exposes a few registers to the outside. This one has the same
//! shape, so the edge agent cannot tell the difference:
//!
//! | Register | Name | Access | Meaning |
//! |---|---|---|---|
//! | 0 | command | write | 1 = start a job (resets the counters), 2 = reset to idle |
//! | 1-2 | target | write | good parts to make (32 bits, high word first) |
//! | 10 | state | read | 0 idle, 1 running, 2 complete, 3 down |
//! | 11-12 | good | read | good parts made in this job |
//! | 13-14 | scrap | read | scrapped parts in this job |
//! | 15-16 | run seconds | read | machine time spent on this job |
//!
//! Larger numbers span two 16-bit registers, high word first (the common "big-endian word order").
//! The agent writes the target and the command in **one** Write Multiple Registers request, so the
//! PLC never starts with half a target.
//!
//! The machine makes one part per `cycle_ms` of machine time; with `scrap_every = n`, every n-th
//! part is scrap. It stops by itself once the target of good parts is reached (state complete).
//! Time is passed in by the caller ([`Plc::advance`]), so tests can run a shift in microseconds and
//! the `plc-sim` program can speed it up for demos.

use crate::modbus::Registers;

/// Register: command (write).
pub const COMMAND: u16 = 0;
/// Registers 1-2: target quantity (write).
pub const TARGET: u16 = 1;
/// Register: machine state (read).
pub const STATE: u16 = 10;
/// Registers 11-12: good count.
pub const GOOD: u16 = 11;
/// Registers 13-14: scrap count.
pub const SCRAP: u16 = 13;
/// Registers 15-16: run seconds.
pub const RUN_SECONDS: u16 = 15;
/// How many registers the PLC has.
pub const REGISTER_COUNT: u16 = 20;
/// How many registers from [`STATE`] make up a [`Reading`].
pub const READING_LENGTH: u16 = 7;

/// Command value: start a job.
pub const START: u16 = 1;
/// Command value: reset to idle and clear the counters.
pub const RESET: u16 = 2;

/// What the machine is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Waiting for a job.
    Idle = 0,
    /// Making parts.
    Running = 1,
    /// Target reached; counters hold the job's totals until the next start or reset.
    Complete = 2,
    /// Stopped by a fault.
    Down = 3,
}

impl State {
    /// The state from its register value.
    pub fn from_register(value: u16) -> State {
        match value {
            1 => State::Running,
            2 => State::Complete,
            3 => State::Down,
            _ => State::Idle,
        }
    }
}

/// One snapshot of the machine, as read from registers 10-16.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// What the machine is doing.
    pub state: State,
    /// Good parts in this job.
    pub good: u32,
    /// Scrapped parts in this job.
    pub scrap: u32,
    /// Machine seconds spent on this job.
    pub run_seconds: u32,
}

/// Two registers as one 32-bit number, high word first.
pub fn join(high: u16, low: u16) -> u32 {
    (u32::from(high) << 16) | u32::from(low)
}

/// A 32-bit number as two registers, high word first.
pub fn split(value: u32) -> [u16; 2] {
    [(value >> 16) as u16, value as u16]
}

impl Reading {
    /// Decode registers 10-16 (exactly [`READING_LENGTH`] values).
    pub fn from_registers(registers: &[u16]) -> Option<Reading> {
        if registers.len() != usize::from(READING_LENGTH) {
            return None;
        }
        Some(Reading {
            state: State::from_register(registers[0]),
            good: join(registers[1], registers[2]),
            scrap: join(registers[3], registers[4]),
            run_seconds: join(registers[5], registers[6]),
        })
    }
}

/// The simulated machine.
pub struct Plc {
    registers: [u16; REGISTER_COUNT as usize],
    cycle_ms: u64,
    scrap_every: u32,
    run_ms: u64,
    part_ms: u64,
    made: u32,
}

impl Plc {
    /// A machine that needs `cycle_ms` of machine time per part and scraps every `scrap_every`-th
    /// part (0 = never).
    pub fn new(cycle_ms: u64, scrap_every: u32) -> Plc {
        Plc {
            registers: [0; REGISTER_COUNT as usize],
            cycle_ms: cycle_ms.max(1),
            scrap_every,
            run_ms: 0,
            part_ms: 0,
            made: 0,
        }
    }

    fn get32(&self, at: u16) -> u32 {
        join(
            self.registers[usize::from(at)],
            self.registers[usize::from(at) + 1],
        )
    }

    fn set32(&mut self, at: u16, value: u32) {
        let [high, low] = split(value);
        self.registers[usize::from(at)] = high;
        self.registers[usize::from(at) + 1] = low;
    }

    /// The current state.
    pub fn state(&self) -> State {
        State::from_register(self.registers[usize::from(STATE)])
    }

    fn set_state(&mut self, state: State) {
        self.registers[usize::from(STATE)] = state as u16;
    }

    fn clear_counters(&mut self) {
        for at in [GOOD, SCRAP, RUN_SECONDS] {
            self.set32(at, 0);
        }
        self.run_ms = 0;
        self.part_ms = 0;
        self.made = 0;
    }

    /// Act on the command register, then clear it (a command is a one-shot, like a push button).
    fn execute_command(&mut self) {
        let command = self.registers[usize::from(COMMAND)];
        self.registers[usize::from(COMMAND)] = 0;
        match command {
            START => {
                self.clear_counters();
                // A target of 0 is already reached.
                let state = if self.get32(TARGET) == 0 {
                    State::Complete
                } else {
                    State::Running
                };
                self.set_state(state);
                crate::info!("Job started: target {} good parts", self.get32(TARGET));
            }
            RESET => {
                self.clear_counters();
                self.set_state(State::Idle);
                crate::info!("Reset to idle");
            }
            _ => {}
        }
    }

    /// Let `machine_ms` of machine time pass: make parts while running, stop at the target.
    pub fn advance(&mut self, machine_ms: u64) {
        if self.state() != State::Running {
            return;
        }
        let target = self.get32(TARGET);
        let mut left = machine_ms;
        while left > 0 {
            let step = left.min(self.cycle_ms - self.part_ms);
            self.part_ms += step;
            self.run_ms += step;
            left -= step;
            if self.part_ms == self.cycle_ms {
                self.part_ms = 0;
                self.made += 1;
                if self.scrap_every > 0 && self.made % self.scrap_every == 0 {
                    self.set32(SCRAP, self.get32(SCRAP) + 1);
                } else {
                    self.set32(GOOD, self.get32(GOOD) + 1);
                }
                if self.get32(GOOD) >= target {
                    self.set_state(State::Complete);
                    crate::info!(
                        "Job complete: {} good, {} scrap",
                        self.get32(GOOD),
                        self.get32(SCRAP)
                    );
                    break; // the machine stops; remaining time is idle time, not run time
                }
            }
        }
        self.set32(RUN_SECONDS, (self.run_ms / 1_000) as u32);
    }
}

impl Registers for Plc {
    fn read(&self, start: u16, count: u16) -> Option<Vec<u16>> {
        let end = usize::from(start) + usize::from(count);
        self.registers
            .get(usize::from(start)..end)
            .map(<[u16]>::to_vec)
    }

    /// Only the command and target registers (0-2) are writable, as on a real machine where the
    /// counters belong to the PLC.
    fn write(&mut self, start: u16, values: &[u16]) -> bool {
        let end = usize::from(start) + values.len();
        if end > usize::from(TARGET) + 2 {
            return false;
        }
        self.registers[usize::from(start)..end].copy_from_slice(values);
        if start == COMMAND {
            self.execute_command(); // the write included the command register
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(plc: &mut Plc, target: u32) {
        let [high, low] = split(target);
        assert!(plc.write(COMMAND, &[START, high, low]));
    }

    fn reading(plc: &Plc) -> Reading {
        Reading::from_registers(&plc.read(STATE, READING_LENGTH).unwrap()).unwrap()
    }

    #[test]
    fn makes_parts_until_the_target() {
        let mut plc = Plc::new(378_000, 0); // 378 s per part: 100 parts = 10.5 machine hours
        start(&mut plc, 100);
        assert_eq!(
            reading(&plc),
            Reading {
                state: State::Running,
                good: 0,
                scrap: 0,
                run_seconds: 0
            }
        );
        plc.advance(378_000 * 40 + 1_000);
        assert_eq!(
            reading(&plc),
            Reading {
                state: State::Running,
                good: 40,
                scrap: 0,
                run_seconds: 15_121
            }
        );
        plc.advance(u64::MAX / 4); // far more time than needed: it stops at the target
        assert_eq!(
            reading(&plc),
            Reading {
                state: State::Complete,
                good: 100,
                scrap: 0,
                run_seconds: 37_800
            }
        );
        assert_eq!(plc.read(COMMAND, 1), Some(vec![0])); // the command was consumed
    }

    #[test]
    fn scrap_is_made_on_top_of_the_target() {
        let mut plc = Plc::new(1_000, 10);
        start(&mut plc, 18);
        plc.advance(1_000_000);
        let done = reading(&plc);
        assert_eq!(
            (done.state, done.good, done.scrap, done.run_seconds),
            (State::Complete, 18, 1, 19)
        );
    }

    #[test]
    fn large_counts_span_two_registers() {
        let mut plc = Plc::new(1, 0);
        start(&mut plc, 70_000);
        plc.advance(100_000);
        let registers = plc.read(GOOD, 2).unwrap();
        assert_eq!(registers, vec![1, (70_000 - 65_536) as u16]);
        assert_eq!(join(registers[0], registers[1]), 70_000);
    }

    #[test]
    fn reset_start_again_and_read_only_counters() {
        let mut plc = Plc::new(1_000, 0);
        start(&mut plc, 5);
        plc.advance(3_000);
        assert!(plc.write(COMMAND, &[RESET]));
        assert_eq!(
            reading(&plc),
            Reading {
                state: State::Idle,
                good: 0,
                scrap: 0,
                run_seconds: 0
            }
        );
        plc.advance(10_000); // idle: nothing happens
        assert_eq!(reading(&plc).good, 0);
        assert!(!plc.write(GOOD, &[9])); // counters are not writable
        assert!(!plc.write(TARGET, &[0, 1, 2]));
        start(&mut plc, 0);
        assert_eq!(plc.state(), State::Complete);
        assert_eq!(plc.read(REGISTER_COUNT, 1), None);
    }
}
