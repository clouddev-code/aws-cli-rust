//! YAML output (`--output yaml` / `yaml-stream`).
//!
//! awscli dumps with ruamel.yaml `YAML(typ='safe')` and block style, which
//! goes through libyaml's emitter (ruamel.yaml.clib). This is a port of the
//! parts of that emitter a JSON-like document needs: block mappings and
//! sequences, `[]` / `{}` for empty collections, scalar style selection
//! (plain, single- or double-quoted) and line folding at 80 columns. Plain
//! scalars are only used when the YAML 1.2 resolver of ruamel.yaml would read
//! them back as the same type. Mapping keys are sorted, as the safe
//! representer does.

use regex::Regex;
use serde_json::Value as J;
use std::sync::LazyLock;

const BEST_WIDTH: usize = 80;
const BEST_INDENT: usize = 2;

/// ruamel.yaml's YAML 1.2 implicit resolvers (`resolver.implicit_resolvers`),
/// keyed by the characters a match can start with.
static RESOLVERS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    let r = |p: &str| Regex::new(&format!("(?x)^(?:{p})$")).unwrap();
    vec![
        ("tTfF", r("true|True|TRUE|false|False|FALSE")),
        (
            "-+0123456789.",
            r(r"[-+]?(?:[0-9][0-9_]*)\.[0-9_]*(?:[eE][-+]?[0-9]+)?
              |[-+]?(?:[0-9][0-9_]*)(?:[eE][-+]?[0-9]+)
              |[-+]?\.[0-9_]+(?:[eE][-+][0-9]+)?
              |[-+]?\.(?:inf|Inf|INF)
              |\.(?:nan|NaN|NAN)"),
        ),
        (
            "-+0123456789",
            r(r"[-+]?0b[0-1_]+
              |[-+]?0o?[0-7_]+
              |[-+]?[0-9_]+
              |[-+]?0x[0-9a-fA-F_]+"),
        ),
        ("<", r("<<")),
        ("~nN", r("~|null|Null|NULL")),
        (
            "0123456789",
            r(r"[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]
              |[0-9][0-9][0-9][0-9]\x20?-[0-9][0-9]?\x20?-[0-9][0-9]?
               (?:[Tt]|[\x20\t]+)[0-9][0-9]?
               :[0-9][0-9]\x20?:[0-9][0-9]\x20?(?:\.[0-9]*)?
               (?:[\x20\t]*(?:Z|[-+][0-9][0-9]?(?::[0-9][0-9])?))?"),
        ),
        ("=", r("=")),
        ("!&*", r(r"!|&|\*")),
    ]
});

/// Whether a plain scalar with this text would resolve to something other
/// than a string.
fn resolves_to_non_string(s: &str) -> bool {
    let Some(first) = s.chars().next() else {
        // The null resolver matches the empty string.
        return true;
    };
    RESOLVERS
        .iter()
        .any(|(starts, re)| starts.contains(first) && re.is_match(s))
}

/// `yaml_emitter_analyze_scalar`
#[derive(Debug, Clone, Copy)]
struct Analysis {
    empty: bool,
    multiline: bool,
    allow_flow_plain: bool,
    allow_block_plain: bool,
    allow_single_quoted: bool,
}

fn is_break(c: char) -> bool {
    matches!(c, '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

fn is_blankz(c: char) -> bool {
    matches!(c, '\0' | ' ' | '\t' | '\r') || is_break(c)
}

/// libyaml's `IS_PRINTABLE` (characters beyond the BMP are not printable).
fn is_printable(c: char) -> bool {
    matches!(c, '\n' | '\x20'..='\x7e' | '\u{a0}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}')
        && c != '\u{feff}'
}

fn analyze(s: &str) -> Analysis {
    if s.is_empty() {
        return Analysis {
            empty: true,
            multiline: false,
            allow_flow_plain: false,
            allow_block_plain: true,
            allow_single_quoted: true,
        };
    }
    let chars: Vec<char> = s.chars().collect();
    let (mut block_ind, mut flow_ind) = (false, false);
    if s.starts_with("---") || s.starts_with("...") {
        block_ind = true;
        flow_ind = true;
    }
    let (mut line_breaks, mut special) = (false, false);
    let (mut leading_space, mut leading_break) = (false, false);
    let (mut trailing_space, mut trailing_break) = (false, false);
    let (mut break_space, mut space_break) = (false, false);
    let (mut previous_space, mut previous_break) = (false, false);
    let mut preceded_by_ws = true;
    let mut followed_by_ws = chars.len() == 1 || is_blankz(chars[1]);
    for (i, &c) in chars.iter().enumerate() {
        if i == 0 {
            if "#,[]{}&*!|>'\"%@`".contains(c) {
                flow_ind = true;
                block_ind = true;
            }
            if c == '?' || c == ':' {
                flow_ind = true;
                if followed_by_ws {
                    block_ind = true;
                }
            }
            if c == '-' && followed_by_ws {
                flow_ind = true;
                block_ind = true;
            }
        } else {
            if ",?[]{}".contains(c) {
                flow_ind = true;
            }
            if c == ':' {
                flow_ind = true;
                if followed_by_ws {
                    block_ind = true;
                }
            }
            if c == '#' && preceded_by_ws {
                flow_ind = true;
                block_ind = true;
            }
        }
        if !is_printable(c) {
            special = true;
        }
        if is_break(c) {
            line_breaks = true;
        }
        if c == ' ' {
            if i == 0 {
                leading_space = true;
            }
            if i == chars.len() - 1 {
                trailing_space = true;
            }
            if previous_break {
                break_space = true;
            }
            previous_space = true;
            previous_break = false;
        } else if is_break(c) {
            if i == 0 {
                leading_break = true;
            }
            if i == chars.len() - 1 {
                trailing_break = true;
            }
            if previous_space {
                space_break = true;
            }
            previous_space = false;
            previous_break = true;
        } else {
            previous_space = false;
            previous_break = false;
        }
        preceded_by_ws = is_blankz(c);
        followed_by_ws = i + 2 >= chars.len() || is_blankz(chars[i + 2]);
    }
    let mut a = Analysis {
        empty: false,
        multiline: line_breaks,
        allow_flow_plain: true,
        allow_block_plain: true,
        allow_single_quoted: true,
    };
    if leading_space || leading_break || trailing_space || trailing_break {
        a.allow_flow_plain = false;
        a.allow_block_plain = false;
    }
    if break_space {
        a.allow_flow_plain = false;
        a.allow_block_plain = false;
        a.allow_single_quoted = false;
    }
    if space_break || special {
        a.allow_flow_plain = false;
        a.allow_block_plain = false;
        a.allow_single_quoted = false;
    }
    if line_breaks {
        a.allow_flow_plain = false;
        a.allow_block_plain = false;
    }
    if flow_ind {
        a.allow_flow_plain = false;
    }
    if block_ind {
        a.allow_block_plain = false;
    }
    a
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Plain,
    Single,
    Double,
}

struct Emitter {
    out: String,
    indent: Option<usize>,
    indents: Vec<Option<usize>>,
    column: usize,
    whitespace: bool,
    indention: bool,
    flow_level: usize,
}

impl Emitter {
    fn new() -> Self {
        Emitter {
            out: String::new(),
            indent: None,
            indents: Vec::new(),
            column: 0,
            whitespace: true,
            indention: true,
            flow_level: 0,
        }
    }

    fn write(&mut self, s: &str) {
        self.column += s.chars().count();
        self.out.push_str(s);
    }

    fn increase_indent(&mut self, flow: bool, indentless: bool) {
        self.indents.push(self.indent);
        self.indent = Some(match self.indent {
            None if flow => BEST_INDENT,
            None => 0,
            Some(i) if !indentless => i + BEST_INDENT,
            Some(i) => i,
        });
    }

    fn pop_indent(&mut self) {
        self.indent = self.indents.pop().flatten();
    }

    fn write_line_break(&mut self, data: Option<char>) {
        self.out.push(data.unwrap_or('\n'));
        self.whitespace = true;
        self.indention = true;
        self.column = 0;
    }

    fn write_indent(&mut self) {
        let indent = self.indent.unwrap_or(0);
        if !self.indention || self.column > indent || (self.column == indent && !self.whitespace) {
            self.write_line_break(None);
        }
        if self.column < indent {
            self.whitespace = true;
            let pad = " ".repeat(indent - self.column);
            self.write(&pad);
        }
    }

    fn write_indicator(&mut self, indicator: &str, need_ws: bool, ws: bool, indention: bool) {
        if self.whitespace || !need_ws {
            self.write(indicator);
        } else {
            self.write(&format!(" {indicator}"));
        }
        self.whitespace = ws;
        self.indention = self.indention && indention;
    }

    /// `expect_node` for a value in block context.
    fn node(&mut self, v: &J, mapping_ctx: bool, simple_key: bool) {
        match v {
            J::Array(items) if !items.is_empty() && self.flow_level == 0 => {
                let indentless = mapping_ctx && !self.indention;
                self.increase_indent(false, indentless);
                for item in items {
                    self.write_indent();
                    self.write_indicator("-", true, false, true);
                    self.node(item, false, false);
                }
                self.pop_indent();
            }
            J::Object(map) if !map.is_empty() && self.flow_level == 0 => {
                self.increase_indent(false, false);
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for key in keys {
                    self.write_indent();
                    // libyaml's `yaml_emitter_check_simple_key` (byte length).
                    if key.len() <= 128 && !analyze(key).multiline {
                        self.scalar(key, true);
                        self.write_indicator(":", false, false, false);
                    } else {
                        // Complex key: `? key` / `: value`.
                        self.write_indicator("?", true, false, true);
                        self.scalar(key, false);
                        self.write_indent();
                        self.write_indicator(":", true, false, true);
                    }
                    self.node(&map[key], true, false);
                }
                self.pop_indent();
            }
            J::Array(_) => {
                self.write_indicator("[", true, true, false);
                self.write_indicator("]", false, false, false);
            }
            J::Object(_) => {
                self.write_indicator("{", true, true, false);
                self.write_indicator("}", false, false, false);
            }
            J::String(s) => self.scalar(s, simple_key),
            other => {
                // null / booleans / numbers: plain, as the safe representer
                // writes them.
                let text = match other {
                    J::Null => "null".to_string(),
                    J::Bool(b) => b.to_string(),
                    J::Number(n) if n.is_f64() => {
                        crate::util::py_float(n.as_f64().unwrap_or_default()).to_lowercase()
                    }
                    J::Number(n) => n.to_string(),
                    _ => unreachable!(),
                };
                self.increase_indent(true, false);
                self.write_plain(&text, true);
                self.pop_indent();
            }
        }
    }

    /// `expect_scalar` for a string, with the style chosen as
    /// `yaml_emitter_select_scalar_style` does.
    fn scalar(&mut self, s: &str, simple_key: bool) {
        self.increase_indent(true, false);
        let a = analyze(s);
        let plain_implicit = !resolves_to_non_string(s);
        let mut style = Style::Double;
        if plain_implicit
            && !(simple_key && (a.empty || a.multiline))
            && ((self.flow_level > 0 && a.allow_flow_plain)
                || (self.flow_level == 0 && a.allow_block_plain))
        {
            style = Style::Plain;
        } else if a.allow_single_quoted && !(simple_key && a.multiline) {
            style = Style::Single;
        }
        if style == Style::Plain && s.is_empty() && (self.flow_level > 0 || simple_key) {
            style = Style::Single;
        }
        let split = !simple_key;
        match style {
            Style::Plain => self.write_plain(s, split),
            Style::Single => self.write_single_quoted(s, split),
            Style::Double => self.write_double_quoted(s, split),
        }
        self.pop_indent();
    }

    fn write_plain(&mut self, text: &str, split: bool) {
        if text.is_empty() {
            return;
        }
        if !self.whitespace {
            self.write(" ");
        }
        self.whitespace = false;
        self.indention = false;
        let chars: Vec<char> = text.chars().collect();
        let (mut spaces, mut breaks) = (false, false);
        let mut start = 0;
        for end in 0..=chars.len() {
            let ch = chars.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end && self.column > BEST_WIDTH && split {
                        self.write_indent();
                        self.whitespace = false;
                        self.indention = false;
                    } else {
                        let data: String = chars[start..end].iter().collect();
                        self.write(&data);
                    }
                    start = end;
                }
            } else if breaks {
                if !ch.is_some_and(is_break) {
                    if chars[start] == '\n' {
                        self.write_line_break(None);
                    }
                    for &br in &chars[start..end] {
                        self.write_line_break((br != '\n').then_some(br));
                    }
                    self.write_indent();
                    self.whitespace = false;
                    self.indention = false;
                    start = end;
                }
            } else if ch.is_none_or(|c| c == ' ' || is_break(c)) {
                let data: String = chars[start..end].iter().collect();
                self.write(&data);
                start = end;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = is_break(c);
            }
        }
    }

    fn write_single_quoted(&mut self, text: &str, split: bool) {
        self.write_indicator("'", true, false, false);
        let chars: Vec<char> = text.chars().collect();
        let (mut spaces, mut breaks) = (false, false);
        let mut start = 0;
        for end in 0..=chars.len() {
            let ch = chars.get(end).copied();
            if spaces {
                if ch != Some(' ') {
                    if start + 1 == end
                        && self.column > BEST_WIDTH
                        && split
                        && start != 0
                        && end != chars.len()
                    {
                        self.write_indent();
                    } else {
                        let data: String = chars[start..end].iter().collect();
                        self.write(&data);
                    }
                    start = end;
                }
            } else if breaks {
                if !ch.is_some_and(is_break) {
                    if chars[start] == '\n' {
                        self.write_line_break(None);
                    }
                    for &br in &chars[start..end] {
                        self.write_line_break((br != '\n').then_some(br));
                    }
                    self.write_indent();
                    start = end;
                }
            } else if ch.is_none_or(|c| c == ' ' || c == '\'' || is_break(c)) && start < end {
                let data: String = chars[start..end].iter().collect();
                self.write(&data);
                start = end;
            }
            if ch == Some('\'') {
                self.write("''");
                start = end + 1;
            }
            if let Some(c) = ch {
                spaces = c == ' ';
                breaks = is_break(c);
            }
        }
        self.write_indicator("'", false, false, false);
    }

    /// libyaml's `yaml_emitter_write_double_quoted`: long lines are folded at
    /// a space (a following space is kept with a `\` escape).
    fn write_double_quoted(&mut self, text: &str, split: bool) {
        self.write_indicator("\"", true, false, false);
        let chars: Vec<char> = text.chars().collect();
        let mut spaces = false;
        for (i, &c) in chars.iter().enumerate() {
            if !is_printable(c) || c == '\u{feff}' || is_break(c) || c == '"' || c == '\\' {
                let data = match c {
                    '\0' => "\\0".to_string(),
                    '\x07' => "\\a".into(),
                    '\x08' => "\\b".into(),
                    '\t' => "\\t".into(),
                    '\n' => "\\n".into(),
                    '\x0b' => "\\v".into(),
                    '\x0c' => "\\f".into(),
                    '\r' => "\\r".into(),
                    '\x1b' => "\\e".into(),
                    '"' => "\\\"".into(),
                    '\\' => "\\\\".into(),
                    '\u{85}' => "\\N".into(),
                    '\u{a0}' => "\\_".into(),
                    '\u{2028}' => "\\L".into(),
                    '\u{2029}' => "\\P".into(),
                    c if (c as u32) <= 0xff => format!("\\x{:02X}", c as u32),
                    c if (c as u32) <= 0xffff => format!("\\u{:04X}", c as u32),
                    c => format!("\\U{:08X}", c as u32),
                };
                self.write(&data);
                spaces = false;
            } else if c == ' ' {
                if split && !spaces && self.column > BEST_WIDTH && i != 0 && i != chars.len() - 1 {
                    self.write_indent();
                    if chars.get(i + 1) == Some(&' ') {
                        self.write("\\");
                    }
                } else {
                    self.write(" ");
                }
                spaces = true;
            } else {
                self.write(&c.to_string());
                spaces = false;
            }
        }
        self.whitespace = false;
        self.indention = false;
        self.write_indicator("\"", false, false, false);
    }
}

/// A string as a YAML scalar on one line (plain if possible, else quoted),
/// as a mapping key (`key = true`) or value.
pub fn scalar(s: &str, key: bool) -> String {
    let mut e = Emitter::new();
    e.scalar(s, key);
    e.out
}

/// `YAMLDumper.dump`: scalars are written as JSON (awscli avoids the YAML
/// document end marker that way); everything else as a block YAML document.
pub fn dump(v: &J) -> String {
    match v {
        J::Array(_) | J::Object(_) => {
            let mut e = Emitter::new();
            e.node(v, false, false);
            // `expect_document_end`
            e.write_indent();
            e.out
        }
        J::Number(n) if n.is_f64() => {
            format!(
                "{}\n",
                crate::util::py_float(n.as_f64().unwrap_or_default())
            )
        }
        other => format!("{other}\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolver_quotes_ambiguous_strings() {
        for s in [
            "true",
            "123",
            "1.5",
            "1e3",
            "0x1F",
            "null",
            "~",
            "",
            "2015-08-30",
            "=",
        ] {
            assert!(resolves_to_non_string(s), "{s}");
        }
        for s in ["yes", "on", "No", "a=b", "1.2.3", "v1"] {
            assert!(!resolves_to_non_string(s), "{s}");
        }
    }

    #[test]
    fn block_layout() {
        assert_eq!(
            dump(&json!({"b": [{"x": 1, "y": []}], "a": "it's"})),
            "a: it's\nb:\n- x: 1\n  y: []\n"
        );
        assert_eq!(dump(&json!("s")), "\"s\"\n");
    }
}
