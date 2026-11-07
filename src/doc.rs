//! Model documentation (HTML fragments) as plain text.

use quick_xml::Reader;
use quick_xml::events::Event;

/// All text of an HTML fragment, or `None` when it is not well-formed XML
/// (`_strip_xml_from_documentation` then keeps the raw text).
pub fn xml_text(html: &str) -> Option<String> {
    let doc = format!("<doc>{html}</doc>");
    let mut reader = Reader::from_str(&doc);
    let mut out = String::new();
    let mut depth = 0usize;
    loop {
        match reader.read_event().ok()? {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(out);
                }
            }
            Event::Text(t) => {
                let raw = t[..].to_string();
                out.push_str(&quick_xml::escape::unescape(&raw).ok()?);
            }
            Event::GeneralRef(r) => {
                let entity = format!("&{};", &r[..]);
                out.push_str(&quick_xml::escape::unescape(&entity).ok()?);
            }
            Event::CData(t) => out.push_str(&t[..]),
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// `get_shape_doc_overview`: the first sentence of the documentation, on
/// one line.
pub fn overview(html: &str) -> String {
    let text = xml_text(html).unwrap_or_else(|| html.to_string());
    let first = text
        .split(['.', ':'])
        .next()
        .unwrap_or_default()
        .to_string();
    let first = if first.is_empty() { first } else { first + "." };
    first.replace(['\n', '\u{2028}'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sentence_of_the_text() {
        assert_eq!(
            overview("<p>The name of the <code>table</code>. More text.</p>"),
            "The name of the table."
        );
        assert_eq!(overview("<p>Valid: a, b</p>"), "Valid.");
        assert_eq!(overview(""), "");
        // Not well-formed: the raw text is used.
        assert_eq!(overview("<p>a &nbsp; b</p>"), "<p>a &nbsp; b</p>.");
    }
}
