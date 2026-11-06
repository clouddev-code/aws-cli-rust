//! Minimal XML tree built on quick-xml, used by the response parsers.

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Element {
    /// Qualified name as written (`ns:Tag` or `Tag`).
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Element>,
}

impl Element {
    /// Name without a namespace prefix.
    pub fn local_name(&self) -> &str {
        self.name.rsplit(':').next().unwrap_or(&self.name)
    }

    pub fn child(&self, local: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.local_name() == local)
    }

    pub fn children_named<'a>(&'a self, local: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children
            .iter()
            .filter(move |c| c.local_name() == local)
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn start_element(e: &BytesStart) -> Result<Element, String> {
    let name = e.name().as_ref().to_string();
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|err| err.to_string())?;
        let key = a.key.as_ref().to_string();
        let value = a
            .normalized_value(quick_xml::XmlVersion::Implicit1_0)
            .map_err(|err| err.to_string())?
            .into_owned();
        attrs.push((key, value));
    }
    Ok(Element {
        name,
        attrs,
        ..Default::default()
    })
}

/// Parse a document and return its root element.
pub fn parse(bytes: &[u8]) -> Result<Element, String> {
    let mut reader = Reader::from_reader(bytes);
    let mut stack: Vec<Element> = Vec::new();
    let mut buf = Vec::new();
    loop {
        match reader
            .read_event_into(&mut buf)
            .map_err(|e| e.to_string())?
        {
            Event::Start(e) => {
                if stack.len() >= super::MAX_DEPTH {
                    return Err("XML nested too deeply".into());
                }
                stack.push(start_element(&e)?)
            }
            Event::Empty(e) => {
                let el = start_element(&e)?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None => return Ok(el),
                }
            }
            Event::End(_) => {
                let el = stack.pop().ok_or("unbalanced XML")?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(el),
                    None => return Ok(el),
                }
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    let raw = t.as_ref().to_string();
                    let text = quick_xml::escape::unescape(&raw).map_err(|e| e.to_string())?;
                    top.text.push_str(&text);
                }
            }
            Event::GeneralRef(r) => {
                if let Some(top) = stack.last_mut() {
                    let entity = format!("&{};", r.as_ref());
                    let text = quick_xml::escape::unescape(&entity).map_err(|e| e.to_string())?;
                    top.text.push_str(&text);
                }
            }
            Event::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(t.as_ref());
                }
            }
            Event::Eof => return Err("empty XML document".into()),
            _ => {}
        }
        buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_with_entities() {
        let el =
            parse(br#"<?xml version="1.0"?><A xmlns="x"><B a="1">t &amp; u</B><C/></A>"#).unwrap();
        assert_eq!(el.name, "A");
        assert_eq!(el.child("B").unwrap().text, "t & u");
        assert_eq!(el.child("B").unwrap().attr("a"), Some("1"));
        assert!(el.child("C").is_some());
    }

    #[test]
    fn rejects_deep_nesting() {
        let n = crate::protocol::MAX_DEPTH;
        let ok = format!("{}{}", "<a>".repeat(n), "</a>".repeat(n));
        assert!(parse(ok.as_bytes()).is_ok());
        let deep = "<a>".repeat(1 << 18);
        assert_eq!(parse(deep.as_bytes()).unwrap_err(), "XML nested too deeply");
    }
}
