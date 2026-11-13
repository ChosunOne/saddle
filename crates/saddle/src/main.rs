mod agent;
mod allow;
mod auth;
mod cache;
mod cancel;
mod compact;
mod config;
mod context;
mod defaults;
mod ignore;
mod net;
mod output;
mod paths;
mod provider;
mod reserved;
mod session;
mod skills;
mod store;
mod term;
mod tools;
mod tui;

use std::io::{BufRead, IsTerminal, Write};
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
    /// Add a grant for this run only, as `scope` or `scope:what`: `--grant fs-read:scratch`,
    /// `--grant exec`. Nothing is written, so it is gone when the session ends (D63).
    #[arg(long, global = true)]
    grant: Vec<String>,
    /// Drop a scope's grants for this run only, as in `--revoke net` (D63).
    #[arg(long, global = true)]
    revoke: Vec<String>,
    /// Use this theme for one run without changing what is saved, as pi's `--use-theme` (D72).
    #[arg(long, global = true, value_name = "NAME")]
    use_theme: Option<String>,
    /// Name the session, so an allowance made with `saddle session allow` can be used by it. One is
    /// generated and printed when the workspace needs approving (D60).
    #[arg(long, global = true)]
    session: Option<String>,
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
    /// Session allowances: letting one session id use a workspace that holds credentials (D60).
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Audit log commands.
    Audit {
        #[command(subcommand)]
        cmd: AuditCmd,
    },
}

#[derive(Subcommand)]
enum SessionCmd {
    /// Allow one session id to use a workspace. Run this as yourself, in a terminal: the file it
    /// writes is outside every root, so nothing the agent runs can write it (D60).
    Allow {
        /// The session id, as printed by the session that was refused.
        id: String,
        /// The workspace to allow. Taken from the waiting request when not given.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// List the session ids waiting for an allowance, with their workspaces.
    Pending,
    /// List the workspaces a session id is already allowed to use, so one can be picked instead of
    /// remembered (D61). `filter` matches the id or the workspace path, ignoring case.
    List {
        #[arg(default_value = "")]
        filter: String,
    },
    /// Withdraw an allowance, so that session id can no longer run in the workspace (D61). Takes
    /// what the list shows: a number, or an id or unique prefix.
    Revoke {
        /// A number from `session list`, or an id or unique prefix.
        id: String,
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
    // First run: write the default config (D30).
    let paths = paths::Paths::from_env()?;
    // Before the defaults are written, so a config created here does not shadow the grants an
    // older install still keeps in `policy.toml` (D63).
    let migrated: Vec<std::path::PathBuf> =
        config::migrate(&paths.user_config(), &paths.policies_dir()).unwrap_or_default();
    let mut created: Vec<String> = defaults::ensure(&paths)?
        .iter()
        .map(|p| format!("created {} with defaults; edit it freely", p.display()))
        .collect();
    if !migrated.is_empty() {
        created.push(format!(
            "moved {} grants file(s) into {}; the originals are kept as .imported",
            migrated.len(),
            paths.user_config().display()
        ));
    }
    // The agent shows these itself (the TUI's intro, or line mode's header).
    if !matches!(cli.cmd, None | Some(Cmd::Run { .. })) {
        for line in &created {
            eprintln!("saddle: {line}");
        }
    }
    let overrides = config::Overrides {
        grant: cli.grant,
        revoke: cli.revoke,
    };
    // One id for the whole invocation: it names the session in the audit log, and it is what an
    // allowance is written against (D60).
    let session = match cli.session {
        Some(id) => {
            allow::check_id(&id)?;
            // A prefix is enough when it names exactly one allowance, so an id does not have to be
            // copied in full (D61). Without a match the id is used as given, which is what an
            // allowance is then made for.
            let allowance = allow::Allowance::new(&paths.state);
            match allow::Allowance::resolve(&id, &allowance.allowed())? {
                Some(full) => full,
                None => id,
            }
        }
        None => session_id(),
    };
    match cli.cmd {
        None => run_agent(
            &cli.dir,
            None,
            cli.agent,
            paths,
            created,
            OneRun {
                session: &session,
                overrides: &overrides,
                theme: cli.use_theme,
            },
        ),
        Some(Cmd::Run { prompt, agent }) => run_agent(
            &cli.dir,
            prompt,
            agent,
            paths,
            created,
            OneRun {
                session: &session,
                overrides: &overrides,
                theme: cli.use_theme,
            },
        ),
        Some(Cmd::Check) => check(&cli.dir, &overrides),
        Some(Cmd::Tool { name, input }) => {
            tool(&cli.dir, &name, &input, &session, &paths, &overrides)
        }
        Some(Cmd::Session { cmd }) => session_cmd(cmd, &paths),
        Some(Cmd::Audit { .. }) => unreachable!("handled above"),
    }
}

/// What this invocation chose that is not written to any file (D63, D72).
struct OneRun<'a> {
    session: &'a str,
    overrides: &'a config::Overrides,
    theme: Option<String>,
}

fn run_agent(
    dir: &Path,
    prompt: Option<String>,
    args: AgentArgs,
    paths: paths::Paths,
    created: Vec<String>,
    one: OneRun<'_>,
) -> Result<ExitCode> {
    let user = UserConfig::load(&paths.user_config())?;
    // Before a provider is chosen and before anything is opened, so a refusal costs nothing: no
    // session, no audit log, no network call (D60).
    let picked = {
        let workspace = dir
            .canonicalize()
            .map(|abs| paths::find_workspace(&abs))
            .unwrap_or_else(|_| dir.to_path_buf());
        gate_workspace(&paths, &workspace, one.session)?
    };
    let session = picked.unwrap_or_else(|| one.session.to_owned());
    let session = session.as_str();
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

    let mut session = Session::open_with(dir, Some(session), one.overrides)?;
    let secrets = provider::secret_env_names(&user.providers);
    session.host_env.retain(|(k, _)| !secrets.contains(k));
    let (context, refused) = context::load(&mut session, &user.context, &paths)?;
    if !context.is_empty() {
        let names: Vec<&str> = context.iter().map(|f| f.label.as_str()).collect();
        notes.push(format!("instructions from {}", names.join(", ")));
    }
    notes.extend(refused);
    if !session.skills.is_empty() {
        let names: Vec<&str> = session.skills.iter().map(|s| s.name.as_str()).collect();
        notes.push(format!("skills: {}", names.join(", ")));
    }
    notes.extend(session.skill_notes.clone());

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
    // A theme that does not build is reported here rather than only in the TUI, so a mistake in
    // config.toml is visible wherever saddle runs. The default is drawn meanwhile (D71).
    if let Err(e) = tui::theme_selected(&user) {
        notes.push(format!("{e:#}"));
    }
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
        // One run's choice wins over the saved setting, and is not written anywhere.
        theme: one.theme.clone().or_else(|| user.ui.theme.clone()),
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
    // Kept so the prompt can be rebuilt when discovery runs again (D70).
    agent.set_context(context.clone());

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
    let result = converse(
        &mut agent,
        &mut session,
        &mut out,
        prompt,
        header.theme.as_deref(),
    );
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
    theme: Option<&str>,
) -> Result<ExitCode> {
    if let Some(prompt) = prompt {
        return Ok(match line_turn(agent, session, out, &prompt, theme)? {
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
        line_turn(agent, session, out, text, theme)?;
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
    theme: Option<&str>,
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
        } else if text == "/reload" {
            // Everything the prompt and the interface were built from, read again (D70). Skills and
            // instruction files reach the model on its next request; a newly added instruction file
            // is also handed to the conversation, so the transcript shows it arriving.
            let cfg = session.setup.context.clone();
            let paths = session.setup.paths.clone();
            let (files, refused) = context::load(session, &cfg, &paths)?;
            let added = agent.rebuild_prompt(session, files);
            agent.note_context(&added);
            for file in &added {
                out.info(&format!("instructions reloaded: {}", file.label));
            }
            for note in refused {
                out.info(&note);
            }
            session.reload_skills()?;
            out.info(&format!(
                "skills reloaded: {}",
                skills::names(&session.skills)
            ));
            for note in &session.skill_notes {
                out.info(note);
            }
            out.info(
                "a skill or instruction file already read into this conversation is unchanged",
            );
            End::Answered
        } else if text == "/theme" || text.starts_with("/theme ") {
            // A screen has the list to choose from (D74); a piped session has the names, and the
            // same switch, so a command meant for the interface is not sent to the model as a
            // prompt and charged for.
            theme_command(
                session,
                out,
                theme,
                text.trim_start_matches("/theme").trim(),
            )?;
            End::Answered
        } else if let Some(rest) = text.strip_prefix("/skill:") {
            // The same expansion as the TUI's, from the skills the session discovered (D69). A
            // prompt written as a skill command works the same way in line mode, so a piped prompt
            // does not need a different spelling.
            let name = rest.split_whitespace().next().unwrap_or_default();
            let args = rest[name.len()..].trim();
            match session.skills.iter().find(|s| s.name == name) {
                Some(skill) => {
                    let prompt = skills::invocation(skill, args);
                    let name = skill.name.clone();
                    session.record_skill_load(&name)?;
                    let end = agent.turn(session, &prompt, out)?;
                    out.usage(agent.turn_usage);
                    end
                }
                None => {
                    out.info(&format!(
                        "no skill `{name}`; this session has {}",
                        skills::names(&session.skills)
                    ));
                    End::Answered
                }
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

/// `/theme` without a screen: report the names, or switch to one and save it, as the interface does
/// (D71, D74).
fn theme_command(
    session: &Session,
    out: &mut output::Output,
    in_force: Option<&str>,
    asked: &str,
) -> Result<()> {
    let config = session.setup.paths.user_config();
    let user = UserConfig::load(&config)?;
    if asked.is_empty() {
        out.info(&format!(
            "theme: {}",
            in_force.unwrap_or_else(|| user.ui.theme.as_deref().unwrap_or("default"))
        ));
        out.info(&format!(
            "built in: {}",
            tui::theme::builtin_names().join(", ")
        ));
        if !user.themes.is_empty() {
            let defined: Vec<&str> = user.themes.keys().map(String::as_str).collect();
            out.info(&format!("defined in config.toml: {}", defined.join(", ")));
        }
        out.info(
            "`/theme <name>` switches and saves it; in the full-screen interface /theme lists them",
        );
        return Ok(());
    }
    // Built before it is written, for the reason the interface gives: a name that does not build
    // must not be saved, or the next session opens on an error whose cause is not on screen.
    match tui::theme::build(asked, &user.themes) {
        Ok(theme) => {
            config::edit(&config, |doc| {
                if doc.get("ui").is_none() {
                    doc.insert("ui", toml_edit::Item::Table(toml_edit::Table::new()));
                }
                doc["ui"]["theme"] = toml_edit::value(asked);
                Ok(())
            })?;
            out.info(&format!("theme: {}; saved in config.toml", theme.name));
        }
        Err(e) => out.info(&format!("{e:#}")),
    }
    Ok(())
}

/// Refuses to open a session in a workspace that puts credentials in reach, unless a person has
/// allowed this session id to use it (D60).
///
/// The warning cannot be answered where it is printed: there is no keypress and no `--force`. It
/// names the session id and the command that allows it, and the allowance is written by a separate
/// invocation, from a person's terminal, into the state directory — outside every root, so the agent
/// cannot grant itself anything (D16). The allowance covers this id and that workspace and is
/// removed when the session ends. `allow-sensitive` in the user's config skips the check entirely
/// for machines where the home directory holds nothing secret.
fn gate_workspace(paths: &paths::Paths, workspace: &Path, id: &str) -> Result<Option<String>> {
    let user = UserConfig::load(&paths.user_config())?;
    if user.allow_sensitive {
        return Ok(None);
    }
    let exposed = paths::exposed(workspace, &paths::sensitive_dirs());
    if exposed.is_empty() {
        return Ok(None);
    }
    let allowance = allow::Allowance::new(&paths.state);
    if allowance.allows(id, workspace) {
        return Ok(None);
    }
    // Sessions already allowed for this workspace. Picking one is how a person avoids remembering
    // an id (D61); it does not approve anything new, because a person made those allowances.
    let reusable = allowance.for_workspace(workspace);
    // Record the request, so the id stays findable after this message has scrolled away.
    allowance.request(id, workspace)?;
    let chosen = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "saddle".into());
    let mut message = format!(
        "this session ({id}) would be rooted at {}\n",
        workspace.display()
    );
    for what in exposed {
        message.push_str(&format!("  in reach: {what}\n"));
    }
    message.push_str(
        "  a root cannot be narrowed from the inside, so everything beneath it is readable and\n  \
         writable by the agent and by any command it runs\n",
    );
    message.push_str("  nothing was opened.\n");
    if !reusable.is_empty() {
        let plural = if reusable.len() == 1 { "" } else { "s" };
        message.push_str(&format!(
            "  {} session id{plural} may already use this workspace; pass one with --session\n  \
             (a prefix is enough), or see `session list`:\n",
            reusable.len()
        ));
        for (n, (old, entry)) in reusable.iter().enumerate() {
            message.push_str(&format!(
                "      {}  {old}  {}\n",
                n + 1,
                allow::ago(entry.when)
            ));
        }
        message.push_str("  or allow this new session id, in your own terminal outside saddle:\n");
    } else {
        message.push_str(
            "  To allow this session id to use that workspace, run in your own terminal, outside\n  \
             saddle:\n",
        );
    }
    message.push_str(&format!(
        "      {chosen} session allow {id} --workspace {}\n",
        workspace.display()
    ));
    message.push_str(&format!(
        "  then start it again with:  {chosen} --session {id} ...\n"
    ));
    message.push_str(
        "  or set `allow-sensitive = true` in config.toml if this machine's home holds no secrets",
    );
    // Only where a person is sitting: a picker that read from a pipe would eat a prompt.
    if !reusable.is_empty() && std::io::stdin().is_terminal() {
        return pick_allowed(&reusable, workspace);
    }
    bail!("{message}")
}

/// Asks which already-allowed session id to use, and returns it (D61).
///
/// This is not approving anything: the allowances listed were made by a person with
/// `saddle session allow`. Picking one just saves copying an id, and an empty answer stops as the
/// refusal would have.
fn pick_allowed(candidates: &[(String, allow::Entry)], workspace: &Path) -> Result<Option<String>> {
    eprintln!("saddle: pick one of these, or press enter to stop:");
    for (n, (id, entry)) in candidates.iter().enumerate() {
        eprintln!(
            "  {}  {id}  {}  {}",
            n + 1,
            allow::ago(entry.when),
            entry.workspace.display()
        );
    }
    eprint!("  number or id: ");
    let _ = std::io::stderr().flush();
    let mut typed = String::new();
    if std::io::stdin().read_line(&mut typed)? == 0 {
        bail!("not approved: no session was opened");
    }
    match allow::Allowance::resolve(&typed, candidates)? {
        Some(id) => {
            eprintln!("  using session {id}");
            let _ = workspace;
            Ok(Some(id))
        }
        None if typed.trim().is_empty() => bail!("not approved: no session was opened"),
        None => bail!("not approved: nothing matched `{}`", typed.trim()),
    }
}

/// `saddle session`: the allowance commands, which are a person's tool and never a tool the agent
/// can call (D45, D46). The files are in the state directory, so a sandboxed child cannot write
/// them either.
fn session_cmd(cmd: SessionCmd, paths: &paths::Paths) -> Result<ExitCode> {
    let allowance = allow::Allowance::new(&paths.state);
    match cmd {
        SessionCmd::Allow { id, workspace } => {
            allow::check_id(&id)?;
            let workspace = match workspace {
                Some(p) => p,
                None => {
                    let waiting = allowance.waiting();
                    match waiting.iter().find(|(waiting, _)| *waiting == id) {
                        Some((_, entry)) => entry.workspace.clone(),
                        None => bail!(
                            "session {id} is not waiting; pass --workspace to name the workspace to \
                             allow it for"
                        ),
                    }
                }
            };
            let workspace = workspace
                .canonicalize()
                .with_context(|| format!("{}", workspace.display()))?;
            allowance.allow(&id, &workspace)?;
            println!(
                "session {id} may use {} until it ends; the allowance is removed when it does",
                workspace.display()
            );
        }
        SessionCmd::List { filter } => {
            let needle = filter.trim().to_lowercase();
            let rows: Vec<(String, allow::Entry)> = allowance
                .allowed()
                .into_iter()
                .filter(|(id, entry)| {
                    needle.is_empty()
                        || id.to_lowercase().contains(&needle)
                        || entry
                            .workspace
                            .to_string_lossy()
                            .to_lowercase()
                            .contains(&needle)
                })
                .collect();
            if rows.is_empty() {
                let hint = if needle.is_empty() {
                    "no session is allowed to use a sensitive workspace"
                } else {
                    "no allowance matches that"
                };
                println!("{hint}");
                return Ok(ExitCode::SUCCESS);
            }
            for (n, (id, entry)) in rows.iter().enumerate() {
                println!(
                    "  {}  {id}  {}  {}",
                    n + 1,
                    allow::ago(entry.when),
                    entry.workspace.display()
                );
            }
            // The command that undoes each line, named where the lines are (D61).
            println!(
                "  withdraw one with: saddle session revoke <number>   (or a prefix of the id)"
            );
        }
        SessionCmd::Pending => {
            let waiting = allowance.waiting();
            if waiting.is_empty() {
                println!("no session is waiting for an allowance");
            }
            for (id, entry) in waiting {
                // A refused session exits, so its request is a record of what was asked for
                // rather than a live process. The age is what tells a person whether it is the
                // message still on their screen.
                let allowed = if allowance.allows(&id, &entry.workspace) {
                    "  (allowed)"
                } else {
                    ""
                };
                println!(
                    "{id}  {}{allowed}  {}",
                    allow::ago(entry.when),
                    entry.workspace.display()
                );
            }
        }
        SessionCmd::Revoke { id } => {
            // The same short forms as `--session`, because a revocation should not be harder to
            // type than the permission it removes (D61).
            let allowed = allowance.allowed();
            let id = match allow::Allowance::resolve(&id, &allowed)? {
                Some(id) => id,
                None => {
                    if allow::check_id(&id).is_ok() && !allowed.iter().any(|(a, _)| *a == id) {
                        bail!("no allowance for {id}; `saddle session list` shows them");
                    }
                    bail!("nothing matches `{id}`; `saddle session list` shows the allowances");
                }
            };
            let entry = allowance
                .granted(&id)
                .map(|e| e.workspace.display().to_string())
                .unwrap_or_default();
            allowance.revoke(&id);
            if entry.is_empty() {
                println!("session {id} withdrawn");
            } else {
                println!("session {id} may no longer use {entry}");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn check(dir: &std::path::Path, overrides: &config::Overrides) -> Result<ExitCode> {
    let tmp = PrivateTmp::create(&session_id())?;
    let setup = Setup::load(dir, tmp.path(), overrides)?;
    // The layering happens in `Setup::load`, so an invalid project file is reported here rather
    // than on the first tool call.
    println!("workspace  {}", setup.workspace.display());
    for what in &setup.exposed {
        // `check` is the review tool, so it reports what a session here would put in reach and
        // still succeeds: no directory is invalid, the person decides (D60).
        println!("           in reach: {what}");
    }
    // D65: the file tools' namespace, which is not the same set as what a child may reach below.
    println!("roots      what read, ls, find, grep, write and edit decide over");
    for (name, path) in setup.roots.iter() {
        let note = if name.as_str() == "tmp" {
            "  (per session)"
        } else {
            ""
        };
        println!("  {:<12} {}{note}", name.as_str(), path.display());
    }
    // Both files, in the order they apply, since a grant can come from either (D43).
    // One file now (D63): the base grants and this workspace's own, with the hash of each so a
    // change is visible.
    println!(
        "grants     {} (sha256 {})",
        setup.policy_path.display(),
        &setup.global_sha256[..16]
    );
    if setup.policy_text.is_empty() {
        println!(
            "           {} has no grants for this directory: it inherits the base",
            setup.workspace_policy_path.display()
        );
    } else {
        println!(
            "           then {} (sha256 {})",
            setup.workspace_policy_path.display(),
            &setup.policy_sha256[..16]
        );
    }
    println!("effective");
    for line in &setup.merged {
        println!("  {line}");
    }
    // D66: directories the file tools reach outside every root, and the tools on each. Not roots:
    // no policy can name one, and they are not part of the session.
    if !setup.tool_paths.is_empty() {
        println!(
            "tool paths  reachable by these tools, outside every root, not part of the session"
        );
        for path in &setup.tool_paths {
            println!("  {:<28} {}", path.written, path.tools.join(", "));
        }
    }
    // A pattern is an input; this list is the authority a command actually runs under (D62).
    if !setup.extras.include.is_empty() {
        println!("exec reads");
        for path in &setup.extras.include {
            println!("  {}", path.display());
        }
    }
    for note in &setup.notes {
        println!("note       {note}");
    }
    match saddle_sandbox::availability() {
        Ok(abi) => println!("exec       available (landlock ABI {abi})"),
        Err(reason) => println!("exec       unavailable: {reason}"),
    }
    // A command used to have no network at all; now it has its own, with one door (D79).
    match saddle_sandbox::network_availability() {
        Ok(what) => println!("net        a command gets {what}"),
        Err(reason) => println!("net        a command has no network: {reason}"),
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

fn tool(
    dir: &std::path::Path,
    name: &str,
    input: &str,
    session: &str,
    paths: &paths::Paths,
    overrides: &config::Overrides,
) -> Result<ExitCode> {
    if !tools::NAMES.contains(&name) {
        bail!("unknown tool `{name}` (known: {})", tools::NAMES.join(", "));
    }
    let input: serde_json::Value = serde_json::from_str(input).context("tool input is not JSON")?;
    // The same gate as the model loop: `saddle tool` runs as the agent (D17, D60).
    let workspace = dir
        .canonicalize()
        .map(|abs| paths::find_workspace(&abs))
        .unwrap_or_else(|_| dir.to_path_buf());
    let picked = gate_workspace(paths, &workspace, session)?;
    let session = picked.as_deref().unwrap_or(session);
    let mut session = Session::open_with(dir, Some(session), overrides)?;
    // Discovery's notes say why a skill was not advertised. Line mode shows them in its header and
    // the TUI in its intro; without this a refused or malformed skill is silent here, which is the
    // shape a silent skip takes (D68).
    for note in &session.skill_notes {
        eprintln!("saddle: {note}");
    }
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
        // Intact but unvouched: the chain holds, yet no index entry names this log, so it cannot
        // be checked against the close record. Said plainly rather than passed over (D10).
        None => println!("chain intact, but no entry in sessions.jsonl names this log"),
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
