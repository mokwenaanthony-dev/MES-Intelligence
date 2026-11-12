//! JSON without a crate: a value type, a recursive-descent parser and a writer.
//!
//! Two choices follow from the project's rules:
//!
//! * **Numbers are kept as their exact text** ([`Value::Number`] holds a `String`). The contract sends
//!   quantities and money as decimal strings, and nothing here should turn `"10.5"` into a binary
//!   float on the way through (ERP ADR 0008).
//! * **Objects keep their keys in order** (a `Vec` of pairs, not a hash map), so what the agent writes
//!   is always the same bytes for the same content: a repeated message is byte-for-byte identical.
//!
//! The parser is a classic recursive-descent parser: one function per grammar rule (value, object,
//! array, string, number), each consuming input from a cursor. Nesting is limited to
//! [`MAX_DEPTH`] levels so a hostile message cannot exhaust the stack.

use std::fmt;

/// Deepest nesting the parser accepts. Contract messages need 3.
pub const MAX_DEPTH: usize = 32;

/// A JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// `null`
    Null,
    /// `true` or `false`
    Bool(bool),
    /// A number, as written (e.g. `"10.500"`), so decimals stay exact.
    Number(String),
    /// A string.
    Str(String),
    /// An array.
    Array(Vec<Value>),
    /// An object, keys in their original order.
    Object(Vec<(String, Value)>),
}

/// Why a text is not valid JSON, and where (byte offset).
#[derive(Debug, PartialEq)]
pub struct ParseError {
    /// Byte offset of the problem in the input.
    pub at: usize,
    /// What was wrong.
    pub reason: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid JSON at byte {}: {}", self.at, self.reason)
    }
}

impl std::error::Error for ParseError {}

impl Value {
    /// A string value.
    pub fn str(text: impl Into<String>) -> Value {
        Value::Str(text.into())
    }

    /// A number value from an integer.
    pub fn int(number: i64) -> Value {
        Value::Number(number.to_string())
    }

    /// An object from `(key, value)` pairs, in that order.
    pub fn object(pairs: Vec<(&str, Value)>) -> Value {
        Value::Object(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// The value under `key`, if this is an object that has it.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The text of a string value.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(text) => Some(text),
            _ => None,
        }
    }

    /// A number or a numeric string as an `i64`, if it is a whole number that fits.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Number(text) | Value::Str(text) => text.parse().ok(),
            _ => None,
        }
    }

    /// The exact text of a number or a numeric string (the contract sends decimals as strings).
    pub fn as_decimal_text(&self) -> Option<&str> {
        match self {
            Value::Number(text) => Some(text),
            Value::Str(text) if is_number(text) => Some(text),
            _ => None,
        }
    }

    /// The pairs of an object.
    pub fn as_object(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Object(pairs) => Some(pairs),
            _ => None,
        }
    }

    /// The value as compact JSON text.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        write_value(self, &mut out);
        out
    }
}

/// Parse a complete JSON text (nothing but whitespace may follow the value).
pub fn parse(text: &str) -> Result<Value, ParseError> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    let value = parser.value(0)?;
    parser.skip_whitespace();
    if parser.pos != parser.bytes.len() {
        return Err(parser.error("unexpected text after the value"));
    }
    Ok(value)
}

/// Whether `text` is a JSON number (used to accept decimal strings such as `"10.500"`).
pub fn is_number(text: &str) -> bool {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    parser.number().is_ok() && parser.pos == text.len()
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn error(&self, reason: &'static str) -> ParseError {
        ParseError {
            at: self.pos,
            reason,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect_word(&mut self, word: &[u8], value: Value) -> Result<Value, ParseError> {
        if self.bytes[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.error("unknown word"))
        }
    }

    /// value = object | array | string | number | true | false | null
    fn value(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.error("nested too deeply"));
        }
        self.skip_whitespace();
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') => self.expect_word(b"true", Value::Bool(true)),
            Some(b'f') => self.expect_word(b"false", Value::Bool(false)),
            Some(b'n') => self.expect_word(b"null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    /// object = '{' [ string ':' value { ',' string ':' value } ] '}'
    fn object(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.pos += 1; // '{'
        let mut pairs = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(pairs));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a key"));
            }
            let key = self.string()?;
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(self.error("expected ':'"));
            }
            self.pos += 1;
            let value = self.value(depth + 1)?;
            pairs.push((key, value));
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(pairs));
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
    }

    /// array = '[' [ value { ',' value } ] ']'
    fn array(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.value(depth + 1)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
    }

    /// string = '"' { character | escape } '"', escapes including \uXXXX surrogate pairs
    fn string(&mut self) -> Result<String, ParseError> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        loop {
            let start = self.pos;
            // Copy a run of ordinary characters in one go (the input is valid UTF-8: it is a &str).
            while let Some(byte) = self.peek() {
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            out.push_str(
                std::str::from_utf8(&self.bytes[start..self.pos])
                    .map_err(|_| self.error("invalid UTF-8"))?,
            );
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let escape = self.peek().ok_or_else(|| self.error("unfinished escape"))?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(self.error("unknown escape")),
                    }
                }
                Some(_) => return Err(self.error("control character in string")),
                None => return Err(self.error("unterminated string")),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self
            .bytes
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| self.error("short \\u escape"))?;
        let text = std::str::from_utf8(digits).map_err(|_| self.error("bad \\u escape"))?;
        let code = u32::from_str_radix(text, 16).map_err(|_| self.error("bad \\u escape"))?;
        self.pos += 4;
        Ok(code)
    }

    /// The character of a `\uXXXX` escape; characters above U+FFFF come as two escapes (a surrogate pair).
    fn unicode_escape(&mut self) -> Result<char, ParseError> {
        let first = self.hex4()?;
        let code = if (0xD800..0xDC00).contains(&first) {
            if !self.bytes[self.pos..].starts_with(b"\\u") {
                return Err(self.error("lone surrogate"));
            }
            self.pos += 2;
            let second = self.hex4()?;
            if !(0xDC00..0xE000).contains(&second) {
                return Err(self.error("bad surrogate pair"));
            }
            0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
        } else {
            first
        };
        char::from_u32(code).ok_or_else(|| self.error("invalid character"))
    }

    /// number = [ '-' ] ( '0' | digit1-9 { digit } ) [ '.' digit { digit } ] [ ( 'e' | 'E' ) [ '+' | '-' ] digit { digit } ]
    fn number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => self.digits(),
            _ => return Err(self.error("expected a digit")),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected a digit after '.'"));
            }
            self.digits();
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected a digit in the exponent"));
            }
            self.digits();
        }
        // The slice holds only ASCII digits and signs, so it is valid UTF-8.
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.error("invalid number"))?;
        Ok(Value::Number(text.to_string()))
    }

    fn digits(&mut self) {
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
    }
}

fn write_value(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(text) => out.push_str(text),
        Value::Str(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, out);
            }
            out.push(']');
        }
        Value::Object(pairs) => {
            out.push('{');
            for (i, (key, item)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_value(item, out);
            }
            out.push('}');
        }
    }
}

/// A string with the escapes JSON requires (quotes, backslashes and control characters).
fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_contract_envelope() {
        let text = r#"{"schema": "job/v1", "event_id": null, "data": {"quantity": "100.000", "operation_number": 10,
                       "planned_activities": {"MACH": "10.500"}, "final": true, "tags": []}}"#;
        let value = parse(text).unwrap();
        assert_eq!(value.get("schema").and_then(Value::as_str), Some("job/v1"));
        assert_eq!(value.get("event_id"), Some(&Value::Null));
        let data = value.get("data").unwrap();
        assert_eq!(
            data.get("quantity").and_then(Value::as_decimal_text),
            Some("100.000")
        );
        assert_eq!(
            data.get("operation_number").and_then(Value::as_i64),
            Some(10)
        );
        assert_eq!(
            data.get("planned_activities")
                .and_then(|a| a.get("MACH"))
                .and_then(Value::as_str),
            Some("10.500")
        );
        assert_eq!(data.get("final"), Some(&Value::Bool(true)));
        assert_eq!(data.get("tags"), Some(&Value::Array(vec![])));
    }

    #[test]
    fn numbers_stay_exact() {
        for text in [
            "0",
            "-0",
            "10.500",
            "1e3",
            "-2.5E-7",
            "123456789012345678901234567890.000001",
        ] {
            assert_eq!(parse(text).unwrap(), Value::Number(text.to_string()));
        }
        for bad in ["01", "1.", ".5", "-", "1e", "+1"] {
            assert!(parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn strings_and_escapes_round_trip() {
        let text = r#""quote \" backslash \\ slash \/ tab \t line \n e-acute é clef 𝄞""#;
        let value = parse(text).unwrap();
        assert_eq!(
            value,
            Value::str("quote \" backslash \\ slash / tab \t line \n e-acute é clef 𝄞")
        );
        assert_eq!(parse(&value.to_json()).unwrap(), value); // writing then reading gives the same value
        assert!(parse(r#""\ud834""#).is_err()); // half a surrogate pair
        assert!(parse("\"a\u{1}b\"").is_err()); // raw control character
    }

    #[test]
    fn writes_compact_json_in_key_order() {
        let value = Value::object(vec![
            ("b", Value::int(2)),
            ("a", Value::str("x\u{1}")),
            ("c", Value::Array(vec![Value::Null, Value::Bool(false)])),
        ]);
        assert_eq!(value.to_json(), r#"{"b":2,"a":"x\u0001","c":[null,false]}"#);
    }

    #[test]
    fn rejects_broken_and_hostile_input() {
        for bad in [
            "",
            "{",
            "{\"a\" 1}",
            "[1,]",
            "{\"a\":1,}",
            "nul",
            "\"open",
            "[1] 2",
            "{1:2}",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
        let deep = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
        assert_eq!(parse(&deep).unwrap_err().reason, "nested too deeply");
        let ok = "[".repeat(MAX_DEPTH) + &"]".repeat(MAX_DEPTH);
        assert!(parse(&ok).is_ok());
    }

    #[test]
    fn decimal_strings_are_recognised() {
        assert!(is_number("10.500") && is_number("-3") && !is_number("ten") && !is_number("1.2.3"));
        assert_eq!(Value::str("abc").as_decimal_text(), None);
    }
}
