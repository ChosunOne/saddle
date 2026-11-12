mod agent;
mod auth;
mod cache;
mod cancel;
mod compact;
mod config;
mod context;
mod defaults;
mod output;
mod paths;
mod provider;
mod reserved;
mod session;
mod store;
mod term;
mod tools;
mod tui;

use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::agent::{Agent, End};
use crate::config::UserConfig;
use crate::provider::Provider;
use crate::session::{PrivateTmp, Session, Setup, session_id};
use crate::term::Terminal;

#[derive(Parser)]
#[command(
    name = "saddle",
    version,
    about = "Coding-agent harness whose plugins run under declared scopes"
)]
struct Cli {
    /// Directory to start from; the workspace is the git toplevel above it.
    #[arg(long, short = 'C', global = true, default_value = ".")]
    dir: PathBuf,
    /// With no command, saddle opens the terminal UI.
    #[command(flatten)]
    agent: AgentArgs,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Args)]
struct AgentArgs {
    /// A built-in provider (synthetic, openai) or a [providers.*] table from config.toml.
    #[arg(long, short)]
    provider: Option<String>,
    /// Overrides the provider's model.
    #[arg(long, short)]
    model: Option<String>,
    /// Model requests per prompt before the loop stops and waits for the user (default: no
    /// limit; `max-steps` in config.toml sets one).
    #[arg(long)]
    max_steps: Option<usize>,
    /// Continue the most recent session in this workspace.
    #[arg(long = "continue", short = 'c')]
    resume_latest: bool,
    /// Write one JSON object per line on stdout instead of text (D52). Implies line mode, even on
    /// a terminal, and leaves stderr for host errors.
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the agent. With a prompt, answer it and exit. Without one, open the terminal UI, or
    /// read one prompt per line when stdin is not a terminal.
    Run {
        prompt: Option<String>,
        #[command(flatten)]
        agent: AgentArgs,
    },
    /// Print the policies in force, the roots and the effective grants.
    Check,
    /// Run one built-in tool as the agent, through the policy gate.
    Tool {
        /// read, write, edit or bash.
        name: String,
        /// The tool input as JSON, e.g. '{"path": "src/main.rs"}'.
        input: String,
    },
    /// Audit log commands.
    Audit {
        #[command(subcommand)]
        cmd: AuditCmd,
    },
}

#[derive(Subcommand)]
enum AuditCmd {
    /// Walk a session log's hash chain and compare its head with sessions.jsonl.
    Verify { log: PathBuf },
}

fn main() -> ExitCode {
    // The sandbox helper (D18) runs before anything else touches the environment or arguments.
    if std::env::args_os().nth(1).is_some_and(|a| a == "__sandbox") {
        saddle_sandbox::helper_main();
    }
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("saddle: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    if let Some(Cmd::Audit {
        cmd: AuditCmd::Verify { log },
    }) = &cli.cmd
    {
        return verify(log);
    }
    // First run: write the default config and policy (D30).
    let paths = paths::Paths::from_env()?;
    let created: Vec<String> = defaults::ensure(&paths)?
        .iter()
        .map(|p| format!("created {} with defaults; edit it freely", p.display()))
        .collect();
    // The agent shows these itself (the TUI's intro, or line mode's header).
    if !matches!(cli.cmd, None | Some(Cmd::Run { .. })) {
        for line in &created {
            eprintln!("saddle: {line}");
        }
    }
    match cli.cmd {
        None => run_agent(&cli.dir, None, cli.agent, paths, created),
        Some(Cmd::Run { prompt, agent }) => run_agent(&cli.dir, prompt, agent, paths, created),
        Some(Cmd::Check) => check(&cli.dir),
        Some(Cmd::Tool { name, input }) => tool(&cli.dir, &name, &input),
        Some(Cmd::Audit { .. }) => unreachable!("handled above"),
    }
}

fn run_agent(
    dir: &Path,
    prompt: Option<String>,
    args: AgentArgs,
    paths: paths::Paths,
    created: Vec<String>,
) -> Result<ExitCode> {
    let user = UserConfig::load(&paths.user_config())?;
    let auth = auth::AuthStore::load(&paths.auth_store())?;
    let mut reported = provider::Reported::stored(&paths.reported_windows());
    let interactive = prompt.is_none()
        && !args.json
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal();
    let mut notes = created;
    // In the TUI, anything missing (a provider, its model, its key) opens the login dialog.
    // Without it there is nowhere to fix that, so it stops here.
    let (provider, start) = match args.provider.or_else(|| user.provider.clone()) {
        None if interactive => (None, tui::Start::ChooseProvider),
        None => bail!(
            "no provider chosen: run `saddle` to pick one, or pass -p (built in: {})",
            provider::builtin_names().join(", ")
        ),
        Some(name) => match provider::resolve(&name, &user.providers, args.model.as_deref()) {
            Ok(config) => {
                // Headless there is no session to update later, so the endpoint is asked now
                // rather than a guess carried into the first turn (D34).
                if !interactive {
                    let key = provider::available_key(&config, auth.key(&name));
                    provider::ask_endpoint(&config, key.as_deref(), &paths.reported_windows());
                    reported = provider::Reported::stored(&paths.reported_windows());
                }
                let p = Provider::new(&name, config, auth.key(&name), &reported);
                match p.missing_key() {
                    Some(_) if interactive => (Some(p), tui::Start::LogIn(name)),
                    Some(why) => bail!("{why}"),
                    None => (Some(p), tui::Start::Ready),
                }
            }
            Err(e) if interactive => {
                notes.push(format!("{e:#}"));
                (None, tui::Start::LogIn(name))
            }
            Err(e) => return Err(e),
        },
    };

    let mut session = Session::open(dir)?;
    let secrets = provider::secret_env_names(&user.providers);
    session.host_env.retain(|(k, _)| !secrets.contains(k));
    let (context, refused) = context::load(&mut session, &user.context, &paths)?;
    if !context.is_empty() {
        let names: Vec<&str> = context.iter().map(|f| f.label.as_str()).collect();
        notes.push(format!("instructions from {}", names.join(", ")));
    }
    notes.extend(refused);

    // A bad `[ui] status` is reported where it can be seen, and the session keeps the default.
    let status = match user.ui.status.as_deref() {
        None => tui::status::Template::default(),
        Some(text) => match tui::status::Template::parse(text) {
            Ok(template) => template,
            Err(e) => {
                notes.push(format!("[ui] status: {e}"));
                tui::status::Template::default()
            }
        },
    };
    let header = tui::Header {
        provider: provider
            .as_ref()
            .map(|p| p.name.clone())
            .unwrap_or_default(),
        model: provider
            .as_ref()
            .map(|p| p.config.model.clone())
            .unwrap_or_default(),
        workspace: session.setup.workspace.clone(),
        audit: session.audit.path().to_path_buf(),
        notes,
        context_window: provider
            .as_ref()
            .map_or(compact::DEFAULT_CONTEXT_WINDOW, Provider::window),
        priced: provider.as_ref().is_some_and(|p| p.price().is_some()),
        mouse: user.ui.mouse.unwrap_or(true),
        status,
        show_reasoning: user.ui.show_reasoning.unwrap_or(true),
        expand_tools: user.ui.expand_tools.unwrap_or(false),
        // Unlimited unless the flag or the config sets a limit (D37).
        max_steps: args.max_steps.or(user.max_steps),
        images: user.ui.images.clone(),
    };
    let store = store::Store::open(&paths.session_db(), session.setup.workspace.clone())?;
    let latest = if args.resume_latest {
        let found = store.latest()?;
        if found.is_none() {
            eprintln!("saddle: no saved session in this workspace; starting a new one");
        }
        found.map(|s| s.id)
    } else {
        None
    };
    let max_steps = header.max_steps;
    let base = agent::base_prompt(&session, &context);
    let system = agent::system_prompt(&session, &context);
    let mut agent = Agent::new(
        provider,
        system,
        base,
        tools::definitions(),
        max_steps,
        Some(store),
        compact::Settings::from_config(&user.compaction),
    );

    if interactive {
        tui::run(agent, session, header, paths, start, latest)?;
        return Ok(ExitCode::SUCCESS);
    }
    let mut out = if args.json {
        output::Output::Json(output::Json::stdout())
    } else {
        output::Output::Human(Terminal::new())
    };
    if let Some(path) = &latest {
        let saved = agent.resume(path)?;
        out.info(&format!(
            "continuing a session of {} messages, {} tokens in and {} out so far",
            saved.items.len(),
            saved.usage.input + saved.usage.cache_read + saved.usage.cache_write,
            saved.usage.output
        ));
    }
    out.opening(
        &header.provider,
        &header.model,
        &header.workspace.display().to_string(),
        &header.audit.display().to_string(),
    );
    for note in &header.notes {
        out.info(note);
    }
    let result = converse(&mut agent, &mut session, &mut out, prompt);
    let closed = session.close();
    let code = result?;
    closed?;
    Ok(code)
}

/// Line mode: one prompt from the command line, or one per line of stdin.
fn converse(
    agent: &mut Agent,
    session: &mut Session,
    out: &mut output::Output,
    prompt: Option<String>,
) -> Result<ExitCode> {
    if let Some(prompt) = prompt {
        return Ok(match line_turn(agent, session, out, &prompt)? {
            End::Answered => ExitCode::SUCCESS,
            _ => ExitCode::from(1),
        });
    }
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        if matches!(text, "/exit" | "/quit") {
            break;
        }
        line_turn(agent, session, out, text)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// One line-mode prompt: `!cmd` and `!!cmd` run a command, `/compact` compacts, anything else is
/// a turn. Ctrl+C cancels it (D19).
fn line_turn(
    agent: &mut Agent,
    session: &mut Session,
    out: &mut output::Output,
    text: &str,
) -> Result<End> {
    cancel::reset();
    let end = {
        let _sigint = cancel::SigintGuard::install();
        if let Some(rest) = text.strip_prefix('!') {
            let (command, share) = match rest.strip_prefix('!') {
                Some(command) => (command, false),
                None => (rest, true),
            };
            agent.shell(session, command.trim(), share, out)?
        } else if let Some(focus) = text.strip_prefix("/compact") {
            let focus = Some(focus.trim()).filter(|f| !f.is_empty());
            match agent.compact(session, compact::Reason::Manual, focus, out)? {
                agent::Compacted::Done | agent::Compacted::Nothing => End::Answered,
                agent::Compacted::Cancelled => End::Cancelled,
                agent::Compacted::Failed => End::Failed,
            }
        } else {
            let end = agent.turn(session, text, out)?;
            out.usage(agent.turn_usage);
            end
        }
    };
    out.turn_end(&end);
    Ok(end)
}

fn check(dir: &std::path::Path) -> Result<ExitCode> {
    let tmp = PrivateTmp::create(&session_id())?;
    let setup = Setup::load(dir, tmp.path())?;
    // The layering happens in `Setup::load`, so an invalid project file is reported here rather
    // than on the first tool call.
    println!("workspace  {}", setup.workspace.display());
    println!("roots");
    for (name, path) in setup.roots.iter() {
        let note = if name.as_str() == "tmp" {
            "  (per session)"
        } else {
            ""
        };
        println!("  {:<12} {}{note}", name.as_str(), path.display());
    }
    // Both files, in the order they apply, since a grant can come from either (D43).
    println!(
        "policy     layered: {} (sha256 {})",
        setup.paths.default_policy().display(),
        &setup.global_sha256[..16]
    );
    println!(
        "           then    {} (sha256 {})",
        setup.policy_path.display(),
        &setup.policy_sha256[..16]
    );
    println!("grants");
    for line in &setup.merged {
        println!("  {line}");
    }
    for note in &setup.notes {
        println!("note       {note}");
    }
    match saddle_sandbox::availability() {
        Ok(abi) => println!("exec       available (landlock ABI {abi})"),
        Err(reason) => println!("exec       unavailable: {reason}"),
    }
    // What an attached picture will do, and what decided it, since a terminal that draws nothing
    // looks the same as one that was never asked (D59).
    {
        let get = |k: &str| std::env::var(k).ok();
        let setting = UserConfig::load(&setup.paths.user_config())?.ui.images;
        let protocol = tui::image::detect(setting.as_deref(), &get);
        let why = match &setting {
            Some(s) => format!("[ui] images = {s}"),
            None => tui::image::reason(protocol, &get),
        };
        println!("images     {protocol:?}, {why}");
    }
    let user = UserConfig::load(&setup.paths.user_config())?;
    let auth = auth::AuthStore::load(&setup.paths.auth_store())?;
    let reported = provider::Reported::stored(&setup.paths.reported_windows());
    match &user.provider {
        None => println!("provider   none chosen yet; run saddle to pick one"),
        Some(name) => match provider::resolve(name, &user.providers, None) {
            Ok(config) => {
                let model = config.model.clone();
                let p = Provider::new(name, config, auth.key(name), &reported);
                let key = match p.missing_key() {
                    Some(_) => "no key yet; log in with /login in the TUI",
                    None => "key available",
                };
                println!("provider   {name} · {model} · {key}");
                println!(
                    "context    {} tokens for this model, {}",
                    p.window(),
                    p.window_source()
                );
                // D36: what a reply costs, and where that came from, since nothing shows it until
                // a session has spent something.
                println!("price      {}", price_line(p.price()));
                if let Some(url) = &p.config.usage_url {
                    // D36: the same figures the status line shows, so a shape that does not parse
                    // can be seen without opening a session.
                    match provider::fetch_usage(&p.config, auth.key(name), url) {
                        Ok(page) => {
                            let (quota, spent) = provider::quota_from_page(&page);
                            println!(
                                "usage      {}",
                                usage_line(&quota, spent, std::time::SystemTime::now())
                            );
                            // When nothing was recognised, the field names are what is needed to
                            // teach the parser the shape; the figures are not printed (D36).
                            if quota.is_empty() && spent.is_none() {
                                let paths = provider::page_paths(&page);
                                if !paths.is_empty() {
                                    println!("           numbers at: {}", paths.join(", "));
                                }
                            }
                        }
                        Err(e) => println!("usage      {url}: {}", models_error(&e)),
                    }
                }
            }
            Err(e) => println!("provider   {name}: {e:#}"),
        },
    }
    Ok(ExitCode::SUCCESS)
}

/// The price as `check` prints it: dollars per million, and nothing when nobody priced the model.
fn price_line(price: Option<provider::Price>) -> String {
    let Some(price) = price else {
        return "not stated; set [providers.<name>.pricing] to show a cost".into();
    };
    let per_million = |p: f64| format!("${:.2}", p * 1_000_000.0);
    let mut parts = vec![
        format!("{} in", per_million(price.input)),
        format!("{} out", per_million(price.output)),
    ];
    if let Some(read) = price.cache_read {
        parts.push(format!("{} cache read", per_million(read)));
    }
    if let Some(write) = price.cache_write {
        parts.push(format!("{} cache write", per_million(write)));
    }
    format!("{} per million tokens", parts.join(", "))
}

/// The two ways a model list or usage page can fail, as one sentence.
fn models_error(e: &provider::ModelsError) -> String {
    match e {
        provider::ModelsError::Rejected(why) => format!("the key was refused: {why}"),
        provider::ModelsError::Failed(why) => why.clone(),
    }
}

/// The usage page as `check` prints it, including "nothing found" rather than an empty line, since
/// a shape the parser does not know is the likeliest reason a figure does not appear (D36).
fn usage_line(quota: &provider::Quota, spent: Option<f64>, now: std::time::SystemTime) -> String {
    let mut parts = Vec::new();
    if let Some(tokens) = quota.tokens {
        parts.push(match quota.tokens_limit {
            Some(limit) => format!("{tokens} of {limit} tokens left"),
            None => format!("{tokens} tokens left"),
        });
    }
    if let Some(requests) = quota.requests {
        parts.push(match quota.requests_limit {
            Some(limit) => format!("{requests} of {limit} requests left"),
            None => format!("{requests} requests left"),
        });
    }
    if let Some(percent) = quota.percent {
        parts.push(format!("{percent:.0}% of the plan left"));
    }
    if let Some(credits) = quota.credits {
        parts.push(format!("{credits:.2} credits left"));
    }
    if let Some(spent) = spent {
        parts.push(format!("{spent:.4} spent"));
    }
    if let Some(reset) = &quota.reset {
        let left = provider::reset_left(reset, now);
        parts.push(if left == "now" {
            "resets now".to_owned()
        } else {
            format!("resets in {left}")
        });
    }
    if parts.is_empty() {
        "the response states no figure saddle recognises".into()
    } else {
        parts.join(", ")
    }
}

fn tool(dir: &std::path::Path, name: &str, input: &str) -> Result<ExitCode> {
    if !tools::NAMES.contains(&name) {
        bail!("unknown tool `{name}` (known: {})", tools::NAMES.join(", "));
    }
    let input: serde_json::Value = serde_json::from_str(input).context("tool input is not JSON")?;
    let mut session = Session::open(dir)?;
    let log = session.audit.path().to_path_buf();
    let result = tools::call(&mut session, name, &input);
    session.close()?;
    let out = result?;
    if out.ok {
        println!("{}", out.content);
    } else {
        eprintln!("{}", out.content);
    }
    eprintln!("audit: {}", log.display());
    Ok(if out.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn verify(log: &std::path::Path) -> Result<ExitCode> {
    let v = saddle_policy::audit::verify(log).with_context(|| format!("{}", log.display()))?;
    println!("{} records, chain intact, head {}", v.records, v.head);
    let paths = paths::Paths::from_env()?;
    let index = std::fs::read_to_string(paths.sessions_index()).unwrap_or_default();
    let canonical = log.canonicalize()?;
    let recorded = index
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|e| {
            e["log"]
                .as_str()
                .map(PathBuf::from)
                .and_then(|p| p.canonicalize().ok())
                .as_ref()
                == Some(&canonical)
        });
    match recorded {
        Some(e) if e["head"] == v.head.as_str() => println!("head matches sessions.jsonl"),
        Some(e) => {
            println!(
                "head differs from sessions.jsonl ({}); the log was truncated or rewritten",
                e["head"]
            );
            return Ok(ExitCode::from(1));
        }
        None => println!("no entry in sessions.jsonl (session still open, or index missing)"),
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_usage_line_says_what_is_left_and_when_it_refills() {
        // The real page, through the parser, so the line is checked against what `saddle check`
        // will actually print for this provider rather than against a hand-built figure (D49).
        let page: serde_json::Value = serde_json::from_str(
            r#"{"subscription":{"limit":500,"requests":0,
                    "renewsAt":"2026-11-12T13:18:26.671Z"},
                "search":{"hourly":{"limit":250,"requests":0}},
                "planWeeklyLimit":{"nextRegenAt":"2026-11-12T08:26:09.000Z",
                    "percentRemaining":94.43746622040817,"nextRegenPercent":2}}"#,
        )
        .unwrap();
        let (quota, spent) = provider::quota_from_page(&page);
        // An hour before the stamp in the page, so the line says how long is left rather than
        // repeating the date the provider already sent (D49).
        let regen = humantime::parse_rfc3339("2026-11-12T08:26:09.000Z").unwrap();
        let now = regen - std::time::Duration::from_secs(3600);
        assert_eq!(
            usage_line(&quota, spent, now),
            "94% of the plan left, resets in 1h"
        );
        // Nothing recognised is said plainly, so the diagnostic reads as a finding.
        assert_eq!(
            usage_line(&provider::Quota::default(), None, now),
            "the response states no figure saddle recognises"
        );
    }
}
