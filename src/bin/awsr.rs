use awsr::config::SharedConfig;
use awsr::driver::{self, Context, exit};
use awsr::http::UreqTransport;
use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

/// Read one line from the terminal (MFA codes, pasted authorization codes).
fn prompt(message: &str) -> Option<String> {
    let mut err = std::io::stderr();
    let _ = write!(err, "{message}");
    let _ = err.flush();
    if std::io::stdin().is_terminal() {
        #[cfg(unix)]
        if let Some(line) = read_tty_line() {
            return line;
        }
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).ok()?;
    Some(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Line editing done by the kernel caps a line at MAX_CANON bytes (1024 on
/// macOS) and silently drops the rest, Enter included, so a long pasted
/// `login --remote` code could never be submitted. Read with ICANON off and
/// do the minimal editing ourselves. Returns `None` if the terminal cannot be
/// switched; the inner `None` means the user cancelled (Ctrl-C / Ctrl-D).
#[cfg(unix)]
fn read_tty_line() -> Option<Option<String>> {
    use std::io::Read;
    let fd = libc::STDIN_FILENO;
    let mut saved = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr fills the struct on success.
    if unsafe { libc::tcgetattr(fd, saved.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: initialized by the successful tcgetattr above.
    let saved = unsafe { saved.assume_init() };
    let mut raw = saved;
    // ISIG off so Ctrl-C comes through as a byte and the terminal is always
    // restored below.
    raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
    raw.c_cc[libc::VMIN] = 1;
    raw.c_cc[libc::VTIME] = 0;
    // SAFETY: raw is a valid termios derived from the current settings.
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return None;
    }
    let mut err = std::io::stderr();
    let mut line: Vec<u8> = Vec::new();
    let mut cancelled = false;
    for byte in std::io::stdin().lock().bytes() {
        match byte {
            Ok(b'\r' | b'\n') => break,
            Ok(0x03) | Err(_) => {
                cancelled = true;
                break;
            }
            Ok(0x04) if line.is_empty() => {
                cancelled = true;
                break;
            }
            Ok(0x7f | 0x08) => {
                if line.pop().is_some() {
                    // Drop any UTF-8 continuation bytes of the erased char.
                    while line.last().is_some_and(|b| b & 0xc0 == 0x80) {
                        line.pop();
                    }
                    let _ = write!(err, "\x08 \x08");
                }
            }
            Ok(b) if b < 0x20 => {}
            Ok(b) => {
                line.push(b);
                let _ = err.write_all(&[b]);
            }
        }
        let _ = err.flush();
    }
    let _ = writeln!(err);
    // SAFETY: restoring the settings captured above.
    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    Some((!cancelled).then(|| String::from_utf8_lossy(&line).into_owned()))
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

/// `determine_terminal_width`: the width of the terminal on stdout.
fn terminal_width() -> Option<usize> {
    #[cfg(unix)]
    {
        let mut ws = std::mem::MaybeUninit::<libc::winsize>::zeroed();
        // SAFETY: TIOCGWINSZ fills a winsize struct on success.
        let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, ws.as_mut_ptr()) };
        if ok == 0 {
            // SAFETY: initialized by the successful ioctl above.
            let ws = unsafe { ws.assume_init() };
            return (ws.ws_col > 0).then_some(ws.ws_col as usize);
        }
    }
    None
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
        terminal_width: terminal_width(),
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
