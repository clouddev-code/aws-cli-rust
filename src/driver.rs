//! Command execution (awscli `clidriver.py`): resolve the command, build
//! parameters, resolve region/credentials/endpoint, sign, send, format.

use crate::cli::params::{self, BinaryFormat, ParamContext};
use crate::cli::{self, ArgTarget, ArgValue, Globals, UsageError};
use crate::client::{self, Client, ClientConfig, ClientError};
use crate::credentials::Resolver;
use crate::loader::Loader;
use crate::model::{Operation, ServiceModel};
use crate::naming::cli_name;
use crate::output::{self, OutputFormat};
use crate::paginate::{self, PaginationArgs, PaginatorConfig};
use crate::value::Value;
use serde_json::Value as J;
use std::io::Write;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Exit codes used by AWS CLI v2.
pub mod exit {
    pub const OK: i32 = 0;
    pub const USAGE: i32 = 252;
    pub const CONFIG: i32 = 253;
    pub const SERVICE: i32 = 254;
    pub const GENERAL: i32 = 255;
}

pub use crate::context::{Context, EnvLookup};

#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) code: i32,
    pub(crate) message: String,
}

impl Failure {
    pub(crate) fn usage(m: impl Into<String>) -> Self {
        Failure {
            code: exit::USAGE,
            message: m.into(),
        }
    }
    pub(crate) fn config(m: impl Into<String>) -> Self {
        Failure {
            code: exit::CONFIG,
            message: m.into(),
        }
    }
    pub(crate) fn general(m: impl std::fmt::Display) -> Self {
        Failure {
            code: exit::GENERAL,
            message: m.to_string(),
        }
    }
}

impl From<UsageError> for Failure {
    fn from(e: UsageError) -> Self {
        Failure::usage(format!("aws: error: {}", e.0))
    }
}

pub fn run(ctx: &Context, argv: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run_inner(ctx, argv, out, err) {
        Ok(()) => exit::OK,
        Err(f) => {
            let _ = writeln!(err, "{}", f.message);
            f.code
        }
    }
}

fn run_inner(
    ctx: &Context,
    argv: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), Failure> {
    let (globals, rest) = cli::split_globals(argv)?;
    if globals.version {
        let _ = writeln!(
            out,
            "aws-cli-rust/{VERSION} (awsr; data-driven port of aws-cli v2)"
        );
        return Ok(());
    }
    let positionals: Vec<&String> = rest.iter().take_while(|t| !t.starts_with("--")).collect();
    let Some(command) = positionals.first().map(|s| s.as_str()) else {
        return Err(Failure::usage(
            "usage: awsr [options] <command> <subcommand> [<subcommand> ...] [parameters]\n\
             aws: error: the following arguments are required: command",
        ));
    };
    if command == "help" {
        let _ = writeln!(out, "Available services:");
        for c in ctx.loader.command_names() {
            let _ = writeln!(out, "  {c}");
        }
        return Ok(());
    }
    if command == "login" {
        let tokens: Vec<String> = rest.iter().skip(1).cloned().collect();
        return crate::commands::login(ctx, &globals, &profile_name(ctx, &globals), &tokens, out);
    }
    // Customizations layered on the `sso` service (awscli customizations/sso).
    if command == "sso"
        && let Some(sub @ ("login" | "logout")) = positionals.get(1).map(|s| s.as_str())
    {
        let tokens: Vec<String> = rest.iter().skip(2).cloned().collect();
        return crate::commands::sso(
            ctx,
            &globals,
            &profile_name(ctx, &globals),
            sub,
            &tokens,
            out,
        );
    }
    let dir = ctx.loader.dir_for_command(command).ok_or_else(|| {
        Failure::usage(format!(
            "aws: error: argument command: Invalid choice: '{command}'"
        ))
    })?;
    let model = ctx.loader.service_model(&dir).map_err(Failure::general)?;

    let op_names: Vec<&str> = model.operation_names().collect();
    let Some(sub) = positionals.get(1).map(|s| s.as_str()) else {
        return Err(Failure::usage(format!(
            "aws: error: the following arguments are required: operation (try 'awsr {command} help')"
        )));
    };
    if sub == "help" {
        let _ = writeln!(out, "Available operations for {command}:");
        let mut names: Vec<String> = op_names.iter().map(|n| cli_name(n)).collect();
        names.sort();
        for n in names {
            let _ = writeln!(out, "  {n}");
        }
        return Ok(());
    }
    let op_name = op_names
        .iter()
        .find(|n| cli_name(n) == sub)
        .ok_or_else(|| {
            Failure::usage(format!(
                "aws: error: argument operation: Invalid choice: '{sub}'"
            ))
        })?;
    let op = model.operation(op_name).map_err(Failure::general)?;

    let paginator = ctx
        .loader
        .load_json(&dir, "paginators-1.json")
        .ok()
        .flatten()
        .and_then(|doc| PaginatorConfig::for_operation(&doc, op_name));
    let input = op.input();
    let table = cli::arg_table(input.as_ref(), paginator.is_some(), globals.no_paginate);

    let op_tokens: Vec<String> = rest.iter().skip(2).cloned().collect();
    if op_tokens.first().map(String::as_str) == Some("help") {
        print_op_help(&op, &table, out);
        return Ok(());
    }
    let parsed = cli::parse_op_args(&op_tokens, &table)?;
    if globals.no_paginate {
        let used: Vec<&str> = parsed
            .iter()
            .map(|(i, _)| &table[*i])
            .filter(|d| {
                matches!(
                    d.target,
                    ArgTarget::StartingToken | ArgTarget::PageSize | ArgTarget::MaxItems
                )
            })
            .map(|d| d.cli_name.as_str())
            .collect();
        if !used.is_empty() {
            return Err(Failure::usage(format!(
                "Cannot specify --no-paginate along with pagination arguments: {}",
                used.join(", ")
            )));
        }
    }

    let configured_format = globals
        .cli_binary_format
        .clone()
        .or_else(|| setting(ctx, &globals, "cli_binary_format"));
    let binary_format = match configured_format.as_deref() {
        None | Some("base64") => BinaryFormat::Base64,
        Some("raw-in-base64-out") => BinaryFormat::RawInBase64Out,
        Some(other) => {
            return Err(Failure::usage(format!(
                "aws: error: invalid cli_binary_format: {other}"
            )));
        }
    };

    // Build parameters in model member order, like `_build_call_parameters`.
    let mut by_member: std::collections::HashMap<String, Value> = Default::default();
    let mut pagination = PaginationArgs::default();
    let mut cli_input: Option<String> = None;
    for (idx, value) in &parsed {
        let def = &table[*idx];
        let pctx = ParamContext {
            cli_name: &def.cli_name,
            binary_format,
        };
        match (&def.target, value) {
            (ArgTarget::Member(name), ArgValue::Bool(b)) => {
                by_member.insert(name.clone(), Value::Bool(*b));
            }
            (ArgTarget::Member(name), ArgValue::Words(words)) => {
                let shape = input
                    .as_ref()
                    .and_then(|s| s.member_named(name))
                    .expect("member from arg table");
                let v = params::unpack(&shape, words, &pctx)
                    .map_err(|e| Failure::usage(e.to_string()))?;
                by_member.insert(name.clone(), v);
            }
            (target, ArgValue::Words(words)) => {
                let w = words[0].clone();
                let num = |w: &str| {
                    w.parse::<i64>().map_err(|_| {
                        Failure::usage(format!(
                            "argument {}: invalid int value: '{w}'",
                            def.cli_name
                        ))
                    })
                };
                match target {
                    ArgTarget::StartingToken => pagination.starting_token = Some(w),
                    ArgTarget::PageSize => pagination.page_size = Some(num(&w)?),
                    ArgTarget::MaxItems => pagination.max_items = Some(num(&w)?.max(0) as usize),
                    ArgTarget::CliInputJson => cli_input = Some(w),
                    ArgTarget::Member(_) => unreachable!(),
                }
            }
            _ => {}
        }
    }
    let mut call_params = crate::value::Map::new();
    if let Some(shape) = &input {
        for (name, _) in shape.members() {
            if let Some(v) = by_member.remove(name) {
                call_params.insert(name.to_string(), v);
            }
        }
    }
    if let Some(raw) = cli_input {
        merge_cli_input(&raw, &mut call_params, binary_format)?;
    }
    if let Some(shape) = &input {
        let missing: Vec<String> = shape
            .required()
            .into_iter()
            .filter(|r| !call_params.contains_key(*r))
            .filter(|r| {
                shape
                    .member_named(r)
                    .is_some_and(|m| !m.bool_trait("idempotencyToken"))
            })
            .map(|r| format!("--{}", cli_name(r)))
            .collect();
        if !missing.is_empty() {
            return Err(Failure::usage(format!(
                "aws: error: the following arguments are required: {}",
                missing.join(", ")
            )));
        }
        // `generate_idempotent_uuid`
        for (name, m) in shape.members() {
            if m.bool_trait("idempotencyToken") && !call_params.contains_key(name) {
                call_params.insert(name.to_string(), Value::Str((ctx.uuid)()));
            }
        }
    }
    let call_params = Value::Map(call_params);

    let client = build_client(ctx, &globals, &model, &op)?;
    let call = |p: &Value, err: &mut dyn Write| {
        client
            .call_json(op_name, p, err)
            .map_err(|e| client_failure(e, &op.name))
    };
    let format = match globals
        .output
        .clone()
        .or_else(|| setting(ctx, &globals, "output"))
    {
        None => OutputFormat::Json,
        Some(f) => OutputFormat::parse(&f).ok_or_else(|| {
            Failure::usage(format!(
                "aws: error: argument --output: Invalid choice: '{f}' (choose from json, text, off)"
            ))
        })?,
    };

    let user_gave_token = paginator.as_ref().is_some_and(|p| {
        p.input_tokens.iter().any(|t| call_params.get(t).is_some())
            || p.limit_key
                .as_ref()
                .is_some_and(|k| call_params.get(k).is_some())
    });
    let result = match &paginator {
        Some(cfg) if !globals.no_paginate && !user_gave_token => {
            paginate::paginate(cfg, &call_params, &pagination, |p| call(p, err))?
        }
        _ => call(&call_params, err)?,
    };

    let result = match &globals.query {
        Some(q) => output::apply_query(q, &result).map_err(Failure::usage)?,
        None => result,
    };
    let mut rendered = Vec::new();
    output::write(format, &result, &mut rendered).map_err(Failure::general)?;
    emit(ctx, &globals, &profile_name(ctx, &globals), &rendered, out).map_err(Failure::general)
}

/// `settings`: profile value from the shared config.
fn setting(ctx: &Context, globals: &Globals, key: &str) -> Option<String> {
    let profile = profile_name(ctx, globals);
    ctx.config.get(&profile, key)
}

fn profile_name(ctx: &Context, globals: &Globals) -> String {
    globals.profile.clone().unwrap_or_else(|| {
        (ctx.env)("AWS_PROFILE")
            .or_else(|| (ctx.env)("AWS_DEFAULT_PROFILE"))
            .unwrap_or_else(|| "default".into())
    })
}

/// `--cli-input-json`: fills in parameters not given on the command line.
fn merge_cli_input(
    raw: &str,
    params: &mut crate::value::Map,
    binary_format: BinaryFormat,
) -> Result<(), Failure> {
    let pctx = ParamContext {
        cli_name: "--cli-input-json",
        binary_format,
    };
    let text =
        match params::load_paramfile(raw, &pctx).map_err(|e| Failure::usage(e.to_string()))? {
            Some(params::Loaded::Text(t)) => t,
            Some(params::Loaded::Bytes(b)) => String::from_utf8_lossy(&b).into_owned(),
            None => raw.to_string(),
        };
    let doc: J = serde_json::from_str(&text)
        .map_err(|e| Failure::usage(format!("Invalid JSON received: {e}")))?;
    let J::Object(obj) = doc else {
        return Err(Failure::usage("Invalid JSON received: expected an object"));
    };
    for (k, v) in obj {
        params.entry(k).or_insert_with(|| Value::from_json(&v));
    }
    Ok(())
}

fn print_op_help(op: &Operation, table: &[cli::ArgDef], out: &mut dyn Write) {
    let _ = writeln!(out, "{}\n", cli_name(&op.name));
    let doc = strip_tags(op.documentation());
    if !doc.is_empty() {
        let _ = writeln!(out, "{doc}\n");
    }
    let _ = writeln!(out, "Options:");
    let input = op.input();
    for def in table {
        let ty = match (&def.target, &input) {
            (ArgTarget::Member(m), Some(s)) => s
                .member_named(m)
                .map(|x| x.type_name().to_string())
                .unwrap_or_default(),
            _ => "string".into(),
        };
        let req = if def.required { " (required)" } else { "" };
        let _ = writeln!(out, "  {} ({ty}){req}", def.cli_name);
    }
}

fn strip_tags(s: &str) -> String {
    let re = regex::Regex::new(r"<[^>]+>").unwrap();
    re.replace_all(s, "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolve region and credentials and build the API client.
fn build_client<'a>(
    ctx: &'a Context<'a>,
    globals: &Globals,
    model: &'a ServiceModel,
    op: &Operation,
) -> Result<Client<'a>, Failure> {
    let profile = profile_name(ctx, globals);
    if globals.profile.is_some() && ctx.config.profile(&profile).is_none() {
        return Err(Failure::config(format!(
            "The config profile ({profile}) could not be found"
        )));
    }
    let region = resolve_region(ctx, globals, &profile).ok_or_else(|| {
        Failure::config(
            "You must specify a region. You can also configure your region by running \"aws configure\".",
        )
    })?;
    let unsigned = globals.no_sign_request || client::is_unsigned(op);
    let credentials = if unsigned {
        None
    } else {
        let resolver = Resolver::new(ctx, &profile, globals.profile.is_some(), Some(&region));
        Some(
            resolver
                .resolve()
                .map_err(|e| Failure {
                    code: e.exit_code(),
                    message: e.to_string(),
                })?
                .0,
        )
    };
    let flag = |env: &str, key: &str| {
        ctx.env_var(env)
            .or_else(|| ctx.config.get(&profile, key))
            .is_some_and(|v| v.eq_ignore_ascii_case("true"))
    };
    let config = ClientConfig {
        region,
        endpoint_url: globals.endpoint_url.clone(),
        use_fips: flag("AWS_USE_FIPS_ENDPOINT", "use_fips_endpoint"),
        use_dualstack: flag("AWS_USE_DUALSTACK_ENDPOINT", "use_dualstack_endpoint"),
        debug: globals.debug,
    };
    Client::new(ctx, model, config, credentials).map_err(Failure::general)
}

/// `--region` -> `AWS_REGION` -> `AWS_DEFAULT_REGION` -> profile `region`.
fn resolve_region(ctx: &Context, globals: &Globals, profile: &str) -> Option<String> {
    globals
        .region
        .clone()
        .or_else(|| ctx.env_var("AWS_REGION"))
        .or_else(|| ctx.env_var("AWS_DEFAULT_REGION"))
        .or_else(|| ctx.config.get(profile, "region"))
        .filter(|r| !r.is_empty())
        .or_else(|| crate::credentials::metadata::instance_region(ctx))
}

/// Pager command (`AWS_PAGER` -> `cli_pager` -> `PAGER` -> `less`); an empty
/// value disables it.
pub fn pager_command(ctx: &Context, globals: &Globals, profile: &str) -> Option<String> {
    if globals.no_cli_pager {
        return None;
    }
    let cmd = (ctx.env)("AWS_PAGER")
        .or_else(|| {
            ctx.config
                .profile(profile)
                .and_then(|p| p.get("cli_pager").cloned())
        })
        .or_else(|| (ctx.env)("PAGER"))
        .unwrap_or_else(|| "less".into());
    let cmd = cmd.trim();
    (!cmd.is_empty()).then(|| cmd.to_string())
}

/// Write command output, through the pager when stdout is a terminal.
fn emit(
    ctx: &Context,
    globals: &Globals,
    profile: &str,
    data: &[u8],
    out: &mut dyn Write,
) -> std::io::Result<()> {
    let pager = if ctx.stdout_is_tty && !data.is_empty() {
        pager_command(ctx, globals, profile)
    } else {
        None
    };
    let Some(cmd) = pager else {
        return out.write_all(data);
    };
    let mut command = std::process::Command::new("sh");
    command
        .arg("-c")
        .arg(&cmd)
        .stdin(std::process::Stdio::piped());
    if (ctx.env)("LESS").is_none() {
        command.env("LESS", "FRX");
    }
    match command.spawn() {
        Ok(mut child) => {
            if let Some(mut stdin) = child.stdin.take() {
                // The user may quit the pager early; a broken pipe is fine.
                let _ = stdin.write_all(data);
            }
            child.wait().map(|_| ())
        }
        Err(_) => out.write_all(data),
    }
}

fn client_failure(e: ClientError, op_name: &str) -> Failure {
    match e {
        ClientError::Service(s) => Failure {
            code: exit::SERVICE,
            message: format!(
                "\nAn error occurred ({}) when calling the {op_name} operation: {}",
                s.code, s.message
            ),
        },
        ClientError::Params(m) => Failure::usage(format!("Parameter validation failed: {m}")),
        other => Failure::general(other),
    }
}

/// Build a real context: discovered data dir, shared config from disk,
/// process env, HTTPS transport, wall clock.
pub fn default_loader() -> Result<Loader, String> {
    Loader::discover().map_err(|e| e.to_string())
}

pub fn random_uuid() -> String {
    let mut b: [u8; 16] = crate::oauth::os_random(16)
        .try_into()
        .expect("16 random bytes");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}
