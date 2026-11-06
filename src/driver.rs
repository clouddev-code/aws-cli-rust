//! Command execution (awscli `clidriver.py`): resolve the command, build
//! parameters, resolve region/credentials/endpoint, sign, send, format.

use crate::cli::params::{self, BinaryFormat, ParamContext};
use crate::cli::{self, ArgTarget, ArgValue, Globals, UsageError};
use crate::client::{self, ChecksumWhen, Client, ClientConfig, ClientError, RetryConfig};
use crate::credentials::Resolver;
use crate::loader::Loader;
use crate::model::{Operation, ServiceModel};
use crate::naming::cli_name;
use crate::output::{self, OutputFormat};
use crate::paginate::{self, PaginationArgs, PaginatorConfig};
use crate::skeleton;
use crate::value::{TimestampStyle, Value};
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

    let commands = crate::customize::operation_commands(&model, command);
    let Some(sub) = positionals.get(1).map(|s| s.as_str()) else {
        return Err(Failure::usage(format!(
            "aws: error: the following arguments are required: operation (try 'awsr {command} help')"
        )));
    };
    if sub == "help" {
        let _ = writeln!(out, "Available operations for {command}:");
        for c in commands.iter().filter(|c| !c.hidden) {
            let _ = writeln!(out, "  {}", c.cli_name);
        }
        return Ok(());
    }
    let op_name = commands
        .iter()
        .find(|c| c.cli_name == sub)
        .map(|c| c.operation.as_str())
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
    let streaming_output = op.streaming_output_member();
    // `S3SelectStreamOutputArgument`: the Records payloads of the event
    // stream go to the outfile.
    let select_output = (model.service_name == "s3" && op.name == "SelectObjectContent")
        .then(|| op.output().and_then(|o| o.payload().map(str::to_string)))
        .flatten();
    let mut table = cli::arg_table(
        input.as_ref(),
        &cli::TableOptions {
            paginator: paginator.as_ref(),
            no_paginate: globals.no_paginate,
            outfile: streaming_output.is_some() || select_output.is_some(),
        },
    );
    crate::customize::customize_arg_table(command, sub, &mut table);

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
    let mut cli_input: Option<(ArgTarget, String)> = None;
    let mut skeleton: Option<String> = None;
    let mut outfile: Option<String> = None;
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
                    ArgTarget::CliInputJson | ArgTarget::CliInputYaml => {
                        if cli_input.is_some() {
                            return Err(Failure::usage(
                                "Only one --cli-input- parameter may be specified.",
                            ));
                        }
                        cli_input = Some((target.clone(), w));
                    }
                    ArgTarget::GenerateCliSkeleton => skeleton = Some(w),
                    ArgTarget::Outfile => outfile = Some(w),
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
    if let Some((target, raw)) = cli_input {
        merge_cli_input(&target, &raw, &mut call_params, binary_format)?;
    }
    match skeleton.as_deref() {
        Some("input") => {
            let _ = out.write_all(skeleton::input_json(input.as_ref()).as_bytes());
            return Ok(());
        }
        Some("yaml-input") => {
            let _ = out.write_all(skeleton::yaml_input(input.as_ref()).as_bytes());
            return Ok(());
        }
        _ => {}
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

    // `--generate-cli-skeleton output` validates the input and prints a
    // sample response instead of calling the service.
    if skeleton.as_deref() == Some("output") {
        if let Some(shape) = &input {
            crate::validate::validate(&call_params, shape)
                .map_err(|r| client_failure(ClientError::Validation(r), &op.name))?;
        }
        let result = op.output().map_or(J::Object(Default::default()), |s| {
            skeleton::generate(&s, true)
        });
        return render_output(ctx, &globals, &op.name, &result, out);
    }

    let client = build_client(ctx, &globals, &model, &op)?;
    let call = |p: &Value, err: &mut dyn Write| {
        let mut v = client
            .call(op_name, p, None, err)
            .map_err(|e| client_failure(e, &op.name))?;
        // `StreamingOutputArgument.save_file`: the payload goes to the file
        // and is left out of the printed response.
        if let (Some(member), Some(path), Value::Map(m)) = (streaming_output, &outfile, &mut v)
            && let Some(Value::Bytes(bytes)) = m.shift_remove(member)
        {
            std::fs::write(path, bytes)
                .map_err(|e| Failure::general(format!("[Errno] {e}: '{path}'")))?;
        }
        if let (Some(member), Some(path), Value::Map(m)) = (&select_output, &outfile, &mut v)
            && let Some(Value::List(events)) = m.shift_remove(member.as_str())
        {
            let mut data = Vec::new();
            for event in &events {
                if let Some(Value::Bytes(b)) = event.get("Records").and_then(|r| r.get("Payload")) {
                    data.extend(b);
                }
            }
            std::fs::write(path, data)
                .map_err(|e| Failure::general(format!("[Errno] {e}: '{path}'")))?;
        }
        Ok::<_, Failure>(v.to_json(TimestampStyle::Iso8601))
    };
    // Fail on a bad --output / --color before calling the service.
    output_settings(ctx, &globals, &op.name)?;

    // A manual (undocumented) paging argument turns pagination off
    // (`check_should_enable_pagination`), as does kinesis list-streams
    // `--exclusive-start-stream-name` (kinesis.py).
    let user_gave_token = paginator.as_ref().is_some_and(|p| {
        p.input_tokens.iter().any(|t| call_params.get(t).is_some())
            || p.limit_key
                .as_ref()
                .is_some_and(|k| call_params.get(k).is_some())
    }) || (command == "kinesis"
        && op.name == "ListStreams"
        && call_params.get("ExclusiveStartStreamName").is_some());
    let result = match &paginator {
        Some(cfg) if !globals.no_paginate && !user_gave_token => {
            paginate::paginate(cfg, &call_params, &pagination, |p| call(p, err))?
        }
        _ => call(&call_params, err)?,
    };
    render_output(ctx, &globals, &op.name, &result, out)
}

/// `--output` (or the profile's `output`) and the table style.
fn output_settings(
    ctx: &Context,
    globals: &Globals,
    title: &str,
) -> Result<(OutputFormat, output::TableStyle), Failure> {
    let format = match globals
        .output
        .clone()
        .or_else(|| setting(ctx, globals, "output"))
    {
        None => OutputFormat::Json,
        Some(f) => OutputFormat::parse(&f).ok_or_else(|| {
            Failure::usage(format!(
                "aws: error: argument --output: Invalid choice: '{f}' (choose from {})",
                output::FORMAT_NAMES.join(", ")
            ))
        })?,
    };
    let color = match globals.color.as_deref() {
        None | Some("auto") => ctx.stdout_is_tty,
        Some("on") => true,
        Some("off") => false,
        Some(other) => {
            return Err(Failure::usage(format!(
                "aws: error: argument --color: Invalid choice: '{other}' (choose from on, off, auto)"
            )));
        }
    };
    let style = output::TableStyle {
        title: title.to_string(),
        terminal_width: ctx.terminal_width.unwrap_or(80),
        color,
    };
    Ok((format, style))
}

/// Apply `--query`, format the result and write it (through the pager).
fn render_output(
    ctx: &Context,
    globals: &Globals,
    title: &str,
    result: &J,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let (format, style) = output_settings(ctx, globals, title)?;
    let queried;
    let result = match &globals.query {
        Some(q) => {
            queried = output::apply_query(q, result).map_err(Failure::usage)?;
            &queried
        }
        None => result,
    };
    let mut rendered = Vec::new();
    output::write(format, result, &style, &mut rendered).map_err(Failure::general)?;
    emit(ctx, globals, &profile_name(ctx, globals), &rendered, out).map_err(Failure::general)
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

/// `--cli-input-json` / `--cli-input-yaml`: fills in parameters not given
/// on the command line.
fn merge_cli_input(
    target: &ArgTarget,
    raw: &str,
    params: &mut crate::value::Map,
    binary_format: BinaryFormat,
) -> Result<(), Failure> {
    let yaml = *target == ArgTarget::CliInputYaml;
    let flag = if yaml {
        "--cli-input-yaml"
    } else {
        "--cli-input-json"
    };
    let pctx = ParamContext {
        cli_name: flag,
        binary_format,
    };
    let text =
        match params::load_paramfile(raw, &pctx).map_err(|e| Failure::usage(e.to_string()))? {
            Some(params::Loaded::Text(t)) => t,
            Some(params::Loaded::Bytes(b)) => String::from_utf8_lossy(&b).into_owned(),
            None => raw.to_string(),
        };
    let doc = if yaml {
        yaml_rust2::YamlLoader::load_from_str(&text)
            .ok()
            .map(|docs| docs.first().map(yaml_to_json).unwrap_or(J::Null))
            .ok_or_else(|| {
                Failure::usage(format!(
                    "Error parsing parameter '{flag}': Invalid YAML received."
                ))
            })?
    } else {
        serde_json::from_str(&text).map_err(|_| {
            Failure::usage(format!(
                "Error parsing parameter '{flag}': Invalid JSON received."
            ))
        })?
    };
    let J::Object(obj) = doc else {
        return Err(Failure::usage(format!(
            "Error parsing parameter '{flag}': Invalid type: expecting map"
        )));
    };
    for (k, v) in obj {
        params.entry(k).or_insert_with(|| Value::from_json(&v));
    }
    Ok(())
}

/// YAML safe-load result as JSON (mapping keys become strings).
fn yaml_to_json(y: &yaml_rust2::Yaml) -> J {
    use yaml_rust2::Yaml;
    match y {
        Yaml::Real(r) => r
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or_else(|| J::String(r.clone()), J::Number),
        Yaml::Integer(i) => J::from(*i),
        Yaml::String(s) => J::String(s.clone()),
        Yaml::Boolean(b) => J::Bool(*b),
        Yaml::Array(a) => J::Array(a.iter().map(yaml_to_json).collect()),
        Yaml::Hash(h) => J::Object(
            h.iter()
                .map(|(k, v)| {
                    let key = match yaml_to_json(k) {
                        J::String(s) => s,
                        other => other.to_string(),
                    };
                    (key, yaml_to_json(v))
                })
                .collect(),
        ),
        Yaml::Null | Yaml::Alias(_) | Yaml::BadValue => J::Null,
    }
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
    let checksum_when = |env: &str, key: &str| {
        ctx.env_var(env)
            .or_else(|| ctx.config.get(&profile, key))
            .map_or(Ok(ChecksumWhen::default()), |v| {
                ChecksumWhen::parse(key, &v)
            })
            .map_err(Failure::general)
    };
    let config = ClientConfig {
        region,
        endpoint_url: globals.endpoint_url.clone(),
        use_fips: flag("AWS_USE_FIPS_ENDPOINT", "use_fips_endpoint"),
        use_dualstack: flag("AWS_USE_DUALSTACK_ENDPOINT", "use_dualstack_endpoint"),
        debug: globals.debug,
        request_checksum_calculation: checksum_when(
            "AWS_REQUEST_CHECKSUM_CALCULATION",
            "request_checksum_calculation",
        )?,
        response_checksum_validation: checksum_when(
            "AWS_RESPONSE_CHECKSUM_VALIDATION",
            "response_checksum_validation",
        )?,
        sigv4a_signing_region_set: ctx
            .env_var("AWS_SIGV4A_SIGNING_REGION_SET")
            .or_else(|| ctx.config.get(&profile, "sigv4a_signing_region_set"))
            .filter(|v| !v.is_empty()),
        retry: retry_config(ctx, &profile)?,
    };
    Client::new(ctx, model, config, credentials).map_err(Failure::general)
}

/// `AWS_RETRY_MODE` / `retry_mode` and `AWS_MAX_ATTEMPTS` / `max_attempts`.
pub(crate) fn retry_config(ctx: &Context, profile: &str) -> Result<RetryConfig, Failure> {
    let get = |env: &str, key: &str| ctx.env_var(env).or_else(|| ctx.config.get(profile, key));
    RetryConfig::parse(
        get("AWS_RETRY_MODE", "retry_mode").as_deref(),
        get("AWS_MAX_ATTEMPTS", "max_attempts").as_deref(),
    )
    .map_err(Failure::config)
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
        ClientError::Validation(r) => {
            Failure::usage(format!("\nParameter validation failed:\n{r}"))
        }
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
