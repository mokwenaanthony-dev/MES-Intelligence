//! # mes-edge: the shop floor's link to the Unified Namespace
//!
//! ISA-95 places the machine's controller (the PLC) at Level 1-2, the MES at Level 3 and the ERP at
//! Level 4. An **edge agent** runs next to each machine: it reads the PLC over the machine's own
//! protocol (here Modbus TCP) and publishes what the machine is doing to the Unified Namespace (here
//! MQTT), and it passes the MES's instructions (the job to run) down to the PLC.
//!
//! The crate uses only Rust's standard library (ADR 0005), so every protocol is implemented here,
//! small and readable:
//!
//! | Module | What it does |
//! |---|---|
//! | [`log`] | log lines with time, level, thread, module and `file:line`, like the Python MES |
//! | [`time`] | UTC timestamps in ISO 8601 without a date library (days-to-civil-date algorithm) |
//! | [`json`] | a JSON value, a recursive-descent parser and a writer; numbers kept as exact text |
//! | [`mqtt`] | an MQTT 3.1.1 client: QoS 1, retained messages, keep-alive, last will |
//! | [`modbus`] | Modbus TCP: a client for the agent and the request handling of the simulated PLC |
//! | [`plc`] | the simulated PLC: registers, commands and a part counter |
//! | [`messages`] | the Level 2/3 messages exchanged with the MES (`docs/EDGE_CONTRACT.md`) |
//! | [`agent`] | the edge agent: job in, PLC driven, state and progress out; idempotent and restartable |
//!
//! Every network-facing part is written so that repeating a message has the same effect as
//! receiving it once (the project's idempotency rule, ERP ADR 0010).

// The project's documentation rule (principle 16), enforced by the compiler: every public item has a
// doc comment, and CI runs clippy with warnings as errors.
#![warn(missing_docs)]

pub mod agent;
pub mod json;
pub mod log;
pub mod messages;
pub mod modbus;
pub mod mqtt;
pub mod plc;
pub mod time;
