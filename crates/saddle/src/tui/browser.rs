//! Opening a link in the person's own browser (D86).
//!
//! A terminal cannot make its own text selectable while a dialog owns the screen, and a click on
//! drawn text reaches saddle rather than the terminal, so "click the link" has to mean something
//! here: the address a sign-in needs is opened by the program that opens addresses, which is what
//! the person would have done with the mouse anyway.
//!
//! This is a person's action, not the model's. No tool reaches it — `Action::OpenUrl` is produced
//! only by a key or a click in the sign-in dialog — so it grants the agent nothing, and the address
//! it opens is one the person's own `config.toml` named, which the agent cannot write (D42).

use std::process::{Command, Stdio};

use anyhow::{Result, bail};

/// The programs that open a URL, in the order they are tried, given the environment and the OS.
///
/// The OS is a parameter rather than read inside, so each platform's list can be checked on any
/// machine the tests run on.
///
/// `$BROWSER` first because it is the person saying which one, then the xdg convention, then the
/// two platforms that are not xdg. A value with arguments in it (`firefox --new-window`) is split,
/// since that is how `$BROWSER` has always been read.
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

/// Whether this is a link to open at all.
///
/// Only `http` and `https`: an address that names a local file or something else a handler might
/// act on is not a link a sign-in put there, and refusing it is cheaper than deciding what else is
/// safe to hand to a browser.
pub fn is_link(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && !lower.contains(char::is_whitespace)
}

/// Opens `url` in the default browser, answering with the program that took it.
pub fn open(url: &str) -> Result<String> {
    if !is_link(url) {
        bail!("not a web address: {url}");
    }
    let browser = std::env::var("BROWSER").ok();
    let mut tried = Vec::new();
    for argv in openers(browser.as_deref(), std::env::consts::OS) {
        let mut command = command_for(&argv, url);
        let program = argv
            .first()
            .map(String::as_str)
            .unwrap_or_default()
            .to_owned();
        let spawned = command.spawn();
        match spawned {
            Ok(mut child) => {
                // Reaped on a thread: the browser is not saddle's to wait for, but a child left
                // unreaped is a zombie for as long as the session lasts.
                std::thread::spawn(move || {
                    let _ = child.wait();
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

/// The command that opens `url` with this program, its own arguments first and the address last.
///
/// No shell: the address is one argument, so nothing in it can be read as syntax, and a `$BROWSER`
/// holding one program and its flags is still one program.
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

/// The same, with a failure answered by a note that says what to do instead.
pub fn open_or_say(url: &str, note: &mut dyn FnMut(&str)) {
    if let Err(e) = open(url) {
        note(&format!(
            "could not open a browser: {}; press c to copy the link",
            e.to_string().split('\n').next().unwrap_or("")
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_browser_is_the_persons_choice_first() {
        // $BROWSER wins, and a value with arguments keeps them: that is how it has always been read.
        let list = openers(Some("firefox --new-window"), "linux");
        assert_eq!(list[0], vec!["firefox", "--new-window"]);
        assert_eq!(list[1], vec!["xdg-open"]);
        // Empty or blank is not a choice.
        assert_eq!(openers(Some("   "), "macos"), vec![vec!["open"]]);
        assert_eq!(openers(None, "macos"), vec![vec!["open"]]);
        // Each platform gets the program it has, and linux keeps the WSL one as a fallback.
        assert_eq!(
            openers(None, "linux"),
            vec![vec!["xdg-open"], vec!["wslview"]]
        );
        assert!(openers(None, "windows")[0][0] == "rundll32");
    }

    #[test]
    fn the_address_is_the_last_argument_and_nothing_is_a_shell() {
        // A `$BROWSER` with flags keeps them, and the address is one argument after them, so a URL
        // with something shell-shaped in it is still just an argument.
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
        // The spawn itself, with a stub where the browser would be: this is the part a unit test of
        // the argv cannot show, and it is the part that could fail on a machine with no browser.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("browser.sh");
        let out = dir.path().join("argv.txt");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"$@\" > {}\n", out.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut cmd = command_for(
            &[script.to_string_lossy().into_owned(), "--new-window".into()],
            "https://auth.test/device?x=1",
        );
        let status = cmd.status().expect("the stub ran");
        assert!(status.success());
        let seen = std::fs::read_to_string(&out).unwrap();
        assert_eq!(seen.trim(), "--new-window https://auth.test/device?x=1");
    }

    #[test]
    fn a_stub_named_by_browser_is_what_opens_the_link() {
        // The real `open`, with `$BROWSER` pointing at a stub: this is the only way to see the choice
        // of program and the spawn together. It is one process, so the variable is set here and
        // nowhere else in the suite.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("browser.sh");
        let out = dir.path().join("argv.txt");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"$@\" > {}\n", out.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: the only test in this file that touches the environment, and it sets one variable.
        unsafe { std::env::set_var("BROWSER", script.to_string_lossy().into_owned()) };
        let answered = open("https://auth.test/device");
        unsafe { std::env::remove_var("BROWSER") };
        assert_eq!(answered.unwrap(), script.to_string_lossy());
        // The browser is not waited for, so the file may be written just after `open` returns.
        let mut seen = String::new();
        for _ in 0..100 {
            seen = std::fs::read_to_string(&out).unwrap_or_default();
            if !seen.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(seen.trim(), "https://auth.test/device");
    }

    #[test]
    fn only_a_web_address_is_a_link() {
        assert!(is_link("https://auth.openai.com/codex/device"));
        assert!(is_link(" http://127.0.0.1:1455/auth/callback "));
        assert!(
            is_link("HTTPS://A.TEST/x"),
            "the scheme is not case sensitive"
        );
        // A local file, a javascript: URL or an address with a space in it is not.
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://a.test/a b",
            "auth.openai.com",
            "",
        ] {
            assert!(!is_link(bad), "{bad} must not be opened");
        }
        let e = open("file:///etc/passwd").unwrap_err().to_string();
        assert!(e.contains("not a web address"), "{e}");
    }
}
