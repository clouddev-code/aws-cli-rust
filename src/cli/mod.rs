//! Command-line parsing: global options (awscli `data/cli.json`) and the
//! per-operation argument table built from the input shape.

pub mod params;
pub mod shorthand;

use crate::model::Shape;
use crate::naming::cli_name;

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
    /// `--flag` / `--no-flag`.
    Boolean,
    /// Every following word up to the next option.
    List,
    Single,
}

/// Where a parsed argument goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgTarget {
    Member(String),
    StartingToken,
    PageSize,
    MaxItems,
    CliInputJson,
}

#[derive(Debug, Clone)]
pub struct ArgDef {
    pub cli_name: String,
    pub target: ArgTarget,
    pub kind: ArgKind,
    pub required: bool,
}

/// Build the argument table for an operation (`ServiceOperation.arg_table`
/// plus awscli's `unify_paging_params`).
///
/// For paginated operations `--starting-token`, `--page-size` and
/// `--max-items` shadow members with the same CLI name. With `--no-paginate`
/// the shadowed members come back, as in awscli.
pub fn arg_table(input: Option<&Shape>, paginated: bool, no_paginate: bool) -> Vec<ArgDef> {
    let mut table = Vec::new();
    if let Some(shape) = input {
        let required = shape.required();
        for (name, member) in shape.members() {
            let kind = match member.type_name() {
                "boolean" => ArgKind::Boolean,
                "list" => ArgKind::List,
                _ => ArgKind::Single,
            };
            table.push(ArgDef {
                cli_name: format!("--{}", cli_name(name)),
                target: ArgTarget::Member(name.to_string()),
                kind,
                required: required.contains(&name) && !member.bool_trait("idempotencyToken"),
            });
        }
    }
    table.push(ArgDef {
        cli_name: "--cli-input-json".into(),
        target: ArgTarget::CliInputJson,
        kind: ArgKind::Single,
        required: false,
    });
    if paginated {
        for (n, t) in [
            ("--starting-token", ArgTarget::StartingToken),
            ("--page-size", ArgTarget::PageSize),
            ("--max-items", ArgTarget::MaxItems),
        ] {
            let shadowed = table.iter().position(|d| d.cli_name == n);
            match (shadowed, no_paginate) {
                (Some(_), true) => continue,
                (Some(idx), false) => {
                    table.remove(idx);
                }
                (None, _) => {}
            }
            table.push(ArgDef {
                cli_name: n.into(),
                target: t,
                kind: ArgKind::Single,
                required: false,
            });
        }
    }
    table
}

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
            unknown.push(token.clone());
            continue;
        }
        let (flag, inline) = match token.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (token.clone(), None),
        };
        let negated = flag.strip_prefix("--no-").map(|f| format!("--{f}"));
        let found = table
            .iter()
            .position(|d| d.cli_name == flag)
            .map(|idx| (idx, false))
            .or_else(|| {
                negated.as_ref().and_then(|n| {
                    table
                        .iter()
                        .position(|d| &d.cli_name == n && d.kind == ArgKind::Boolean)
                        .map(|idx| (idx, true))
                })
            });
        let Some((idx, is_negated)) = found else {
            unknown.push(token.clone());
            continue;
        };
        let def = &table[idx];
        let value = match def.kind {
            ArgKind::Boolean => ArgValue::Bool(!is_negated),
            ArgKind::Single => {
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
        out.retain(|(j, _)| *j != idx);
        out.push((idx, value));
    }
    if !unknown.is_empty() {
        return Err(UsageError(format!(
            "Unknown options: {}",
            unknown.join(", ")
        )));
    }
    // With --cli-input-json the required members may come from the JSON
    // document, so the caller re-checks after merging (awscli does the same by
    // marking arguments optional).
    if out
        .iter()
        .any(|(j, _)| table[*j].target == ArgTarget::CliInputJson)
    {
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
        let table = vec![
            ArgDef {
                cli_name: "--ids".into(),
                target: ArgTarget::Member("Ids".into()),
                kind: ArgKind::List,
                required: true,
            },
            ArgDef {
                cli_name: "--dry-run".into(),
                target: ArgTarget::Member("DryRun".into()),
                kind: ArgKind::Boolean,
                required: false,
            },
            ArgDef {
                cli_name: "--count".into(),
                target: ArgTarget::Member("Count".into()),
                kind: ArgKind::Single,
                required: false,
            },
        ];
        let parsed = parse_op_args(
            &s(&["--ids", "a", "b", "--no-dry-run", "--count", "-1"]),
            &table,
        )
        .unwrap();
        assert_eq!(parsed[0], (0, ArgValue::Words(s(&["a", "b"]))));
        assert_eq!(parsed[1], (1, ArgValue::Bool(false)));
        assert_eq!(parsed[2], (2, ArgValue::Words(s(&["-1"]))));
        let err = parse_op_args(&s(&["--count", "1"]), &table).unwrap_err();
        assert_eq!(err.0, "the following arguments are required: --ids");
        assert!(
            parse_op_args(&s(&["--ids", "a", "--bogus"]), &table)
                .unwrap_err()
                .0
                .contains("Unknown options")
        );
    }
}
