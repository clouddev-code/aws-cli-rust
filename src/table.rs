//! `--output table` (awscli `formatter.TableFormatter` and `table.py`).
//!
//! The layout follows awscli exactly, including Python's round-half-even
//! column scaling and the conversion to a vertical layout when the table is
//! wider than the terminal.

use crate::eaw::text_length;
use serde_json::Value as J;

/// Colors used by `ColorizedStyler` (colorama `Style`/`Fore` codes).
const ROW_ON: &str = "\x1b[1m\x1b[34m";
const INDENT_ON: &str = "\x1b[2m\x1b[33m";
const RESET: &str = "\x1b[0m";

#[derive(Debug, Clone, Default)]
struct Section {
    title: String,
    headers: Vec<String>,
    rows: Vec<Vec<String>>,
    indent_level: usize,
    num_cols: Option<usize>,
    max_widths: Vec<usize>,
}

impl Section {
    fn update_max_widths(&mut self, row: &[String]) {
        if self.max_widths.is_empty() {
            self.max_widths = row.iter().map(|e| text_length(e)).collect();
        } else {
            for (i, el) in row.iter().enumerate() {
                if let Some(w) = self.max_widths.get_mut(i) {
                    *w = (*w).max(text_length(el));
                }
            }
        }
    }

    fn add_header(&mut self, headers: Vec<String>) {
        self.update_max_widths(&headers);
        if self.num_cols.is_none() {
            self.num_cols = Some(headers.len());
        }
        self.headers = headers;
    }

    fn add_row(&mut self, row: Vec<String>) -> Result<(), String> {
        let n = *self.num_cols.get_or_insert(row.len());
        if row.len() != n {
            return Err(format!(
                "Row should have {n} elements, instead it has {}",
                row.len()
            ));
        }
        self.update_max_widths(&row);
        self.rows.push(row);
        Ok(())
    }

    /// `calculate_column_widths`; with `max_width` the widths are scaled to
    /// add up to it exactly.
    fn column_widths(&self, padding: usize, max_width: Option<usize>) -> Vec<usize> {
        let unscaled: Vec<usize> = self.max_widths.iter().map(|w| w + padding).collect();
        let Some(max_width) = max_width else {
            return unscaled;
        };
        if unscaled.is_empty() {
            return unscaled;
        }
        let scale = max_width as f64 / unscaled.iter().sum::<usize>() as f64;
        let mut scaled: Vec<i64> = unscaled
            .iter()
            .map(|w| (scale * *w as f64).round_ties_even() as i64)
            .collect();
        let mut off_by = scaled.iter().sum::<i64>() - max_width as i64;
        while off_by != 0 {
            let order: Vec<usize> = if off_by < 0 {
                (0..scaled.len()).rev().collect()
            } else {
                (0..scaled.len()).collect()
            };
            for i in order {
                if off_by > 0 {
                    scaled[i] -= 1;
                    off_by -= 1;
                } else {
                    scaled[i] += 1;
                    off_by += 1;
                }
                if off_by == 0 {
                    break;
                }
            }
        }
        scaled.into_iter().map(|w| w.max(0) as usize).collect()
    }

    fn total_width(&self, padding: usize, outer_padding: usize) -> usize {
        let border = 2;
        let total: usize = self
            .column_widths(0, None)
            .iter()
            .map(|w| w + padding)
            .sum::<usize>()
            + border
            + 2 * outer_padding;
        total.max(text_length(&self.title) + border + 2 * outer_padding)
    }
}

/// `center_text`
fn center_text(
    text: &str,
    length: usize,
    left_edge: &str,
    right_edge: &str,
    text_length: usize,
) -> String {
    let char_start = (length / 2) as i64 - (text_length / 2) as i64 - 1;
    let so_far = self::text_length(left_edge) as i64 + char_start + text_length as i64;
    let right = length as i64 - self::text_length(right_edge) as i64 - so_far;
    format!(
        "{left_edge}{}{text}{}{right_edge}",
        " ".repeat(char_start.max(0) as usize),
        " ".repeat(right.max(0) as usize)
    )
}

/// `align_left`
fn align_left(
    text: &str,
    length: usize,
    left_edge: &str,
    right_edge: &str,
    text_length: usize,
) -> String {
    let left_padding = 2;
    let computed =
        text_length + left_padding + self::text_length(left_edge) + self::text_length(right_edge);
    let padding = if length >= computed { left_padding } else { 0 };
    let so_far = left_edge.chars().count() + padding + text_length;
    let right = length as i64 - so_far as i64 - right_edge.chars().count() as i64;
    format!(
        "{left_edge}{}{text}{}{right_edge}",
        " ".repeat(padding),
        " ".repeat(right.max(0) as usize)
    )
}

struct MultiTable {
    sections: Vec<Section>,
    terminal_width: usize,
    color: bool,
}

impl MultiTable {
    fn current(&mut self) -> &mut Section {
        self.sections.last_mut().expect("a section was started")
    }

    fn new_section(&mut self, title: &str, indent_level: usize) {
        self.sections.push(Section {
            title: title.to_string(),
            indent_level,
            ..Default::default()
        });
    }

    fn max_width(&self) -> usize {
        self.sections
            .iter()
            .map(|s| s.total_width(4, s.indent_level))
            .max()
            .unwrap_or(0)
    }

    fn row_element(&self, text: &str) -> String {
        if self.color {
            format!("{ROW_ON}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    fn indent_char(&self) -> String {
        if self.color {
            format!("{INDENT_ON}|{RESET}")
        } else {
            "|".into()
        }
    }

    fn render(&mut self, out: &mut String) {
        let mut max_width = self.max_width();
        if max_width > self.terminal_width {
            convert_to_vertical(&mut self.sections);
            max_width = self.max_width();
        }
        out.push_str(&"-".repeat(max_width));
        out.push('\n');
        for section in &self.sections {
            self.render_section(section, max_width, out);
        }
    }

    /// `IndentedStream.write` for one line.
    fn write_line(&self, section: &Section, line: &str, out: &mut String) {
        let indent = self.indent_char().repeat(section.indent_level);
        out.push_str(&indent);
        out.push_str(line);
        out.push_str(&indent);
        out.push('\n');
    }

    fn render_section(&self, section: &Section, max_width: usize, out: &mut String) {
        let max_width = max_width.saturating_sub(section.indent_level * 2);
        if !section.title.is_empty() {
            let line = center_text(
                &section.title,
                max_width,
                "|",
                "|",
                text_length(&section.title),
            );
            self.write_line(section, &line, out);
            if section.headers.is_empty() && section.rows.is_empty() {
                let line = format!("+{}+", "-".repeat(max_width.saturating_sub(2)));
                self.write_line(section, &line, out);
            }
        }
        if !section.headers.is_empty() {
            let widths = section.column_widths(4, Some(max_width));
            let mut line = String::new();
            for (i, (width, header)) in widths.iter().zip(&section.headers).enumerate() {
                let left = if i == 0 { "|" } else { "" };
                line.push_str(&center_text(header, *width, left, "|", text_length(header)));
            }
            self.write_line(section, &line_break(&widths), out);
            self.write_line(section, &line, out);
        }
        if !section.rows.is_empty() {
            let widths = section.column_widths(4, Some(max_width));
            if widths.is_empty() {
                return;
            }
            self.write_line(section, &line_break(&widths), out);
            for row in &section.rows {
                let mut line = String::new();
                for (i, (width, element)) in widths.iter().zip(row).enumerate() {
                    let left = if i == 0 { "|" } else { "" };
                    line.push_str(&align_left(
                        &self.row_element(element),
                        *width,
                        left,
                        "|",
                        text_length(element),
                    ));
                }
                self.write_line(section, &line, out);
            }
            self.write_line(section, &line_break(&widths), out);
        }
    }
}

/// `_write_line_break`: `+-----+------+`
fn line_break(widths: &[usize]) -> String {
    let mut s = String::new();
    for (i, w) in widths.iter().enumerate() {
        if i == 0 {
            s.push_str(&format!("+{}+", "-".repeat(w.saturating_sub(2))));
        } else {
            s.push_str(&format!("{}+", "-".repeat(w.saturating_sub(1))));
        }
    }
    s
}

/// `convert_to_vertical_table`: single-row sections become key/value rows.
fn convert_to_vertical(sections: &mut [Section]) {
    for section in sections.iter_mut() {
        if section.rows.len() == 1 && !section.headers.is_empty() {
            let mut new = Section {
                title: section.title.clone(),
                indent_level: section.indent_level,
                ..Default::default()
            };
            for (h, e) in section.headers.iter().zip(&section.rows[0]) {
                // Two columns, so this cannot fail.
                let _ = new.add_row(vec![h.clone(), e.clone()]);
            }
            *section = new;
        }
    }
}

fn is_scalar(v: &J) -> bool {
    !(v.is_array() || v.is_object())
}

/// Python truthiness of a response value.
fn truthy(v: &J) -> bool {
    match v {
        J::Null => false,
        J::Bool(b) => *b,
        J::Number(n) => n.as_f64() != Some(0.0),
        J::String(s) => !s.is_empty(),
        J::Array(a) => !a.is_empty(),
        J::Object(o) => !o.is_empty(),
    }
}

/// `str(value)` of a table cell.
fn cell(v: &J) -> String {
    match v {
        J::String(s) => s.clone(),
        J::Bool(true) => "True".into(),
        J::Bool(false) => "False".into(),
        J::Null => "None".into(),
        J::Number(n) if n.is_f64() => crate::util::py_float(n.as_f64().unwrap_or_default()),
        J::Number(n) => n.to_string(),
        // Only reachable for odd responses; awscli would print the repr.
        other => other.to_string(),
    }
}

/// Scalar keys (sorted) and the remaining keys (sorted).
fn group_scalar_keys(m: &serde_json::Map<String, J>) -> (Vec<String>, Vec<String>) {
    let mut headers: Vec<String> = m
        .iter()
        .filter(|(_, v)| is_scalar(v))
        .map(|(k, _)| k.clone())
        .collect();
    let mut more: Vec<String> = m
        .iter()
        .filter(|(_, v)| !is_scalar(v))
        .map(|(k, _)| k.clone())
        .collect();
    headers.sort();
    more.sort();
    (headers, more)
}

struct Builder {
    table: MultiTable,
}

impl Builder {
    fn build(&mut self, title: Option<&str>, current: &J, indent: usize) -> Result<bool, String> {
        if !truthy(current) {
            return Ok(false);
        }
        if let Some(t) = title {
            self.table.new_section(t, indent);
        }
        match current {
            J::Array(items) => {
                if items[0].is_object() {
                    self.sub_table_from_list(items, indent, title)?;
                } else {
                    for item in items {
                        match item {
                            _ if is_scalar(item) => {
                                self.table.current().add_row(vec![cell(item)])?
                            }
                            // Iterating a dict yields its keys.
                            J::Object(m) => {
                                self.table.current().add_row(m.keys().cloned().collect())?
                            }
                            J::Array(a) if a.iter().all(is_scalar) => {
                                self.table.current().add_row(a.iter().map(cell).collect())?
                            }
                            _ => {
                                self.build(None, item, 0)?;
                            }
                        }
                    }
                }
            }
            J::Object(m) => self.sub_table_from_dict(m, indent)?,
            _ => {}
        }
        Ok(true)
    }

    fn sub_table_from_dict(
        &mut self,
        m: &serde_json::Map<String, J>,
        indent: usize,
    ) -> Result<(), String> {
        let (headers, more) = group_scalar_keys(m);
        if headers.len() == 1 {
            let k = &headers[0];
            self.table.current().add_row(vec![k.clone(), cell(&m[k])])?;
        } else if !headers.is_empty() {
            let row = headers.iter().map(|k| cell(&m[k])).collect();
            self.table.current().add_header(headers);
            self.table.current().add_row(row)?;
        }
        for key in more {
            self.build(Some(&key), &m[&key], indent + 1)?;
        }
        Ok(())
    }

    fn sub_table_from_list(
        &mut self,
        items: &[J],
        indent: usize,
        title: Option<&str>,
    ) -> Result<(), String> {
        let mut headers = Vec::new();
        let mut more = Vec::new();
        for item in items {
            if let J::Object(m) = item {
                let (h, r) = group_scalar_keys(m);
                headers.extend(h);
                more.extend(r);
            }
        }
        headers.sort();
        headers.dedup();
        more.sort();
        more.dedup();
        self.table.current().add_header(headers.clone());
        let empty = serde_json::Map::new();
        for (i, element) in items.iter().enumerate() {
            if i > 0 && !more.is_empty() {
                self.table.new_section(title.unwrap_or_default(), indent);
                self.table.current().add_header(headers.clone());
            }
            let m = element.as_object().unwrap_or(&empty);
            let row = headers
                .iter()
                .map(|h| m.get(h).map(cell).unwrap_or_default())
                .collect();
            self.table.current().add_row(row)?;
            for key in &more {
                if let Some(v) = m.get(key) {
                    self.build(Some(key), v, indent + 1)?;
                }
            }
        }
        Ok(())
    }
}

/// Render `data` as awscli's table. Returns an empty string when there is
/// nothing to show; `Err` carries awscli's error for responses it cannot lay
/// out (rows of different lengths).
pub fn render(title: &str, data: &J, terminal_width: usize, color: bool) -> Result<String, String> {
    let mut builder = Builder {
        table: MultiTable {
            sections: Vec::new(),
            terminal_width,
            color,
        },
    };
    let mut out = String::new();
    if builder.build(Some(title), data, 0)? {
        builder.table.render(&mut out);
    }
    Ok(out)
}
