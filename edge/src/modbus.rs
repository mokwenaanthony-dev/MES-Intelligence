//! Modbus TCP: how the edge agent talks to the machine's PLC.
//!
//! Modbus is the oldest and most widespread PLC protocol (1979, Modicon). A PLC exposes numbered
//! 16-bit **holding registers**; a client reads and writes them by number. Over TCP every request
//! and response is an **ADU**: a 7-byte MBAP header, then the **PDU** (function code and data):
//!
//! ```text
//! MBAP: transaction id (2) | protocol id = 0 (2) | length of what follows (2) | unit id (1)
//! PDU:  function code (1)  | data ...
//! ```
//!
//! Three function codes cover the agent's needs:
//!
//! | Code | Name | Request data | Response data |
//! |---|---|---|---|
//! | 0x03 | Read Holding Registers | start, count (1-125) | byte count, values |
//! | 0x06 | Write Single Register | address, value | the same echoed |
//! | 0x10 | Write Multiple Registers | start, count (1-123), byte count, values | start, count |
//!
//! A refused request comes back with the function code's top bit set and an exception code
//! (1 illegal function, 2 illegal data address, 3 illegal data value). All numbers are big-endian.
//!
//! The client side ([`Client`]) is used by the agent; the server side ([`handle_pdu`], [`serve`]) by
//! the simulated PLC. Both check every length, because a PLC network is still a network.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Read Holding Registers.
pub const READ_HOLDING: u8 = 0x03;
/// Write Single Register.
pub const WRITE_SINGLE: u8 = 0x06;
/// Write Multiple Registers.
pub const WRITE_MULTIPLE: u8 = 0x10;

/// Exception: the function code is not supported.
pub const ILLEGAL_FUNCTION: u8 = 1;
/// Exception: a register address is outside what the device has (or is read-only).
pub const ILLEGAL_ADDRESS: u8 = 2;
/// Exception: a count or length in the request is wrong.
pub const ILLEGAL_VALUE: u8 = 3;

/// Something that holds registers: the simulated PLC implements it.
pub trait Registers {
    /// The values of `count` registers from `start`, or `None` if any is outside the device.
    fn read(&self, start: u16, count: u16) -> Option<Vec<u16>>;
    /// Write `values` from `start`; false if any register is outside the device or read-only.
    fn write(&mut self, start: u16, values: &[u16]) -> bool;
}

fn exception(function: u8, code: u8) -> Vec<u8> {
    vec![function | 0x80, code]
}

fn be16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

/// Answer one request PDU (the server side of the protocol).
pub fn handle_pdu(pdu: &[u8], registers: &mut impl Registers) -> Vec<u8> {
    let Some(&function) = pdu.first() else {
        return exception(0, ILLEGAL_FUNCTION);
    };
    match function {
        READ_HOLDING => {
            if pdu.len() != 5 {
                return exception(function, ILLEGAL_VALUE);
            }
            let (start, count) = (be16(pdu, 1), be16(pdu, 3));
            if !(1..=125).contains(&count) {
                return exception(function, ILLEGAL_VALUE);
            }
            match registers.read(start, count) {
                Some(values) => {
                    let mut out = vec![function, (count * 2) as u8];
                    for value in values {
                        out.extend_from_slice(&value.to_be_bytes());
                    }
                    out
                }
                None => exception(function, ILLEGAL_ADDRESS),
            }
        }
        WRITE_SINGLE => {
            if pdu.len() != 5 {
                return exception(function, ILLEGAL_VALUE);
            }
            if registers.write(be16(pdu, 1), &[be16(pdu, 3)]) {
                pdu.to_vec() // the response echoes the request
            } else {
                exception(function, ILLEGAL_ADDRESS)
            }
        }
        WRITE_MULTIPLE => {
            if pdu.len() < 6 {
                return exception(function, ILLEGAL_VALUE);
            }
            let (start, count, byte_count) = (be16(pdu, 1), be16(pdu, 3), usize::from(pdu[5]));
            if !(1..=123).contains(&count)
                || byte_count != usize::from(count) * 2
                || pdu.len() != 6 + byte_count
            {
                return exception(function, ILLEGAL_VALUE);
            }
            let values: Vec<u16> = (0..usize::from(count))
                .map(|i| be16(pdu, 6 + 2 * i))
                .collect();
            if registers.write(start, &values) {
                let mut out = vec![function];
                out.extend_from_slice(&start.to_be_bytes());
                out.extend_from_slice(&count.to_be_bytes());
                out
            } else {
                exception(function, ILLEGAL_ADDRESS)
            }
        }
        _ => exception(function, ILLEGAL_FUNCTION),
    }
}

/// Read one ADU: `(transaction id, unit id, PDU)`.
fn read_adu(stream: &mut impl Read) -> io::Result<(u16, u8, Vec<u8>)> {
    let mut header = [0u8; 7];
    stream.read_exact(&mut header)?;
    let (transaction, protocol, length) = (
        be16(&header, 0),
        be16(&header, 2),
        usize::from(be16(&header, 4)),
    );
    if protocol != 0 || !(2..=254).contains(&length) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a Modbus TCP frame",
        ));
    }
    let mut pdu = vec![0u8; length - 1]; // the length counts the unit id too
    stream.read_exact(&mut pdu)?;
    Ok((transaction, header[6], pdu))
}

/// Build one ADU around a PDU.
fn adu(transaction: u16, unit: u8, pdu: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(7 + pdu.len());
    out.extend_from_slice(&transaction.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&((pdu.len() + 1) as u16).to_be_bytes());
    out.push(unit);
    out.extend_from_slice(pdu);
    out
}

/// Serve one client connection until it closes: read a request, answer it, repeat.
pub fn serve_connection<R: Registers>(
    mut stream: TcpStream,
    registers: Arc<Mutex<R>>,
) -> io::Result<()> {
    loop {
        let (transaction, unit, pdu) = match read_adu(&mut stream) {
            Ok(frame) => frame,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()), // client hung up
            Err(e) => return Err(e),
        };
        let response = {
            // A poisoned lock means another connection panicked mid-write; the registers are still
            // plain numbers, so carry on with them rather than take the whole PLC down.
            let mut guard = registers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            handle_pdu(&pdu, &mut *guard)
        };
        stream.write_all(&adu(transaction, unit, &response))?;
    }
}

/// Accept connections forever, one thread each (a PLC usually allows a few clients at once).
pub fn serve<R: Registers + Send + 'static>(listener: TcpListener, registers: Arc<Mutex<R>>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let peer = stream
                    .peer_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_default();
                let registers = Arc::clone(&registers);
                crate::info!("Modbus client connected from {peer}");
                let name = format!("modbus-{peer}");
                let spawned = std::thread::Builder::new().name(name).spawn(move || {
                    if let Err(e) = serve_connection(stream, registers) {
                        crate::warning!("Modbus connection from {peer} ended: {e}");
                    }
                });
                if let Err(e) = spawned {
                    crate::error!("Could not start a thread for a Modbus client: {e}");
                }
            }
            Err(e) => crate::warning!("Accept failed: {e}"),
        }
    }
}

/// A Modbus TCP client (the agent's side).
pub struct Client {
    stream: TcpStream,
    unit: u8,
    transaction: u16,
}

impl Client {
    /// Connect to `address` (e.g. `plc-press3:5020`), with a timeout for connecting and each answer.
    pub fn connect(address: &str, unit: u8, timeout: Duration) -> io::Result<Client> {
        let socket = address.to_socket_addrs()?.next().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, format!("cannot resolve {address}"))
        })?;
        let stream = TcpStream::connect_timeout(&socket, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        Ok(Client {
            stream,
            unit,
            transaction: 0,
        })
    }

    /// Send a request PDU and return the response PDU, checking that it answers this request.
    fn request(&mut self, pdu: &[u8]) -> io::Result<Vec<u8>> {
        self.transaction = self.transaction.wrapping_add(1);
        self.stream
            .write_all(&adu(self.transaction, self.unit, pdu))?;
        let (transaction, _unit, response) = read_adu(&mut self.stream)?;
        if transaction != self.transaction {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "answer to another request",
            ));
        }
        match response.first() {
            Some(&code) if code == pdu[0] => Ok(response),
            Some(&code) if code == pdu[0] | 0x80 => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "PLC refused function {:#04x}: exception {}",
                    pdu[0],
                    response.get(1).copied().unwrap_or(0)
                ),
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected function code in answer",
            )),
        }
    }

    /// Read `count` holding registers from `start` (function 0x03).
    pub fn read_holding(&mut self, start: u16, count: u16) -> io::Result<Vec<u16>> {
        let mut pdu = vec![READ_HOLDING];
        pdu.extend_from_slice(&start.to_be_bytes());
        pdu.extend_from_slice(&count.to_be_bytes());
        let response = self.request(&pdu)?;
        if response.len() != 2 + usize::from(count) * 2
            || usize::from(response[1]) != usize::from(count) * 2
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "wrong number of registers in answer",
            ));
        }
        Ok((0..usize::from(count))
            .map(|i| be16(&response, 2 + 2 * i))
            .collect())
    }

    /// Write `values` from `start` in one request (function 0x10), so the PLC sees them together.
    pub fn write_multiple(&mut self, start: u16, values: &[u16]) -> io::Result<()> {
        let mut pdu = vec![WRITE_MULTIPLE];
        pdu.extend_from_slice(&start.to_be_bytes());
        pdu.extend_from_slice(&(values.len() as u16).to_be_bytes());
        pdu.push((values.len() * 2) as u8);
        for value in values {
            pdu.extend_from_slice(&value.to_be_bytes());
        }
        self.request(&pdu).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ten registers; 8 and 9 are read-only.
    struct Bank([u16; 10]);

    impl Registers for Bank {
        fn read(&self, start: u16, count: u16) -> Option<Vec<u16>> {
            self.0
                .get(usize::from(start)..usize::from(start) + usize::from(count))
                .map(<[u16]>::to_vec)
        }
        fn write(&mut self, start: u16, values: &[u16]) -> bool {
            let end = usize::from(start) + values.len();
            if end > 8 {
                return false;
            }
            self.0[usize::from(start)..end].copy_from_slice(values);
            true
        }
    }

    #[test]
    fn request_handling() {
        let mut bank = Bank([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(
            handle_pdu(&[0x03, 0, 2, 0, 2], &mut bank),
            vec![0x03, 4, 0, 2, 0, 3]
        );
        assert_eq!(
            handle_pdu(&[0x03, 0, 9, 0, 2], &mut bank),
            vec![0x83, ILLEGAL_ADDRESS]
        );
        assert_eq!(
            handle_pdu(&[0x03, 0, 0, 0, 0], &mut bank),
            vec![0x83, ILLEGAL_VALUE]
        );
        assert_eq!(
            handle_pdu(&[0x06, 0, 1, 0x12, 0x34], &mut bank),
            vec![0x06, 0, 1, 0x12, 0x34]
        );
        assert_eq!(bank.0[1], 0x1234);
        assert_eq!(
            handle_pdu(&[0x10, 0, 3, 0, 2, 4, 0, 7, 0, 8], &mut bank),
            vec![0x10, 0, 3, 0, 2]
        );
        assert_eq!(&bank.0[3..5], &[7, 8]);
        assert_eq!(
            handle_pdu(&[0x10, 0, 8, 0, 1, 2, 0, 1], &mut bank),
            vec![0x90, ILLEGAL_ADDRESS]
        ); // read-only
        assert_eq!(
            handle_pdu(&[0x10, 0, 3, 0, 2, 3, 0, 7, 0], &mut bank),
            vec![0x90, ILLEGAL_VALUE]
        ); // bad byte count
        assert_eq!(
            handle_pdu(&[0x2B, 0x0E], &mut bank),
            vec![0xAB, ILLEGAL_FUNCTION]
        );
        assert_eq!(handle_pdu(&[], &mut bank), vec![0x80, ILLEGAL_FUNCTION]);
    }

    #[test]
    fn client_and_server_over_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let bank = Arc::new(Mutex::new(Bank([0; 10])));
        let served = Arc::clone(&bank);
        std::thread::spawn(move || serve(listener, served));

        let mut client = Client::connect(&address, 1, Duration::from_secs(2)).unwrap();
        client.write_multiple(2, &[500, 600, 700]).unwrap();
        assert_eq!(
            client.read_holding(0, 6).unwrap(),
            vec![0, 0, 500, 600, 700, 0]
        );
        let refused = client.write_multiple(8, &[1]).unwrap_err();
        assert!(refused.to_string().contains("exception 2"), "{refused}");
        assert_eq!(bank.lock().unwrap().0[4], 700);
        // The connection is still usable after a refusal.
        assert_eq!(client.read_holding(4, 1).unwrap(), vec![700]);
    }

    #[test]
    fn frames_are_checked() {
        let mut not_modbus: &[u8] = &[0, 1, 0, 9, 0, 3, 1, 3, 0];
        assert!(read_adu(&mut not_modbus).is_err()); // protocol id 9
        let frame = adu(5, 1, &[0x03, 0, 0, 0, 1]);
        assert_eq!(frame, vec![0, 5, 0, 0, 0, 6, 1, 0x03, 0, 0, 0, 1]);
        assert_eq!(
            read_adu(&mut frame.as_slice()).unwrap(),
            (5, 1, vec![0x03, 0, 0, 0, 1])
        );
    }
}
