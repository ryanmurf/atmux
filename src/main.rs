use std::{io, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use atmux::{app::App, config::Config, tmux::Tmux, ui};
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
    /// Check tmux, configuration, folders, and launcher profiles.
    Doctor,
}

fn main() -> Result<()> {
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
        Some(Commands::Doctor) => return doctor(&config_path),
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
    let folders = config.directories();
    println!("✓ folders    {} discovered", folders.len());
    let harnesses = config.harnesses();
    for harness in harnesses {
        println!(
            "✓ {harness:<10} {} profile(s)",
            config.profiles_for(&harness).len()
        );
    }
    Ok(())
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
