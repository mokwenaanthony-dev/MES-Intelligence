//! An MQTT 3.1.1 client with the standard library only.
//!
//! MQTT is a small binary protocol over TCP. Every packet is a **fixed header** (one byte: packet
//! type and flags, then the *remaining length* as a variable-length integer), a **variable header**
//! and a **payload**. This module implements what the edge agent needs, following the OASIS
//! MQTT 3.1.1 specification section by section:
//!
//! | Packet | Section | Used for |
//! |---|---|---|
//! | CONNECT / CONNACK | 3.1, 3.2 | opening a session, with keep-alive and a **last will** |
//! | PUBLISH / PUBACK | 3.3, 3.4 | sending and receiving messages at **QoS 1** (at least once), retained or not |
//! | SUBSCRIBE / SUBACK | 3.8, 3.9 | asking the broker for a topic |
//! | PINGREQ / PINGRESP | 3.12, 3.13 | proving the connection is alive |
//! | DISCONNECT | 3.14 | a clean goodbye (the broker then drops the last will) |
//!
//! **Framing.** TCP delivers a stream of bytes, not packets: one read can return half a packet or
//! three. The client keeps a buffer and cuts complete packets off its front ([`take_frame`]), so a
//! read that times out halfway never loses or corrupts data.
//!
//! **QoS 1** means the broker answers each PUBLISH with a PUBACK carrying the same packet ID, and the
//! sender repeats until it gets one. Messages can therefore arrive twice; the agent is built so that
//! does not matter (every message it sends is state, published retained).
//!
//! TLS is not in the standard library, so this client speaks plain TCP: run it next to the broker
//! (same host or network), or behind a local TLS tunnel. `mqtts://` URLs are refused, not
//! silently downgraded.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// Largest packet the client accepts, so a broken or hostile peer cannot make it allocate gigabytes.
pub const MAX_PACKET: usize = 1024 * 1024;

/// How long to wait for the broker's acknowledgement of CONNECT, PUBLISH or SUBSCRIBE.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(10);

/// Packet types (the high four bits of the first byte).
pub mod kind {
    /// Client request to connect.
    pub const CONNECT: u8 = 1;
    /// Connect acknowledgement.
    pub const CONNACK: u8 = 2;
    /// A message.
    pub const PUBLISH: u8 = 3;
    /// QoS 1 acknowledgement.
    pub const PUBACK: u8 = 4;
    /// Subscribe request.
    pub const SUBSCRIBE: u8 = 8;
    /// Subscribe acknowledgement.
    pub const SUBACK: u8 = 9;
    /// Ping request.
    pub const PINGREQ: u8 = 12;
    /// Ping response.
    pub const PINGRESP: u8 = 13;
    /// Clean disconnect.
    pub const DISCONNECT: u8 = 14;
}

/// A message received from the broker.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// The topic it was published on.
    pub topic: String,
    /// The payload (empty for a cleared retained topic).
    pub payload: Vec<u8>,
    /// True when the broker sent it from its retained store (sent on subscribe).
    pub retain: bool,
}

/// The message the broker publishes for the client if the connection dies without DISCONNECT.
#[derive(Clone, Debug)]
pub struct Will {
    /// Where to publish it.
    pub topic: String,
    /// What to publish.
    pub payload: Vec<u8>,
    /// Whether the broker keeps it as the topic's retained state.
    pub retain: bool,
}

/// Everything needed to open a session.
#[derive(Clone, Debug)]
pub struct Options {
    /// Broker host name or address.
    pub host: String,
    /// Broker port (1883 for plain MQTT).
    pub port: u16,
    /// Client identifier; with `clean_session = false` the broker keeps the session under this name.
    pub client_id: String,
    /// Longest silence before the broker may drop the client; the client pings at half of it.
    pub keep_alive: Duration,
    /// Start a fresh session (true) or resume the stored one (false).
    pub clean_session: bool,
    /// Optional user name.
    pub username: Option<String>,
    /// Optional password (never logged).
    pub password: Option<String>,
    /// Optional last will.
    pub will: Option<Will>,
}

impl Options {
    /// Options from a URL such as `mqtt://localhost:1883` or `mqtt://user:secret@broker:1883`.
    pub fn from_url(url: &str, client_id: &str) -> Result<Options, String> {
        let rest = if let Some(rest) = url.strip_prefix("mqtt://") {
            rest
        } else if url.starts_with("mqtts://") {
            return Err("mqtts:// (TLS) is not supported by the std-only agent; use mqtt:// to a local broker".into());
        } else {
            return Err(format!("not an mqtt:// URL: {}", redact(url)));
        };
        let rest = rest.trim_end_matches('/');
        let (credentials, address) = match rest.rsplit_once('@') {
            Some((credentials, address)) => (Some(credentials), address),
            None => (None, rest),
        };
        let (username, password) = match credentials {
            Some(text) => match text.split_once(':') {
                Some((user, password)) => (Some(user.to_string()), Some(password.to_string())),
                None => (Some(text.to_string()), None),
            },
            None => (None, None),
        };
        let (host, port) = match address.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse()
                    .map_err(|_| format!("bad port in {}", redact(url)))?,
            ),
            None => (address, 1883),
        };
        if host.is_empty() {
            return Err(format!("no host in {}", redact(url)));
        }
        Ok(Options {
            host: host.to_string(),
            port,
            client_id: client_id.to_string(),
            keep_alive: Duration::from_secs(30),
            clean_session: true,
            username,
            password,
            will: None,
        })
    }
}

/// A URL with any user name and password removed, safe to log.
pub fn redact(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(scheme_end), Some(at)) if at > scheme_end => {
            format!("{}***@{}", &url[..scheme_end + 3], &url[at + 1..])
        }
        _ => url.to_string(),
    }
}

// --- encoding -------------------------------------------------------------------------------------

/// The remaining length as MQTT's variable-length integer: 7 bits per byte, low bits first, the
/// top bit set on every byte except the last (spec 2.2.3). At most 4 bytes (268,435,455).
pub fn encode_length(mut length: usize, out: &mut Vec<u8>) {
    loop {
        let mut byte = (length % 128) as u8;
        length /= 128;
        if length > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if length == 0 {
            break;
        }
    }
}

/// A UTF-8 string with its two-byte length in front (spec 1.5.3).
fn put_str(text: &str, out: &mut Vec<u8>) {
    put_bytes(text.as_bytes(), out);
}

fn put_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// A whole packet: first byte, remaining length, then the rest.
fn packet(first: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(first);
    encode_length(body.len(), &mut out);
    out.extend_from_slice(body);
    out
}

/// CONNECT (spec 3.1): protocol name "MQTT", level 4, flags, keep-alive, then the payload fields
/// in the fixed order client ID, will topic, will message, user name, password.
pub fn connect_packet(options: &Options) -> Vec<u8> {
    let mut flags = 0u8;
    if options.clean_session {
        flags |= 0x02;
    }
    if let Some(will) = &options.will {
        flags |= 0x04 | (1 << 3); // will flag, will QoS 1
        if will.retain {
            flags |= 0x20;
        }
    }
    if options.username.is_some() {
        flags |= 0x80;
    }
    if options.password.is_some() {
        flags |= 0x40;
    }
    let mut body = Vec::new();
    put_str("MQTT", &mut body);
    body.push(4); // protocol level 4 = MQTT 3.1.1
    body.push(flags);
    body.extend_from_slice(&(options.keep_alive.as_secs().min(65_535) as u16).to_be_bytes());
    put_str(&options.client_id, &mut body);
    if let Some(will) = &options.will {
        put_str(&will.topic, &mut body);
        put_bytes(&will.payload, &mut body);
    }
    if let Some(user) = &options.username {
        put_str(user, &mut body);
    }
    if let Some(password) = &options.password {
        put_str(password, &mut body);
    }
    packet(kind::CONNECT << 4, &body)
}

/// PUBLISH (spec 3.3). QoS 0 has no packet ID; QoS 1 carries one.
pub fn publish_packet(
    topic: &str,
    payload: &[u8],
    qos: u8,
    retain: bool,
    packet_id: u16,
) -> Vec<u8> {
    let mut body = Vec::with_capacity(topic.len() + payload.len() + 4);
    put_str(topic, &mut body);
    if qos > 0 {
        body.extend_from_slice(&packet_id.to_be_bytes());
    }
    body.extend_from_slice(payload);
    packet((kind::PUBLISH << 4) | (qos << 1) | u8::from(retain), &body)
}

/// PUBACK (spec 3.4): the packet ID of the PUBLISH being acknowledged.
pub fn puback_packet(packet_id: u16) -> Vec<u8> {
    packet(kind::PUBACK << 4, &packet_id.to_be_bytes())
}

/// SUBSCRIBE (spec 3.8). The low nibble of the first byte must be 0b0010.
pub fn subscribe_packet(packet_id: u16, filter: &str, qos: u8) -> Vec<u8> {
    let mut body = packet_id.to_be_bytes().to_vec();
    put_str(filter, &mut body);
    body.push(qos);
    packet((kind::SUBSCRIBE << 4) | 0x02, &body)
}

/// PINGREQ (spec 3.12).
pub fn pingreq_packet() -> Vec<u8> {
    vec![kind::PINGREQ << 4, 0]
}

/// DISCONNECT (spec 3.14).
pub fn disconnect_packet() -> Vec<u8> {
    vec![kind::DISCONNECT << 4, 0]
}

// --- decoding -------------------------------------------------------------------------------------

/// A packet received from the broker.
#[derive(Debug, PartialEq)]
pub enum Packet {
    /// CONNACK: whether a stored session was resumed, and the return code (0 = accepted).
    ConnAck {
        /// The broker resumed a stored session.
        session_present: bool,
        /// 0 accepted; 1-5 refused (protocol, identifier, unavailable, credentials, not authorised).
        code: u8,
    },
    /// PUBLISH: a message, with its packet ID when QoS is 1 or 2.
    Publish {
        /// The message.
        message: Message,
        /// QoS of this delivery.
        qos: u8,
        /// Packet ID to acknowledge (QoS above 0).
        packet_id: Option<u16>,
    },
    /// PUBACK for one of our QoS 1 publishes.
    PubAck(u16),
    /// SUBACK: the packet ID and the granted QoS per filter (0x80 = refused).
    SubAck(u16, Vec<u8>),
    /// PINGRESP.
    PingResp,
    /// Any other packet type, ignored by this client.
    Other(u8),
}

fn malformed(reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("malformed MQTT packet: {reason}"),
    )
}

/// Cut one complete packet off the front of `buffer`: `Ok(Some((first byte, body)))`, or `Ok(None)`
/// when more bytes are needed. Errors on a malformed length or a packet above [`MAX_PACKET`].
pub fn take_frame(buffer: &mut Vec<u8>) -> io::Result<Option<(u8, Vec<u8>)>> {
    if buffer.len() < 2 {
        return Ok(None);
    }
    // Decode the remaining length: up to four bytes after the first.
    let mut length = 0usize;
    let mut multiplier = 1usize;
    let mut index = 1;
    loop {
        let Some(&byte) = buffer.get(index) else {
            return Ok(None); // the length itself is not complete yet
        };
        length += usize::from(byte & 0x7F) * multiplier;
        index += 1;
        if byte & 0x80 == 0 {
            break;
        }
        if index > 4 {
            return Err(malformed("remaining length longer than 4 bytes"));
        }
        multiplier *= 128;
    }
    if length > MAX_PACKET {
        return Err(malformed("packet too large"));
    }
    if buffer.len() < index + length {
        return Ok(None);
    }
    let first = buffer[0];
    let body = buffer[index..index + length].to_vec();
    buffer.drain(..index + length);
    Ok(Some((first, body)))
}

fn u16_at(body: &[u8], at: usize) -> io::Result<u16> {
    match body.get(at..at + 2) {
        Some(bytes) => Ok(u16::from_be_bytes([bytes[0], bytes[1]])),
        None => Err(malformed("too short")),
    }
}

/// Turn a frame into a [`Packet`].
pub fn parse_packet(first: u8, body: &[u8]) -> io::Result<Packet> {
    Ok(match first >> 4 {
        kind::CONNACK => {
            if body.len() != 2 {
                return Err(malformed("CONNACK length"));
            }
            Packet::ConnAck {
                session_present: body[0] & 1 == 1,
                code: body[1],
            }
        }
        kind::PUBLISH => {
            let qos = (first >> 1) & 0x03;
            let topic_length = usize::from(u16_at(body, 0)?);
            let topic_end = 2 + topic_length;
            let topic = body
                .get(2..topic_end)
                .ok_or_else(|| malformed("topic past the end"))?;
            let topic =
                String::from_utf8(topic.to_vec()).map_err(|_| malformed("topic is not UTF-8"))?;
            let (packet_id, payload_start) = if qos > 0 {
                (Some(u16_at(body, topic_end)?), topic_end + 2)
            } else {
                (None, topic_end)
            };
            Packet::Publish {
                message: Message {
                    topic,
                    payload: body.get(payload_start..).unwrap_or_default().to_vec(),
                    retain: first & 1 == 1,
                },
                qos,
                packet_id,
            }
        }
        kind::PUBACK => Packet::PubAck(u16_at(body, 0)?),
        kind::SUBACK => {
            Packet::SubAck(u16_at(body, 0)?, body.get(2..).unwrap_or_default().to_vec())
        }
        kind::PINGRESP => Packet::PingResp,
        other => Packet::Other(other),
    })
}

// --- the client -----------------------------------------------------------------------------------

/// A connected MQTT session. Not thread-safe: the agent uses it from one thread.
pub struct Client {
    stream: TcpStream,
    buffer: Vec<u8>,
    inbox: VecDeque<Message>,
    next_id: u16,
    keep_alive: Duration,
    last_sent: Instant,
    last_received: Instant,
}

impl Client {
    /// Open the TCP connection, send CONNECT and wait for CONNACK.
    pub fn connect(options: &Options) -> io::Result<Client> {
        let address = (options.host.as_str(), options.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("cannot resolve {}", options.host),
                )
            })?;
        let stream = TcpStream::connect_timeout(&address, ACK_TIMEOUT)?;
        stream.set_nodelay(true)?; // small packets: send at once
        let mut client = Client {
            stream,
            buffer: Vec::new(),
            inbox: VecDeque::new(),
            next_id: 0,
            keep_alive: options.keep_alive,
            last_sent: Instant::now(),
            last_received: Instant::now(),
        };
        client.send(&connect_packet(options))?;
        let deadline = Instant::now() + ACK_TIMEOUT;
        loop {
            match client.read_packet(deadline)? {
                Some(Packet::ConnAck { code: 0, .. }) => return Ok(client),
                Some(Packet::ConnAck { code, .. }) => {
                    let reason = match code {
                        1 => "unacceptable protocol version",
                        2 => "client identifier rejected",
                        3 => "server unavailable",
                        4 => "bad user name or password",
                        5 => "not authorised",
                        _ => "unknown reason",
                    };
                    return Err(io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        format!("broker refused: {reason}"),
                    ));
                }
                Some(_) => continue,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "no CONNACK from the broker",
                    ))
                }
            }
        }
    }

    fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes)?;
        self.last_sent = Instant::now();
        Ok(())
    }

    /// The next packet ID: 1..=65535, wrapping (0 is not allowed).
    fn next_packet_id(&mut self) -> u16 {
        self.next_id = self.next_id.checked_add(1).unwrap_or(1);
        self.next_id
    }

    /// Read until a whole packet is buffered or `deadline` passes (`Ok(None)`). Incoming messages
    /// are acknowledged and queued here, so they are never lost while waiting for an ack.
    fn read_packet(&mut self, deadline: Instant) -> io::Result<Option<Packet>> {
        let mut chunk = [0u8; 4096];
        loop {
            if let Some((first, body)) = take_frame(&mut self.buffer)? {
                self.last_received = Instant::now();
                let packet = parse_packet(first, &body)?;
                if let Packet::Publish {
                    message, packet_id, ..
                } = &packet
                {
                    if let Some(id) = packet_id {
                        self.send(&puback_packet(*id))?; // acknowledge first: QoS 1 may redeliver anyway
                    }
                    self.inbox.push_back(message.clone());
                }
                return Ok(Some(packet));
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            self.stream
                .set_read_timeout(Some((deadline - now).max(Duration::from_millis(1))))?;
            match self.stream.read(&mut chunk) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the broker closed the connection",
                    ))
                }
                Ok(n) => self.buffer.extend_from_slice(&chunk[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None)
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Wait for a packet that `wanted` accepts, until [`ACK_TIMEOUT`].
    fn wait_for(&mut self, what: &str, wanted: impl Fn(&Packet) -> bool) -> io::Result<Packet> {
        let deadline = Instant::now() + ACK_TIMEOUT;
        loop {
            match self.read_packet(deadline)? {
                Some(packet) if wanted(&packet) => return Ok(packet),
                Some(_) => continue,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("no {what} from the broker"),
                    ))
                }
            }
        }
    }

    /// Publish at QoS 1 and wait for the broker's PUBACK.
    pub fn publish(&mut self, topic: &str, payload: &[u8], retain: bool) -> io::Result<()> {
        let id = self.next_packet_id();
        self.send(&publish_packet(topic, payload, 1, retain, id))?;
        self.wait_for("PUBACK", |p| *p == Packet::PubAck(id))?;
        Ok(())
    }

    /// Subscribe at QoS 1 and wait for SUBACK. Retained messages on the topic follow at once.
    pub fn subscribe(&mut self, filter: &str) -> io::Result<()> {
        let id = self.next_packet_id();
        self.send(&subscribe_packet(id, filter, 1))?;
        match self.wait_for(
            "SUBACK",
            |p| matches!(p, Packet::SubAck(got, _) if *got == id),
        )? {
            Packet::SubAck(_, codes) if codes.first().is_some_and(|c| *c <= 2) => Ok(()),
            _ => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("subscription to {filter} refused"),
            )),
        }
    }

    /// The next received message, waiting up to `timeout`; pings the broker when it is due and
    /// fails if the broker has been silent for 1.5 keep-alive periods (the connection is dead).
    pub fn poll(&mut self, timeout: Duration) -> io::Result<Option<Message>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(message) = self.inbox.pop_front() {
                return Ok(Some(message));
            }
            if !self.keep_alive.is_zero() {
                if self.last_sent.elapsed() >= self.keep_alive / 2 {
                    self.send(&pingreq_packet())?;
                }
                if self.last_received.elapsed() > self.keep_alive * 3 / 2 {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the broker stopped answering",
                    ));
                }
            }
            // Wake up in time for the next ping even if the caller asked to wait longer.
            let ping_due = self.last_sent + self.keep_alive / 2;
            let wake = if self.keep_alive.is_zero() {
                deadline
            } else {
                deadline.min(ping_due)
            };
            if self.read_packet(wake)?.is_none()
                && Instant::now() >= deadline
                && self.inbox.is_empty()
            {
                return Ok(None);
            }
        }
    }

    /// Say goodbye cleanly: the broker discards the last will.
    pub fn disconnect(mut self) -> io::Result<()> {
        self.send(&disconnect_packet())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reverse of `encode_length`: low 7 bits first, so fold from the last byte.
    fn decode_length(bytes: &[u8]) -> usize {
        bytes
            .iter()
            .rev()
            .fold(0, |length, byte| length * 128 + usize::from(byte & 0x7F))
    }

    #[test]
    fn remaining_length_examples_from_the_spec() {
        // Spec table 2.4: the boundaries of 1, 2, 3 and 4 byte lengths.
        for (length, bytes) in [
            (0, vec![0x00]),
            (127, vec![0x7F]),
            (128, vec![0x80, 0x01]),
            (16_383, vec![0xFF, 0x7F]),
            (16_384, vec![0x80, 0x80, 0x01]),
            (2_097_151, vec![0xFF, 0xFF, 0x7F]),
            (2_097_152, vec![0x80, 0x80, 0x80, 0x01]),
            (268_435_455, vec![0xFF, 0xFF, 0xFF, 0x7F]),
        ] {
            let mut out = Vec::new();
            encode_length(length, &mut out);
            assert_eq!(out, bytes, "length {length}");
            assert_eq!(decode_length(&bytes), length);
        }
    }

    #[test]
    fn connect_packet_bytes() {
        let mut options = Options::from_url("mqtt://u:p@broker:1883", "edge-1").unwrap();
        options.keep_alive = Duration::from_secs(60);
        options.will = Some(Will {
            topic: "a/b".into(),
            payload: b"x".to_vec(),
            retain: true,
        });
        let bytes = connect_packet(&options);
        let expected: Vec<u8> = [
            &[0x10, 32][..],                             // CONNECT, remaining length 32
            &[0, 4, b'M', b'Q', b'T', b'T', 4],          // protocol name and level
            &[0b1110_1110, 0, 60], // user, password, will retain, will QoS 1, will, clean; 60 s
            &[0, 6, b'e', b'd', b'g', b'e', b'-', b'1'], // client ID
            &[0, 3, b'a', b'/', b'b', 0, 1, b'x'], // will topic and message
            &[0, 1, b'u', 0, 1, b'p'], // user name and password
        ]
        .concat();
        assert_eq!(bytes, expected);
    }

    #[test]
    fn publish_round_trip_and_framing() {
        let bytes = publish_packet("c/state/job", b"{}", 1, true, 7);
        assert_eq!(bytes[0], 0x33); // PUBLISH, QoS 1, retain
                                    // Feed it one byte at a time, as a slow network might: no frame until the last byte.
        let mut buffer = Vec::new();
        for (i, byte) in bytes.iter().enumerate() {
            buffer.push(*byte);
            let frame = take_frame(&mut buffer).unwrap();
            assert_eq!(frame.is_some(), i == bytes.len() - 1);
            if let Some((first, body)) = frame {
                let packet = parse_packet(first, &body).unwrap();
                assert_eq!(
                    packet,
                    Packet::Publish {
                        message: Message {
                            topic: "c/state/job".into(),
                            payload: b"{}".to_vec(),
                            retain: true
                        },
                        qos: 1,
                        packet_id: Some(7)
                    }
                );
            }
        }
        assert!(buffer.is_empty());
        // Two packets in one read come out one by one.
        let mut buffer = [puback_packet(1), pingreq_packet()].concat();
        assert_eq!(take_frame(&mut buffer).unwrap(), Some((0x40, vec![0, 1])));
        assert_eq!(take_frame(&mut buffer).unwrap(), Some((0xC0, vec![])));
        assert_eq!(take_frame(&mut buffer).unwrap(), None);
    }

    #[test]
    fn rejects_malformed_and_oversized_packets() {
        let mut too_long = vec![0x30, 0x80, 0x80, 0x80, 0x80, 0x01];
        assert!(take_frame(&mut too_long).is_err());
        let mut huge = vec![0x30];
        encode_length(MAX_PACKET + 1, &mut huge);
        assert!(take_frame(&mut huge).is_err());
        assert!(parse_packet(0x30, &[0, 9, b'a']).is_err()); // topic longer than the packet
        assert!(parse_packet(0x20, &[0]).is_err()); // short CONNACK
    }

    #[test]
    fn subscribe_and_small_packets() {
        assert_eq!(
            subscribe_packet(2, "a/#", 1),
            vec![0x82, 8, 0, 2, 0, 3, b'a', b'/', b'#', 1]
        );
        assert_eq!(
            parse_packet(0x90, &[0, 2, 1]).unwrap(),
            Packet::SubAck(2, vec![1])
        );
        assert_eq!(
            parse_packet(0x20, &[1, 0]).unwrap(),
            Packet::ConnAck {
                session_present: true,
                code: 0
            }
        );
        assert_eq!(disconnect_packet(), vec![0xE0, 0]);
    }

    #[test]
    fn urls() {
        let options = Options::from_url("mqtt://broker", "c").unwrap();
        assert_eq!(
            (options.host.as_str(), options.port, options.username),
            ("broker", 1883, None)
        );
        let options = Options::from_url("mqtt://edge:p%40ss@10.0.0.5:1884/", "c").unwrap();
        assert_eq!((options.host.as_str(), options.port), ("10.0.0.5", 1884));
        assert_eq!(options.username.as_deref(), Some("edge"));
        assert!(Options::from_url("mqtts://broker:8883", "c").is_err());
        assert!(Options::from_url("http://broker", "c").is_err());
        assert!(Options::from_url("mqtt://broker:notaport", "c").is_err());
        assert_eq!(
            redact("mqtt://edge:secret@broker:1883"),
            "mqtt://***@broker:1883"
        );
        assert_eq!(redact("mqtt://broker:1883"), "mqtt://broker:1883");
    }
}
