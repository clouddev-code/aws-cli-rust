//! Credential providers and `sso login`/`logout` end to end, with a fake
//! transport and a throwaway HOME directory. No network is used.

mod common;

use awsr::config::{SharedConfig, parse_ini};
use awsr::credentials::cache::{sha1_hex, sha256_hex};
use awsr::driver::{self, Context};
use awsr::http::{Request, Response, Transport, TransportError};
use awsr::oauth::CallbackServer;
use chrono::{TimeZone, Utc};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Fixed clock: 2015-08-30T12:36:00Z (epoch 1440938160).
const NOW: i64 = 1440938160;

struct FakeTransport {
    requests: Mutex<Vec<Request>>,
    responses: Mutex<VecDeque<Response>>,
}

impl Transport for FakeTransport {
    fn send(&self, req: &Request) -> Result<Response, TransportError> {
        self.requests.lock().unwrap().push(req.clone());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| TransportError("connection refused".into()))
    }
}

fn ok(body: &str) -> Response {
    Response {
        status: 200,
        headers: vec![],
        body: body.as_bytes().to_vec(),
    }
}

fn error(status: u16, code: &str, body: &str) -> Response {
    Response {
        status,
        headers: vec![("x-amzn-errortype".into(), code.into())],
        body: body.as_bytes().to_vec(),
    }
}

const CALLER_IDENTITY: &str = "<GetCallerIdentityResponse><GetCallerIdentityResult><Account>123456789012</Account></GetCallerIdentityResult></GetCallerIdentityResponse>";

fn assume_role_response(key: &str) -> Response {
    ok(&format!(
        "<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>{key}</AccessKeyId>\
         <SecretAccessKey>secret-{key}</SecretAccessKey><SessionToken>token-{key}</SessionToken>\
         <Expiration>2015-08-30T13:36:00Z</Expiration></Credentials><AssumedRoleUser>\
         <Arn>arn:aws:sts::123456789012:assumed-role/r/s</Arn><AssumedRoleId>AROA:s</AssumedRoleId>\
         </AssumedRoleUser></AssumeRoleResult></AssumeRoleResponse>"
    ))
}

struct Home(PathBuf);

impl Home {
    fn new(name: &str) -> Home {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "awsr-p3-{name}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Home(dir)
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, contents).unwrap();
        p
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
    requests: Vec<Request>,
    prompts: Vec<String>,
    opened: Vec<String>,
}

struct Setup<'a> {
    home: &'a Home,
    config: &'a str,
    credentials: &'a str,
    env: Vec<(&'a str, String)>,
    prompt_answer: Option<&'a str>,
    /// When set, the fake OAuth redirect returns this code with the state
    /// taken from the URL the CLI asked the browser to open.
    callback_code: Option<&'a str>,
}

struct FakeCallback {
    opened: Arc<Mutex<Vec<String>>>,
    code: String,
}

impl CallbackServer for FakeCallback {
    fn redirect_uri(&self) -> String {
        "http://127.0.0.1:43210/oauth/callback".into()
    }
    fn wait(&self, _expected_state: &str) -> Result<Option<(String, String)>, String> {
        let url = self
            .opened
            .lock()
            .unwrap()
            .last()
            .cloned()
            .ok_or("browser was not opened")?;
        let state = awsr::oauth::query_params(&url)
            .into_iter()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v)
            .ok_or("no state in authorize URL")?;
        Ok(Some((self.code.clone(), state)))
    }
}

impl<'a> Setup<'a> {
    fn new(home: &'a Home, config: &'a str) -> Self {
        Setup {
            home,
            config,
            credentials: "",
            env: vec![("AWS_DEFAULT_REGION", "us-east-1".into())],
            prompt_answer: None,
            callback_code: None,
        }
    }

    fn run(&self, args: &[&str], responses: Vec<Response>) -> Option<Run> {
        let loader = common::loader_or_skip()?;
        let transport = FakeTransport {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into()),
        };
        let mut env: HashMap<String, String> = self
            .env
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        env.insert("HOME".into(), self.home.0.to_string_lossy().into_owned());
        let prompts = Mutex::new(Vec::new());
        let answer = self.prompt_answer.map(str::to_string);
        let opened: Arc<Mutex<Vec<String>>> = Arc::default();
        let (opened_cb, opened_browser) = (opened.clone(), opened.clone());
        let code = self.callback_code.map(str::to_string);
        let ctx = Context {
            loader,
            config: SharedConfig::from_parts(&parse_ini(self.config), parse_ini(self.credentials)),
            env: Box::new(move |k| env.get(k).cloned()),
            transport: &transport,
            now: Box::new(|| Utc.timestamp_opt(NOW, 0).unwrap()),
            uuid: Box::new(|| "11111111-1111-4111-8111-111111111111".into()),
            prompt: Box::new(|p| {
                prompts.lock().unwrap().push(p.to_string());
                answer.clone()
            }),
            sleep: Box::new(|_| {}),
            open_browser: Box::new(move |url| {
                opened_browser.lock().unwrap().push(url.to_string());
                true
            }),
            stdout_is_tty: false,
            stderr_is_tty: false,
            terminal_width: None,
            local_time: Box::new(|t| t.naive_utc()),
            random: Box::new(|n| (0..n).map(|i| (i * 31 + 7) as u8).collect()),
            callback: Box::new(move |_| match &code {
                Some(c) => Ok(Box::new(FakeCallback {
                    opened: opened_cb.clone(),
                    code: c.clone(),
                }) as Box<dyn CallbackServer>),
                None => Err("no callback in tests".into()),
            }),
        };
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = driver::run(&ctx, &argv, &mut out, &mut err);
        drop(ctx);
        Some(Run {
            code,
            stdout: String::from_utf8(out).unwrap(),
            stderr: String::from_utf8(err).unwrap(),
            requests: std::mem::take(&mut *transport.requests.lock().unwrap()),
            prompts: std::mem::take(&mut *prompts.lock().unwrap()),
            opened: std::mem::take(&mut *opened.lock().unwrap()),
        })
    }
}

fn body(r: &Request) -> String {
    String::from_utf8_lossy(&r.body).into_owned()
}

fn access_key(r: &Request) -> String {
    let auth = r.header("Authorization").unwrap_or_default();
    auth.split("Credential=")
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or_default()
        .to_string()
}

fn cache_files(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn assume_role_with_source_profile_and_cache() {
    let home = Home::new("assume");
    let mut s = Setup::new(
        &home,
        "[profile admin]\nrole_arn = arn:aws:iam::123456789012:role/admin\nsource_profile = base\nrole_session_name = me\n",
    );
    s.credentials = "[base]\naws_access_key_id = AKIDBASE\naws_secret_access_key = basesecret\n";
    let Some(r) = s.run(
        &["sts", "get-caller-identity", "--profile", "admin"],
        vec![assume_role_response("ASIAROLE"), ok(CALLER_IDENTITY)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.requests.len(), 2);
    let assume = &r.requests[0];
    assert!(body(assume).contains("Action=AssumeRole"));
    assert!(body(assume).contains("RoleSessionName=me"));
    assert_eq!(
        access_key(assume),
        "AKIDBASE",
        "AssumeRole is signed with the source profile"
    );
    assert_eq!(access_key(&r.requests[1]), "ASIAROLE");
    assert_eq!(
        r.requests[1].header("X-Amz-Security-Token"),
        Some("token-ASIAROLE")
    );

    let files = cache_files(&home.path(".aws/cli/cache"));
    assert_eq!(files.len(), 1);
    #[cfg(unix)]
    assert_eq!(mode(&files[0]), 0o600);
    let cached: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
    assert_eq!(
        cached["Credentials"]["Expiration"],
        "2015-08-30T13:36:00UTC"
    );
    assert_eq!(cached["Credentials"]["AccountId"], "123456789012");

    // Second run: served from the cache, only the API call goes out.
    let r = s
        .run(
            &["sts", "get-caller-identity", "--profile", "admin"],
            vec![ok(CALLER_IDENTITY)],
        )
        .unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.requests.len(), 1);
    assert_eq!(access_key(&r.requests[0]), "ASIAROLE");
}

#[test]
fn assume_role_cache_key_matches_aws_cli() {
    // Key = sha1(json.dumps({"RoleArn": ..., "RoleSessionName": "me"}, sort_keys=True)).
    let home = Home::new("cachekey");
    let mut s = Setup::new(
        &home,
        "[profile admin]\nrole_arn = arn:aws:iam::123456789012:role/admin\nsource_profile = base\nrole_session_name = me\n",
    );
    s.credentials = "[base]\naws_access_key_id = AKIDBASE\naws_secret_access_key = basesecret\n";
    let key =
        sha1_hex(r#"{"RoleArn": "arn:aws:iam::123456789012:role/admin", "RoleSessionName": "me"}"#);
    // A cache entry written by the real AWS CLI is reused.
    home.write(
        &format!(".aws/cli/cache/{key}.json"),
        r#"{"Credentials": {"AccessKeyId": "ASIAFROMCLI", "SecretAccessKey": "s", "SessionToken": "t", "Expiration": "2015-08-30T14:00:00UTC"}}"#,
    );
    let Some(r) = s.run(
        &["sts", "get-caller-identity", "--profile", "admin"],
        vec![ok(CALLER_IDENTITY)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.requests.len(), 1);
    assert_eq!(access_key(&r.requests[0]), "ASIAFROMCLI");
}

#[test]
fn role_chaining_with_mfa_prompt() {
    let home = Home::new("chain");
    let mut s = Setup::new(
        &home,
        "[profile first]\nrole_arn = arn:aws:iam::1:role/first\nsource_profile = base\nmfa_serial = arn:aws:iam::1:mfa/me\n\
         [profile second]\nrole_arn = arn:aws:iam::2:role/second\nsource_profile = first\nduration_seconds = 900\n",
    );
    s.credentials = "[base]\naws_access_key_id = AKIDBASE\naws_secret_access_key = basesecret\n";
    s.prompt_answer = Some("123456");
    let Some(r) = s.run(
        &["sts", "get-caller-identity", "--profile", "second"],
        vec![
            assume_role_response("ASIAFIRST"),
            assume_role_response("ASIASECOND"),
            ok(CALLER_IDENTITY),
        ],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.prompts,
        vec!["Enter MFA code for arn:aws:iam::1:mfa/me: "]
    );
    assert!(
        body(&r.requests[0])
            .contains("SerialNumber=arn%3Aaws%3Aiam%3A%3A1%3Amfa%2Fme&TokenCode=123456")
    );
    assert_eq!(access_key(&r.requests[0]), "AKIDBASE");
    assert!(body(&r.requests[1]).contains("DurationSeconds=900"));
    assert_eq!(access_key(&r.requests[1]), "ASIAFIRST");
    assert_eq!(access_key(&r.requests[2]), "ASIASECOND");
}

#[test]
fn credential_cache_memory_keeps_role_credentials_out_of_the_file_cache() {
    // aws-cli 2.42.0 `credential_cache`: each hop follows its own profile.
    let home = Home::new("credcache");
    let token = home.write("token.jwt", "eyJ.token.sig");
    let config = format!(
        "[profile admin]\nrole_arn = arn:aws:iam::1:role/admin\nsource_profile = base\nrole_session_name = me\ncredential_cache = memory\n\
         [profile filed]\nrole_arn = arn:aws:iam::1:role/filed\nsource_profile = base\nrole_session_name = me\ncredential_cache = file\n\
         [profile web]\nrole_arn = arn:aws:iam::1:role/web\nweb_identity_token_file = {}\nrole_session_name = pod\ncredential_cache = memory\n\
         [profile bad]\nrole_arn = arn:aws:iam::1:role/bad\nsource_profile = base\ncredential_cache = disk\n",
        token.display()
    );
    let mut s = Setup::new(&home, &config);
    s.credentials = "[base]\naws_access_key_id = AKIDBASE\naws_secret_access_key = basesecret\n";
    let cache_dir = home.path(".aws/cli/cache");
    for _ in 0..2 {
        let Some(r) = s.run(
            &["sts", "get-caller-identity", "--profile", "admin"],
            vec![assume_role_response("ASIAROLE"), ok(CALLER_IDENTITY)],
        ) else {
            return;
        };
        assert_eq!(r.code, 0, "{}", r.stderr);
        assert_eq!(r.requests.len(), 2, "every run calls AssumeRole");
        assert!(cache_files(&cache_dir).is_empty());
    }

    let web = ok(
        "<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult><Credentials>\
        <AccessKeyId>ASIAWEB</AccessKeyId><SecretAccessKey>s</SecretAccessKey><SessionToken>t</SessionToken>\
        <Expiration>2015-08-30T13:36:00Z</Expiration></Credentials></AssumeRoleWithWebIdentityResult>\
        </AssumeRoleWithWebIdentityResponse>",
    );
    let r = s
        .run(
            &["sts", "get-caller-identity", "--profile", "web"],
            vec![web, ok(CALLER_IDENTITY)],
        )
        .unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(access_key(&r.requests[1]), "ASIAWEB");
    assert!(cache_files(&cache_dir).is_empty());

    let r = s
        .run(
            &["sts", "get-caller-identity", "--profile", "filed"],
            vec![assume_role_response("ASIAFILE"), ok(CALLER_IDENTITY)],
        )
        .unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(cache_files(&cache_dir).len(), 1);

    let r = s
        .run(&["sts", "get-caller-identity", "--profile", "bad"], vec![])
        .unwrap();
    assert_eq!(r.code, 253);
    assert!(r.requests.is_empty());
    assert!(
        r.stderr.contains(
            "The credential_cache \"disk\" in profile \"bad\" is not valid. Valid values are: file, memory."
        ),
        "{}",
        r.stderr
    );
}

#[test]
fn assume_role_configuration_errors() {
    let home = Home::new("loop");
    let s = Setup::new(
        &home,
        "[profile a]\nrole_arn = arn:aws:iam::1:role/a\nsource_profile = b\n[profile b]\nrole_arn = arn:aws:iam::1:role/b\nsource_profile = a\n\
         [profile both]\nrole_arn = arn:aws:iam::1:role/x\nsource_profile = a\ncredential_source = Environment\n",
    );
    let Some(r) = s.run(&["sts", "get-caller-identity", "--profile", "a"], vec![]) else {
        return;
    };
    assert_eq!(r.code, 253);
    assert!(
        r.stderr
            .contains("Infinite loop in credential configuration detected"),
        "{}",
        r.stderr
    );
    let r = s
        .run(&["sts", "get-caller-identity", "--profile", "both"], vec![])
        .unwrap();
    assert_eq!(r.code, 253);
    assert!(
        r.stderr
            .contains("contains both source_profile and credential_source"),
        "{}",
        r.stderr
    );
}

#[test]
fn credential_source_environment() {
    let home = Home::new("credsrc");
    let mut s = Setup::new(
        &home,
        "[profile ci]\nrole_arn = arn:aws:iam::1:role/ci\ncredential_source = Environment\n",
    );
    s.env.push(("AWS_ACCESS_KEY_ID", "AKIDENV".into()));
    s.env.push(("AWS_SECRET_ACCESS_KEY", "envsecret".into()));
    let Some(r) = s.run(
        &["sts", "get-caller-identity", "--profile", "ci"],
        vec![assume_role_response("ASIACI"), ok(CALLER_IDENTITY)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(access_key(&r.requests[0]), "AKIDENV");
    assert_eq!(access_key(&r.requests[1]), "ASIACI");
}

#[test]
fn web_identity_from_environment_is_unsigned() {
    let home = Home::new("webid");
    let token = home.write("token.jwt", "eyJ.token.sig");
    let mut s = Setup::new(&home, "");
    s.env.push((
        "AWS_WEB_IDENTITY_TOKEN_FILE",
        token.to_string_lossy().into_owned(),
    ));
    s.env
        .push(("AWS_ROLE_ARN", "arn:aws:iam::1:role/web".into()));
    s.env.push(("AWS_ROLE_SESSION_NAME", "pod".into()));
    let resp = ok(
        "<AssumeRoleWithWebIdentityResponse><AssumeRoleWithWebIdentityResult><Credentials>\
        <AccessKeyId>ASIAWEB</AccessKeyId><SecretAccessKey>s</SecretAccessKey><SessionToken>t</SessionToken>\
        <Expiration>2015-08-30T13:36:00Z</Expiration></Credentials></AssumeRoleWithWebIdentityResult>\
        </AssumeRoleWithWebIdentityResponse>",
    );
    let Some(r) = s.run(
        &["sts", "get-caller-identity"],
        vec![resp, ok(CALLER_IDENTITY)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    let web = &r.requests[0];
    assert!(
        web.header("Authorization").is_none(),
        "AssumeRoleWithWebIdentity is unsigned"
    );
    assert!(body(web).contains("Action=AssumeRoleWithWebIdentity"));
    assert!(body(web).contains("WebIdentityToken=eyJ.token.sig"));
    assert_eq!(access_key(&r.requests[1]), "ASIAWEB");
}

const SSO_CONFIG: &str = "[profile dev]\nsso_session = corp\nsso_account_id = 111122223333\nsso_role_name = Dev\nregion = us-west-2\n\
    [sso-session corp]\nsso_start_url = https://corp.awsapps.com/start\nsso_region = ap-northeast-1\n";

fn role_credentials() -> Response {
    // Expires one hour after NOW, in milliseconds.
    ok(&format!(
        r#"{{"roleCredentials":{{"accessKeyId":"ASIASSO","secretAccessKey":"s","sessionToken":"t","expiration":{}}}}}"#,
        (NOW + 3600) * 1000
    ))
}

#[test]
fn sso_session_profile_uses_cached_token() {
    let home = Home::new("sso");
    home.write(
        &format!(".aws/sso/cache/{}.json", sha1_hex("corp")),
        r#"{"startUrl":"https://corp.awsapps.com/start","region":"ap-northeast-1","accessToken":"tok-123","expiresAt":"2015-08-30T20:00:00Z"}"#,
    );
    let s = Setup::new(&home, SSO_CONFIG);
    let Some(r) = s.run(
        &["sts", "get-caller-identity", "--profile", "dev"],
        vec![role_credentials(), ok(CALLER_IDENTITY)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    let sso = &r.requests[0];
    assert_eq!(sso.method, "GET");
    assert_eq!(sso.host, "portal.sso.ap-northeast-1.amazonaws.com");
    assert_eq!(sso.path, "/federation/credentials");
    assert!(
        sso.query
            .contains(&("account_id".into(), "111122223333".into()))
    );
    assert!(sso.query.contains(&("role_name".into(), "Dev".into())));
    assert_eq!(sso.header("x-amz-sso_bearer_token"), Some("tok-123"));
    assert!(sso.header("Authorization").is_none());
    assert_eq!(access_key(&r.requests[1]), "ASIASSO");
    assert_eq!(
        r.requests[1].host, "sts.us-east-1.amazonaws.com",
        "AWS_DEFAULT_REGION wins over the profile"
    );

    let files = cache_files(&home.path(".aws/cli/cache"));
    assert_eq!(files.len(), 1);
    let cached: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
    assert_eq!(cached["ProviderType"], "sso");
    assert_eq!(cached["Credentials"]["Expiration"], "2015-08-30T13:36:00Z");

    let r = s
        .run(
            &["sts", "get-caller-identity", "--profile", "dev"],
            vec![ok(CALLER_IDENTITY)],
        )
        .unwrap();
    assert_eq!(
        r.requests.len(),
        1,
        "second run uses the cached role credentials"
    );
}

#[test]
fn sso_token_close_to_expiry_is_refreshed() {
    let home = Home::new("ssorefresh");
    let token_path = home.write(
        &format!(".aws/sso/cache/{}.json", sha1_hex("corp")),
        r#"{"startUrl":"https://corp.awsapps.com/start","region":"ap-northeast-1","accessToken":"old","expiresAt":"2015-08-30T12:40:00Z",
            "refreshToken":"refresh-1","clientId":"cid","clientSecret":"csecret","registrationExpiresAt":"2015-12-01T00:00:00Z"}"#,
    );
    let s = Setup::new(&home, SSO_CONFIG);
    let Some(r) = s.run(
        &["sts", "get-caller-identity", "--profile", "dev"],
        vec![
            ok(r#"{"accessToken":"new-token","expiresIn":28800,"refreshToken":"refresh-2","tokenType":"Bearer"}"#),
            role_credentials(),
            ok(CALLER_IDENTITY),
        ],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    let create = &r.requests[0];
    assert_eq!(create.host, "oidc.ap-northeast-1.amazonaws.com");
    let sent: serde_json::Value = serde_json::from_slice(&create.body).unwrap();
    assert_eq!(sent["grantType"], "refresh_token");
    assert_eq!(sent["refreshToken"], "refresh-1");
    assert_eq!(
        r.requests[1].header("x-amz-sso_bearer_token"),
        Some("new-token")
    );
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(token_path).unwrap()).unwrap();
    assert_eq!(saved["accessToken"], "new-token");
    assert_eq!(saved["refreshToken"], "refresh-2");
    assert_eq!(saved["expiresAt"], "2015-08-30T20:36:00Z");
}

#[test]
fn legacy_sso_profile_with_expired_token() {
    let home = Home::new("ssolegacy");
    home.write(
        &format!(
            ".aws/sso/cache/{}.json",
            sha1_hex("https://legacy.awsapps.com/start")
        ),
        r#"{"accessToken":"t","expiresAt":"2015-08-30T00:00:00Z"}"#,
    );
    let s = Setup::new(
        &home,
        "[profile old]\nsso_start_url = https://legacy.awsapps.com/start\nsso_region = us-east-1\nsso_account_id = 1\nsso_role_name = R\n",
    );
    let Some(r) = s.run(&["sts", "get-caller-identity", "--profile", "old"], vec![]) else {
        return;
    };
    assert_eq!(r.code, 255);
    assert!(r.stderr.contains("run aws sso login"), "{}", r.stderr);
    assert!(r.requests.is_empty());
}

#[test]
fn sso_login_device_flow_and_logout() {
    let home = Home::new("ssologin");
    let s = Setup::new(&home, SSO_CONFIG);
    let Some(r) = s.run(
        &["sso", "login", "--profile", "dev", "--no-browser", "--use-device-code"],
        vec![
            ok(r#"{"clientId":"cid","clientSecret":"csecret","clientSecretExpiresAt":1448000000}"#),
            ok(r#"{"deviceCode":"dev-code","userCode":"ABCD-EFGH","verificationUri":"https://device.sso.ap-northeast-1.amazonaws.com/","verificationUriComplete":"https://device.sso.ap-northeast-1.amazonaws.com/?user_code=ABCD-EFGH","expiresIn":600,"interval":1}"#),
            error(400, "AuthorizationPendingException", r#"{"error":"authorization_pending"}"#),
            ok(r#"{"accessToken":"fresh","expiresIn":28800,"refreshToken":"r1","tokenType":"Bearer"}"#),
        ],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.stdout.contains("ABCD-EFGH"), "{}", r.stdout);
    assert!(
        r.stdout
            .contains("Browser will not be automatically opened.")
    );
    assert!(
        r.stdout
            .ends_with("Successfully logged into Start URL: https://corp.awsapps.com/start\n")
    );
    let register: serde_json::Value = serde_json::from_slice(&r.requests[0].body).unwrap();
    assert_eq!(register["clientName"], "botocore-client-corp");
    assert_eq!(register["clientType"], "public");
    let poll: serde_json::Value = serde_json::from_slice(&r.requests[3].body).unwrap();
    assert_eq!(
        poll["grantType"],
        "urn:ietf:params:oauth:grant-type:device_code"
    );
    assert_eq!(poll["deviceCode"], "dev-code");

    let token_path = home.path(&format!(".aws/sso/cache/{}.json", sha1_hex("corp")));
    let token: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&token_path).unwrap()).unwrap();
    assert_eq!(token["accessToken"], "fresh");
    assert_eq!(token["registrationExpiresAt"], "2015-11-20T06:13:20Z");
    #[cfg(unix)]
    assert_eq!(mode(&token_path), 0o600);
    assert_eq!(
        cache_files(&home.path(".aws/sso/cache")).len(),
        2,
        "token + client registration"
    );

    // The new token now works for the SSO profile.
    let r = s
        .run(
            &["sts", "get-caller-identity", "--profile", "dev"],
            vec![role_credentials(), ok(CALLER_IDENTITY)],
        )
        .unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.requests[0].header("x-amz-sso_bearer_token"),
        Some("fresh")
    );

    let r = s.run(&["sso", "logout"], vec![ok("")]).unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.requests[0].path, "/logout");
    assert_eq!(
        r.requests[0].header("x-amz-sso_bearer_token"),
        Some("fresh")
    );
    assert!(!token_path.exists());
    assert!(
        cache_files(&home.path(".aws/cli/cache")).is_empty(),
        "cached SSO role credentials removed"
    );
}

#[test]
fn login_session_cached_and_refreshed_with_dpop() {
    let home = Home::new("login");
    let key = awsr_test_key();
    let token = |expires: &str| {
        serde_json::json!({
            "accessToken": {"accessKeyId": "ASIALOGIN", "secretAccessKey": "s", "sessionToken": "t",
                            "accountId": "123456789012", "expiresAt": expires},
            "refreshToken": "rt-1", "clientId": "arn:aws:signin:::devtools/same-device",
            "dpopKey": key, "signInRegion": "us-east-1"
        })
        .to_string()
    };
    let path = home.write(
        &format!(
            ".aws/login/cache/{}.json",
            sha256_hex("arn:aws:iam::123456789012:user/me")
        ),
        &token("2015-08-30T13:00:00Z"),
    );
    let s = Setup::new(
        &home,
        "[profile console]\nlogin_session = arn:aws:iam::123456789012:user/me\n",
    );
    let Some(r) = s.run(
        &["sts", "get-caller-identity", "--profile", "console"],
        vec![ok(CALLER_IDENTITY)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.requests.len(),
        1,
        "fresh login credentials need no refresh"
    );
    assert_eq!(access_key(&r.requests[0]), "ASIALOGIN");

    // Within five minutes of expiry: refresh through Sign-In with DPoP.
    std::fs::write(&path, token("2015-08-30T12:38:00Z")).unwrap();
    let r = s
        .run(
            &["sts", "get-caller-identity", "--profile", "console"],
            vec![
                // `tokenOutput` is the payload member, so the wire body is unwrapped.
                ok(r#"{"accessToken":{"accessKeyId":"ASIANEW","secretAccessKey":"s2","sessionToken":"t2"},"refreshToken":"rt-2","expiresIn":900,"tokenType":"aws_sigv4"}"#),
                ok(CALLER_IDENTITY),
            ],
        )
        .unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    let refresh = &r.requests[0];
    assert_eq!(refresh.path, "/v1/token");
    let dpop = refresh.header("DPoP").expect("DPoP proof header");
    assert_eq!(dpop.split('.').count(), 3);
    let sent: serde_json::Value = serde_json::from_slice(&refresh.body).unwrap();
    assert_eq!(
        sent["grantType"], "refresh_token",
        "tokenInput is the payload member"
    );
    assert_eq!(sent["refreshToken"], "rt-1");
    assert_eq!(access_key(&r.requests[1]), "ASIANEW");
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["refreshToken"], "rt-2");
    assert_eq!(saved["accessToken"]["expiresAt"], "2015-08-30T12:51:00Z");
    assert_eq!(saved["accessToken"]["accountId"], "123456789012");
}

/// A throwaway P-256 key (SEC1 PEM).
fn awsr_test_key() -> String {
    let out = std::process::Command::new("openssl")
        .args(["ecparam", "-name", "prime256v1", "-genkey", "-noout"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8(o.stdout).unwrap(),
        _ => panic!("openssl is required to generate a test key"),
    }
}

#[test]
fn container_credentials() {
    let home = Home::new("container");
    let token_file = home.write("token", "Bearer abc");
    let mut s = Setup::new(&home, "");
    s.env.push((
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        "http://169.254.170.23/v1/credentials".into(),
    ));
    s.env.push((
        "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
        token_file.to_string_lossy().into_owned(),
    ));
    let creds = ok(
        r#"{"AccessKeyId":"ASIAECS","SecretAccessKey":"s","Token":"t","Expiration":"2015-08-30T13:36:00Z"}"#,
    );
    let Some(r) = s.run(
        &["sts", "get-caller-identity"],
        vec![creds, ok(CALLER_IDENTITY)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.requests[0].url(), "http://169.254.170.23/v1/credentials");
    assert_eq!(r.requests[0].header("Authorization"), Some("Bearer abc"));
    assert_eq!(
        r.requests[0].timeout,
        Some(std::time::Duration::from_secs(2))
    );
    assert_eq!(access_key(&r.requests[1]), "ASIAECS");

    let mut s = Setup::new(&home, "");
    s.env.push((
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        "http://evil.example.com/creds".into(),
    ));
    let r = s.run(&["sts", "get-caller-identity"], vec![]).unwrap();
    assert_eq!(r.code, 255);
    assert!(r.stderr.contains("Unsupported host"), "{}", r.stderr);
    assert!(
        r.requests.is_empty(),
        "nothing is sent to a disallowed host"
    );

    // An allowed address in the userinfo is not the host.
    let mut s = Setup::new(&home, "");
    s.env.push((
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        "http://169.254.170.2:x@evil.example.com/creds".into(),
    ));
    s.env
        .push(("AWS_CONTAINER_AUTHORIZATION_TOKEN", "secret".into()));
    let r = s.run(&["sts", "get-caller-identity"], vec![]).unwrap();
    assert_eq!(r.code, 255);
    assert!(r.stderr.contains("Unsupported host"), "{}", r.stderr);
    assert!(r.requests.is_empty(), "the token is not sent");
}

#[test]
fn instance_metadata_v2_credentials() {
    let home = Home::new("imds");
    let s = Setup::new(&home, "");
    let Some(r) = s.run(
        &["sts", "get-caller-identity"],
        vec![
            ok("imds-token"),
            ok("my-role\n"),
            ok(r#"{"Code":"Success","AccessKeyId":"ASIAEC2","SecretAccessKey":"s","Token":"t","Expiration":"2015-08-30T18:00:00Z"}"#),
            ok(CALLER_IDENTITY),
        ],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.requests[0].method, "PUT");
    assert_eq!(
        r.requests[0].url(),
        "http://169.254.169.254/latest/api/token"
    );
    assert_eq!(
        r.requests[0].header("x-aws-ec2-metadata-token-ttl-seconds"),
        Some("21600")
    );
    assert_eq!(
        r.requests[0].timeout,
        Some(std::time::Duration::from_secs(1))
    );
    assert_eq!(
        r.requests[2].url(),
        "http://169.254.169.254/latest/meta-data/iam/security-credentials/my-role"
    );
    assert_eq!(
        r.requests[2].header("x-aws-ec2-metadata-token"),
        Some("imds-token")
    );
    assert_eq!(access_key(&r.requests[3]), "ASIAEC2");

    let mut s = Setup::new(&home, "");
    s.env.push(("AWS_EC2_METADATA_DISABLED", "true".into()));
    let r = s.run(&["sts", "get-caller-identity"], vec![]).unwrap();
    assert_eq!(r.code, 253);
    assert!(r.stderr.contains("Unable to locate credentials"));
    assert!(r.requests.is_empty());
}

#[test]
fn pager_selection() {
    let home = Home::new("pager");
    let s = Setup::new(&home, "[default]\ncli_pager = most\n");
    let Some(loader) = common::loader_or_skip() else {
        return;
    };
    let transport = FakeTransport {
        requests: Mutex::new(Vec::new()),
        responses: Mutex::new(VecDeque::new()),
    };
    let env: HashMap<&str, &str> = HashMap::from([("PAGER", "more")]);
    let ctx = Context {
        loader,
        config: SharedConfig::from_parts(&parse_ini(s.config), parse_ini("")),
        env: Box::new(move |k| env.get(k).map(|v| v.to_string())),
        transport: &transport,
        now: Box::new(Utc::now),
        uuid: Box::new(String::new),
        prompt: Box::new(|_| None),
        sleep: Box::new(|_| {}),
        open_browser: Box::new(|_| false),
        stdout_is_tty: true,
        stderr_is_tty: false,
        terminal_width: None,
        local_time: Box::new(|t| t.naive_utc()),
        random: Box::new(|n| vec![7; n]),
        callback: Box::new(|_| Err("no callback in tests".into())),
    };
    let mut g = awsr::cli::Globals::default();
    assert_eq!(
        driver::pager_command(&ctx, &g, "default").as_deref(),
        Some("most")
    );
    assert_eq!(
        driver::pager_command(&ctx, &g, "other").as_deref(),
        Some("more")
    );
    g.no_cli_pager = true;
    assert_eq!(driver::pager_command(&ctx, &g, "default"), None);
}

#[test]
fn sso_login_gives_up_when_authorization_never_completes() {
    let home = Home::new("ssotimeout");
    let s = Setup::new(&home, SSO_CONFIG);
    let mut responses = vec![
        ok(r#"{"clientId":"cid","clientSecret":"csecret","clientSecretExpiresAt":1448000000}"#),
        ok(
            r#"{"deviceCode":"d","userCode":"U","verificationUri":"https://x/","verificationUriComplete":"https://x/?c=U","expiresIn":10,"interval":5}"#,
        ),
    ];
    // The fixed clock never reaches the deadline; the attempt cap must stop it.
    for _ in 0..10 {
        responses.push(error(
            400,
            "AuthorizationPendingException",
            r#"{"error":"authorization_pending"}"#,
        ));
    }
    let Some(r) = s.run(
        &[
            "sso",
            "login",
            "--profile",
            "dev",
            "--no-browser",
            "--use-device-code",
        ],
        responses,
    ) else {
        return;
    };
    assert_eq!(r.code, 255);
    assert!(r.stderr.contains("has expired"), "{}", r.stderr);
    assert!(
        r.requests.len() <= 6,
        "polled {} times",
        r.requests.len() - 2
    );
}

fn id_token(sub: &str) -> String {
    use base64::Engine;
    let payload =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(r#"{{"sub":"{sub}"}}"#));
    format!("eyJhbGciOiJFUzI1NiJ9.{payload}.sig")
}

fn login_token_response() -> Response {
    ok(&format!(
        r#"{{"accessToken":{{"accessKeyId":"ASIALOGIN","secretAccessKey":"s","sessionToken":"t"}},"tokenType":"aws_sigv4","expiresIn":900,"refreshToken":"rt-1","idToken":"{}"}}"#,
        id_token("arn:aws:iam::123456789012:user/me")
    ))
}

fn param(url: &str, key: &str) -> Option<String> {
    awsr::oauth::query_params(url)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

/// The PKCE verifier sent to the token endpoint must hash to the
/// challenge in the authorize URL (base64url, padding trimmed).
fn assert_pkce(authorize_url: &str, verifier: &str) {
    use base64::Engine;
    use sha2::Digest;
    let expected = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    assert_eq!(verifier.len(), 64);
    assert_eq!(
        authorize_url.rsplit("&code_challenge=").next(),
        Some(expected.as_str())
    );
}

#[test]
fn login_same_device_pkce() {
    let home = Home::new("login-pkce");
    let mut s = Setup::new(&home, "");
    s.callback_code = Some("auth-code-1");
    let Some(r) = s.run(
        &["login"],
        vec![login_token_response(), ok(CALLER_IDENTITY)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    let url = r.opened.last().expect("browser opened").clone();
    assert!(url.contains("/v1/authorize?response_type=code&client_id=arn%3Aaws%3Asignin%3A%3A%3Adevtools%2Fsame-device&state="), "{url}");
    assert_eq!(
        param(&url, "code_challenge_method").as_deref(),
        Some("SHA-256")
    );
    assert_eq!(param(&url, "scope").as_deref(), Some("openid"));
    assert_eq!(
        param(&url, "redirect_uri").as_deref(),
        Some("http://127.0.0.1:43210/oauth/callback")
    );

    let token_req = &r.requests[0];
    assert!(
        url.starts_with(&format!("https://{}/", token_req.host)),
        "authorize URL uses the Sign-In endpoint"
    );
    assert_eq!(token_req.path, "/v1/token");
    assert!(token_req.header("DPoP").is_some());
    let sent: serde_json::Value = serde_json::from_slice(&token_req.body).unwrap();
    assert_eq!(sent["grantType"], "authorization_code");
    assert_eq!(sent["code"], "auth-code-1");
    assert_eq!(sent["clientId"], "arn:aws:signin:::devtools/same-device");
    assert_eq!(sent["redirectUri"], "http://127.0.0.1:43210/oauth/callback");
    assert_pkce(&url, sent["codeVerifier"].as_str().unwrap());

    let session = "arn:aws:iam::123456789012:user/me";
    let cache_path = home.path(&format!(".aws/login/cache/{}.json", sha256_hex(session)));
    let cached: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cache_path).unwrap()).unwrap();
    assert_eq!(cached["accessToken"]["accountId"], "123456789012");
    assert_eq!(cached["accessToken"]["expiresAt"], "2015-08-30T12:51:00Z");
    assert_eq!(cached["signInRegion"], "us-east-1");
    assert!(
        cached["dpopKey"]
            .as_str()
            .unwrap()
            .starts_with("-----BEGIN EC PRIVATE KEY-----")
    );
    #[cfg(unix)]
    assert_eq!(mode(&cache_path), 0o600);
    let config = std::fs::read_to_string(home.path(".aws/config")).unwrap();
    assert_eq!(config, format!("[default]\nlogin_session = {session}\n"));
    assert!(r.stdout.contains(&format!(
        "Updated profile default to use {session} credentials."
    )));
    // The command only logs in; the second canned response is unused.
    assert_eq!(r.requests.len(), 1);
}

#[test]
fn login_remote_paste_code_and_profile_checks() {
    use base64::Engine;
    let home = Home::new("login-remote");
    let pasted = base64::engine::general_purpose::STANDARD
        .encode("state=11111111-1111-4111-8111-111111111111&code=remote-code");
    let mut s = Setup::new(&home, "[profile keys]\naws_access_key_id = AKID\n");
    s.prompt_answer = Some(&pasted);
    let Some(r) = s.run(
        &["login", "--remote", "--profile", "work"],
        vec![login_token_response()],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.opened.is_empty(), "no browser for the cross-device flow");
    assert!(r.stdout.contains("/v1/authorize?response_type=code&client_id=arn%3Aaws%3Asignin%3A%3A%3Adevtools%2Fcross-device"));
    assert_eq!(
        r.prompts,
        vec!["\nEnter the authorization code displayed in your browser: "]
    );
    let sent: serde_json::Value = serde_json::from_slice(&r.requests[0].body).unwrap();
    assert_eq!(sent["code"], "remote-code");
    assert_eq!(
        sent["redirectUri"],
        format!("https://{}/v1/sessions/confirmation", r.requests[0].host)
    );
    let config = std::fs::read_to_string(home.path(".aws/config")).unwrap();
    assert!(
        config.contains("[profile work]\nlogin_session = arn:aws:iam::123456789012:user/me\n"),
        "{config}"
    );

    // Profiles with other credentials are refused before anything is sent.
    let r = s.run(&["login", "--profile", "keys"], vec![]).unwrap();
    assert_eq!(r.code, 253);
    assert!(
        r.stderr
            .contains("already configured with Access Key credentials"),
        "{}",
        r.stderr
    );
    assert!(r.requests.is_empty());
}

#[test]
fn login_rejects_mismatched_state() {
    use base64::Engine;
    let home = Home::new("login-state");
    let pasted = base64::engine::general_purpose::STANDARD.encode("state=forged&code=x");
    let mut s = Setup::new(&home, "");
    s.prompt_answer = Some(&pasted);
    let Some(r) = s.run(&["login", "--remote"], vec![]) else {
        return;
    };
    assert_eq!(r.code, 255);
    assert!(
        r.stderr.contains("does not match expected value"),
        "{}",
        r.stderr
    );
    assert!(
        r.requests.is_empty(),
        "no token exchange with a forged state"
    );
}

#[test]
fn sso_login_pkce_for_sso_session() {
    let home = Home::new("sso-pkce");
    let mut s = Setup::new(&home, SSO_CONFIG);
    s.callback_code = Some("sso-code");
    let Some(r) = s.run(
        &["sso", "login", "--profile", "dev"],
        vec![
            ok(r#"{"clientId":"cid","clientSecret":"csecret","clientSecretExpiresAt":1448000000}"#),
            ok(r#"{"accessToken":"pkce-token","expiresIn":28800,"refreshToken":"r1","tokenType":"Bearer"}"#),
        ],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    let register: serde_json::Value = serde_json::from_slice(&r.requests[0].body).unwrap();
    assert_eq!(
        register["grantTypes"],
        serde_json::json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(
        register["redirectUris"],
        serde_json::json!(["http://127.0.0.1/oauth/callback"])
    );
    assert_eq!(register["issuerUrl"], "https://corp.awsapps.com/start");
    assert_eq!(
        register["scopes"],
        serde_json::json!(["sso:account:access"])
    );

    let url = r.opened.last().expect("browser opened").clone();
    assert!(url.starts_with("https://oidc.ap-northeast-1.amazonaws.com/authorize?response_type=code&client_id=cid&redirect_uri="), "{url}");
    assert_eq!(
        param(&url, "code_challenge_method").as_deref(),
        Some("S256")
    );
    assert_eq!(param(&url, "scopes").as_deref(), Some("sso:account:access"));
    let create: serde_json::Value = serde_json::from_slice(&r.requests[1].body).unwrap();
    assert_eq!(create["grantType"], "authorization_code");
    assert_eq!(create["code"], "sso-code");
    assert_eq!(
        create["redirectUri"],
        "http://127.0.0.1:43210/oauth/callback"
    );
    assert_pkce(&url, create["codeVerifier"].as_str().unwrap());

    let token: serde_json::Value = serde_json::from_slice(
        &std::fs::read(home.path(&format!(".aws/sso/cache/{}.json", sha1_hex("corp")))).unwrap(),
    )
    .unwrap();
    assert_eq!(token["accessToken"], "pkce-token");
    assert!(
        r.stdout
            .ends_with("Successfully logged into Start URL: https://corp.awsapps.com/start\n")
    );

    // The cached auth-code registration is reused on the next login.
    let r = s
        .run(
            &["sso", "login", "--profile", "dev"],
            vec![ok(
                r#"{"accessToken":"t2","expiresIn":28800,"tokenType":"Bearer"}"#,
            )],
        )
        .unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.requests.len(), 1, "no new RegisterClient");
}

#[test]
fn logout_removes_login_sessions() {
    let home = Home::new("logout");
    let session = "arn:aws:iam::123456789012:user/me";
    let token =
        r#"{"accessToken": {"accessKeyId": "ASIA", "secretAccessKey": "s"}, "refreshToken": "r"}"#;
    let mine = home.write(
        &format!(".aws/login/cache/{}.json", sha256_hex(session)),
        token,
    );
    let other = home.write(".aws/login/cache/other.json", token);
    let unrelated = home.write(".aws/login/cache/unrelated.json", r#"{"startUrl": "x"}"#);
    let config = format!(
        "[profile console]\nlogin_session = {session}\n[profile plain]\nregion = us-east-1\n"
    );
    let s = Setup::new(&home, &config);
    let Some(r) = s.run(&["logout", "--profile", "console"], vec![]) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        r.stdout
            .starts_with("Removed cached login credentials for profile 'console'.")
    );
    assert!(!mine.exists() && other.exists());
    // A profile without a login session only warns.
    let r = s.run(&["logout", "--profile", "plain"], vec![]).unwrap();
    assert_eq!(r.code, 0);
    assert_eq!(
        r.stderr,
        "warning: no login session found for profile 'plain'\n"
    );
    // --all removes every login token, not other cached documents.
    let r = s.run(&["logout", "--all"], vec![]).unwrap();
    assert_eq!(
        r.stdout,
        "Removed 1 cached login credential. Note, any local developer tools that have already loaded access tokens may continue to use them until their expiration. Access tokens expire in 15 minutes.\n"
    );
    assert!(!other.exists() && unrelated.exists());
    let r = s.run(&["logout", "--all"], vec![]).unwrap();
    assert_eq!(r.stdout, "No cached login session tokens found.\n");
    assert!(r.requests.is_empty());
}
