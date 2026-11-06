use awsr::config::SharedConfig;
use awsr::driver::{self, Context, exit};
use awsr::http::UreqTransport;
use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

/// Read one line from the terminal (MFA codes).
fn prompt(message: &str) -> Option<String> {
    let mut err = std::io::stderr();
    let _ = write!(err, "{message}");
    let _ = err.flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    Some(line.trim_end_matches(['\r', '\n']).to_string())
}

fn open_browser(url: &str) -> bool {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn timeout(args: &[String], flag: &str) -> Option<Duration> {
    let pos = args.iter().position(|a| a == flag)?;
    let secs: u64 = args.get(pos + 1)?.parse().ok()?;
    // 0 means "no timeout" in the AWS CLI.
    (secs > 0).then(|| Duration::from_secs(secs))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let loader = match driver::default_loader() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(exit::CONFIG);
        }
    };
    let transport = UreqTransport::new(
        timeout(&args, "--cli-connect-timeout").or(Some(Duration::from_secs(60))),
        timeout(&args, "--cli-read-timeout").or(Some(Duration::from_secs(60))),
    );
    let ctx = Context {
        loader,
        config: SharedConfig::load(),
        env: Box::new(|k| std::env::var(k).ok()),
        transport: &transport,
        now: Box::new(chrono::Utc::now),
        uuid: Box::new(driver::random_uuid),
        prompt: Box::new(prompt),
        sleep: Box::new(std::thread::sleep),
        open_browser: Box::new(open_browser),
        stdout_is_tty: std::io::stdout().is_terminal(),
        random: Box::new(awsr::oauth::os_random),
        callback: Box::new(awsr::oauth::LocalCallbackServer::bind),
    };
    let code = driver::run(
        &ctx,
        &args,
        &mut std::io::stdout().lock(),
        &mut std::io::stderr().lock(),
    );
    std::process::exit(code);
}
