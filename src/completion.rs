//! Shell completion (`awsr_completer`, used like awscli's `aws_completer`):
//! completes service commands, operations, waiters, option names and the
//! values of options with a fixed set of choices, from the local models only.
//! awscli's server-side completion of resource names is not implemented.

use crate::cli::{self, ArgKind, ArgTarget};
use crate::config::SharedConfig;
use crate::loader::Loader;
use crate::paginate::PaginatorConfig;
use serde_json::Value as J;

/// Global options that take a value, with their choices (empty: free text).
fn global_value_choices(flag: &str, loader: &Loader, config: &SharedConfig) -> Option<Vec<String>> {
    let fixed = |v: &[&str]| Some(v.iter().map(|s| s.to_string()).collect());
    match flag {
        "--output" => fixed(crate::output::FORMAT_NAMES),
        "--color" => fixed(&["on", "off", "auto"]),
        "--cli-binary-format" => fixed(&["base64", "raw-in-base64-out"]),
        "--cli-error-format" => fixed(&["legacy", "json", "yaml", "text", "table", "enhanced"]),
        "--profile" => Some(config.profiles.keys().cloned().collect()),
        "--region" => Some(regions(loader)),
        "--endpoint-url"
        | "--query"
        | "--ca-bundle"
        | "--cli-read-timeout"
        | "--cli-connect-timeout" => Some(Vec::new()),
        _ => None,
    }
}

const GLOBAL_FLAGS: &[&str] = &[
    "--debug",
    "--endpoint-url",
    "--no-verify-ssl",
    "--no-paginate",
    "--output",
    "--query",
    "--profile",
    "--region",
    "--version",
    "--color",
    "--no-sign-request",
    "--ca-bundle",
    "--cli-read-timeout",
    "--cli-connect-timeout",
    "--cli-binary-format",
    "--no-cli-pager",
    "--cli-error-format",
    "--help",
];

fn regions(loader: &Loader) -> Vec<String> {
    let mut out: Vec<String> = loader
        .partitions()
        .ok()
        .and_then(|p| p.get("partitions").and_then(J::as_array).cloned())
        .into_iter()
        .flatten()
        .filter_map(|p| p.get("regions").and_then(J::as_object).cloned())
        .flat_map(|r| r.keys().cloned().collect::<Vec<_>>())
        .filter(|r| !r.starts_with("aws-"))
        .collect();
    out.sort();
    out
}

/// Candidates for the word under the cursor. `line` is the command line up
/// to the cursor (`COMP_LINE[..COMP_POINT]`), starting with the program name.
pub fn complete(loader: &Loader, config: &SharedConfig, line: &str) -> Vec<String> {
    let mut words: Vec<&str> = line.split_whitespace().collect();
    let current = if line.ends_with(char::is_whitespace) || words.is_empty() {
        ""
    } else {
        words.pop().unwrap_or("")
    };
    // Skip the program name, then split off global options and their values.
    let mut positionals: Vec<&str> = Vec::new();
    let mut used: Vec<&str> = Vec::new();
    let mut i = 1;
    while i < words.len() {
        let w = words[i];
        // command, operation (or `wait` and the waiter name); words after
        // that are option values.
        let wanted = if positionals.get(1) == Some(&"wait") {
            3
        } else {
            2
        };
        if w.starts_with("--") {
            used.push(w);
            if global_value_choices(w, loader, config).is_some() {
                i += 1;
            }
        } else if positionals.len() < wanted {
            positionals.push(w);
        }
        i += 1;
    }
    let prev = words.get(1..).and_then(|w| w.last()).copied().unwrap_or("");
    if let Some(choices) = global_value_choices(prev, loader, config) {
        return filter(choices, current);
    }

    let mut candidates = Vec::new();
    let command = positionals.first().copied();
    let Some(command) = command else {
        if current.starts_with('-') {
            return filter(
                GLOBAL_FLAGS.iter().map(|s| s.to_string()).collect(),
                current,
            );
        }
        let mut names = loader.command_names();
        names.extend([
            "help".to_string(),
            "login".to_string(),
            "logout".to_string(),
        ]);
        names.sort();
        return filter(names, current);
    };
    let Some(dir) = loader.dir_for_command(command) else {
        return Vec::new();
    };
    let Ok(model) = loader.service_model(&dir) else {
        return Vec::new();
    };
    let commands = crate::customize::operation_commands(&model, command);
    let waiters = loader
        .load_json(&dir, "waiters-2.json")
        .ok()
        .flatten()
        .map(|d| crate::waiter::waiters(&d))
        .unwrap_or_default();

    // Which operation (and argument table) the line is in.
    let (op_name, cli_op) = match positionals.get(1).copied() {
        None => {
            if current.starts_with('-') {
                return filter(
                    GLOBAL_FLAGS.iter().map(|s| s.to_string()).collect(),
                    current,
                );
            }
            candidates.extend(
                commands
                    .iter()
                    .filter(|c| !c.hidden)
                    .map(|c| c.cli_name.clone()),
            );
            if !waiters.is_empty() {
                candidates.push("wait".into());
            }
            if command == "sso" {
                candidates.extend(["login".to_string(), "logout".to_string()]);
            }
            candidates.push("help".into());
            candidates.sort();
            return filter(candidates, current);
        }
        Some("wait") if !waiters.is_empty() => match positionals.get(2) {
            None => {
                let names = waiters
                    .iter()
                    .map(|w| crate::waiter::waiter_cli_name(&w.name))
                    .collect();
                return filter(names, current);
            }
            Some(name) => match waiters
                .iter()
                .find(|w| crate::waiter::waiter_cli_name(&w.name) == *name)
            {
                Some(w) => (w.operation.clone(), name.to_string()),
                None => return Vec::new(),
            },
        },
        Some(sub) => match commands.iter().find(|c| c.cli_name == sub) {
            Some(c) => (c.operation.clone(), sub.to_string()),
            None => return Vec::new(),
        },
    };
    let Ok(op) = model.operation(&op_name) else {
        return Vec::new();
    };
    let paginator = loader
        .load_json(&dir, "paginators-1.json")
        .ok()
        .flatten()
        .and_then(|d| PaginatorConfig::for_operation(&d, &op_name));
    let input = op.input();
    let mut table = cli::arg_table(
        input.as_ref(),
        &cli::TableOptions {
            paginator: paginator.as_ref(),
            no_paginate: used.contains(&"--no-paginate"),
            outfile: op.streaming_output_member().is_some(),
        },
    );
    crate::customize::customize_arg_table(command, &cli_op, &mut table);

    // The value of an operation option: enum values.
    if let Some(def) = table.iter().find(|d| d.cli_name == prev) {
        match (&def.kind, &def.target) {
            (ArgKind::Single | ArgKind::List, ArgTarget::Member(m)) => {
                let values = input
                    .as_ref()
                    .and_then(|s| s.member_named(m))
                    .map(|s| match s.type_name() {
                        "list" => s.member().map(|m| m.enum_values()).unwrap_or_default(),
                        _ => s.enum_values(),
                    })
                    .unwrap_or_default();
                return filter(values.iter().map(|v| v.to_string()).collect(), current);
            }
            (ArgKind::OptionalValue, ArgTarget::GenerateCliSkeleton) => {
                return filter(
                    cli::SKELETON_CHOICES
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    current,
                );
            }
            (ArgKind::Boolean(_), _) => {}
            _ => return Vec::new(),
        }
    }
    if current.is_empty() || current.starts_with('-') {
        candidates.extend(
            table
                .iter()
                .filter(|d| !d.hidden && d.cli_name.starts_with("--"))
                .filter(|d| !used.contains(&d.cli_name.as_str()))
                .map(|d| d.cli_name.clone()),
        );
        candidates.extend(
            GLOBAL_FLAGS
                .iter()
                .filter(|f| !used.contains(f))
                .map(|s| s.to_string()),
        );
    }
    filter(candidates, current)
}

fn filter(mut candidates: Vec<String>, prefix: &str) -> Vec<String> {
    candidates.retain(|c| c.starts_with(prefix));
    candidates.dedup();
    candidates
}

#[cfg(test)]
mod tests {
    #[test]
    fn filters_by_prefix() {
        assert_eq!(
            super::filter(vec!["ab".into(), "ac".into(), "b".into()], "a"),
            vec!["ab".to_string(), "ac".to_string()]
        );
    }
}
