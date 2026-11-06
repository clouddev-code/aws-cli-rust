//! `credential_process` (botocore `ProcessProvider`).

use super::CredentialError;
use crate::sigv4::Credentials;
use std::process::Command;

fn process_err(m: impl Into<String>) -> CredentialError {
    CredentialError::Retrieval(format!(
        "Error when retrieving credentials from custom-process: {}",
        m.into()
    ))
}

/// `ProcessProvider`: run the command (no shell) and read Version 1 JSON.
pub fn run_process(cmd: &str) -> Result<Credentials, CredentialError> {
    let argv = shlex_split(cmd);
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| process_err("empty command"))?;
    let out = Command::new(prog)
        .args(args)
        .output()
        .map_err(|e| process_err(e.to_string()))?;
    if !out.status.success() {
        return Err(process_err(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    parse_process_output(&out.stdout)
}

pub fn parse_process_output(stdout: &[u8]) -> Result<Credentials, CredentialError> {
    let v: serde_json::Value =
        serde_json::from_slice(stdout).map_err(|e| process_err(format!("invalid JSON: {e}")))?;
    if v.get("Version").and_then(|x| x.as_i64()) != Some(1) {
        return Err(process_err("Unsupported version for credential process"));
    }
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    Ok(Credentials {
        access_key: field("AccessKeyId").ok_or_else(|| process_err("missing AccessKeyId"))?,
        secret_key: field("SecretAccessKey")
            .ok_or_else(|| process_err("missing SecretAccessKey"))?,
        session_token: field("SessionToken"),
    })
}

/// POSIX-style word splitting with single/double quotes and backslashes.
pub fn shlex_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    cur.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                cur.push(n);
                            }
                        }
                        c => cur.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_output_and_shlex() {
        let c = parse_process_output(
            br#"{"Version":1,"AccessKeyId":"A","SecretAccessKey":"S","SessionToken":"T"}"#,
        )
        .unwrap();
        assert_eq!(c.session_token.as_deref(), Some("T"));
        assert!(parse_process_output(br#"{"Version":2}"#).is_err());
        assert_eq!(
            shlex_split(r#"/bin/helper --name "a b" 'c d' e\ f"#),
            ["/bin/helper", "--name", "a b", "c d", "e f"]
        );
    }
}
