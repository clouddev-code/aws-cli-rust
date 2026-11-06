//! Timestamp parsing and wire formatting (botocore `utils.parse_timestamp`
//! and the `Serializer._timestamp_*` helpers).

use crate::value::Value;
use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Timelike, Utc};

/// Wire representation of a serialized timestamp.
#[derive(Debug, Clone, PartialEq)]
pub enum Wire {
    Text(String),
    Int(i64),
    Float(f64),
}

impl Wire {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Wire::Text(s) => s.clone().into(),
            Wire::Int(i) => (*i).into(),
            Wire::Float(f) => crate::value::float_to_json(*f),
        }
    }
}

impl std::fmt::Display for Wire {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Wire::Text(s) => f.write_str(s),
            Wire::Int(i) => write!(f, "{i}"),
            Wire::Float(v) => f.write_str(&crate::util::py_float(*v)),
        }
    }
}

/// Parse any timestamp-ish input: epoch number, epoch string, ISO 8601
/// (full or date only) or RFC 822. Values without a zone are UTC.
pub fn parse(input: &str) -> Option<DateTime<Utc>> {
    let s = input.trim();
    if let Ok(f) = s.parse::<f64>() {
        return from_epoch(f);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(dt) = DateTime::parse_from_rfc2822(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // RFC 822 with a literal "GMT"/"UTC" zone, which chrono's rfc2822 parser
    // handles, but some services also send a trailing "Z" or omit the weekday.
    for fmt in ["%a, %d %b %Y %H:%M:%S GMT", "%d %b %Y %H:%M:%S GMT"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Utc.from_utc_datetime(&n));
        }
    }
    // JSONFileCache writes `%Y-%m-%dT%H:%M:%S%Z`, i.e. a literal "UTC" suffix.
    let trimmed = s
        .strip_suffix('Z')
        .or_else(|| s.strip_suffix("UTC"))
        .unwrap_or(s);
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y%m%dT%H%M%S",
    ] {
        if let Ok(n) = NaiveDateTime::parse_from_str(trimmed, fmt) {
            return Some(Utc.from_utc_datetime(&n));
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
        return Some(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?));
    }
    None
}

pub fn from_epoch(secs: f64) -> Option<DateTime<Utc>> {
    let whole = secs.floor();
    let micros = ((secs - whole) * 1_000_000.0).round() as u32;
    let (whole, micros) = if micros >= 1_000_000 {
        (whole + 1.0, 0)
    } else {
        (whole, micros)
    };
    DateTime::from_timestamp(whole as i64, micros * 1000)
}

/// Normalize a parameter value into a UTC datetime (`parse_to_aware_datetime`).
pub fn from_value(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::Timestamp(t) => Some(*t),
        Value::Int(i) => DateTime::from_timestamp(*i, 0),
        Value::Float(f) => from_epoch(*f),
        Value::Str(s) => parse(s),
        _ => None,
    }
}

/// Serialize using one of the model timestamp formats.
pub fn format(t: &DateTime<Utc>, format: &str) -> Wire {
    match format.to_ascii_lowercase().as_str() {
        "unixtimestamp" => {
            let micros = t.nanosecond() / 1000;
            if micros > 0 {
                Wire::Float((t.timestamp() * 1_000_000 + micros as i64) as f64 / 1_000_000.0)
            } else {
                Wire::Int(t.timestamp())
            }
        }
        // RFC 822 never carries sub-second precision.
        "rfc822" => Wire::Text(t.format("%a, %d %b %Y %H:%M:%S GMT").to_string()),
        _ => {
            if t.nanosecond() / 1000 > 0 {
                Wire::Text(t.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string())
            } else {
                Wire::Text(t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            }
        }
    }
}

pub fn epoch_seconds(t: &DateTime<Utc>) -> f64 {
    t.timestamp() as f64 + (t.nanosecond() / 1000) as f64 / 1_000_000.0
}

/// Mirrors Python's `datetime.isoformat()` for an aware UTC datetime, which
/// is what the CLI prints by default: `2015-08-30T12:36:00+00:00`, or with a
/// six digit fraction when microseconds are present.
pub fn python_isoformat(t: &DateTime<Utc>) -> String {
    let base = if t.nanosecond() / 1000 > 0 {
        t.to_rfc3339_opts(SecondsFormat::Micros, false)
    } else {
        t.to_rfc3339_opts(SecondsFormat::Secs, false)
    };
    base.replace("Z", "+00:00")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_inputs() {
        let t = parse("2015-01-25T08:00:00Z").unwrap();
        assert_eq!(t.timestamp(), 1422172800);
        assert_eq!(parse("1422172800").unwrap(), t);
        assert_eq!(parse("Sun, 25 Jan 2015 08:00:00 GMT").unwrap(), t);
        assert_eq!(parse("2015-01-25").unwrap().timestamp(), 1422144000);
        assert_eq!(parse("2015-01-25T08:00:00UTC").unwrap(), t);
        assert_eq!(
            parse("2015-01-25T08:00:00.123Z").unwrap().nanosecond(),
            123_000_000
        );
    }

    #[test]
    fn formats_like_botocore() {
        let t = parse("2015-01-25T08:00:00Z").unwrap();
        assert_eq!(
            format(&t, "iso8601"),
            Wire::Text("2015-01-25T08:00:00Z".into())
        );
        assert_eq!(format(&t, "unixTimestamp"), Wire::Int(1422172800));
        assert_eq!(
            format(&t, "rfc822"),
            Wire::Text("Sun, 25 Jan 2015 08:00:00 GMT".into())
        );
        let m = parse("2015-01-25T08:00:00.123Z").unwrap();
        assert_eq!(
            format(&m, "iso8601"),
            Wire::Text("2015-01-25T08:00:00.123000Z".into())
        );
        assert_eq!(format(&m, "unixtimestamp"), Wire::Float(1422172800.123));
        assert_eq!(python_isoformat(&t), "2015-01-25T08:00:00+00:00");
        assert_eq!(python_isoformat(&m), "2015-01-25T08:00:00.123000+00:00");
    }
}
