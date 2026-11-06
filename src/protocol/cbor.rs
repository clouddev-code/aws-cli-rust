//! CBOR for `smithy-rpc-v2-cbor` (botocore `CBORSerializer` /
//! `BaseCBORParser`).
//!
//! Decoding produces a `serde_json::Value` so the JSON shape walker can be
//! reused: byte strings become base64 text (what a JSON blob looks like),
//! tag-1 timestamps become epoch numbers and non-finite floats become the
//! `"NaN"` / `"Infinity"` / `"-Infinity"` strings JSON uses.

use super::ProtocolError;
use crate::model::Shape;
use crate::timestamp::{self, Wire};
use crate::value::Value;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value as J;

type Result<T> = std::result::Result<T, ProtocolError>;

const UINT: u8 = 0;
const NEGINT: u8 = 1;
const BYTES: u8 = 2;
const TEXT: u8 = 3;
const ARRAY: u8 = 4;
const MAP: u8 = 5;
const TAG: u8 = 6;
const NULL: u8 = 0xf6;

// ------------------------------------------------------------------ encode

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    if n < 24 {
        out.push(m | n as u8);
    } else if n < 0x100 {
        out.extend([m | 24, n as u8]);
    } else if n < 0x1_0000 {
        out.push(m | 25);
        out.extend((n as u16).to_be_bytes());
    } else if n < 0x1_0000_0000 {
        out.push(m | 26);
        out.extend((n as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend(n.to_be_bytes());
    }
}

fn int(out: &mut Vec<u8>, i: i64) {
    if i >= 0 {
        head(out, UINT, i as u64);
    } else {
        head(out, NEGINT, (-1 - i) as u64);
    }
}

fn text(out: &mut Vec<u8>, s: &str) {
    head(out, TEXT, s.len() as u64);
    out.extend(s.as_bytes());
}

/// Non-finite values are written as half-precision floats.
fn float(out: &mut Vec<u8>, f: f64, double: bool) {
    let special: Option<u16> = if f.is_nan() {
        Some(0x7e00)
    } else if f == f64::INFINITY {
        Some(0x7c00)
    } else if f == f64::NEG_INFINITY {
        Some(0xfc00)
    } else {
        None
    };
    match special {
        Some(h) => {
            out.push(0xf9);
            out.extend(h.to_be_bytes());
        }
        None if double => {
            out.push(0xfb);
            out.extend(f.to_be_bytes());
        }
        None => {
            out.push(0xfa);
            out.extend((f as f32).to_be_bytes());
        }
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        Value::Int(i) => Some(*i as f64),
        Value::Str(s) => match s.as_str() {
            "NaN" => Some(f64::NAN),
            "Infinity" => Some(f64::INFINITY),
            "-Infinity" => Some(f64::NEG_INFINITY),
            other => other.parse().ok(),
        },
        _ => None,
    }
}

/// Untyped values (documents, or a shape mismatch the JSON path would also
/// pass through).
fn encode_plain(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(NULL),
        Value::Bool(b) => out.push(if *b { 0xf5 } else { 0xf4 }),
        Value::Int(i) => int(out, *i),
        Value::Float(f) => float(out, *f, true),
        Value::Str(s) => text(out, s),
        Value::Bytes(b) => {
            head(out, BYTES, b.len() as u64);
            out.extend(b);
        }
        Value::Timestamp(t) => {
            head(out, TAG, 1);
            float(out, t.timestamp_micros() as f64 / 1e6, true);
        }
        Value::List(items) => {
            head(out, ARRAY, items.len() as u64);
            items.iter().for_each(|i| encode_plain(i, out));
        }
        Value::Map(m) => {
            head(out, MAP, m.len() as u64);
            for (k, v) in m {
                text(out, k);
                encode_plain(v, out);
            }
        }
    }
}

/// Serialize `v` as `shape` (`_serialize_data_item`).
pub fn encode(v: &Value, shape: &Shape, out: &mut Vec<u8>) -> Result<()> {
    match shape.type_name() {
        "structure" if shape.is_document() => encode_plain(v, out),
        "structure" => {
            let members: Vec<_> = v
                .as_map()
                .map(|m| m.iter().filter(|(_, v)| !v.is_null()).collect())
                .unwrap_or_default();
            head(out, MAP, members.len() as u64);
            for (key, val) in members {
                let m = shape.member_named(key).ok_or_else(|| {
                    ProtocolError(format!("unknown parameter {key} for shape {}", shape.name))
                })?;
                text(out, m.ser_name().unwrap_or(key));
                encode(val, &m, out)?;
            }
        }
        "map" => {
            let vs = shape
                .value()
                .ok_or_else(|| ProtocolError("map without value".into()))?;
            let entries: Vec<_> = v.as_map().map(|m| m.iter().collect()).unwrap_or_default();
            head(out, MAP, entries.len() as u64);
            for (k, val) in entries {
                text(out, k);
                if val.is_null() {
                    out.push(NULL);
                } else {
                    encode(val, &vs, out)?;
                }
            }
        }
        "list" => {
            let m = shape
                .member()
                .ok_or_else(|| ProtocolError("list without member".into()))?;
            let items = v.as_list().unwrap_or(&[]);
            head(out, ARRAY, items.len() as u64);
            for i in items {
                if i.is_null() {
                    out.push(NULL);
                } else {
                    encode(i, &m, out)?;
                }
            }
        }
        "string" => match v {
            Value::Str(s) => text(out, s),
            other => encode_plain(other, out),
        },
        "blob" => {
            let bytes = match v {
                Value::Bytes(b) => b.as_slice(),
                Value::Str(s) => s.as_bytes(),
                other => return Err(ProtocolError(format!("invalid blob: {other:?}"))),
            };
            head(out, BYTES, bytes.len() as u64);
            out.extend(bytes);
        }
        "timestamp" => {
            let t = timestamp::from_value(v)
                .ok_or_else(|| ProtocolError(format!("invalid timestamp: {v:?}")))?;
            head(out, TAG, 1);
            match timestamp::format(&t, "unixtimestamp") {
                Wire::Int(i) => int(out, i),
                Wire::Float(f) => float(out, f, true),
                Wire::Text(s) => text(out, &s),
            }
        }
        "float" | "double" => {
            let f = as_f64(v).ok_or_else(|| ProtocolError(format!("invalid number: {v:?}")))?;
            float(out, f, shape.type_name() == "double");
        }
        "integer" | "long" | "short" | "byte" => match v {
            Value::Int(i) => int(out, *i),
            Value::Str(s) => int(
                out,
                s.parse()
                    .map_err(|_| ProtocolError(format!("invalid integer: {s}")))?,
            ),
            other => encode_plain(other, out),
        },
        _ => encode_plain(v, out),
    }
    Ok(())
}

// ------------------------------------------------------------------ decode

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    depth: usize,
}

fn eof() -> ProtocolError {
    ProtocolError(
        "End of stream reached; this indicates a malformed CBOR response from the server".into(),
    )
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        let end = self.pos.checked_add(n).ok_or_else(eof)?;
        let s = self.buf.get(self.pos..end).ok_or_else(eof)?;
        self.pos = end;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Consume the break code if it is next.
    fn at_break(&mut self) -> bool {
        if self.buf.get(self.pos) == Some(&0xff) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn uint(&mut self, info: u8) -> Result<u64> {
        Ok(match info {
            0..24 => info as u64,
            24 => self.byte()? as u64,
            25 => u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into().unwrap()),
            _ => {
                return Err(ProtocolError(format!(
                    "Invalid CBOR integer: unparsable additional info {info}"
                )));
            }
        })
    }

    fn len(&mut self, info: u8) -> Result<usize> {
        usize::try_from(self.uint(info)?).map_err(|_| eof())
    }

    fn bytes(&mut self, info: u8) -> Result<Vec<u8>> {
        if info != 31 {
            let n = self.len(info)?;
            return Ok(self.take(n)?.to_vec());
        }
        let mut out = Vec::new();
        while !self.at_break() {
            let info = self.byte()? & 0x1f;
            let n = self.len(info)?;
            out.extend(self.take(n)?);
        }
        Ok(out)
    }

    fn item(&mut self) -> Result<J> {
        self.depth += 1;
        if self.depth > super::MAX_DEPTH {
            return Err(ProtocolError(
                "CBOR response nested too deeply; this indicates a malformed CBOR response from the server".into(),
            ));
        }
        let v = self.data_item();
        self.depth -= 1;
        v
    }

    fn data_item(&mut self) -> Result<J> {
        let initial = self.byte()?;
        let (major, info) = (initial >> 5, initial & 0x1f);
        Ok(match major {
            UINT => J::from(self.uint(info)?),
            NEGINT => {
                let n = self.uint(info)? as i128;
                let v = -1 - n;
                i64::try_from(v).map_or_else(|_| float_json(v as f64), J::from)
            }
            BYTES => J::String(B64.encode(self.bytes(info)?)),
            TEXT => J::String(
                String::from_utf8(self.bytes(info)?)
                    .map_err(|_| ProtocolError("invalid UTF-8 in CBOR text".into()))?,
            ),
            ARRAY => {
                let mut items = Vec::new();
                if info == 31 {
                    while !self.at_break() {
                        items.push(self.item()?);
                    }
                } else {
                    for _ in 0..self.len(info)? {
                        items.push(self.item()?);
                    }
                }
                J::Array(items)
            }
            MAP => {
                let mut obj = serde_json::Map::new();
                let mut pair = |r: &mut Self| -> Result<()> {
                    let k = match r.item()? {
                        J::String(s) => s,
                        other => other.to_string(),
                    };
                    obj.insert(k, r.item()?);
                    Ok(())
                };
                if info == 31 {
                    while !self.at_break() {
                        pair(self)?;
                    }
                } else {
                    for _ in 0..self.len(info)? {
                        pair(self)?;
                    }
                }
                J::Object(obj)
            }
            TAG => {
                let tag = self.uint(info)?;
                let v = self.item()?;
                if tag != 1 {
                    return Err(ProtocolError(format!(
                        "Found CBOR tag not supported by botocore: {tag}"
                    )));
                }
                if !v.is_number() {
                    return Err(ProtocolError(format!(
                        "Unable to parse datetime value: {v}"
                    )));
                }
                v
            }
            _ => match info {
                20 => J::Bool(false),
                21 => J::Bool(true),
                22 | 23 => J::Null,
                25 => float_json(half(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))),
                26 => float_json(f32::from_be_bytes(self.take(4)?.try_into().unwrap()) as f64),
                27 => float_json(f64::from_be_bytes(self.take(8)?.try_into().unwrap())),
                _ => {
                    return Err(ProtocolError(format!(
                        "Invalid additional info found for major type 7: {info}"
                    )));
                }
            },
        })
    }
}

/// IEEE 754 half precision to f64.
fn half(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = (h >> 10) & 0x1f;
    let frac = (h & 0x3ff) as f64;
    sign * match exp {
        0 => frac * 2f64.powi(-24),
        0x1f if frac == 0.0 => f64::INFINITY,
        0x1f => f64::NAN,
        e => (1.0 + frac / 1024.0) * 2f64.powi(e as i32 - 15),
    }
}

fn float_json(f: f64) -> J {
    if f.is_nan() {
        J::String("NaN".into())
    } else if f == f64::INFINITY {
        J::String("Infinity".into())
    } else if f == f64::NEG_INFINITY {
        J::String("-Infinity".into())
    } else {
        serde_json::Number::from_f64(f).map_or(J::Null, J::Number)
    }
}

/// Decode one data item. An empty body decodes to `null`.
pub fn decode(buf: &[u8]) -> Result<J> {
    if buf.is_empty() {
        return Ok(J::Null);
    }
    Reader {
        buf,
        pos: 0,
        depth: 0,
    }
    .item()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_indefinite_and_half_floats() {
        // {_ "a": [_ 1, -2], "b": h'0102', "c": 1.5 (half), "d": NaN (half)}
        let buf = [
            0xbf, 0x61, b'a', 0x9f, 0x01, 0x21, 0xff, 0x61, b'b', 0x42, 1, 2, 0x61, b'c', 0xf9,
            0x3e, 0x00, 0x61, b'd', 0xf9, 0x7e, 0x00, 0xff,
        ];
        let v = decode(&buf).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"a": [1, -2], "b": "AQI=", "c": 1.5, "d": "NaN"})
        );
    }

    #[test]
    fn plain_round_trip() {
        let mut m = crate::value::Map::new();
        m.insert("n".into(), Value::Int(-300));
        m.insert("big".into(), Value::Int(1 << 40));
        m.insert("s".into(), Value::Str("hé".into()));
        m.insert(
            "l".into(),
            Value::List(vec![Value::Bool(true), Value::Null]),
        );
        let mut out = Vec::new();
        encode_plain(&Value::Map(m), &mut out);
        assert_eq!(
            decode(&out).unwrap(),
            serde_json::json!({"n": -300, "big": 1u64 << 40, "s": "hé", "l": [true, null]})
        );
    }

    #[test]
    fn rejects_deep_nesting() {
        // [[[...1...]]] at the limit decodes; a 1 MB body of 0x81 must error, not overflow.
        let mut ok = vec![0x81; crate::protocol::MAX_DEPTH - 1];
        ok.push(0x01);
        assert!(decode(&ok).is_ok());
        let deep = vec![0x81; 1 << 20];
        assert!(decode(&deep).unwrap_err().0.contains("nested too deeply"));
    }
}
