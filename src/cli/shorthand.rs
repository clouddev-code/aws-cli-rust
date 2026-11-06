//! Shorthand syntax parser (awscli `shorthand.py`).
//!
//! `Name=foo,Values=a,b` -> `{"Name": "foo", "Values": ["a", "b"]}`. The
//! parser is model-independent; [`back_compat`] then coerces scalars and
//! wraps single values into lists using the model, like `BackCompatVisitor`.

use crate::model::Shape;
use crate::value::{Map, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct ShorthandError(pub String);

impl std::fmt::Display for ShorthandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type Result<T> = std::result::Result<T, ShorthandError>;

pub fn parse(input: &str) -> Result<Value> {
    let mut p = Parser {
        s: input.chars().collect(),
        i: 0,
        input,
        resolve_files: false,
    };
    p.parameter()
}

// Character classes from shorthand.py (`_START_WORD`, `_FIRST_FOLLOW_CHARS`,
// `_SECOND_FOLLOW_CHARS`), written out as predicates.
fn start_word(c: char) -> bool {
    matches!(c, '!' | '#'..='&' | '('..='+' | '-'..='<' | '>'..='Z' | '\\'..='z') || c >= '|'
}

fn first_follow(c: char) -> bool {
    c.is_whitespace()
        || matches!(c, '!' | '#'..='&' | '('..='+' | '-'..='\\' | '^'..='|')
        || c >= '~'
}

fn second_follow(c: char) -> bool {
    c.is_whitespace() || matches!(c, '!' | '#'..='&' | '('..='+' | '-'..='<') || c >= '>'
}

struct Parser<'a> {
    s: Vec<char>,
    i: usize,
    input: &'a str,
    /// Set by `key@=value`: `file://` / `fileb://` values are loaded.
    resolve_files: bool,
}

/// `_resolve_paramfiles`: only `file://` and `fileb://` are honoured.
fn resolve_paramfile(v: String) -> Result<Value> {
    let (path, binary) = if let Some(p) = v.strip_prefix("fileb://") {
        (p, true)
    } else if let Some(p) = v.strip_prefix("file://") {
        (p, false)
    } else {
        return Ok(Value::Str(v));
    };
    let bytes = std::fs::read(path)
        .map_err(|e| ShorthandError(format!("Unable to load paramfile {v}: {e}")))?;
    Ok(if binary {
        Value::Bytes(bytes)
    } else {
        Value::Str(
            String::from_utf8(bytes)
                .map_err(|e| ShorthandError(format!("Unable to load paramfile {v}: {e}")))?,
        )
    })
}

impl Parser<'_> {
    fn eof(&self) -> bool {
        self.i >= self.s.len()
    }

    fn cur(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn err(&self, expected: &str) -> ShorthandError {
        let actual = self.cur().map_or("EOF".to_string(), |c| c.to_string());
        ShorthandError(format!(
            "Expected: '{expected}', received: '{actual}' for input:\n{}\n{}^",
            self.input,
            " ".repeat(self.i)
        ))
    }

    fn ws(&mut self) {
        while self.cur().is_some_and(char::is_whitespace) {
            self.i += 1;
        }
    }

    fn expect(&mut self, c: char, consume_ws: bool) -> Result<()> {
        if consume_ws {
            self.ws();
        }
        if self.cur() != Some(c) {
            return Err(self.err(&c.to_string()));
        }
        self.i += 1;
        if consume_ws {
            self.ws();
        }
        Ok(())
    }

    fn parameter(&mut self) -> Result<Value> {
        let mut params = Map::new();
        let (k, v) = self.keyval()?;
        params.insert(k, v);
        while !self.eof() {
            self.expect(',', true)?;
            let (k, v) = self.keyval()?;
            if params.contains_key(&k) {
                return Err(ShorthandError(format!(
                    "Second instance of key \"{k}\" encountered for input:\n{}",
                    self.input
                )));
            }
            params.insert(k, v);
        }
        Ok(Value::Map(params))
    }

    fn keyval(&mut self) -> Result<(String, Value)> {
        let key = self.key();
        self.at_sign();
        self.expect('=', true)?;
        Ok((key, self.values()?))
    }

    /// Optional `@` before `=`.
    fn at_sign(&mut self) {
        let save = self.i;
        self.ws();
        if self.cur() == Some('@') {
            self.i += 1;
            self.resolve_files = true;
        } else {
            self.i = save;
            self.resolve_files = false;
        }
    }

    fn finish(&self, v: String) -> Result<Value> {
        if self.resolve_files {
            resolve_paramfile(v)
        } else {
            Ok(Value::Str(v))
        }
    }

    fn key(&mut self) -> String {
        let start = self.i;
        while self
            .cur()
            .is_some_and(|c| c.is_ascii_alphanumeric() || "-_.#/:".contains(c))
        {
            self.i += 1;
        }
        self.s[start..self.i].iter().collect()
    }

    fn values(&mut self) -> Result<Value> {
        match self.cur() {
            None => Ok(Value::Str(String::new())),
            Some('[') => self.explicit_list(),
            Some('{') => self.hash_literal(),
            _ => self.csv_value(),
        }
    }

    fn csv_value(&mut self) -> Result<Value> {
        let first = self.first_value()?;
        self.ws();
        if self.cur() != Some(',') {
            return Ok(first);
        }
        self.expect(',', true)?;
        let mut list = vec![first.clone()];
        loop {
            let before = self.i;
            match self.second_value() {
                Ok(v) => {
                    self.ws();
                    if self.eof() {
                        list.push(v);
                        break;
                    }
                    if self.expect(',', true).is_err() {
                        // `key=value` follows: rewind to the comma.
                        self.backtrack_to_comma(before);
                        break;
                    }
                    list.push(v);
                }
                Err(e) => {
                    if self.eof() {
                        return Err(e);
                    }
                    self.backtrack_to_comma(before);
                    break;
                }
            }
        }
        Ok(if list.len() == 1 {
            first
        } else {
            Value::List(list)
        })
    }

    fn backtrack_to_comma(&mut self, from: usize) {
        self.i = from;
        while self.i > 0 && self.s[self.i - 1] != ',' {
            self.i -= 1;
        }
        self.i = self.i.saturating_sub(1);
    }

    /// Consume `(\\,|start)(\\,|follow)*`, unescaping `\,`.
    fn word(&mut self, follow: fn(char) -> bool) -> Option<String> {
        let start = self.i;
        let mut j = self.i;
        let escaped = |j: usize| self.s.get(j) == Some(&'\\') && self.s.get(j + 1) == Some(&',');
        if escaped(j) {
            j += 2;
        } else if self.s.get(j).is_some_and(|&c| start_word(c)) {
            j += 1;
        } else {
            return None;
        }
        loop {
            if escaped(j) {
                j += 2;
            } else if self.s.get(j).is_some_and(|&c| follow(c)) {
                j += 1;
            } else {
                break;
            }
        }
        self.i = j;
        let raw: String = self.s[start..j].iter().collect();
        Some(raw.replace("\\,", ",").trim_end().to_string())
    }

    fn first_value(&mut self) -> Result<Value> {
        match self.cur() {
            Some('\'') => self.quoted('\''),
            Some('"') => self.quoted('"'),
            _ => {
                let w = self.word(first_follow).unwrap_or_default();
                self.finish(w)
            }
        }
    }

    fn second_value(&mut self) -> Result<Value> {
        match self.cur() {
            Some('\'') => self.quoted('\''),
            Some('"') => self.quoted('"'),
            _ => {
                let w = self
                    .word(second_follow)
                    .ok_or_else(|| self.err("<second>"))?;
                self.finish(w)
            }
        }
    }

    fn quoted(&mut self, q: char) -> Result<Value> {
        let start = self.i;
        self.i += 1;
        let mut out = String::new();
        loop {
            match self.cur() {
                None => {
                    self.i = start;
                    return Err(self.err(&format!(
                        "<{} quoted>",
                        if q == '"' { "double" } else { "singled" }
                    )));
                }
                Some('\\') if self.s.get(self.i + 1) == Some(&q) => {
                    out.push('\\');
                    out.push(q);
                    self.i += 2;
                }
                Some(c) if c == q => {
                    self.i += 1;
                    break;
                }
                Some(c) => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
        let out = out
            .replace(&format!("\\{q}"), &q.to_string())
            .replace("\\\\", "\\");
        self.finish(out)
    }

    fn explicit_list(&mut self) -> Result<Value> {
        self.expect('[', true)?;
        let mut values = Vec::new();
        while self.cur() != Some(']') {
            if self.eof() {
                return Err(self.err("]"));
            }
            values.push(self.explicit_values()?);
            self.ws();
            if self.cur() != Some(']') {
                self.expect(',', false)?;
                self.ws();
            }
        }
        self.expect(']', false)?;
        Ok(Value::List(values))
    }

    fn explicit_values(&mut self) -> Result<Value> {
        match self.cur() {
            Some('[') => self.explicit_list(),
            Some('{') => self.hash_literal(),
            _ => self.first_value(),
        }
    }

    fn hash_literal(&mut self) -> Result<Value> {
        self.expect('{', true)?;
        let mut map = Map::new();
        while self.cur() != Some('}') {
            if self.eof() {
                return Err(self.err("}"));
            }
            let key = self.key();
            self.at_sign();
            self.expect('=', true)?;
            let v = self.explicit_values()?;
            self.ws();
            if self.cur() != Some('}') {
                self.expect(',', false)?;
                self.ws();
            }
            map.insert(key, v);
        }
        self.expect('}', false)?;
        Ok(Value::Map(map))
    }
}

/// `BackCompatVisitor`: coerce shorthand strings using the model.
pub fn back_compat(value: &mut Value, shape: &Shape) -> Result<()> {
    match shape.type_name() {
        "structure" => {
            if shape.is_document() {
                return Err(ShorthandError(
                    "Shorthand syntax does not support document types. Use JSON input for top-level argument to specify nested parameter".into(),
                ));
            }
            if let Value::Map(m) = value {
                for (name, member) in shape.members() {
                    if let Some(v) = m.get_mut(name) {
                        back_compat(v, &member)?;
                    }
                }
            }
        }
        "list" => {
            if !matches!(value, Value::List(_)) && !value.is_null() {
                *value = Value::List(vec![std::mem::replace(value, Value::Null)]);
            }
            if let (Value::List(items), Some(member)) = (value, shape.member()) {
                for item in items {
                    back_compat(item, &member)?;
                }
            }
        }
        "map" => {
            if let (Value::Map(m), Some(vs)) = (value, shape.value()) {
                for v in m.values_mut() {
                    back_compat(v, &vs)?;
                }
            }
        }
        t => {
            if let Value::Str(s) = value {
                let invalid =
                    |what: &str| ShorthandError(format!("invalid literal for {what}: '{s}'"));
                *value = match t {
                    "integer" | "long" => Value::Int(
                        s.trim()
                            .parse()
                            .map_err(|_| invalid("int() with base 10"))?,
                    ),
                    "float" | "double" => {
                        Value::Float(s.trim().parse().map_err(|_| invalid("float()"))?)
                    }
                    "boolean" if s.eq_ignore_ascii_case("true") => Value::Bool(true),
                    "boolean" if s.eq_ignore_ascii_case("false") => Value::Bool(false),
                    _ => return Ok(()),
                };
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(s: &str) -> serde_json::Value {
        parse(s)
            .unwrap()
            .to_json(crate::value::TimestampStyle::Epoch)
    }

    #[test]
    fn cases_from_awscli_tests() {
        // Selected from awscli tests/unit/test_shorthand.py.
        assert_eq!(p("foo=bar"), json!({"foo": "bar"}));
        assert_eq!(p("foo=bar,baz=qux"), json!({"foo": "bar", "baz": "qux"}));
        assert_eq!(p("a=b,c"), json!({"a": ["b", "c"]}));
        assert_eq!(p("a=b,c,d=e"), json!({"a": ["b", "c"], "d": "e"}));
        assert_eq!(
            p("Name=foo,Values=a,b"),
            json!({"Name": "foo", "Values": ["a", "b"]})
        );
        assert_eq!(p("foo=[a,b,c]"), json!({"foo": ["a", "b", "c"]}));
        assert_eq!(p("foo=[]"), json!({"foo": []}));
        assert_eq!(p("foo={a=b,c=d}"), json!({"foo": {"a": "b", "c": "d"}}));
        assert_eq!(
            p("foo=[{a=b},{c=d}]"),
            json!({"foo": [{"a": "b"}, {"c": "d"}]})
        );
        assert_eq!(p("foo='bar,baz'"), json!({"foo": "bar,baz"}));
        assert_eq!(p(r#"foo="bar baz""#), json!({"foo": "bar baz"}));
        assert_eq!(p(r"foo=a\,b"), json!({"foo": "a,b"}));
        assert_eq!(p("foo=bar=baz"), json!({"foo": "bar=baz"}));
        assert_eq!(p("foo="), json!({"foo": ""}));
        assert_eq!(p("foo=a b,c"), json!({"foo": ["a b", "c"]}));
        assert!(parse("foo=bar,foo=baz").is_err());
        assert!(parse("foo").is_err());
    }
}
