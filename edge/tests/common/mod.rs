//! A tiny MQTT 3.1.1 broker for tests, so the agent's end-to-end test runs without Mosquitto.
//!
//! It does just enough: CONNECT with a last will, SUBSCRIBE with `+` and `#` wildcards, retained
//! messages (delivered on subscribe, cleared by an empty payload), PUBLISH at QoS 0 and 1, PINGREQ
//! and DISCONNECT. Messages are delivered to subscribers at QoS 0. The real broker is still tested:
//! CI runs the same scenario against Mosquitto (`MQTT_URL`).

#![allow(dead_code)] // each test file uses a different part of this module

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use mes_edge::mqtt::{self, kind};

struct Session {
    id: usize,
    writer: TcpStream,
    filters: Vec<String>,
}

#[derive(Default)]
struct State {
    retained: BTreeMap<String, Vec<u8>>,
    sessions: Vec<Session>,
    next_id: usize,
}

/// A running test broker on a free local port.
pub struct TestBroker {
    /// `mqtt://127.0.0.1:<port>`
    pub url: String,
    state: Arc<Mutex<State>>,
}

/// MQTT topic matching: `+` is one level, `#` is the rest.
pub fn topic_matches(filter: &str, topic: &str) -> bool {
    let mut topic_levels = topic.split('/');
    for level in filter.split('/') {
        match (level, topic_levels.next()) {
            ("#", _) => return true,
            ("+", Some(_)) => {}
            (expected, Some(actual)) if expected == actual => {}
            _ => return false,
        }
    }
    topic_levels.next().is_none()
}

fn read_str(body: &[u8], at: &mut usize) -> String {
    let length = usize::from(u16::from_be_bytes([body[*at], body[*at + 1]]));
    let text = String::from_utf8(body[*at + 2..*at + 2 + length].to_vec()).unwrap();
    *at += 2 + length;
    text
}

fn read_bytes(body: &[u8], at: &mut usize) -> Vec<u8> {
    let length = usize::from(u16::from_be_bytes([body[*at], body[*at + 1]]));
    let bytes = body[*at + 2..*at + 2 + length].to_vec();
    *at += 2 + length;
    bytes
}

impl TestBroker {
    /// Start listening; every connection gets its own thread.
    pub fn start() -> TestBroker {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("mqtt://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&shared);
                std::thread::spawn(move || connection(stream, state));
            }
        });
        TestBroker { url, state }
    }

    /// The retained payload of a topic, if any.
    pub fn retained(&self, topic: &str) -> Option<Vec<u8>> {
        self.state.lock().unwrap().retained.get(topic).cloned()
    }
}

/// Store a retained message (or clear it) and deliver to every matching subscriber.
fn route(state: &Arc<Mutex<State>>, topic: &str, payload: &[u8], retain: bool) {
    let mut state = state.lock().unwrap();
    if retain {
        if payload.is_empty() {
            state.retained.remove(topic);
        } else {
            state.retained.insert(topic.to_string(), payload.to_vec());
        }
    }
    let packet = mqtt::publish_packet(topic, payload, 0, false, 0);
    for session in &mut state.sessions {
        if session.filters.iter().any(|f| topic_matches(f, topic)) {
            let _ = session.writer.write_all(&packet);
        }
    }
}

fn connection(mut stream: TcpStream, state: Arc<Mutex<State>>) {
    let id = {
        let mut state = state.lock().unwrap();
        state.next_id += 1;
        let id = state.next_id;
        let writer = stream.try_clone().unwrap();
        state.sessions.push(Session {
            id,
            writer,
            filters: Vec::new(),
        });
        id
    };
    let mut will: Option<(String, Vec<u8>, bool)> = None;
    let mut clean_goodbye = false;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    'connection: loop {
        let frame = match mqtt::take_frame(&mut buffer) {
            Ok(Some(frame)) => frame,
            Ok(None) => match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);
                    continue;
                }
            },
            Err(_) => break,
        };
        let (first, body) = frame;
        let reply: Vec<u8> = match first >> 4 {
            kind::CONNECT => {
                let mut at = 0;
                read_str(&body, &mut at); // "MQTT"
                at += 1; // level
                let flags = body[at];
                at += 3; // flags and keep-alive
                read_str(&body, &mut at); // client id
                if flags & 0x04 != 0 {
                    let topic = read_str(&body, &mut at);
                    let payload = read_bytes(&body, &mut at);
                    will = Some((topic, payload, flags & 0x20 != 0));
                }
                vec![0x20, 2, 0, 0]
            }
            kind::SUBSCRIBE => {
                let packet_id = [body[0], body[1]];
                let mut at = 2;
                let mut filters = Vec::new();
                while at < body.len() {
                    filters.push(read_str(&body, &mut at));
                    at += 1; // requested QoS
                }
                let mut reply = vec![0x90, 2 + filters.len() as u8, packet_id[0], packet_id[1]];
                reply.extend(std::iter::repeat_n(1u8, filters.len()));
                let mut state = state.lock().unwrap();
                // Acknowledge first, then send the retained messages (as real brokers do).
                let _ = stream.write_all(&reply);
                let retained: Vec<Vec<u8>> = state
                    .retained
                    .iter()
                    .filter(|(topic, _)| filters.iter().any(|f| topic_matches(f, topic)))
                    .map(|(topic, payload)| mqtt::publish_packet(topic, payload, 0, true, 0))
                    .collect();
                for packet in retained {
                    let _ = stream.write_all(&packet);
                }
                if let Some(session) = state.sessions.iter_mut().find(|s| s.id == id) {
                    session.filters.extend(filters);
                }
                continue;
            }
            kind::PUBLISH => {
                let qos = (first >> 1) & 3;
                let mut at = 0;
                let topic = read_str(&body, &mut at);
                let mut reply = Vec::new();
                if qos > 0 {
                    reply = mqtt::puback_packet(u16::from_be_bytes([body[at], body[at + 1]]));
                    at += 2;
                }
                route(&state, &topic, &body[at..], first & 1 == 1);
                reply
            }
            kind::PINGREQ => vec![kind::PINGRESP << 4, 0],
            kind::DISCONNECT => {
                clean_goodbye = true;
                break 'connection;
            }
            _ => Vec::new(),
        };
        if !reply.is_empty() && stream.write_all(&reply).is_err() {
            break;
        }
    }
    state.lock().unwrap().sessions.retain(|s| s.id != id);
    if !clean_goodbye {
        if let Some((topic, payload, retain)) = will {
            route(&state, &topic, &payload, retain);
        }
    }
}

#[test]
fn wildcards() {
    assert!(topic_matches("a/+/c", "a/b/c"));
    assert!(topic_matches("a/#", "a/b/c"));
    assert!(!topic_matches("a/+", "a/b/c"));
    assert!(!topic_matches("a/b/c", "a/b"));
}
