//! Hand-written commands that are not generated from service models
//! (awscli `customizations/`).

use crate::cli::Globals;
use crate::context::Context;
use crate::credentials::CredentialError;
use crate::credentials::sso::{self, PendingAuthorization, SsoConfig};
use crate::driver::Failure;
use std::io::Write;

fn credential_failure(e: CredentialError) -> Failure {
    Failure {
        code: e.exit_code(),
        message: e.to_string(),
    }
}

/// `aws logout` (customizations/login/logout.py): remove the cached
/// `aws login` session of the profile, or with `--all` every cached login
/// session.
pub(crate) fn logout(
    ctx: &Context,
    profile: &str,
    tokens: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), Failure> {
    let mut all = false;
    for t in tokens {
        match t.as_str() {
            "--all" => all = true,
            other => {
                return Err(Failure::usage(format!(
                    "aws: error: Unknown options: {other}"
                )));
            }
        }
    }
    let cache = crate::credentials::login::token_cache(ctx);
    if all {
        let removed = cache.remove_matching(|doc| {
            doc.get("accessToken")
                .and_then(|t| t.as_object())
                .is_some_and(|t| t.contains_key("accessKeyId"))
        });
        if removed > 0 {
            let plural = if removed == 1 { "" } else { "s" };
            let _ = writeln!(
                out,
                "Removed {removed} cached login credential{plural}. Note, any local developer tools that have already loaded access tokens may continue to use them until their expiration. Access tokens expire in 15 minutes."
            );
        } else {
            let _ = writeln!(out, "No cached login session tokens found.");
        }
        return Ok(());
    }
    let Some(session) = ctx.config.get(profile, "login_session") else {
        let _ = writeln!(
            err,
            "warning: no login session found for profile '{profile}'"
        );
        return Ok(());
    };
    let _ = cache.remove(&crate::credentials::cache::sha256_hex(&session));
    let _ = writeln!(
        out,
        "Removed cached login credentials for profile '{profile}'. Note, any local developer tools that have already loaded the access token may continue to use it until its expiration. Access tokens expire in 15 minutes."
    );
    Ok(())
}

/// `aws sso login` / `aws sso logout`.
pub(crate) fn sso(
    ctx: &Context,
    globals: &Globals,
    profile: &str,
    sub: &str,
    tokens: &[String],
    out: &mut dyn Write,
) -> Result<(), Failure> {
    if sub == "logout" {
        if let Some(t) = tokens.first() {
            return Err(Failure::usage(format!("aws: error: Unknown options: {t}")));
        }
        sso::logout(ctx);
        return Ok(());
    }

    let mut sso_session: Option<String> = None;
    let mut no_browser = false;
    let mut use_device_code = false;
    let mut redirect_port: Option<u16> = None;
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i].as_str() {
            "--sso-session" => {
                i += 1;
                sso_session = Some(tokens.get(i).cloned().ok_or_else(|| {
                    Failure::usage("aws: error: argument --sso-session: expected one argument")
                })?);
            }
            "--no-browser" => no_browser = true,
            "--use-device-code" => use_device_code = true,
            "--redirect-port" => {
                i += 1;
                redirect_port = Some(parse_port(tokens.get(i))?);
            }
            "help" => {
                let _ = writeln!(
                    out,
                    "sso login [--sso-session <name>] [--no-browser] [--use-device-code] [--redirect-port <port>]"
                );
                return Ok(());
            }
            other => {
                return Err(Failure::usage(format!(
                    "aws: error: Unknown options: {other}"
                )));
            }
        }
        i += 1;
    }
    let cfg = login_config(ctx, profile, sso_session, globals.profile.is_some())?;
    let mut on_pending = |p: &PendingAuthorization| {
        let opened = !no_browser && (ctx.open_browser)(&p.verification_uri_complete);
        let intro = if opened {
            "Attempting to open your default browser.\nIf the browser does not open or you wish to use a different device to authorize this request, open the following URL:\n"
        } else {
            "Browser will not be automatically opened.\nPlease visit the following URL:\n"
        };
        let _ = write!(
            out,
            "{intro}\n{}\n\nThen enter the code:\n\n{}\n\nAlternatively, you may visit the following URL which will autofill the code upon loading:\n{}\n",
            p.verification_uri, p.user_code, p.verification_uri_complete
        );
        let _ = out.flush();
    };
    if cfg.session_name.is_some() && !use_device_code {
        // Authorization code + PKCE, the awscli default for sso-session profiles.
        let mut on_authorize = |url: &str| browser_message(ctx, out, url, no_browser);
        sso::auth_code_login(ctx, &cfg, redirect_port, &mut on_authorize)
            .map_err(credential_failure)?;
    } else {
        sso::device_login(ctx, &cfg, &mut on_pending).map_err(credential_failure)?;
    }
    let _ = writeln!(out, "Successfully logged into Start URL: {}", cfg.start_url);
    Ok(())
}

fn parse_port(v: Option<&String>) -> Result<u16, Failure> {
    let v = v.ok_or_else(|| {
        Failure::usage("aws: error: argument --redirect-port: expected one argument")
    })?;
    v.parse().ok().filter(|p| *p > 0).ok_or_else(|| {
        Failure::usage(format!(
            "aws: error: argument --redirect-port: invalid port: '{v}'"
        ))
    })
}

/// `OpenBrowserHandler` / `PrintOnlyHandler` for flows without a user code.
fn browser_message(ctx: &Context, out: &mut dyn Write, url: &str, no_browser: bool) {
    let opened = !no_browser && (ctx.open_browser)(url);
    let intro = if opened {
        "Attempting to open your default browser.\nIf the browser does not open, open the following URL:\n"
    } else {
        "Browser will not be automatically opened.\nPlease visit the following URL:\n"
    };
    let _ = write!(out, "{intro}\n{url}\n");
    let _ = out.flush();
}

/// `aws login`: browser sign-in with authorization code + PKCE (or
/// `--remote` to paste a code), then record the session in the profile.
pub(crate) fn login(
    ctx: &Context,
    globals: &Globals,
    profile: &str,
    tokens: &[String],
    out: &mut dyn Write,
) -> Result<(), Failure> {
    use crate::credentials::login::{self, LoginOptions, LoginPrompt};
    let mut remote = false;
    let mut redirect_port = None;
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i].as_str() {
            "--remote" => remote = true,
            "--redirect-port" => {
                i += 1;
                redirect_port = Some(parse_port(tokens.get(i))?);
            }
            "help" => {
                let _ = writeln!(out, "login [--remote] [--redirect-port <port>]");
                return Ok(());
            }
            other => {
                return Err(Failure::usage(format!(
                    "aws: error: Unknown options: {other}"
                )));
            }
        }
        i += 1;
    }

    // `ensure_profile_does_not_have_existing_credentials`
    let section = ctx.config.profile(profile).unwrap_or_default();
    let style = [
        ("web_identity_token_file", "Web Identity"),
        ("sso_role_name", "SSO"),
        ("sso_account_id", "SSO"),
        ("aws_access_key_id", "Access Key"),
        ("role_arn", "Assume Role"),
        ("credential_process", "Credential Process"),
    ]
    .into_iter()
    .find(|(k, _)| section.contains_key(*k))
    .map(|(_, s)| s);
    if let Some(style) = style {
        return Err(Failure::config(format!(
            "Profile '{profile}' is already configured with {style} credentials.\n\nYou may run 'aws login --profile new-profile-name' to create a new profile with the specified name. Otherwise you must first manually remove the existing credentials from '{profile}'.\n"
        )));
    }

    // `_resolve_region`, prompting when nothing is configured.
    let configured = globals
        .region
        .clone()
        .or_else(|| ctx.env_var("AWS_REGION"))
        .or_else(|| ctx.env_var("AWS_DEFAULT_REGION"))
        .or_else(|| section.get("region").cloned());
    let prompted_region = configured.is_none();
    let region = configured.unwrap_or_else(|| {
        let answer = (ctx.prompt)("No AWS region has been configured.\nAWS Region [us-east-1]: ")
            .unwrap_or_default();
        let answer = answer.trim();
        if answer.is_empty() {
            "us-east-1".to_string()
        } else {
            answer.to_string()
        }
    });

    let opts = LoginOptions {
        remote,
        redirect_port,
        region: region.clone(),
        endpoint_url: globals.endpoint_url.clone(),
    };
    let mut show = |p: LoginPrompt| match p {
        LoginPrompt::OpenBrowser(url) => browser_message(ctx, out, url, false),
        LoginPrompt::PasteCode(url) => {
            let _ = write!(
                out,
                "Browser will not be automatically opened.\nPlease visit the following URL on a device with a browser and sign in:\n\n{url}\n"
            );
            let _ = out.flush();
        }
    };
    let (session, mut token) =
        login::interactive_login(ctx, &opts, &mut show).map_err(credential_failure)?;

    if let Some(existing) = section.get("login_session")
        && existing != &session
    {
        let answer = (ctx.prompt)(&format!(
            "\nProfile {profile} is already configured to use session {existing}. Do you want to overwrite it to use {session} instead? (y/n): "
        ))
        .unwrap_or_default();
        let answer = answer.trim().to_ascii_lowercase();
        if answer != "y" && answer != "yes" {
            return Ok(());
        }
    }
    if globals.endpoint_url.is_none() {
        token["signInRegion"] = serde_json::Value::String(region.clone());
    }
    login::token_cache(ctx)
        .set(&crate::credentials::cache::sha256_hex(&session), &token)
        .map_err(|e| Failure::general(format!("Unable to write the login token cache: {e}")))?;
    let section_name = if profile == "default" {
        "default".to_string()
    } else {
        format!("profile {profile}")
    };
    let mut values = vec![("login_session", session.as_str())];
    if prompted_region {
        values.push(("region", region.as_str()));
    }
    crate::config::update_config_file(&ctx.config_file(), &section_name, &values)
        .map_err(|e| Failure::general(format!("Unable to update the config file: {e}")))?;
    let _ = writeln!(
        out,
        "\nUpdated profile {profile} to use {session} credentials."
    );
    if profile != "default" {
        let _ = writeln!(
            out,
            "Use \"--profile {profile}\" to use the new credentials, such as \"aws sts get-caller-identity --profile {profile}\""
        );
    }
    Ok(())
}

/// `BaseSSOCommand._get_sso_config`.
fn login_config(
    ctx: &Context,
    profile: &str,
    session: Option<String>,
    explicit: bool,
) -> Result<SsoConfig, Failure> {
    let section = ctx.config.profile(profile);
    if explicit && section.is_none() {
        // botocore `ProfileNotFound`.
        return Err(Failure::general(format!(
            "The config profile ({profile}) could not be found"
        )));
    }
    let section = section.unwrap_or_default();
    let session = session.or_else(|| section.get("sso_session").cloned());
    let (source, session_name) = match &session {
        Some(name) => (
            ctx.config.sso_sessions.get(name).cloned().ok_or_else(|| {
                Failure::config(format!(
                    "The specified sso-session does not exist: \"{name}\""
                ))
            })?,
            Some(name.clone()),
        ),
        None => (section, None),
    };
    let missing: Vec<&str> = ["sso_start_url", "sso_region"]
        .into_iter()
        .filter(|k| !source.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        let mut msg = format!(
            "Missing the following required SSO configuration values: {}. ",
            missing.join(", ")
        );
        msg.push_str("To make sure this profile is properly configured to use SSO, please run: aws configure sso");
        return Err(Failure::config(msg));
    }
    let registration_scopes = source
        .get("sso_registration_scopes")
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(SsoConfig {
        start_url: source["sso_start_url"].clone(),
        region: source["sso_region"].clone(),
        session_name,
        registration_scopes,
    })
}
