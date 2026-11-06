//! `application/vnd.amazon.eventstream` framing (botocore `eventstream.py`).
//!
//! The transport hands over complete bodies, so a response is decoded into
//! all of its messages at once.

use super::ProtocolError;

type Result<T> = std::result::Result<T, ProtocolError>;

const PRELUDE_LENGTH: usize = 12;
const MAX_HEADERS_LENGTH: usize = 128 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum HeaderValue {
    Bool(bool),
    /// byte, short, integer and long headers.
    Int(i64),
    Bytes(Vec<u8>),
    Str(String),
    /// Milliseconds since the epoch.
    Timestamp(i64),
    Uuid([u8; 16]),
}

impl HeaderValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            HeaderValue::Str(s) => Some(s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub headers: Vec<(String, HeaderValue)>,
    pub payload: Vec<u8>,
}

impl Message {
    pub fn header(&self, name: &str) -> Option<&HeaderValue> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.header(name).and_then(HeaderValue::as_str)
    }

    /// `:message-type` of `error` or `exception`.
    pub fn is_error(&self) -> bool {
        matches!(
            self.header_str(":message-type"),
            Some("error" | "exception")
        )
    }
}

fn err(m: impl Into<String>) -> ProtocolError {
    ProtocolError(m.into())
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}

fn take<'a>(data: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if data.len() < n {
        return Err(err("Truncated event stream header"));
    }
    let (head, rest) = data.split_at(n);
    *data = rest;
    Ok(head)
}

fn parse_headers(mut data: &[u8]) -> Result<Vec<(String, HeaderValue)>> {
    let mut out: Vec<(String, HeaderValue)> = Vec::new();
    while !data.is_empty() {
        let n = take(&mut data, 1)?[0] as usize;
        let name = String::from_utf8(take(&mut data, n)?.to_vec())
            .map_err(|_| err("Invalid UTF-8 in event stream header name"))?;
        let ty = take(&mut data, 1)?[0];
        let int = |d: &mut &[u8], n: usize| -> Result<i64> {
            let b = take(d, n)?;
            Ok(match n {
                1 => b[0] as i8 as i64,
                2 => i16::from_be_bytes(b.try_into().unwrap()) as i64,
                4 => i32::from_be_bytes(b.try_into().unwrap()) as i64,
                _ => i64::from_be_bytes(b.try_into().unwrap()),
            })
        };
        let value = match ty {
            0 => HeaderValue::Bool(true),
            1 => HeaderValue::Bool(false),
            2 => HeaderValue::Int(int(&mut data, 1)?),
            3 => HeaderValue::Int(int(&mut data, 2)?),
            4 => HeaderValue::Int(int(&mut data, 4)?),
            5 => HeaderValue::Int(int(&mut data, 8)?),
            6 | 7 => {
                let len = u16::from_be_bytes(take(&mut data, 2)?.try_into().unwrap()) as usize;
                let bytes = take(&mut data, len)?.to_vec();
                if ty == 6 {
                    HeaderValue::Bytes(bytes)
                } else {
                    HeaderValue::Str(
                        String::from_utf8(bytes)
                            .map_err(|_| err("Invalid UTF-8 in event stream header"))?,
                    )
                }
            }
            8 => HeaderValue::Timestamp(int(&mut data, 8)?),
            9 => HeaderValue::Uuid(take(&mut data, 16)?.try_into().unwrap()),
            other => return Err(err(format!("Unknown event stream header type: {other}"))),
        };
        if out.iter().any(|(k, _)| *k == name) {
            return Err(err(format!("Duplicate header present: \"{name}\"")));
        }
        out.push((name, value));
    }
    Ok(out)
}

/// Decode every message in `data`, validating both CRCs.
pub fn decode(mut data: &[u8]) -> Result<Vec<Message>> {
    let mut out = Vec::new();
    while !data.is_empty() {
        if data.len() < PRELUDE_LENGTH {
            return Err(err("Truncated event stream message"));
        }
        let total = be_u32(data) as usize;
        let headers_len = be_u32(&data[4..]) as usize;
        let prelude_crc = be_u32(&data[8..]);
        let computed = crc32fast::hash(&data[..8]);
        if computed != prelude_crc {
            return Err(err(format!(
                "Checksum mismatch: expected 0x{prelude_crc:08x}, calculated 0x{computed:08x}"
            )));
        }
        if headers_len > MAX_HEADERS_LENGTH {
            return Err(err(format!(
                "Header length of {headers_len} exceeded the maximum of {MAX_HEADERS_LENGTH}"
            )));
        }
        if total < PRELUDE_LENGTH + headers_len + 4 || data.len() < total {
            return Err(err("Truncated event stream message"));
        }
        let payload_end = total - 4;
        let message_crc = be_u32(&data[payload_end..]);
        let computed = crc32fast::hash(&data[..payload_end]);
        if computed != message_crc {
            return Err(err(format!(
                "Checksum mismatch: expected 0x{message_crc:08x}, calculated 0x{computed:08x}"
            )));
        }
        let headers_end = PRELUDE_LENGTH + headers_len;
        out.push(Message {
            headers: parse_headers(&data[PRELUDE_LENGTH..headers_end])?,
            payload: data[headers_end..payload_end].to_vec(),
        });
        data = &data[total..];
    }
    Ok(out)
}

/// Encode one message (used by tests to build responses).
pub fn encode(headers: &[(&str, HeaderValue)], payload: &[u8]) -> Vec<u8> {
    let mut h = Vec::new();
    for (name, value) in headers {
        h.push(name.len() as u8);
        h.extend(name.as_bytes());
        match value {
            HeaderValue::Bool(b) => h.push(if *b { 0 } else { 1 }),
            HeaderValue::Int(i) => {
                h.push(5);
                h.extend(i.to_be_bytes());
            }
            HeaderValue::Bytes(b) => {
                h.push(6);
                h.extend((b.len() as u16).to_be_bytes());
                h.extend(b);
            }
            HeaderValue::Str(s) => {
                h.push(7);
                h.extend((s.len() as u16).to_be_bytes());
                h.extend(s.as_bytes());
            }
            HeaderValue::Timestamp(t) => {
                h.push(8);
                h.extend(t.to_be_bytes());
            }
            HeaderValue::Uuid(u) => {
                h.push(9);
                h.extend(u);
            }
        }
    }
    let total = PRELUDE_LENGTH + h.len() + payload.len() + 4;
    let mut out = Vec::with_capacity(total);
    out.extend((total as u32).to_be_bytes());
    out.extend((h.len() as u32).to_be_bytes());
    out.extend(crc32fast::hash(&out).to_be_bytes());
    out.extend(h);
    out.extend(payload);
    out.extend(crc32fast::hash(&out).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Vectors from botocore tests/unit/botocore/test_eventstream.py.
    const ALL_HEADERS_TYPES: &[u8] = b"\x00\x00\x00\x62\x00\x00\x00\x52\x03\xb5\xcb\x9c\
\x010\x00\x011\x01\x012\x02\x02\x013\x03\x00\x03\
\x014\x04\x00\x00\x00\x04\x015\x05\x00\x00\x00\x00\x00\x00\x00\x05\
\x016\x06\x00\x05bytes\x017\x07\x00\x04utf8\
\x018\x08\x00\x00\x00\x00\x00\x00\x00\x08\x019\x090123456789abcdef\
\x63\x35\x36\x71";

    const ERROR_EVENT_MESSAGE: &[u8] = b"\x00\x00\x00\x52\x00\x00\x00\x42\xbf\x23\x63\x7e\
\x0d:message-type\x07\x00\x05error\
\x0b:error-code\x07\x00\x04code\
\x0e:error-message\x07\x00\x07message\
\x6b\x6c\xea\x3d";

    #[test]
    fn decodes_botocore_positive_cases() {
        let all = decode(ALL_HEADERS_TYPES).unwrap();
        assert_eq!(all.len(), 1);
        let h = &all[0].headers;
        assert_eq!(h[0], ("0".into(), HeaderValue::Bool(true)));
        assert_eq!(h[1], ("1".into(), HeaderValue::Bool(false)));
        assert_eq!(h[2], ("2".into(), HeaderValue::Int(2)));
        assert_eq!(h[3], ("3".into(), HeaderValue::Int(3)));
        assert_eq!(h[4], ("4".into(), HeaderValue::Int(4)));
        assert_eq!(h[5], ("5".into(), HeaderValue::Int(5)));
        assert_eq!(h[6], ("6".into(), HeaderValue::Bytes(b"bytes".to_vec())));
        assert_eq!(h[7], ("7".into(), HeaderValue::Str("utf8".into())));
        assert_eq!(h[8], ("8".into(), HeaderValue::Timestamp(8)));
        assert_eq!(h[9], ("9".into(), HeaderValue::Uuid(*b"0123456789abcdef")));

        let mut both = ALL_HEADERS_TYPES.to_vec();
        both.extend(ERROR_EVENT_MESSAGE);
        let msgs = decode(&both).unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[1].is_error());
        assert_eq!(msgs[1].header_str(":error-code"), Some("code"));
        // INT8_HEADER: a signed byte.
        let int8 =
            decode(b"\x00\x00\x00\x17\x00\x00\x00\x07)\x86\x01X\x04byte\x02\xff\xc2\xf8i\xdc")
                .unwrap();
        assert_eq!(int8[0].headers[0].1, HeaderValue::Int(-1));
    }

    #[test]
    fn rejects_botocore_negative_cases() {
        for (data, want) in [
            (
                &b"\x00\x00\x00=\xff\x00\x01\x02\x07\xfd\x83\x96\x0ccontent-type\x07\x00\x10application/json{'foo':'bar'}\x8d\x9c\x08\xb1"[..],
                "Checksum mismatch",
            ),
            (
                &b"\x00\x00\x00\x1d\x00\x00\x00\x00\xfdR\x8cZ{'foo':'bar'\x8d\xc3e96"[..],
                "Checksum mismatch",
            ),
            (
                &b"\x00\x00\x00\x24\x00\x00\x00\x14\x4b\xb9\x82\xd0\x04test\x04asdf\x04test\x04asdf\xf3\xf4\x75\x63"[..],
                "Duplicate header",
            ),
            (
                &b"\x00\x00\x00\x3d\xff\x00\x01\x02\x15\x83\xf5\xc2\x0ccontent-type\x07\x00\x10application/json{'foo':'bar'}\x2f\x37\x7f\x5d"[..],
                "Header length",
            ),
        ] {
            let e = decode(data).unwrap_err();
            assert!(e.0.contains(want), "{e:?}");
        }
    }

    #[test]
    fn encode_round_trips() {
        let msg = encode(
            &[
                (":event-type", HeaderValue::Str("Records".into())),
                ("n", HeaderValue::Int(-7)),
            ],
            b"payload",
        );
        let back = decode(&msg).unwrap();
        assert_eq!(back[0].header_str(":event-type"), Some("Records"));
        assert_eq!(back[0].header("n"), Some(&HeaderValue::Int(-7)));
        assert_eq!(back[0].payload, b"payload");
    }
}
