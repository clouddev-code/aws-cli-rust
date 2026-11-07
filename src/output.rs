//! Output formatting (awscli `formatter.py`, `text.py`) and `--query`.

use serde_json::Value as J;
use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Json,
    Text,
    Table,
    Yaml,
    YamlStream,
    Off,
}

/// `CLI_OUTPUT_FORMATS` names, in awscli's order.
pub const FORMAT_NAMES: &[&str] = &["json", "text", "table", "yaml", "yaml-stream", "off"];

impl OutputFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "json" => Some(OutputFormat::Json),
            "text" => Some(OutputFormat::Text),
            "table" => Some(OutputFormat::Table),
            "yaml" => Some(OutputFormat::Yaml),
            "yaml-stream" => Some(OutputFormat::YamlStream),
            "off" => Some(OutputFormat::Off),
            _ => None,
        }
    }
}

/// What the table formatter needs besides the data.
#[derive(Debug, Clone, Default)]
pub struct TableStyle {
    /// Section title of the whole table: the operation name.
    pub title: String,
    pub terminal_width: usize,
    /// `--color on`, or `auto` on a terminal (`ColorizedStyler`).
    pub color: bool,
}

pub fn apply_query(expr: &str, data: &J) -> Result<J, String> {
    let compiled =
        jmespath::compile(expr).map_err(|e| format!("Bad value for --query {expr}: {e}"))?;
    let var = jmespath::Variable::from_serializable(data).map_err(|e| e.to_string())?;
    let result = compiled.search(var).map_err(|e| e.to_string())?;
    serde_json::to_value(&*result).map_err(|e| e.to_string())
}

pub fn write(
    format: OutputFormat,
    data: &J,
    table: &TableStyle,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let empty = data == &J::Object(Default::default());
    match format {
        OutputFormat::Off => Ok(()),
        OutputFormat::Table => {
            let s = crate::table::render(&table.title, data, table.terminal_width, table.color)
                .map_err(std::io::Error::other)?;
            out.write_all(s.as_bytes())
        }
        // Nothing is printed for an empty response.
        OutputFormat::Yaml | OutputFormat::YamlStream if empty => Ok(()),
        OutputFormat::Yaml => out.write_all(crate::yaml::dump(data).as_bytes()),
        // Each response (page) becomes a one-element list.
        OutputFormat::YamlStream => {
            out.write_all(crate::yaml::dump(&J::Array(vec![data.clone()])).as_bytes())
        }
        OutputFormat::Json => {
            // The CLI prints nothing for an empty response.
            if empty {
                return Ok(());
            }
            let mut buf = Vec::new();
            let fmt = serde_json::ser::PrettyFormatter::with_indent(b"    ");
            let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
            serde::Serialize::serialize(data, &mut ser).map_err(std::io::Error::other)?;
            out.write_all(&buf)?;
            out.write_all(b"\n")
        }
        OutputFormat::Text => {
            let mut s = String::new();
            format_text(data, None, None, &mut s);
            out.write_all(s.as_bytes())
        }
    }
}

/// Python `str()` of a JSON scalar.
fn py_str(v: &J) -> String {
    match v {
        J::String(s) => s.clone(),
        J::Bool(true) => "True".into(),
        J::Bool(false) => "False".into(),
        J::Null => "None".into(),
        J::Number(n) if n.is_f64() => crate::util::py_float(n.as_f64().unwrap()),
        other => other.to_string(),
    }
}

fn is_container(v: &J) -> bool {
    v.is_object() || v.is_array()
}

fn format_text(
    item: &J,
    identifier: Option<&str>,
    scalar_keys: Option<&[String]>,
    out: &mut String,
) {
    match item {
        J::Object(map) => {
            let (mut scalars, non_scalars): (Vec<String>, Vec<(String, &J)>) = match scalar_keys {
                None => {
                    let mut keys: Vec<&String> = map.keys().collect();
                    keys.sort();
                    let mut sc = Vec::new();
                    let mut ns = Vec::new();
                    for k in keys {
                        let v = &map[k];
                        if is_container(v) {
                            ns.push((k.clone(), v));
                        } else {
                            sc.push(py_str(v));
                        }
                    }
                    (sc, ns)
                }
                Some(keys) => {
                    let sc = keys
                        .iter()
                        .map(|k| map.get(k).map(py_str).unwrap_or_default())
                        .collect();
                    let mut rest: Vec<&String> = map.keys().filter(|k| !keys.contains(k)).collect();
                    rest.sort();
                    (sc, rest.into_iter().map(|k| (k.clone(), &map[k])).collect())
                }
            };
            if !scalars.is_empty() {
                if let Some(id) = identifier {
                    scalars.insert(0, id.to_uppercase());
                }
                out.push_str(&scalars.join("\t"));
                out.push('\n');
            }
            for (k, v) in non_scalars {
                format_text(v, Some(&k), None, out);
            }
        }
        J::Array(list) => {
            if list.is_empty() {
                return;
            }
            if list.iter().any(J::is_object) {
                let mut keys: Vec<String> = list
                    .iter()
                    .filter_map(J::as_object)
                    .flat_map(|m| {
                        m.iter()
                            .filter(|(_, v)| !is_container(v))
                            .map(|(k, _)| k.clone())
                    })
                    .collect();
                keys.sort();
                keys.dedup();
                for el in list {
                    format_text(el, identifier, Some(&keys), out);
                }
            } else if list.iter().any(J::is_array) {
                let (scalars, non): (Vec<&J>, Vec<&J>) =
                    list.iter().partition(|v| !is_container(v));
                if !scalars.is_empty() {
                    scalar_list(&scalars, identifier, out);
                }
                for n in non {
                    format_text(n, identifier, None, out);
                }
            } else {
                scalar_list(&list.iter().collect::<Vec<_>>(), identifier, out);
            }
        }
        other => {
            out.push_str(&py_str(other));
            out.push('\n');
        }
    }
}

fn scalar_list(items: &[&J], identifier: Option<&str>, out: &mut String) {
    match identifier {
        Some(id) => {
            for i in items {
                out.push_str(&format!("{}\t{}\n", id.to_uppercase(), py_str(i)));
            }
        }
        None => {
            out.push_str(
                &items
                    .iter()
                    .map(|i| py_str(i))
                    .collect::<Vec<_>>()
                    .join("\t"),
            );
            out.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(v: J) -> String {
        let mut buf = Vec::new();
        write(OutputFormat::Text, &v, &TableStyle::default(), &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn text_output_matches_awscli_layout() {
        let v = json!({
            "Reservations": [{"ReservationId": "r-1", "Instances": [{"InstanceId": "i-1", "State": {"Name": "running"}}]}],
            "Count": 1
        });
        assert_eq!(
            text(v),
            "1\nRESERVATIONS\tr-1\nINSTANCES\ti-1\nSTATE\trunning\n"
        );
        assert_eq!(text(json!({"Names": ["a", "b"]})), "NAMES\ta\nNAMES\tb\n");
        assert_eq!(text(json!(["a", "b"])), "a\tb\n");
        assert_eq!(text(json!({"Flag": true, "N": 1.5})), "True\t1.5\n");
    }

    #[test]
    fn json_output_and_query() {
        let mut buf = Vec::new();
        let style = TableStyle::default();
        write(
            OutputFormat::Json,
            &json!({"A": [1, "ü"]}),
            &style,
            &mut buf,
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "{\n    \"A\": [\n        1,\n        \"ü\"\n    ]\n}\n"
        );
        let mut empty = Vec::new();
        write(OutputFormat::Json, &json!({}), &style, &mut empty).unwrap();
        assert!(empty.is_empty());
        assert_eq!(apply_query("A[0]", &json!({"A": [7]})).unwrap(), json!(7));
    }
}
