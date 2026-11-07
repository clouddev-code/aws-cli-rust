//! Command-line parsing: global options (awscli `data/cli.json`) and the
//! per-operation argument table built from the input shape.

pub mod params;
pub mod shorthand;

use crate::model::Shape;
use crate::naming::cli_name;
use crate::paginate::PaginatorConfig;

#[derive(Debug, Clone, PartialEq)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

/// Global options, accepted anywhere on the command line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Globals {
    pub debug: bool,
    pub endpoint_url: Option<String>,
    pub no_verify_ssl: bool,
    pub no_paginate: bool,
    pub output: Option<String>,
    pub query: Option<String>,
    pub profile: Option<String>,
    pub region: Option<String>,
    pub version: bool,
    pub color: Option<String>,
    pub no_sign_request: bool,
    pub ca_bundle: Option<String>,
    pub cli_read_timeout: Option<String>,
    pub cli_connect_timeout: Option<String>,
    pub cli_binary_format: Option<String>,
    pub no_cli_pager: bool,
    pub cli_error_format: Option<String>,
}

const GLOBAL_FLAGS: &[&str] = &[
    "--debug",
    "--no-verify-ssl",
    "--no-paginate",
    "--version",
    "--no-sign-request",
    "--no-cli-pager",
    "--cli-auto-prompt",
    "--no-cli-auto-prompt",
];

const GLOBAL_VALUES: &[&str] = &[
    "--endpoint-url",
    "--output",
    "--query",
    "--profile",
    "--region",
    "--color",
    "--ca-bundle",
    "--cli-read-timeout",
    "--cli-connect-timeout",
    "--cli-binary-format",
    "--cli-error-format",
];

/// argparse treats `-x` as an option but `-1` as a value.
fn is_option(token: &str) -> bool {
    token.starts_with('-')
        && token.len() > 1
        && !token[1..].starts_with(|c: char| c.is_ascii_digit() || c == '.')
}

/// Pull global options out of `argv`; everything else is returned in order.
pub fn split_globals(argv: &[String]) -> Result<(Globals, Vec<String>), UsageError> {
    let mut g = Globals::default();
    let mut rest = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let token = &argv[i];
        let (flag, inline) = match token.split_once('=') {
            Some((f, v)) if token.starts_with("--") => (f, Some(v.to_string())),
            _ => (token.as_str(), None),
        };
        if GLOBAL_FLAGS.contains(&flag) {
            match flag {
                "--debug" => g.debug = true,
                "--no-verify-ssl" => g.no_verify_ssl = true,
                "--no-paginate" => g.no_paginate = true,
                "--version" => g.version = true,
                "--no-sign-request" => g.no_sign_request = true,
                "--no-cli-pager" => g.no_cli_pager = true,
                _ => {}
            }
            i += 1;
            continue;
        }
        if GLOBAL_VALUES.contains(&flag) {
            let value = match inline {
                Some(v) => v,
                None => {
                    i += 1;
                    argv.get(i).cloned().ok_or_else(|| {
                        UsageError(format!("argument {flag}: expected one argument"))
                    })?
                }
            };
            let slot = match flag {
                "--endpoint-url" => &mut g.endpoint_url,
                "--output" => &mut g.output,
                "--query" => &mut g.query,
                "--profile" => &mut g.profile,
                "--region" => &mut g.region,
                "--color" => &mut g.color,
                "--ca-bundle" => &mut g.ca_bundle,
                "--cli-read-timeout" => &mut g.cli_read_timeout,
                "--cli-connect-timeout" => &mut g.cli_connect_timeout,
                "--cli-binary-format" => &mut g.cli_binary_format,
                _ => &mut g.cli_error_format,
            };
            *slot = Some(value);
            i += 1;
            continue;
        }
        rest.push(token.clone());
        i += 1;
    }
    Ok((g, rest))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    /// `--flag` stores `true`; its `--no-flag` twin stores `false`.
    Boolean(bool),
    /// Every following word up to the next option.
    List,
    Single,
    /// argparse `nargs='?'`: an optional value (`--generate-cli-skeleton`).
    OptionalValue,
    /// A bare word (argparse positional), e.g. `outfile`.
    Positional,
}

/// Where a parsed argument goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgTarget {
    Member(String),
    StartingToken,
    PageSize,
    MaxItems,
    CliInputJson,
    CliInputYaml,
    GenerateCliSkeleton,
    /// `outfile` for operations with a streaming blob output
    /// (`StreamingOutputArgument`).
    Outfile,
}

#[derive(Debug, Clone)]
pub struct ArgDef {
    pub cli_name: String,
    pub target: ArgTarget,
    pub kind: ArgKind,
    pub required: bool,
    /// `_UNDOCUMENTED`: accepted but left out of help and completion.
    pub hidden: bool,
    /// `_DOCUMENT_AS_REQUIRED`: optional because of a hidden alias, but
    /// documented as required.
    pub document_required: bool,
}

impl ArgDef {
    pub fn new(cli_name: impl Into<String>, target: ArgTarget, kind: ArgKind) -> Self {
        ArgDef {
            cli_name: cli_name.into(),
            target,
            kind,
            required: false,
            hidden: false,
            document_required: false,
        }
    }

    /// Name without the leading `--`.
    pub fn name(&self) -> &str {
        self.cli_name.trim_start_matches("--")
    }
}

/// Inputs to [`arg_table`] besides the input shape.
#[derive(Debug, Clone, Copy, Default)]
pub struct TableOptions<'a> {
    pub paginator: Option<&'a PaginatorConfig>,
    pub no_paginate: bool,
    /// The operation has a streaming output (`outfile` positional). Such
    /// operations get neither `--cli-input-*` nor `--generate-cli-skeleton`.
    pub outfile: bool,
}

/// Build the argument table for an operation (`ServiceOperation.arg_table`
/// plus the `cliinput`, `paginate`, `generatecliskeleton` and
/// `streamingoutputarg` customizations), in awscli's order.
///
/// For paginated operations the input tokens and limit key stay usable but
/// are undocumented, and `--starting-token`, `--page-size` (only with a limit
/// key) and `--max-items` shadow members with the same CLI name. With
/// `--no-paginate` the shadowed members come back, as in awscli.
pub fn arg_table(input: Option<&Shape>, opts: &TableOptions) -> Vec<ArgDef> {
    let mut table = Vec::new();
    if let Some(shape) = input {
        let required = shape.required();
        let paging_members: Vec<&str> = opts
            .paginator
            .map(|p| {
                p.input_tokens
                    .iter()
                    .chain(p.limit_key.iter())
                    .map(String::as_str)
                    .collect()
            })
            .unwrap_or_default();
        for (name, member) in shape.members() {
            let flag = format!("--{}", cli_name(name));
            let target = ArgTarget::Member(name.to_string());
            let kind = match member.type_name() {
                "boolean" => ArgKind::Boolean(true),
                "list" => ArgKind::List,
                _ => ArgKind::Single,
            };
            let mut def = ArgDef::new(flag.clone(), target.clone(), kind);
            def.required = required.contains(&name) && !member.bool_trait("idempotencyToken");
            def.hidden = paging_members.contains(&name);
            let negative = (kind == ArgKind::Boolean(true)).then(|| ArgDef {
                cli_name: format!("--no-{}", cli_name(name)),
                kind: ArgKind::Boolean(false),
                required: false,
                ..def.clone()
            });
            table.push(def);
            table.extend(negative);
        }
    }
    if opts.outfile {
        let mut def = ArgDef::new("outfile", ArgTarget::Outfile, ArgKind::Positional);
        def.required = true;
        table.push(def);
    } else {
        table.push(ArgDef::new(
            "--cli-input-json",
            ArgTarget::CliInputJson,
            ArgKind::Single,
        ));
        table.push(ArgDef::new(
            "--cli-input-yaml",
            ArgTarget::CliInputYaml,
            ArgKind::Single,
        ));
    }
    if let Some(p) = opts.paginator {
        let mut paging = vec![("--starting-token", ArgTarget::StartingToken)];
        if p.limit_key.is_some() {
            paging.push(("--page-size", ArgTarget::PageSize));
        }
        paging.push(("--max-items", ArgTarget::MaxItems));
        for (n, t) in paging {
            let shadowed = table.iter().position(|d| d.cli_name == n);
            match (shadowed, opts.no_paginate) {
                (Some(_), true) => continue,
                (Some(idx), false) => {
                    table.remove(idx);
                }
                (None, _) => {}
            }
            table.push(ArgDef::new(n, t, ArgKind::Single));
        }
    }
    if !opts.outfile {
        table.push(ArgDef::new(
            "--generate-cli-skeleton",
            ArgTarget::GenerateCliSkeleton,
            ArgKind::OptionalValue,
        ));
    }
    table
}

/// `--generate-cli-skeleton` choices; a bare flag means `input`.
pub const SKELETON_CHOICES: &[&str] = &["input", "output", "yaml-input"];

#[derive(Debug, Clone, PartialEq)]
pub enum ArgValue {
    Bool(bool),
    Words(Vec<String>),
}

/// Parse operation arguments against the table. Returns values in the order
/// they were given, keyed by table index.
pub fn parse_op_args(
    tokens: &[String],
    table: &[ArgDef],
) -> Result<Vec<(usize, ArgValue)>, UsageError> {
    let mut out: Vec<(usize, ArgValue)> = Vec::new();
    let mut unknown = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        i += 1;
        if !is_option(token) {
            let free = table.iter().enumerate().position(|(idx, d)| {
                d.kind == ArgKind::Positional && !out.iter().any(|(j, _)| *j == idx)
            });
            match free {
                Some(idx) => out.push((idx, ArgValue::Words(vec![token.clone()]))),
                None => unknown.push(token.clone()),
            }
            continue;
        }
        let (flag, inline) = match token.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (token.clone(), None),
        };
        let Some(idx) = table.iter().position(|d| d.cli_name == flag) else {
            unknown.push(token.clone());
            continue;
        };
        let def = &table[idx];
        let value = match def.kind {
            ArgKind::Boolean(v) => ArgValue::Bool(v),
            ArgKind::OptionalValue => {
                let v = inline.or_else(|| {
                    let v = tokens.get(i).filter(|t| !is_option(t)).cloned();
                    i += usize::from(v.is_some());
                    v
                });
                let v = v.unwrap_or_else(|| SKELETON_CHOICES[0].to_string());
                if def.target == ArgTarget::GenerateCliSkeleton
                    && !SKELETON_CHOICES.contains(&v.as_str())
                {
                    return Err(UsageError(format!(
                        "argument {}: Invalid choice: '{v}' (choose from {})",
                        def.cli_name,
                        SKELETON_CHOICES
                            .iter()
                            .map(|c| format!("'{c}'"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
                ArgValue::Words(vec![v])
            }
            ArgKind::Single | ArgKind::Positional => {
                let v = match inline {
                    Some(v) => v,
                    None => {
                        let v = tokens
                            .get(i)
                            .filter(|t| !is_option(t))
                            .cloned()
                            .ok_or_else(|| {
                                UsageError(format!(
                                    "argument {}: expected one argument",
                                    def.cli_name
                                ))
                            })?;
                        i += 1;
                        v
                    }
                };
                ArgValue::Words(vec![v])
            }
            ArgKind::List => {
                let mut words: Vec<String> = inline.into_iter().collect();
                while let Some(t) = tokens.get(i).filter(|t| !is_option(t)) {
                    words.push(t.clone());
                    i += 1;
                }
                if words.is_empty() {
                    return Err(UsageError(format!(
                        "argument {}: expected at least one argument",
                        def.cli_name
                    )));
                }
                ArgValue::Words(words)
            }
        };
        // `--flag` and `--no-flag` share a destination; the last one wins.
        out.retain(|(j, _)| {
            *j != idx
                && !(table[*j].target == def.target && matches!(def.kind, ArgKind::Boolean(_)))
        });
        out.push((idx, value));
    }
    if !unknown.is_empty() {
        return Err(UsageError(format!(
            "Unknown options: {}",
            unknown.join(", ")
        )));
    }
    // With `--cli-input-*` the required members may come from the document,
    // so the caller re-checks after merging; `--generate-cli-skeleton` (but
    // not `output`) needs none (`OverrideRequiredArgsArgument`).
    let overrides_required = out.iter().any(|(j, v)| match &table[*j].target {
        ArgTarget::CliInputJson | ArgTarget::CliInputYaml => true,
        ArgTarget::GenerateCliSkeleton => {
            !matches!(v, ArgValue::Words(w) if w.first().map(String::as_str) == Some("output"))
        }
        _ => false,
    });
    if overrides_required {
        return Ok(out);
    }
    let missing: Vec<&str> = table
        .iter()
        .enumerate()
        .filter(|(idx, d)| d.required && !out.iter().any(|(j, _)| j == idx))
        .map(|(_, d)| d.cli_name.as_str())
        .collect();
    if !missing.is_empty() {
        return Err(UsageError(format!(
            "the following arguments are required: {}",
            missing.join(", ")
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn globals_anywhere() {
        let (g, rest) = split_globals(&s(&[
            "ec2",
            "--region",
            "us-west-2",
            "describe-instances",
            "--output=text",
            "--debug",
        ]))
        .unwrap();
        assert_eq!(g.region.as_deref(), Some("us-west-2"));
        assert_eq!(g.output.as_deref(), Some("text"));
        assert!(g.debug);
        assert_eq!(rest, s(&["ec2", "describe-instances"]));
    }

    #[test]
    fn op_args() {
        let mut ids = ArgDef::new("--ids", ArgTarget::Member("Ids".into()), ArgKind::List);
        ids.required = true;
        let dry = ArgTarget::Member("DryRun".into());
        let table = vec![
            ids,
            ArgDef::new("--dry-run", dry.clone(), ArgKind::Boolean(true)),
            ArgDef::new("--no-dry-run", dry, ArgKind::Boolean(false)),
            ArgDef::new(
                "--count",
                ArgTarget::Member("Count".into()),
                ArgKind::Single,
            ),
            ArgDef::new(
                "--generate-cli-skeleton",
                ArgTarget::GenerateCliSkeleton,
                ArgKind::OptionalValue,
            ),
        ];
        let parsed = parse_op_args(
            &s(&[
                "--ids",
                "a",
                "b",
                "--dry-run",
                "--no-dry-run",
                "--count",
                "-1",
            ]),
            &table,
        )
        .unwrap();
        assert_eq!(parsed[0], (0, ArgValue::Words(s(&["a", "b"]))));
        assert_eq!(parsed[1], (2, ArgValue::Bool(false)));
        assert_eq!(parsed[2], (3, ArgValue::Words(s(&["-1"]))));
        let err = parse_op_args(&s(&["--count", "1"]), &table).unwrap_err();
        assert_eq!(err.0, "the following arguments are required: --ids");
        assert!(
            parse_op_args(&s(&["--ids", "a", "--bogus"]), &table)
                .unwrap_err()
                .0
                .contains("Unknown options")
        );
        // A bare --generate-cli-skeleton means `input` and lifts required
        // arguments; `output` keeps them.
        let parsed = parse_op_args(&s(&["--generate-cli-skeleton"]), &table).unwrap();
        assert_eq!(parsed[0], (4, ArgValue::Words(s(&["input"]))));
        assert!(parse_op_args(&s(&["--generate-cli-skeleton", "output"]), &table).is_err());
        assert!(parse_op_args(&s(&["--generate-cli-skeleton", "bogus"]), &table).is_err());
    }
}
