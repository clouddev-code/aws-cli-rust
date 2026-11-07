//! `help` pages (awscli `clidocs.py`, rendered the way `aws ... help` shows
//! them through docutils' man writer and mandoc).
//!
//! The text layout follows the man output: section headings at column 0,
//! bodies indented by 7 columns, 3 more per nesting level, paragraphs
//! wrapped at 78 columns, `o` bullets with a hanging indent. Model
//! documentation is HTML and examples are reStructuredText; both are reduced
//! to the same small set of blocks. Character attributes (bold, underline)
//! are not reproduced.

use crate::cli::{ArgDef, ArgKind, ArgTarget};
use crate::model::{Operation, Shape};
use crate::paginate::PaginatorConfig;
use serde_json::Value as J;
use std::sync::LazyLock;

const WIDTH: usize = 78;
const BODY: usize = 7;
const STEP: usize = 3;

/// awscli `data/cli.json`: global options and the top-level description.
static CLI_JSON: LazyLock<J> =
    LazyLock::new(|| serde_json::from_str(include_str!("../assets/cli.json")).expect("cli.json"));

// ---------------------------------------------------------------------------
// Blocks and text layout

#[derive(Debug, Clone, PartialEq)]
enum Block {
    Para(String),
    /// Items of an `o` list; each item is a list of blocks.
    Bullets(Vec<Vec<Block>>),
    /// `NOTE:` / `IMPORTANT:` admonitions.
    Note(String, Vec<Block>),
    /// Preformatted lines.
    Code(Vec<String>),
    Heading(String),
    Indented(Vec<Block>),
}

/// Text being laid out, with at most one blank line between blocks.
struct TextDoc {
    out: String,
    blank: bool,
}

impl TextDoc {
    fn new() -> Self {
        TextDoc {
            out: String::new(),
            blank: false,
        }
    }

    fn sep(&mut self) {
        self.blank = true;
    }

    fn raw_line(&mut self, line: &str) {
        if self.blank && !self.out.is_empty() {
            self.out.push('\n');
        }
        self.blank = false;
        self.out.push_str(line.trim_end());
        self.out.push('\n');
    }

    fn line(&mut self, indent: usize, text: &str) {
        self.raw_line(&format!("{}{text}", " ".repeat(indent)));
    }

    /// Word-wrap `text`; the first line starts with `prefix` and the rest
    /// are indented by `hanging` more columns.
    fn wrap(&mut self, indent: usize, prefix: &str, text: &str, hanging: usize) {
        let mut line = format!("{}{prefix}", " ".repeat(indent));
        let mut has_word = false;
        let limit = WIDTH;
        for word in text.split_whitespace() {
            let width = crate::eaw::text_length(&line);
            if has_word && width + 1 + crate::eaw::text_length(word) > limit {
                self.raw_line(&line);
                line = " ".repeat(indent + hanging);
                has_word = false;
            }
            if has_word {
                line.push(' ');
            }
            line.push_str(word);
            has_word = true;
        }
        if has_word || !prefix.is_empty() {
            self.raw_line(&line);
        }
    }

    fn heading(&mut self, title: &str) {
        if !self.out.is_empty() {
            self.blank = true;
        }
        self.raw_line(title);
    }

    fn blocks(&mut self, blocks: &[Block], indent: usize) {
        for block in blocks {
            match block {
                Block::Para(text) => {
                    self.wrap(indent, "", text, 0);
                    self.sep();
                }
                Block::Bullets(items) => {
                    for item in items {
                        let (first, rest) = match item.split_first() {
                            Some((Block::Para(t), rest)) => (t.as_str(), rest),
                            _ => ("", item.as_slice()),
                        };
                        self.wrap(indent, "o ", first, 2);
                        self.sep();
                        self.blocks(rest, indent + 2);
                    }
                }
                Block::Note(kind, inner) => {
                    self.line(indent, &format!("{kind}:"));
                    self.blocks(inner, indent + STEP);
                    self.sep();
                }
                Block::Code(lines) => {
                    for l in lines {
                        self.line(indent + STEP, l);
                    }
                    self.sep();
                }
                Block::Heading(text) => {
                    self.line(indent + STEP, text);
                    self.sep();
                }
                Block::Indented(inner) => self.blocks(inner, indent + STEP),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTML documentation

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let Some(end) = tail.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let name = &tail[1..end];
        let decoded = match name {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            n if n.starts_with("#x") || n.starts_with("#X") => u32::from_str_radix(&n[2..], 16)
                .ok()
                .and_then(char::from_u32),
            n if n.starts_with('#') => n[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// One open element while parsing HTML.
struct Frame {
    tag: String,
    blocks: Vec<Block>,
    items: Vec<Vec<Block>>,
    text: String,
}

impl Frame {
    fn new(tag: &str) -> Self {
        Frame {
            tag: tag.to_string(),
            blocks: Vec::new(),
            items: Vec::new(),
            text: String::new(),
        }
    }

    fn flush(&mut self) {
        let t = self.text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !t.is_empty() {
            self.blocks.push(Block::Para(t));
        }
        self.text.clear();
    }
}

const BLOCK_TAGS: &[&str] = &[
    "p",
    "div",
    "ul",
    "ol",
    "li",
    "note",
    "important",
    "pre",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "dl",
    "dt",
    "dd",
    "table",
    "tr",
    "blockquote",
    "fullname",
];

/// Reduce model documentation (HTML fragments, not always well-formed) to
/// blocks.
fn html_blocks(html: &str) -> Vec<Block> {
    let mut stack = vec![Frame::new("")];
    let mut rest = html;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            push_text(&mut stack, rest);
            break;
        };
        push_text(&mut stack, &rest[..lt]);
        let tail = &rest[lt..];
        let Some(gt) = tail.find('>') else {
            push_text(&mut stack, tail);
            break;
        };
        let inner = &tail[1..gt];
        rest = &tail[gt + 1..];
        let closing = inner.starts_with('/');
        let name: String = inner
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        if name == "br" {
            stack.last_mut().unwrap().flush();
            continue;
        }
        if name == "pre" && !closing {
            // Preformatted text up to </pre>, kept as lines.
            let end = rest
                .to_ascii_lowercase()
                .find("</pre>")
                .unwrap_or(rest.len());
            let text = decode_entities(&strip_tags(&rest[..end]));
            rest = rest.get(end + 6..).unwrap_or("");
            let top = stack.last_mut().unwrap();
            top.flush();
            let lines: Vec<String> = text
                .trim_matches('\n')
                .lines()
                .map(str::to_string)
                .collect();
            if !lines.is_empty() {
                top.blocks.push(Block::Code(lines));
            }
            continue;
        }
        if !BLOCK_TAGS.contains(&name.as_str()) {
            continue;
        }
        if !closing {
            if inner.ends_with('/') {
                continue;
            }
            stack.last_mut().unwrap().flush();
            stack.push(Frame::new(&name));
            continue;
        }
        // Close up to the matching element; stray end tags are ignored.
        if !stack.iter().skip(1).any(|f| f.tag == name) {
            continue;
        }
        while stack.len() > 1 {
            let done = stack.last().unwrap().tag == name;
            close_frame(&mut stack);
            if done {
                break;
            }
        }
    }
    while stack.len() > 1 {
        close_frame(&mut stack);
    }
    let mut root = stack.pop().unwrap();
    root.flush();
    root.blocks
}

fn push_text(stack: &mut [Frame], text: &str) {
    // Some help texts carry reST inline literals (``file://``).
    let text = ascii_punctuation(&decode_entities(text)).replace("``", "");
    stack.last_mut().unwrap().text.push_str(&text);
}

/// mandoc's ASCII output replaces typographic punctuation.
fn ascii_punctuation(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201c}' | '\u{201d}' => '"',
            '\u{2013}' | '\u{2014}' => '-',
            c => c,
        })
        .collect()
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn close_frame(stack: &mut Vec<Frame>) {
    let mut frame = stack.pop().unwrap();
    frame.flush();
    let parent = stack.last_mut().unwrap();
    match frame.tag.as_str() {
        "li" => {
            if matches!(parent.tag.as_str(), "ul" | "ol") {
                parent.items.push(frame.blocks);
            } else {
                parent.blocks.push(Block::Bullets(vec![frame.blocks]));
            }
        }
        "ul" | "ol" => {
            let mut items = frame.items;
            // Text directly inside the list (no <li>) becomes an item.
            if !frame.blocks.is_empty() {
                items.push(frame.blocks);
            }
            if !items.is_empty() {
                parent.blocks.push(Block::Bullets(items));
            }
        }
        "note" => parent.blocks.push(Block::Note("NOTE".into(), frame.blocks)),
        "important" => parent
            .blocks
            .push(Block::Note("IMPORTANT".into(), frame.blocks)),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let text = frame
                .blocks
                .iter()
                .filter_map(|b| match b {
                    Block::Para(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ");
            if !text.is_empty() {
                parent.blocks.push(Block::Heading(text));
            }
        }
        "dd" | "blockquote" => parent.blocks.push(Block::Indented(frame.blocks)),
        // awscli leaves the service's full name out of the description.
        "fullname" => {}
        // Table cells are inline; a row is one paragraph.
        _ => parent.blocks.extend(frame.blocks),
    }
}

// ---------------------------------------------------------------------------
// reStructuredText examples

/// Inline markup: ``code``, **bold**, *em*, `text <url>`_ links.
fn rst_inline(s: &str) -> String {
    static LINK: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"`([^`<]*?)\s*<[^>]*>`_{1,2}").unwrap());
    static REF: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"`([^`]+)`_{1,2}").unwrap());
    static EM: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(^|[\s(])\*([^*\s][^*]*?)\*").unwrap());
    let s = LINK.replace_all(s, "$1");
    let s = REF.replace_all(&s, "$1");
    let s = s.replace("``", "").replace("**", "");
    EM.replace_all(&s, "$1$2").into_owned()
}

/// Reduce an examples file to blocks: paragraphs, `::` literal blocks,
/// bullets and section titles.
fn rst_blocks(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut i = 0;
    let is_underline = |l: &str| {
        l.len() >= 3
            && l.chars().all(|c| "=-~^\"'`#*+".contains(c))
            && l.chars().collect::<std::collections::HashSet<_>>().len() == 1
    };
    let mut literal_next = false;
    let flush = |para: &mut Vec<String>, blocks: &mut Vec<Block>, literal: &mut bool| {
        if para.is_empty() {
            return;
        }
        let mut text = para.join(" ");
        if let Some(t) = text.strip_suffix("::") {
            *literal = true;
            text = if t.ends_with(' ') || t.is_empty() {
                t.trim_end().to_string()
            } else {
                format!("{t}:")
            };
        }
        let text = rst_inline(&text);
        if let Some(item) = text.strip_prefix("- ").or_else(|| text.strip_prefix("* ")) {
            match blocks.last_mut() {
                Some(Block::Bullets(items)) => items.push(vec![Block::Para(item.to_string())]),
                _ => blocks.push(Block::Bullets(vec![vec![Block::Para(item.to_string())]])),
            }
        } else if !text.trim().is_empty() {
            blocks.push(Block::Para(text));
        }
        para.clear();
    };
    while i < lines.len() {
        let line = lines[i];
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if line.trim().is_empty() {
            flush(&mut para, &mut blocks, &mut literal_next);
            i += 1;
            continue;
        }
        if line.trim_start().starts_with(".. ") && !indented {
            flush(&mut para, &mut blocks, &mut literal_next);
            // `.. code::` style directives introduce a literal block.
            literal_next = line.contains("code");
            i += 1;
            continue;
        }
        if literal_next && indented && para.is_empty() {
            let base = line.len() - line.trim_start().len();
            let mut code = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                if !(l.trim().is_empty() || l.starts_with(' ') || l.starts_with('\t')) {
                    break;
                }
                code.push(l.get(base..).unwrap_or("").trim_end().to_string());
                i += 1;
            }
            while code.last().is_some_and(String::is_empty) {
                code.pop();
            }
            blocks.push(Block::Code(code));
            literal_next = false;
            continue;
        }
        if i + 1 < lines.len() && is_underline(lines[i + 1]) && para.is_empty() {
            blocks.push(Block::Para(rst_inline(line.trim())));
            i += 2;
            continue;
        }
        if is_underline(line) {
            i += 1;
            continue;
        }
        if para.is_empty() {
            literal_next = false;
        }
        para.push(line.trim().to_string());
        i += 1;
    }
    flush(&mut para, &mut blocks, &mut literal_next);
    blocks
}

// ---------------------------------------------------------------------------
// Pages

fn title_line(doc: &mut TextDoc, name: &str) {
    let t = format!("{}()", name.to_uppercase());
    let gap = WIDTH.saturating_sub(2 * t.len()).max(1);
    doc.raw_line(&format!("{t}{}{t}", " ".repeat(gap)));
    doc.sep();
}

fn footer_line(doc: &mut TextDoc, name: &str) {
    let t = format!("{}()", name.to_uppercase());
    doc.sep();
    doc.raw_line(&format!("{}{t}", " ".repeat(WIDTH.saturating_sub(t.len()))));
}

fn name_section(doc: &mut TextDoc, name: &str) {
    doc.heading("NAME");
    doc.line(BODY, &format!("{name} -"));
}

fn global_synopsis() -> Vec<String> {
    CLI_JSON["options"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, opt)| {
            if matches!(opt["action"].as_str(), Some("store_true" | "store_false")) {
                format!("[--{name}]")
            } else {
                format!("[--{name} <value>]")
            }
        })
        .collect()
}

/// `GlobalOptionsDocumenter.doc_global_options` (bodies stay at the section
/// indent, as in awscli's pre-rendered file).
fn global_options(doc: &mut TextDoc) {
    doc.heading("GLOBAL OPTIONS");
    for (name, opt) in CLI_JSON["options"].as_object().into_iter().flatten() {
        let ty = match (opt["action"].as_str(), opt["type"].as_str()) {
            (Some("store_true" | "store_false"), _) => "boolean",
            (_, Some(t)) => t,
            _ => "string",
        };
        doc.line(BODY, &format!("--{name} ({ty})"));
        doc.sep();
        doc.blocks(&html_blocks(opt["help"].as_str().unwrap_or("")), BODY);
        if let Some(choices) = opt["choices"].as_array() {
            let items = choices
                .iter()
                .filter_map(J::as_str)
                .map(|c| vec![Block::Para(c.to_string())])
                .collect();
            doc.blocks(&[Block::Bullets(items)], BODY);
        }
    }
}

fn bullet_list(doc: &mut TextDoc, heading: &str, names: &[String]) {
    doc.heading(heading);
    doc.sep();
    let items = names.iter().map(|n| vec![Block::Para(n.clone())]).collect();
    doc.blocks(&[Block::Bullets(items)], BODY);
}

/// `aws help`
pub fn top_level(commands: &[String]) -> String {
    let mut doc = TextDoc::new();
    title_line(&mut doc, "aws");
    name_section(&mut doc, "aws");
    doc.heading("DESCRIPTION");
    doc.wrap(BODY, "", CLI_JSON["description"].as_str().unwrap_or(""), 0);
    doc.heading("SYNOPSIS");
    doc.sep();
    doc.line(BODY + STEP, CLI_JSON["synopsis"].as_str().unwrap_or(""));
    doc.sep();
    let usage = CLI_JSON["help_usage"]
        .as_str()
        .unwrap_or("")
        .replace('*', "");
    doc.wrap(BODY, "", &usage, 0);
    global_options(&mut doc);
    bullet_list(&mut doc, "AVAILABLE SERVICES", commands);
    bullet_list(&mut doc, "SEE ALSO", &["aws help topics".to_string()]);
    footer_line(&mut doc, "aws");
    doc.out
}

/// `aws <service> help` (which lists `help` too) and `aws <service> wait
/// help`.
pub fn command_list(
    name: &str,
    description_html: &str,
    commands: &[String],
    list_help: bool,
) -> String {
    let mut doc = TextDoc::new();
    title_line(&mut doc, name);
    name_section(&mut doc, name);
    doc.heading("DESCRIPTION");
    doc.blocks(&html_blocks(description_html), BODY);
    let mut names: Vec<String> = commands.to_vec();
    if list_help {
        names.push("help".into());
    }
    names.sort();
    bullet_list(&mut doc, "AVAILABLE COMMANDS", &names);
    footer_line(&mut doc, name);
    doc.out
}

/// Everything an operation (or waiter) page needs.
pub struct OperationPage<'a> {
    /// Command name shown (`get-caller-identity`, `instance-running`).
    pub name: &'a str,
    pub op: &'a Operation<'a>,
    /// Replaces the operation's documentation (waiters).
    pub description: Option<String>,
    pub table: &'a [ArgDef],
    pub paginator: Option<&'a PaginatorConfig>,
    /// `examples/<command>/<name>.rst`
    pub examples: Option<String>,
    /// Waiters have no output section content.
    pub show_output: bool,
}

const CLI_INPUT_JSON_HELP: &str = "Reads arguments from the JSON string provided. The JSON string follows the format provided by --generate-cli-skeleton. If other arguments are provided on the command line, those values will override the JSON-provided values. It is not possible to pass arbitrary binary values using a JSON-provided value as the string will be taken literally. This may not be specified along with --cli-input-yaml.";
const SKELETON_HELP: &str = "Prints a JSON skeleton to standard output without sending an API request. If provided with no value or the value input, prints a sample input JSON that can be used as an argument for --cli-input-json. Similarly, if provided yaml-input it will print a sample input YAML that can be used with --cli-input-yaml. If provided with the value output, it validates the command inputs and returns a sample output JSON for that command. The generated JSON skeleton is not stable between versions of the AWS CLI and there are no backwards compatibility guarantees in the JSON skeleton generated.";
const STARTING_TOKEN_HELP: &str = "<p>A token to specify where to start paginating. This is the <code>NextToken</code> from a previously truncated response.</p><p>For usage examples, see Pagination in the <i>AWS Command Line Interface User Guide</i>.</p>";
const PAGE_SIZE_HELP: &str = "<p>The size of each page to get in the AWS service call. This does not affect the number of items returned in the command's output. Setting a smaller page size results in more calls to the AWS service, retrieving fewer items in each call. This can help prevent the AWS service calls from timing out.</p><p>For usage examples, see Pagination in the <i>AWS Command Line Interface User Guide</i>.</p>";
const MAX_ITEMS_HELP: &str = "<p>The total number of items to return in the command's output. If the total number of items available is more than the value specified, a <code>NextToken</code> is provided in the command's output. To resume pagination, provide the <code>NextToken</code> value in the <code>starting-token</code> argument of a subsequent command. <b>Do not</b> use the <code>NextToken</code> response element directly outside of the AWS CLI.</p><p>For usage examples, see Pagination in the <i>AWS Command Line Interface User Guide</i>.</p>";
const OUTFILE_HELP: &str = "Filename where the content will be saved";
const EXAMPLES_NOTE: &str = "<p>To use the following examples, you must have the AWS CLI installed and configured. See the Getting started guide in the <i>AWS CLI User Guide</i> for more information.</p><p>Unless otherwise stated, all examples have unix-like quotation rules. These examples will need to be adapted to your terminal's quoting rules. See Using quotation marks with strings in the <i>AWS CLI User Guide</i>.</p>";

/// `_get_argument_type_name`
fn type_name(shape: &Shape) -> String {
    if shape.is_jsonvalue() && shape.location() == Some("header") {
        return "JSON".into();
    }
    if shape.is_document() {
        return "document".into();
    }
    if shape.type_name() == "blob" && shape.is_streaming() {
        return "streaming blob".into();
    }
    if shape.type_name() == "structure" && shape.bool_trait("union") {
        return "tagged union structure".into();
    }
    shape.type_name().to_string()
}

const SCALAR_TYPES: &[&str] = &[
    "string",
    "integer",
    "long",
    "boolean",
    "double",
    "float",
    "blob",
    "timestamp",
];

/// An argument as shown in help: its names (boolean pairs joined) and
/// whether it is documented as required.
fn documented_args(table: &[ArgDef]) -> Vec<(String, &ArgDef)> {
    let mut out: Vec<(String, &ArgDef)> = Vec::new();
    for def in table.iter().filter(|d| !d.hidden) {
        match (&def.kind, &def.target) {
            (ArgKind::Boolean(false), t) if out.iter().any(|(_, d)| &d.target == t) => {
                let entry = out.iter_mut().find(|(_, d)| &d.target == t).unwrap();
                entry.0 = format!("{} | {}", entry.0, def.cli_name);
            }
            (_, ArgTarget::CliInputYaml) => {
                if let Some(entry) = out
                    .iter_mut()
                    .find(|(_, d)| d.target == ArgTarget::CliInputJson)
                {
                    entry.0 = format!("{} | {}", entry.0, def.cli_name);
                }
            }
            _ => out.push((def.cli_name.clone(), def)),
        }
    }
    out
}

pub fn operation(page: &OperationPage) -> String {
    let mut doc = TextDoc::new();
    let input = page.op.input();
    title_line(&mut doc, page.name);
    name_section(&mut doc, page.name);

    doc.heading("DESCRIPTION");
    match &page.description {
        Some(d) => {
            doc.wrap(BODY, "", &rst_inline(d), 0);
            doc.sep();
        }
        None => doc.blocks(&html_blocks(page.op.documentation()), BODY),
    }
    if page.op.model.meta_str("uid").is_some() {
        doc.line(BODY, "See also: AWS API Documentation");
        doc.sep();
    }
    if page.op.uses_document_types() {
        doc.wrap(BODY, "", &format!("{} uses document type values. Document types follow the JSON data model where valid values are: strings, numbers, booleans, null, arrays, and objects. For command input, options and nested parameters that are labeled with the type document must be provided as JSON. Shorthand syntax does not support document types.", page.name), 0);
        doc.sep();
    }
    if let Some(p) = page.paginator {
        doc.wrap(BODY, "", &format!("{} is a paginated operation. Multiple API calls may be issued in order to retrieve the entire data set of results. You can disable pagination by providing the --no-paginate argument. When using --output text and the --query argument on a paginated response, the --query argument must extract data from the results of the following query expressions: {}", page.name, p.result_keys.join(", ")), 0);
        doc.sep();
    }

    let args = documented_args(page.table);
    doc.heading("SYNOPSIS");
    doc.sep();
    doc.line(BODY + 5, page.name);
    for (names, def) in &args {
        let s = if names.contains(" | ") {
            names.clone()
        } else if def.cli_name.starts_with("--") {
            format!("{names} <value>")
        } else {
            format!("<{names}>")
        };
        if def.required || def.document_required {
            doc.line(BODY + STEP, &s);
        } else {
            doc.line(BODY + STEP, &format!("[{s}]"));
        }
    }
    for g in global_synopsis() {
        doc.line(BODY + STEP, &g);
    }

    doc.heading("OPTIONS");
    if args.is_empty() {
        doc.line(BODY, "None");
    }
    for (names, def) in &args {
        option(&mut doc, names, def, input.as_ref(), page);
    }

    global_options(&mut doc);

    if let Some(ex) = &page.examples {
        doc.heading("EXAMPLES");
        doc.blocks(
            &[Block::Note("NOTE".into(), html_blocks(EXAMPLES_NOTE))],
            BODY,
        );
        doc.blocks(&rst_blocks(ex), BODY);
    }

    doc.heading("OUTPUT");
    let output = page.op.output().filter(|_| page.show_output);
    let members = output.as_ref().map(Shape::members).unwrap_or_default();
    if members.is_empty() {
        doc.line(BODY, "None");
    }
    for (name, m) in members {
        member(&mut doc, name, &m, &mut Vec::new(), false, BODY);
    }
    footer_line(&mut doc, page.name);
    doc.out
}

fn option(
    doc: &mut TextDoc,
    names: &str,
    def: &ArgDef,
    input: Option<&Shape>,
    page: &OperationPage,
) {
    let shape = match &def.target {
        ArgTarget::Member(m) => input.and_then(|s| s.member_named(m)),
        _ => None,
    };
    let required = if def.required || def.document_required {
        " [required]"
    } else {
        ""
    };
    let (ty, html) = match (&def.target, &shape) {
        (ArgTarget::Member(_), Some(s)) => (type_name(s), s.documentation().to_string()),
        (ArgTarget::PageSize, _) => ("integer".to_string(), PAGE_SIZE_HELP.to_string()),
        (ArgTarget::MaxItems, _) => ("integer".to_string(), MAX_ITEMS_HELP.to_string()),
        (ArgTarget::StartingToken, _) => ("string".to_string(), STARTING_TOKEN_HELP.to_string()),
        (ArgTarget::CliInputJson, _) => ("string".to_string(), CLI_INPUT_JSON_HELP.to_string()),
        (ArgTarget::GenerateCliSkeleton, _) => ("string".to_string(), SKELETON_HELP.to_string()),
        (ArgTarget::Outfile, _) => ("string".to_string(), OUTFILE_HELP.to_string()),
        _ => ("string".to_string(), String::new()),
    };
    let header = format!("{names} ({ty}){required}");
    // Plain-text help (awscli's own arguments) continues on the same line.
    if !html.trim_start().starts_with('<') {
        doc.wrap(BODY, "", &format!("{header} {html}"), 0);
        doc.sep();
        return;
    }
    doc.line(BODY, &header);
    doc.blocks(&html_blocks(&html), BODY + STEP);
    let Some(shape) = shape else {
        doc.sep();
        return;
    };
    if shape.type_name() == "blob" && shape.is_streaming() {
        doc.blocks(&[Block::Note("NOTE".into(), vec![Block::Para("This argument is of type: streaming blob. Its value must be the path to a file (e.g. path/to/file) and must not be prefixed with file:// or fileb://".into())])], BODY + STEP);
    }
    union_note(doc, &shape, BODY + STEP);
    enums_and_constraints(doc, &shape, BODY + STEP);
    // `_document_nested_structure`
    let mut stack = vec![shape.name.clone()];
    match shape.type_name() {
        "structure" => {
            let req = shape.required();
            for (n, m) in shape.members() {
                member(doc, n, &m, &mut stack, req.contains(&n), BODY + STEP);
            }
        }
        "list" => {
            if let Some(m) = shape.member() {
                member(doc, "", &m, &mut stack, false, BODY + STEP);
            }
        }
        "map" => {
            if let (Some(k), Some(v)) = (shape.key(), shape.value()) {
                member(
                    doc,
                    k.ser_name().unwrap_or("key"),
                    &k,
                    &mut stack,
                    false,
                    BODY + STEP,
                );
                member(
                    doc,
                    v.ser_name().unwrap_or("value"),
                    &v,
                    &mut stack,
                    false,
                    BODY + STEP,
                );
            }
        }
        _ => {}
    }
    option_example(doc, def, &shape, page);
}

fn union_note(doc: &mut TextDoc, shape: &Shape, indent: usize) {
    if shape.type_name() == "structure" && shape.bool_trait("union") {
        let keys: Vec<&str> = shape.members().iter().map(|(n, _)| *n).collect();
        doc.blocks(
            &[Block::Note(
                "NOTE".into(),
                vec![Block::Para(format!(
                    "This is a Tagged Union structure. Only one of the following top level keys can be set: {}.",
                    keys.join(", ")
                ))],
            )],
            indent,
        );
    }
}

/// `_document_enums` and `_document_constraints`
fn enums_and_constraints(doc: &mut TextDoc, shape: &Shape, indent: usize) {
    let enums = shape.enum_values();
    if shape.type_name() == "string" && !enums.is_empty() {
        doc.line(indent, "Possible values:");
        doc.sep();
        let items = enums
            .iter()
            .map(|e| vec![Block::Para(e.to_string())])
            .collect();
        doc.blocks(&[Block::Bullets(items)], indent);
    }
    let constraints: Vec<String> = ["min", "max", "pattern"]
        .iter()
        .filter_map(|c| {
            shape.get(c).map(|v| match v {
                J::String(s) => format!("{c}: {s}"),
                J::Number(n) => format!("{c}: {n}"),
                other => format!("{c}: {other}"),
            })
        })
        .collect();
    if !constraints.is_empty() {
        doc.line(indent, "Constraints:");
        doc.sep();
        let items = constraints
            .into_iter()
            .map(|c| vec![Block::Para(c)])
            .collect();
        doc.blocks(&[Block::Bullets(items)], indent);
    }
}

/// `_doc_member`: `Name -> (type) [required]` with its docs and nested
/// members one level deeper.
fn member(
    doc: &mut TextDoc,
    name: &str,
    shape: &Shape,
    stack: &mut Vec<String>,
    required: bool,
    indent: usize,
) {
    if stack.contains(&shape.name) && stack.iter().filter(|n| **n == shape.name).count() > 1 {
        if shape.type_name() == "structure" {
            doc.line(indent, "( ... recursive ... )");
            doc.sep();
        }
        return;
    }
    stack.push(shape.name.clone());
    let ty = type_name(shape);
    let mut header = if name.is_empty() {
        format!("({ty})")
    } else {
        format!("{name} -> ({ty})")
    };
    if required {
        header.push_str(" [required]");
    }
    doc.line(indent, &header);
    let inner = indent + STEP;
    doc.blocks(&html_blocks(shape.documentation()), inner);
    union_note(doc, shape, inner);
    enums_and_constraints(doc, shape, inner);
    doc.sep();
    match shape.type_name() {
        "structure" => {
            let req = shape.required();
            for (n, m) in shape.members() {
                member(doc, n, &m, stack, req.contains(&n), inner);
            }
        }
        "map" => {
            if let (Some(k), Some(v)) = (shape.key(), shape.value()) {
                member(doc, k.ser_name().unwrap_or("key"), &k, stack, false, inner);
                member(
                    doc,
                    v.ser_name().unwrap_or("value"),
                    &v,
                    stack,
                    false,
                    inner,
                );
            }
        }
        "list" => {
            if let Some(m) = shape.member() {
                member(doc, "", &m, stack, false, inner);
            }
        }
        _ => {}
    }
    doc.sep();
    stack.pop();
}

/// `doc_option_example`: shorthand and JSON syntax for complex arguments.
fn option_example(doc: &mut TextDoc, def: &ArgDef, shape: &Shape, page: &OperationPage) {
    if matches!(def.kind, ArgKind::Boolean(_)) {
        return;
    }
    if supports_shorthand(shape) {
        match shorthand_example(def, shape, page) {
            None => return,
            Some(ex) if !ex.is_empty() => {
                doc.line(BODY, "Shorthand Syntax:");
                doc.sep();
                doc.blocks(
                    &[Block::Code(ex.lines().map(str::to_string).collect())],
                    BODY,
                );
            }
            Some(_) => {}
        }
    }
    let member_scalar = shape
        .member()
        .is_some_and(|m| SCALAR_TYPES.contains(&m.type_name()));
    if shape.type_name() == "list" && member_scalar {
        let m = shape.member().unwrap();
        let ex = json_value_name(&m, false);
        doc.line(BODY, "Syntax:");
        doc.sep();
        doc.blocks(&[Block::Code(vec![format!("{ex} {ex} ...")])], BODY);
    } else if !SCALAR_TYPES.contains(&shape.type_name()) {
        let mut text = String::new();
        let mut indent = 0;
        json_example(&mut text, &mut indent, shape, &mut Vec::new());
        doc.line(BODY, "JSON Syntax:");
        doc.sep();
        doc.blocks(
            &[Block::Code(text.lines().map(str::to_string).collect())],
            BODY,
        );
    }
}

fn is_document_container(shape: &Shape) -> bool {
    if shape.is_document() {
        return true;
    }
    match shape.type_name() {
        "list" => shape.member().is_some_and(|m| is_document_container(&m)),
        "map" => shape.value().is_some_and(|v| is_document_container(&v)),
        _ => false,
    }
}

/// `_supports_shorthand_syntax`
fn supports_shorthand(shape: &Shape) -> bool {
    if is_document_container(shape) {
        return false;
    }
    match shape.type_name() {
        "structure" | "map" => true,
        "list" => shape
            .member()
            .is_some_and(|m| matches!(m.type_name(), "structure" | "list" | "map")),
        _ => false,
    }
}

/// `ParamShorthandDocGen.generate_shorthand_example`: `None` means "do not
/// document"; an empty string means "too complex".
fn shorthand_example(def: &ArgDef, shape: &Shape, page: &OperationPage) -> Option<String> {
    let old_list_case = matches!(
        (page.op.model.service_name.as_str(), page.name, def.name()),
        ("firehose", "put-record-batch", "records")
            | (
                "workspaces",
                "reboot-workspaces",
                "reboot-workspace-requests"
            )
            | (
                "workspaces",
                "rebuild-workspaces",
                "rebuild-workspace-requests"
            )
            | (
                "workspaces",
                "terminate-workspaces",
                "terminate-workspace-requests"
            )
            | ("elb", "remove-tags", "tags")
            | ("elb", "describe-instance-health", "instances")
            | (
                "elb",
                "deregister-instances-from-load-balancer",
                "instances"
            )
            | ("elb", "register-instances-with-load-balancer", "instances")
    );
    if shape.type_name() == "list"
        && let Some(m) = shape.member()
        && m.type_name() == "structure"
        && m.members().len() == 1
        && old_list_case
    {
        let member_name = m.members()[0].0;
        if shape.get("min").and_then(J::as_i64) == Some(1)
            && shape.get("max").and_then(J::as_i64) == Some(1)
        {
            return Some(format!("{} {member_name}1", def.cli_name));
        }
        return Some(format!(
            "{} {member_name}1 {member_name}2 {member_name}3",
            def.cli_name
        ));
    }
    if shape.type_name() == "structure"
        && shape.members().len() == 1
        && shape
            .member_named("Value")
            .is_some_and(|v| v.type_name() == "string")
    {
        return None;
    }
    let mut stack = Vec::new();
    let docs = if shape.type_name() == "list" {
        shape
            .member()
            .and_then(|m| shorthand_docs(&m, &mut stack))
            .map(|d| d + " ...")
    } else {
        shorthand_docs(shape, &mut stack)
    };
    Some(docs.unwrap_or_default())
}

/// `_shorthand_docs`; `None` when the shape is too complex.
fn shorthand_docs(shape: &Shape, stack: &mut Vec<String>) -> Option<String> {
    if stack.len() > 3 {
        return None;
    }
    match shape.type_name() {
        "structure" => {
            let mut parts = Vec::new();
            for (name, m) in shape.members() {
                if is_document_container(&m) {
                    continue;
                }
                if stack.contains(&m.name) {
                    parts.push(format!("{name}=( ... recursive ... )"));
                    continue;
                }
                stack.push(m.name.clone());
                let v = shorthand_docs(&m, stack);
                stack.pop();
                parts.push(format!("{name}={}", v?));
            }
            let inner = parts.join(",");
            Some(if stack.is_empty() {
                inner
            } else {
                format!("{{{inner}}}")
            })
        }
        "list" => {
            let m = shape.member()?;
            stack.push(m.name.clone());
            let el = shorthand_docs(&m, stack);
            stack.pop();
            let el = el?;
            Some(
                if matches!(m.type_name(), "structure" | "list" | "map") || stack.len() > 1 {
                    format!("[{el},{el}]")
                } else {
                    format!("{el},{el}")
                },
            )
        }
        "map" => {
            let (k, v) = (shape.key()?, shape.value()?);
            stack.push(v.name.clone());
            let value = shorthand_docs(&v, stack);
            stack.pop();
            let value = value?;
            let mut start = format!("KeyName1={value},KeyName2={value}");
            let enums = k.enum_values();
            if !enums.is_empty() && stack.is_empty() {
                start.push_str("\n\nWhere valid key names are:\n");
                for e in enums {
                    start.push_str(&format!("  {e}\n"));
                }
            } else if !stack.is_empty() {
                start = format!("{{{start}}}");
            }
            Some(start)
        }
        other => Some(other.to_string()),
    }
}

/// `_json_example_value_name`
fn json_value_name(shape: &Shape, include_enum: bool) -> String {
    match shape.type_name() {
        "string" => {
            let enums = shape.enum_values();
            if !enums.is_empty() && include_enum {
                enums
                    .iter()
                    .map(|e| format!("\"{e}\""))
                    .collect::<Vec<_>>()
                    .join("|")
            } else {
                "\"string\"".into()
            }
        }
        "boolean" => "true|false".into(),
        other => other.to_string(),
    }
}

/// `_json_example`, writing into `text` with two-space indentation.
fn json_example(text: &mut String, indent: &mut usize, shape: &Shape, stack: &mut Vec<String>) {
    if stack.contains(&shape.name) && stack.iter().filter(|n| **n == shape.name).count() > 1 {
        if shape.type_name() == "structure" {
            text.push_str("{ ... recursive ... }");
        }
        return;
    }
    stack.push(shape.name.clone());
    let newline = |text: &mut String, indent: usize| {
        text.push('\n');
        text.push_str(&"  ".repeat(indent));
    };
    match shape.type_name() {
        "list" => {
            text.push('[');
            if let Some(m) = shape.member() {
                if SCALAR_TYPES.contains(&m.type_name()) {
                    text.push_str(&format!("{}, ...", json_value_name(&m, true)));
                } else {
                    *indent += 1;
                    newline(text, *indent);
                    json_example(text, indent, &m, stack);
                    newline(text, *indent);
                    text.push_str("...");
                    *indent -= 1;
                    newline(text, *indent);
                }
            }
            text.push(']');
        }
        "map" => {
            text.push('{');
            *indent += 1;
            if let (Some(k), Some(v)) = (shape.key(), shape.value()) {
                text.push_str(&format!("{}: ", json_value_name(&k, true)));
                if SCALAR_TYPES.contains(&v.type_name()) {
                    text.push_str(&json_value_name(&v, true));
                } else {
                    *indent += 1;
                    json_example(text, indent, &v, stack);
                    *indent -= 1;
                }
            }
            newline(text, *indent);
            text.push_str("...");
            *indent -= 1;
            text.push('}');
        }
        "structure" if shape.is_document() => text.push_str("{...}"),
        "structure" => {
            text.push('{');
            *indent += 1;
            newline(text, *indent);
            let members = shape.members();
            for (i, (name, m)) in members.iter().enumerate() {
                if SCALAR_TYPES.contains(&m.type_name()) {
                    text.push_str(&format!("\"{name}\": {}", json_value_name(m, true)));
                } else {
                    text.push_str(&format!("\"{name}\": "));
                    json_example(text, indent, m, stack);
                }
                if i + 1 < members.len() {
                    text.push(',');
                    newline(text, *indent);
                }
            }
            *indent -= 1;
            newline(text, *indent);
            text.push('}');
        }
        _ => {}
    }
    stack.pop();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_blocks() {
        let b = html_blocks(
            "<p>Intro <code>x</code> &amp; more.</p><ul><li><p>one</p></li><li>two</li></ul><note><p>careful</p></note>",
        );
        assert_eq!(
            b,
            vec![
                Block::Para("Intro x & more.".into()),
                Block::Bullets(vec![
                    vec![Block::Para("one".into())],
                    vec![Block::Para("two".into())]
                ]),
                Block::Note("NOTE".into(), vec![Block::Para("careful".into())]),
            ]
        );
        // Broken markup still yields the text.
        assert_eq!(
            html_blocks("<p>a <b>b</p> c &nbsp;"),
            vec![Block::Para("a b".into()), Block::Para("c".into())]
        );
    }

    #[test]
    fn rst_examples() {
        let b = rst_blocks(
            "**Example 1: To list**\n\nThe following ``list`` example. ::\n\n    aws x list\n\nOutput::\n\n    {}\n",
        );
        assert_eq!(
            b,
            vec![
                Block::Para("Example 1: To list".into()),
                Block::Para("The following list example.".into()),
                Block::Code(vec!["aws x list".into()]),
                Block::Para("Output:".into()),
                Block::Code(vec!["{}".into()]),
            ]
        );
    }

    #[test]
    fn wrapping_and_bullets() {
        let mut doc = TextDoc::new();
        doc.blocks(
            &[Block::Bullets(vec![vec![Block::Para("word ".repeat(20))]])],
            BODY,
        );
        let lines: Vec<&str> = doc.out.lines().collect();
        assert!(lines[0].starts_with("       o word"));
        assert!(lines[1].starts_with("         word"));
        assert!(lines.iter().all(|l| l.len() <= WIDTH));
    }
}
