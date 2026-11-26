use std::env;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;

#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
#[cfg(test)]
use std::time::Duration;
#[cfg(test)]
use std::{fs, sync::Mutex};

use anyhow::{Result, bail};

pub type Report = Arc<dyn Fn(String) + Send + Sync>;

pub fn openers(browser: Option<&str>, os: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    if let Some(line) = browser.map(str::trim).filter(|b| !b.is_empty()) {
        let argv: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        if !argv.is_empty() {
            out.push(argv);
        }
    }
    match os {
        "macos" => out.push(vec!["open".to_owned()]),
        "windows" => out.push(vec![
            "rundll32".into(),
            "url.dll,FileProtocolHandler".into(),
        ]),
        _ => {
            out.push(vec!["xdg-open".to_owned()]);
            out.push(vec!["wslview".to_owned()]);
        }
    }
    out
}

pub fn is_link(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && !lower.contains(char::is_whitespace)
}

pub fn no_display(get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let set = |name: &str| get(name).is_some_and(|v| !v.trim().is_empty());
    match env::consts::OS {
        "linux" | "freebsd" | "openbsd" | "netbsd" => {
            if set("DISPLAY") || set("WAYLAND_DISPLAY") {
                return None;
            }
            Some(
                "this process has no display (DISPLAY and WAYLAND_DISPLAY are unset), so no browser \
                 can be started here"
                    .to_owned(),
            )
        }
        _ => None,
    }
}

pub fn open(url: &str, report: &Report) -> Result<String> {
    if !is_link(url) {
        bail!("not a web address: {url}");
    }
    if let Some(why) = no_display(&|k| env::var(k).ok()) {
        bail!("{why}");
    }
    let browser = env::var("BROWSER").ok();
    let mut tried = Vec::new();
    for argv in openers(browser.as_deref(), env::consts::OS) {
        let mut command = command_for(&argv, url);
        let program = argv
            .first()
            .map(String::as_str)
            .unwrap_or_default()
            .to_owned();
        command.stderr(Stdio::piped()).stdout(Stdio::null());
        match command.spawn() {
            Ok(child) => {
                let seen = program.clone();
                let report = report.clone();
                thread::spawn(move || match child.wait_with_output() {
                    Ok(out) if !out.status.success() => {
                        let why = String::from_utf8_lossy(&out.stderr);
                        let why = why.lines().next().unwrap_or("").trim().to_owned();
                        report(format!(
                            "{seen} could not open the link{}; press c to copy it, or set $BROWSER",
                            if why.is_empty() {
                                String::new()
                            } else {
                                format!(" ({why})")
                            }
                        ));
                    }
                    _ => {}
                });
                return Ok(program);
            }
            Err(e) => tried.push(format!("{program}: {e}")),
        }
    }
    Err(anyhow::anyhow!(
        "no browser would open it ({})",
        tried.join("; ")
    ))
}

pub fn command_for(argv: &[String], url: &str) -> Command {
    let (program, args) = argv
        .split_first()
        .map(|(p, a)| (p.as_str(), a))
        .unwrap_or(("", &[]));
    let mut command = Command::new(program);
    command
        .args(args)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

pub fn open_or_say(url: &str, note: &Report) {
    if let Err(e) = open(url, note) {
        note(format!(
            "could not open a browser: {}; press c to copy the link",
            e.to_string().split('\n').next().unwrap_or("")
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV: Mutex<()> = Mutex::new(());

    #[test]
    fn the_browser_is_the_persons_choice_first() {
        let list = openers(Some("firefox --new-window"), "linux");
        assert_eq!(list[0], vec!["firefox", "--new-window"]);
        assert_eq!(list[1], vec!["xdg-open"]);
        assert_eq!(openers(Some("   "), "macos"), vec![vec!["open"]]);
        assert_eq!(openers(None, "macos"), vec![vec!["open"]]);
        assert_eq!(
            openers(None, "linux"),
            vec![vec!["xdg-open"], vec!["wslview"]]
        );
        assert!(openers(None, "windows")[0][0] == "rundll32");
    }

    #[test]
    fn the_address_is_the_last_argument_and_nothing_is_a_shell() {
        let cmd = command_for(
            &["firefox".to_owned(), "--new-window".to_owned()],
            "https://a.test/x?a=1&b=$(id)",
        );
        assert_eq!(cmd.get_program(), "firefox");
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["--new-window", "https://a.test/x?a=1&b=$(id)"]
                .map(str::to_owned)
                .to_vec()
        );
    }

    #[test]
    fn a_browser_is_started_with_the_address_and_not_waited_for() {
        let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("browser.sh");
        let out = dir.path().join("argv.txt");
        fs::write(
            &script,
            format!("#!/bin/sh\necho \"$@\" > {}\n", out.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let mut cmd = command_for(
            &[script.to_string_lossy().into_owned(), "--new-window".into()],
            "https://auth.test/device?x=1",
        );
        let status = cmd.status().expect("the stub ran");
        assert!(status.success());
        let seen = fs::read_to_string(&out).unwrap();
        assert_eq!(seen.trim(), "--new-window https://auth.test/device?x=1");
    }

    #[test]
    fn a_stub_named_by_browser_is_what_opens_the_link() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("browser.sh");
        let out = dir.path().join("argv.txt");
        fs::write(
            &script,
            format!("#!/bin/sh\necho \"$@\" > {}\n", out.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let was = env::var_os("DISPLAY");
        unsafe {
            env::set_var("BROWSER", script.to_string_lossy().into_owned());
            env::set_var("DISPLAY", ":0");
        }
        let quiet: Report = Arc::new(|_| {});
        let answered = open("https://auth.test/device", &quiet);
        unsafe {
            env::remove_var("BROWSER");
            match was {
                Some(v) => env::set_var("DISPLAY", v),
                None => env::remove_var("DISPLAY"),
            }
        }
        assert_eq!(answered.unwrap(), script.to_string_lossy());
        let mut seen = String::new();
        for _ in 0..100 {
            seen = fs::read_to_string(&out).unwrap_or_default();
            if !seen.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(seen.trim(), "https://auth.test/device");
    }

    #[test]
    fn a_process_with_no_display_says_so_before_trying() {
        let none = |_: &str| None;
        let empty = |_: &str| Some(String::new());
        let x11 = |k: &str| (k == "DISPLAY").then(|| ":0".to_owned());
        let wayland = |k: &str| (k == "WAYLAND_DISPLAY").then(|| "wayland-0".to_owned());
        if cfg!(any(target_os = "linux", target_os = "freebsd")) {
            assert!(no_display(&none).is_some());
            assert!(
                no_display(&empty).is_some(),
                "an empty value is not a display"
            );
            assert!(no_display(&x11).is_none());
            assert!(no_display(&wayland).is_none());
            let why = no_display(&none).unwrap();
            assert!(why.contains("DISPLAY"), "{why}");
            assert!(why.contains("WAYLAND_DISPLAY"), "{why}");
        }
    }

    #[test]
    fn a_program_that_starts_and_then_fails_is_reported() {
        let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("no-handler.sh");
        fs::write(
            &script,
            "#!/bin/sh\necho 'no method available for opening' >&2\nexit 3\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let said: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = said.clone();
        let report: Report = Arc::new(move |why: String| sink.lock().unwrap().push(why));
        let was = env::var_os("DISPLAY");
        unsafe {
            env::set_var("BROWSER", script.to_string_lossy().into_owned());
            env::set_var("DISPLAY", ":0");
        }
        let started = open("https://auth.test/device", &report);
        unsafe {
            env::remove_var("BROWSER");
            match was {
                Some(v) => env::set_var("DISPLAY", v),
                None => env::remove_var("DISPLAY"),
            }
        }
        assert!(started.is_ok(), "the program did start");
        let mut seen = Vec::new();
        for _ in 0..100 {
            seen = said.lock().unwrap().clone();
            if !seen.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(seen.len(), 1, "one note, not a stream: {seen:?}");
        assert!(seen[0].contains("no method available"), "{}", seen[0]);
        assert!(seen[0].contains("press c to copy it"), "{}", seen[0]);
    }

    #[test]
    fn only_a_web_address_is_a_link() {
        assert!(is_link("https://auth.openai.com/codex/device"));
        assert!(is_link(" http://127.0.0.1:1455/auth/callback "));
        assert!(
            is_link("HTTPS://A.TEST/x"),
            "the scheme is not case sensitive"
        );
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://a.test/a b",
            "auth.openai.com",
            "",
        ] {
            assert!(!is_link(bad), "{bad} must not be opened");
        }
        let quiet: Report = Arc::new(|_| {});
        let e = open("file:///etc/passwd", &quiet).unwrap_err().to_string();
        assert!(e.contains("not a web address"), "{e}");
    }
}
