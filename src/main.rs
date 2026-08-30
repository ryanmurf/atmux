use std::{io, net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use atmux::{app::App, config::Config, tmux::Tmux, ui, web};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    /// Use a configuration file other than ~/.config/atmux/config.toml.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Create the default configuration file and print its path.
    Init {
        /// Replace an existing configuration file.
        #[arg(long)]
        force: bool,
    },
    /// Print the active configuration path.
    ConfigPath,
    /// Internal fail-closed bridge for fixed recovery scripts.
    #[command(hide = true)]
    ScopedExec {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Check tmux, configuration, folders, and launcher profiles.
    Doctor,
    /// Run the streaming web dashboard and stateless MCP server.
    Web {
        /// Address for the HTTP server.
        #[arg(long, default_value = "127.0.0.1:7345")]
        bind: SocketAddr,
        /// Acknowledge that non-loopback access is equivalent to shell access.
        #[arg(long)]
        allow_remote: bool,
        /// Additional accepted HTTP Host value (repeatable, including port when used).
        #[arg(long = "allowed-host", value_name = "HOST")]
        allowed_hosts: Vec<String>,
        /// Additional accepted browser Origin value (repeatable, including scheme and port).
        #[arg(long = "allowed-origin", value_name = "ORIGIN")]
        allowed_origins: Vec<String>,
    },
    /// Inspect, collect, backfill, or import native Pulse usage data.
    #[cfg(feature = "pulse")]
    Pulse {
        #[command(subcommand)]
        command: PulseCommand,
    },
}

#[cfg(feature = "pulse")]
#[derive(Debug, Subcommand)]
enum PulseCommand {
    /// Diagnose one configured account without changing its database.
    Doctor {
        /// Explicit configured Pulse account id.
        #[arg(long)]
        account_id: i64,
    },
    /// Run one bounded collection/report cycle without starting a scheduler.
    Push {
        /// Required acknowledgement that this is a one-shot operation.
        #[arg(long)]
        once: bool,
        /// Explicitly scan bounded full token history instead of the recent window.
        #[arg(long)]
        backfill: bool,
        /// Explicitly start a new generation after a completed backfill.
        #[arg(long, requires = "backfill")]
        restart_backfill: bool,
        /// Explicit configured Pulse account id.
        #[arg(long)]
        account_id: i64,
    },
    /// Read and reconcile a legacy Claude Pulse `SQLite` database non-destructively.
    Import {
        /// Legacy `SQLite` database opened read-only.
        source: PathBuf,
        /// Existing configured target Pulse account id.
        #[arg(long)]
        account_id: i64,
        /// Required when the legacy database contains multiple accounts.
        #[arg(long)]
        source_account_id: Option<i64>,
        /// Attribution for legacy rows that do not identify a machine.
        #[arg(long)]
        fallback_machine: Option<String>,
        /// Map OLD=CANONICAL legacy machine names (repeatable, at most 64).
        #[arg(long = "machine-alias", value_parser = parse_key_value)]
        machine_alias: Vec<(String, String)>,
        /// Inspect, plan, and reconcile without writing target rows.
        #[arg(long)]
        dry_run: bool,
        /// Replace an inline legacy key with `PROFILE=ENV_NAME` (repeatable).
        #[arg(long = "credential-env", value_parser = parse_key_value)]
        credential_env: Vec<(String, String)>,
        /// Replace an inline legacy key with PROFILE=/absolute/file (repeatable).
        #[arg(long = "credential-file", value_parser = parse_key_value)]
        credential_file: Vec<(String, String)>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    atmux::tls::install_crypto_provider()?;
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or(Config::path()?);
    match cli.command {
        Some(Commands::Init { force }) => {
            Config::write_default(&config_path, force)?;
            println!("{}", config_path.display());
            return Ok(());
        }
        Some(Commands::ConfigPath) => {
            println!("{}", config_path.display());
            return Ok(());
        }
        Some(Commands::ScopedExec { command }) => {
            return Tmux::scoped_exec(&config_path, command);
        }
        Some(Commands::Doctor) => return doctor(&config_path),
        Some(Commands::Web {
            bind,
            allow_remote,
            allowed_hosts,
            allowed_origins,
        }) => {
            let (config, _) = Config::load(Some(&config_path))?;
            return web::serve(config, bind, allow_remote, allowed_hosts, allowed_origins).await;
        }
        #[cfg(feature = "pulse")]
        Some(Commands::Pulse { command }) => {
            let (config, _) = Config::load(Some(&config_path))?;
            return run_pulse_command(&config, command).await;
        }
        None => {}
    }

    let (config, config_path) = Config::load(Some(&config_path))?;
    let app = App::new(config, config_path)?;
    run(app)
}

fn doctor(config_path: &std::path::Path) -> Result<()> {
    Tmux::check()?;
    println!("✓ tmux       available");
    let (config, path) = Config::load(Some(config_path))?;
    println!("✓ config     {}", path.display());
    match Tmux::check_agent_resources(&config.agent_resources)? {
        Some(memory_max_bytes) => println!(
            "✓ MemoryMax  {memory_max_bytes} bytes (systemd user-scope probe passed and was collected)"
        ),
        None => println!("- MemoryMax  disabled for agent scopes"),
    }
    let folders = config.directories();
    println!("✓ folders    {} discovered", folders.len());
    let harnesses = config.harnesses();
    for harness in harnesses {
        println!(
            "✓ {harness:<10} {} profile(s)",
            config.profiles_for(&harness).len()
        );
    }
    println!("✓ machine    {} (this machine)", config.node.id);
    if config.node.token_env.is_some() || config.node.token_file.is_some() {
        match atmux::machine::resolve_token(
            &config.node.id,
            config.node.token_env.as_deref(),
            config.node.token_file.as_deref(),
        ) {
            Ok(_) => println!("✓ node token resolved for non-loopback access"),
            Err(error) => println!("✗ node token {error:#}"),
        }
    }
    for machine in &config.machines {
        // Report a bad URL or unreadable credential without contacting anything.
        match atmux::remote::RemoteMachine::from_config(machine) {
            Ok(remote) => println!(
                "✓ remote     {} at {} ({})",
                remote.id,
                remote.address(),
                if remote.is_authenticated() {
                    "token configured"
                } else {
                    "no token"
                }
            ),
            Err(error) => println!("✗ remote     {} {error:#}", machine.id),
        }
    }
    Ok(())
}

#[cfg(feature = "pulse")]
async fn run_pulse_command(config: &Config, command: PulseCommand) -> Result<()> {
    match command {
        PulseCommand::Doctor { account_id } => run_pulse_doctor(config, account_id).await,
        command @ PulseCommand::Push { .. } => run_pulse_push(config, command).await,
        command @ PulseCommand::Import { .. } => run_pulse_import(config, command).await,
    }
}

#[cfg(feature = "pulse")]
async fn run_pulse_doctor(config: &Config, account_id: i64) -> Result<()> {
    use atmux::pulse::{AccountId, Instant, MachineName};

    let account_id = AccountId::new(account_id)?;
    let store = atmux::pulse::ops::open_doctor_store(&config.pulse).await?;
    let result = atmux::pulse::ops::doctor(
        &config.pulse,
        store.as_ref(),
        atmux::pulse::ops::DoctorRequest {
            account_id,
            machine: MachineName::new(config.node.id.clone())?,
            now: Instant::now(),
            paths: atmux::pulse::ops::LocalProfilePaths::current()?,
        },
    )
    .await?;
    println!(
        "Pulse doctor account={} machine={} schema={} integrity={}",
        result.account_id.get(),
        result.machine,
        result.schema_version,
        if result.integrity_ok { "ok" } else { "failed" }
    );
    println!(
        "report credentials: ingest={:?} node={:?}",
        result.report_ingest_secret, result.report_node_secret
    );
    for profile in result.profiles {
        let last_poll = profile
            .last_polled_at
            .map_or_else(|| "never".to_owned(), atmux::pulse::Instant::to_iso8601);
        println!(
            "{} ({:?}): persisted={} config_match={} preflight={} credential={:?} gauge={:?} last_poll={last_poll}",
            profile.profile,
            profile.vendor,
            profile.persisted,
            profile.configuration_matches,
            profile.preflight_healthy,
            profile.credential,
            profile.gauge,
        );
    }
    anyhow::ensure!(result.integrity_ok, "Pulse database integrity check failed");
    Ok(())
}

#[cfg(feature = "pulse")]
async fn run_pulse_push(config: &Config, command: PulseCommand) -> Result<()> {
    use atmux::pulse::{AccountId, Instant, MachineName};

    let PulseCommand::Push {
        once,
        backfill,
        restart_backfill,
        account_id,
    } = command
    else {
        unreachable!("push helper accepts only PulseCommand::Push");
    };
    anyhow::ensure!(once, "pulse push requires the explicit --once flag");
    let account_id = AccountId::new(account_id)?;
    let store = atmux::pulse::ops::open_operational_store(&config.pulse).await?;
    let (cancel_sender, mut cancellation) = tokio::sync::watch::channel(false);
    let cancel_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = cancel_sender.send(true);
        }
    });
    let result = atmux::pulse::ops::push_once_native(
        &config.pulse,
        store,
        atmux::pulse::ops::PushOnceRequest {
            account_id,
            machine: MachineName::new(config.node.id.clone())?,
            started_at: Instant::now(),
            backfill,
            restart_backfill,
            paths: atmux::pulse::ops::LocalProfilePaths::current()?,
        },
        &mut cancellation,
    )
    .await;
    cancel_task.abort();
    let result = result?;
    print_pulse_job("usage", result.collections.usage);
    print_pulse_job("context", result.collections.context);
    print_pulse_job("tokens", result.collections.tokens);
    print_pulse_job("gemini", result.collections.gemini);
    println!(
        "backfill_truncated={} report={:?}",
        result.collections.backfill_truncated, result.report
    );
    ensure_pulse_push_complete(&result)
}

#[cfg(feature = "pulse")]
fn ensure_pulse_push_complete(result: &atmux::pulse::ops::PushOnceResult) -> Result<()> {
    use atmux::pulse::ops::PushReportResult;

    anyhow::ensure!(!result.cancelled, "Pulse push was cancelled");
    anyhow::ensure!(
        !result.collections.backfill_truncated,
        "Pulse backfill is incomplete; rerun is required"
    );
    match result.report {
        PushReportResult::Failed { kind, .. } => {
            anyhow::bail!("Pulse report failed: {kind:?}")
        }
        PushReportResult::Cancelled { .. } => anyhow::bail!("Pulse report was cancelled"),
        PushReportResult::Sent {
            truncated: true, ..
        } => anyhow::bail!("Pulse report is incomplete; rerun is required"),
        PushReportResult::NotConfigured | PushReportResult::Sent { .. } => Ok(()),
    }
}

#[cfg(feature = "pulse")]
async fn run_pulse_import(config: &Config, command: PulseCommand) -> Result<()> {
    use atmux::pulse::{AccountId, Instant, MachineName, ProfileName};

    let PulseCommand::Import {
        source,
        account_id,
        source_account_id,
        fallback_machine,
        machine_alias,
        dry_run,
        credential_env,
        credential_file,
    } = command
    else {
        unreachable!("import helper accepts only PulseCommand::Import");
    };
    let account_id = AccountId::new(account_id)?;
    require_configured_import_account(&config.pulse, account_id)?;
    let store = if dry_run {
        atmux::pulse::ops::open_doctor_store(&config.pulse).await?
    } else {
        atmux::pulse::ops::open_operational_store(&config.pulse).await?
    };
    let mut request = atmux::pulse::import::ImportRequest::new(
        source,
        account_id,
        config.pulse.credentials.default_refresh,
        Instant::now(),
    );
    request.source_account_id = source_account_id;
    request.fallback_machine = fallback_machine.map(MachineName::new).transpose()?;
    for (source, target) in machine_alias {
        let source = MachineName::new(source)?;
        let target = MachineName::new(target)?;
        anyhow::ensure!(
            request.machine_aliases.insert(source, target).is_none(),
            "each legacy machine can have only one canonical alias"
        );
    }
    request.dry_run = dry_run;
    for (profile, environment) in credential_env {
        insert_import_credential(
            &mut request,
            ProfileName::new(profile)?,
            atmux::pulse::import::ExternalCredential::Environment(environment),
        )?;
    }
    for (profile, file) in credential_file {
        insert_import_credential(
            &mut request,
            ProfileName::new(profile)?,
            atmux::pulse::import::ExternalCredential::File(PathBuf::from(file)),
        )?;
    }
    let report = atmux::pulse::import::import_legacy_sqlite(store.as_ref(), request).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    anyhow::ensure!(
        report.reconciliation_complete && report.reconciliation_exact,
        "Pulse import reconciliation was not exact"
    );
    Ok(())
}

#[cfg(feature = "pulse")]
fn require_configured_import_account(
    config: &atmux::pulse::PulseConfig,
    account_id: atmux::pulse::AccountId,
) -> Result<()> {
    anyhow::ensure!(
        config
            .accounts
            .iter()
            .any(|account| account.id == account_id.get()),
        "Pulse import target account is not explicitly configured"
    );
    Ok(())
}

#[cfg(feature = "pulse")]
fn print_pulse_job(label: &str, report: Option<atmux::pulse::scheduler::JobReport>) {
    if let Some(report) = report {
        println!(
            "{label}: attempted={} succeeded={} failed={}",
            report.attempted, report.succeeded, report.failed
        );
    } else {
        println!("{label}: not run");
    }
}

#[cfg(feature = "pulse")]
fn insert_import_credential(
    request: &mut atmux::pulse::import::ImportRequest,
    profile: atmux::pulse::ProfileName,
    credential: atmux::pulse::import::ExternalCredential,
) -> Result<()> {
    anyhow::ensure!(
        request.credentials.insert(profile, credential).is_none(),
        "each imported profile can have only one external credential reference"
    );
    Ok(())
}

#[cfg(feature = "pulse")]
fn parse_key_value(value: &str) -> std::result::Result<(String, String), String> {
    let Some((key, value)) = value.split_once('=') else {
        return Err("expected PROFILE=VALUE".to_owned());
    };
    if key.is_empty() || value.is_empty() {
        return Err("PROFILE and VALUE must both be nonempty".to_owned());
    }
    Ok((key.to_owned(), value.to_owned()))
}

fn run(mut app: App) -> Result<()> {
    let mut terminal = TerminalSession::enter()?;
    while !app.should_quit {
        terminal
            .terminal
            .draw(|frame| ui::render(frame, &mut app))
            .context("failed to draw the terminal UI")?;

        let until_refresh = app
            .refresh_interval()
            .saturating_sub(app.last_refresh.elapsed());
        let wait = until_refresh.min(Duration::from_millis(100));
        if event::poll(wait).context("failed to poll terminal input")? {
            match event::read().context("failed to read terminal input")? {
                Event::Key(key) => {
                    if let Err(error) = app.handle_key(key) {
                        app.message = Some((error.to_string(), true));
                    }
                }
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost | Event::Paste(_) => {}
            }
        }
        if app.last_refresh.elapsed() >= app.refresh_interval()
            && let Err(error) = app.refresh()
        {
            app.message = Some((error.to_string(), true));
            app.last_refresh = std::time::Instant::now();
        }
    }

    let next_attach = app.next_attach.take();
    drop(terminal);
    if let Some(session) = next_attach {
        Tmux::attach(&session)?;
    }
    Ok(())
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

#[cfg(test)]
mod cli_tests {
    use clap::CommandFactory as _;

    use super::*;

    #[test]
    fn scoped_exec_preserves_every_trailing_argument() {
        let cli = Cli::try_parse_from([
            "atmux",
            "--config",
            "/tmp/atmux.toml",
            "scoped-exec",
            "--",
            "/usr/bin/printf",
            "$FOO",
            "${FOO}",
            "$$",
            "quoted value",
            "--not-an-atmux-option",
        ])
        .unwrap();
        let Some(Commands::ScopedExec { command }) = cli.command else {
            panic!("expected scoped-exec");
        };
        assert_eq!(
            command,
            [
                "/usr/bin/printf",
                "$FOO",
                "${FOO}",
                "$$",
                "quoted value",
                "--not-an-atmux-option",
            ]
        );
    }

    #[test]
    fn scoped_exec_is_hidden_from_operator_help() {
        let help = Cli::command().render_long_help().to_string();
        assert!(!help.contains("scoped-exec"));
    }
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable terminal raw mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(error).context("failed to enter the alternate screen");
        }
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("failed to initialize terminal")?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(all(test, feature = "pulse"))]
mod tests {
    use super::*;

    #[test]
    fn pulse_cli_requires_explicit_account_and_preserves_one_shot_flags() {
        let cli = Cli::try_parse_from([
            "atmux",
            "pulse",
            "push",
            "--once",
            "--backfill",
            "--account-id",
            "7",
        ])
        .expect("parse pulse push");
        let Some(Commands::Pulse {
            command:
                PulseCommand::Push {
                    once,
                    backfill,
                    restart_backfill,
                    account_id,
                },
        }) = cli.command
        else {
            panic!("expected pulse push");
        };
        assert!(once);
        assert!(backfill);
        assert!(!restart_backfill);
        assert_eq!(account_id, 7);
        assert!(Cli::try_parse_from(["atmux", "pulse", "doctor"]).is_err());
        assert!(
            Cli::try_parse_from([
                "atmux",
                "pulse",
                "push",
                "--once",
                "--restart-backfill",
                "--account-id",
                "7",
            ])
            .is_err()
        );
    }

    #[test]
    fn import_credential_references_are_key_value_only() {
        assert_eq!(
            parse_key_value("claude-max=CLAUDE_MAX_KEY").expect("mapping"),
            ("claude-max".to_owned(), "CLAUDE_MAX_KEY".to_owned())
        );
        assert!(parse_key_value("missing-separator").is_err());
        assert!(parse_key_value("=empty-profile").is_err());
        assert!(parse_key_value("empty-value=").is_err());
    }

    #[test]
    fn import_machine_alias_is_repeatable_and_typed() {
        let cli = Cli::try_parse_from([
            "atmux",
            "pulse",
            "import",
            "/tmp/legacy.sqlite3",
            "--account-id",
            "7",
            "--machine-alias",
            "midnight.local=midnight",
            "--machine-alias",
            "Mac.lan=midnight",
        ])
        .expect("parse machine aliases");
        let Some(Commands::Pulse {
            command: PulseCommand::Import { machine_alias, .. },
        }) = cli.command
        else {
            panic!("expected Pulse import");
        };
        assert_eq!(
            machine_alias,
            vec![
                ("midnight.local".to_owned(), "midnight".to_owned()),
                ("Mac.lan".to_owned(), "midnight".to_owned()),
            ]
        );
    }

    #[test]
    fn import_account_must_be_explicitly_configured() {
        use atmux::pulse::{AccountId, PulseAccountConfig, PulseConfig};

        let account_id = AccountId::new(7).expect("account id");
        let mut config = PulseConfig::default();
        assert!(require_configured_import_account(&config, account_id).is_err());
        config.accounts.push(PulseAccountConfig {
            id: account_id.get(),
            identity: "configured@example.test".to_owned(),
            display_name: None,
            profiles: Vec::new(),
        });
        require_configured_import_account(&config, account_id).expect("configured account");
    }

    #[test]
    fn pulse_push_incomplete_cancelled_and_report_failed_are_cli_errors() {
        use atmux::pulse::{
            AccountId,
            error::PulseErrorKind,
            ops::{PushCollectionResult, PushOnceResult},
        };

        let mut result = PushOnceResult {
            account_id: AccountId::new(7).expect("account id"),
            collections: PushCollectionResult::default(),
            report: atmux::pulse::ops::PushReportResult::NotConfigured,
            cancelled: false,
        };
        ensure_pulse_push_complete(&result).expect("complete local-only push");

        result.collections.backfill_truncated = true;
        assert!(ensure_pulse_push_complete(&result).is_err());
        result.collections.backfill_truncated = false;
        result.report = atmux::pulse::ops::PushReportResult::Failed {
            kind: PulseErrorKind::Storage,
            truncated: false,
        };
        assert!(ensure_pulse_push_complete(&result).is_err());
        result.report = atmux::pulse::ops::PushReportResult::Sent {
            chunks: 1,
            rows: 10,
            truncated: true,
        };
        assert!(ensure_pulse_push_complete(&result).is_err());
        result.report = atmux::pulse::ops::PushReportResult::Cancelled {
            chunks: 0,
            rows: 0,
            truncated: false,
        };
        assert!(ensure_pulse_push_complete(&result).is_err());
    }
}
