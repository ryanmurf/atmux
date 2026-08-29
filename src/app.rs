use std::{
    collections::HashMap,
    env,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{layout::Rect, widgets::ListState};

use crate::{
    config::{AgentProfile, Config, ProfileMode},
    project::{self, ProjectPreferences},
    status::AgentStatus,
    tmux::{Session, Tmux},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Normal,
    Filter,
    Help,
    ConfirmKill,
    Launch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchStage {
    Directory,
    Harness,
    Profile,
    Mode,
    Name,
}

#[derive(Debug)]
pub struct Launcher {
    pub stage: LaunchStage,
    pub directories: Vec<PathBuf>,
    pub directory_query: String,
    pub directory_selected: usize,
    pub directory: Option<PathBuf>,
    project_preferences: Option<ProjectPreferences>,
    pub harnesses: Vec<String>,
    pub harness_selected: usize,
    pub profiles: Vec<AgentProfile>,
    pub profile_selected: usize,
    pub modes: Vec<ProfileMode>,
    pub mode_selected: usize,
    pub name: String,
    name_pristine: bool,
}

impl Launcher {
    fn new(config: &Config) -> Self {
        let mut directories = config.directories();
        if directories.is_empty()
            && let Ok(current) = env::current_dir()
        {
            directories.push(current);
        }
        Self {
            stage: LaunchStage::Directory,
            directories,
            directory_query: String::new(),
            directory_selected: 0,
            directory: None,
            project_preferences: None,
            harnesses: config.harnesses(),
            harness_selected: 0,
            profiles: Vec::new(),
            profile_selected: 0,
            modes: Vec::new(),
            mode_selected: 0,
            name: String::new(),
            name_pristine: false,
        }
    }

    #[must_use]
    pub fn filtered_directories(&self) -> Vec<&PathBuf> {
        let query = self.directory_query.to_lowercase();
        self.directories
            .iter()
            .filter(|path| {
                query.is_empty() || path.to_string_lossy().to_lowercase().contains(&query)
            })
            .collect()
    }

    fn selected_directory(&self) -> Option<PathBuf> {
        self.filtered_directories()
            .get(self.directory_selected)
            .map(|path| (*path).clone())
    }

    fn move_selection(&mut self, delta: isize) {
        match self.stage {
            LaunchStage::Directory => {
                let len = self.filtered_directories().len();
                self.directory_selected = move_index(self.directory_selected, delta, len);
            }
            LaunchStage::Harness => {
                self.harness_selected =
                    move_index(self.harness_selected, delta, self.harnesses.len());
            }
            LaunchStage::Profile => {
                self.profile_selected =
                    move_index(self.profile_selected, delta, self.profiles.len());
            }
            LaunchStage::Mode => {
                self.mode_selected = move_index(self.mode_selected, delta, self.modes.len());
            }
            LaunchStage::Name => {}
        }
    }
}

#[derive(Debug)]
pub struct App {
    pub config: Config,
    pub config_path: PathBuf,
    pub sessions: Vec<Session>,
    pub list_state: ListState,
    pub filter: String,
    pub mode: Mode,
    pub launcher: Option<Launcher>,
    pub message: Option<(String, bool)>,
    pub preview_scroll: u16,
    pub session_area: Rect,
    pub should_quit: bool,
    pub next_attach: Option<String>,
    pub current_session: Option<String>,
    pub last_refresh: Instant,
    previous_hashes: HashMap<String, u64>,
    tmux: Tmux,
}

impl App {
    /// Creates an application and reads the initial tmux state.
    ///
    /// # Errors
    ///
    /// Returns an error when tmux is unavailable or its session state cannot be read.
    pub fn new(config: Config, config_path: PathBuf) -> Result<Self> {
        Tmux::check()?;
        let tmux = Tmux;
        let current_session = tmux.current_session();
        let mut app = Self {
            config,
            config_path,
            sessions: Vec::new(),
            list_state: ListState::default(),
            filter: String::new(),
            mode: Mode::Normal,
            launcher: None,
            message: None,
            preview_scroll: 0,
            session_area: Rect::default(),
            should_quit: false,
            next_attach: None,
            current_session,
            last_refresh: Instant::now(),
            previous_hashes: HashMap::new(),
            tmux,
        };
        app.refresh()?;
        Ok(app)
    }

    #[must_use]
    pub const fn refresh_interval(&self) -> Duration {
        Duration::from_millis(self.config.general.refresh_ms)
    }

    /// Refreshes all session metadata and the selected pane preview.
    ///
    /// # Errors
    ///
    /// Returns an error when tmux cannot provide its current session state.
    pub fn refresh(&mut self) -> Result<()> {
        let selected_name = self.selected_session().map(|session| session.name.clone());
        let sessions = self
            .tmux
            .sessions(&self.previous_hashes, &self.config.status)?;
        self.previous_hashes = sessions
            .iter()
            .map(|session| (session.pane_id.clone(), session.content_hash))
            .collect();
        self.sessions = sessions;
        self.restore_selection(selected_name.as_deref());
        self.refresh_preview();
        self.last_refresh = Instant::now();
        Ok(())
    }

    fn refresh_preview(&mut self) {
        let selected = self
            .selected_session()
            .map(|session| (session.name.clone(), session.pane_id.clone()));
        let Some((name, pane_id)) = selected else {
            return;
        };
        if self.current_session.as_deref() == Some(&name) {
            if let Some(session) = self
                .sessions
                .iter_mut()
                .find(|item| item.pane_id == pane_id)
            {
                session.content.clear();
                session.content.push_str(
                    "atmux is running in this session.\n\nSelect another session to inspect its live pane.",
                );
            }
            return;
        }
        let Ok(content) = self
            .tmux
            .capture(&pane_id, self.config.general.preview_lines)
        else {
            return;
        };
        if let Some(session) = self
            .sessions
            .iter_mut()
            .find(|item| item.pane_id == pane_id)
        {
            session.content = content;
        }
    }

    fn restore_selection(&mut self, name: Option<&str>) {
        let visible = self.filtered_indices();
        if visible.is_empty() {
            self.list_state.select(None);
            return;
        }
        let selected = name
            .and_then(|name| {
                visible
                    .iter()
                    .position(|&index| self.sessions[index].name == name)
            })
            .unwrap_or_else(|| {
                self.list_state
                    .selected()
                    .unwrap_or(0)
                    .min(visible.len() - 1)
            });
        self.list_state.select(Some(selected));
    }

    #[must_use]
    pub fn filtered_indices(&self) -> Vec<usize> {
        let query = self.filter.to_lowercase();
        self.sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                (query.is_empty()
                    || session.name.to_lowercase().contains(&query)
                    || session
                        .path
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&query)
                    || session.agent.to_string().to_lowercase().contains(&query))
                .then_some(index)
            })
            .collect()
    }

    #[must_use]
    pub fn selected_session(&self) -> Option<&Session> {
        let visible = self.filtered_indices();
        self.list_state
            .selected()
            .and_then(|selected| visible.get(selected))
            .and_then(|&index| self.sessions.get(index))
    }

    #[must_use]
    pub fn status_counts(&self) -> (usize, usize) {
        self.sessions.iter().fold((0, 0), |mut counts, session| {
            match session.status {
                AgentStatus::Working => counts.0 += 1,
                AgentStatus::Waiting => counts.1 += 1,
                AgentStatus::Other => {}
            }
            counts
        })
    }

    /// Applies one keyboard event to the active UI mode.
    ///
    /// # Errors
    ///
    /// Returns an error when the event requires a tmux operation that fails.
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.kind != KeyEventKind::Press {
            return Ok(());
        }
        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Filter => {
                self.handle_filter_key(key);
                Ok(())
            }
            Mode::Help => {
                self.mode = Mode::Normal;
                Ok(())
            }
            Mode::ConfirmKill => self.handle_confirm_key(key),
            Mode::Launch => self.handle_launch_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => self.move_session(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_session(-1),
            KeyCode::Char('g') | KeyCode::Home => self.select_edge(false),
            KeyCode::Char('G') | KeyCode::End => self.select_edge(true),
            KeyCode::Enter | KeyCode::Char('s') => self.activate_selected()?,
            KeyCode::Char('e') => self.quick_edit_selected()?,
            KeyCode::Char('n') => {
                self.launcher = Some(Launcher::new(&self.config));
                self.mode = Mode::Launch;
            }
            KeyCode::Char('/') => {
                self.filter.clear();
                self.list_state.select(Some(0));
                self.mode = Mode::Filter;
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('x') => {
                if self.selected_session().is_some() {
                    self.mode = Mode::ConfirmKill;
                }
            }
            KeyCode::Char('r') => self.refresh()?,
            KeyCode::PageUp => {
                self.preview_scroll = self.preview_scroll.saturating_add(8);
            }
            KeyCode::PageDown => {
                self.preview_scroll = self.preview_scroll.saturating_sub(8);
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.filter.pop();
                self.list_state.select(Some(0));
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.push(character);
                self.list_state.select(Some(0));
            }
            KeyCode::Up => self.move_session(-1),
            KeyCode::Down => self.move_session(1),
            _ => {}
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                let name = self
                    .selected_session()
                    .map(|session| session.name.clone())
                    .context("no tmux session selected")?;
                if self.current_session.as_deref() == Some(&name) {
                    self.message = Some(("atmux cannot kill its own session".to_owned(), true));
                } else {
                    self.tmux.kill(&name)?;
                    self.message = Some((format!("killed {name}"), false));
                    self.refresh()?;
                }
                self.mode = Mode::Normal;
            }
            _ => self.mode = Mode::Normal,
        }
        Ok(())
    }

    fn handle_launch_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.code == KeyCode::Esc {
            self.launch_back();
            return Ok(());
        }
        match key.code {
            KeyCode::Up => {
                if let Some(launcher) = &mut self.launcher {
                    launcher.move_selection(-1);
                }
            }
            KeyCode::Down => {
                if let Some(launcher) = &mut self.launcher {
                    launcher.move_selection(1);
                }
            }
            KeyCode::Char('k')
                if self.launcher.as_ref().is_some_and(|launcher| {
                    matches!(
                        launcher.stage,
                        LaunchStage::Harness | LaunchStage::Profile | LaunchStage::Mode
                    )
                }) =>
            {
                if let Some(launcher) = &mut self.launcher {
                    launcher.move_selection(-1);
                }
            }
            KeyCode::Char('j')
                if self.launcher.as_ref().is_some_and(|launcher| {
                    matches!(
                        launcher.stage,
                        LaunchStage::Harness | LaunchStage::Profile | LaunchStage::Mode
                    )
                }) =>
            {
                if let Some(launcher) = &mut self.launcher {
                    launcher.move_selection(1);
                }
            }
            KeyCode::Enter | KeyCode::Tab => self.launch_next()?,
            KeyCode::BackTab => self.launch_back(),
            KeyCode::Backspace => {
                if let Some(launcher) = &mut self.launcher {
                    match launcher.stage {
                        LaunchStage::Directory => {
                            launcher.directory_query.pop();
                            launcher.directory_selected = 0;
                        }
                        LaunchStage::Name => {
                            launcher.name.pop();
                            launcher.name_pristine = false;
                        }
                        LaunchStage::Harness | LaunchStage::Profile | LaunchStage::Mode => {}
                    }
                }
            }
            KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(launcher) = &mut self.launcher {
                    match launcher.stage {
                        LaunchStage::Directory => {
                            launcher.directory_query.push(character);
                            launcher.directory_selected = 0;
                        }
                        LaunchStage::Name => {
                            if launcher.name_pristine {
                                launcher.name.clear();
                                launcher.name_pristine = false;
                            }
                            if is_session_name_character(character) {
                                launcher.name.push(character);
                            }
                        }
                        LaunchStage::Harness | LaunchStage::Profile | LaunchStage::Mode => {}
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn launch_next(&mut self) -> Result<()> {
        let stage = self.launcher.as_ref().map(|launcher| launcher.stage);
        match stage {
            Some(LaunchStage::Directory) => {
                let directory = self
                    .launcher
                    .as_ref()
                    .and_then(Launcher::selected_directory)
                    .context("no project directory matches the filter")?;
                let preferences = project::load(&directory)?;
                let launcher = self.launcher.as_mut().expect("launcher exists");
                launcher.directory = Some(directory);
                launcher.project_preferences = preferences;
                if let Some(harness) = launcher
                    .project_preferences
                    .as_ref()
                    .and_then(|preferences| preferences.harness.as_deref())
                    && let Some(index) = launcher
                        .harnesses
                        .iter()
                        .position(|candidate| candidate.eq_ignore_ascii_case(harness))
                {
                    launcher.harness_selected = index;
                }
                launcher.stage = LaunchStage::Harness;
            }
            Some(LaunchStage::Harness) => {
                let harness = self
                    .launcher
                    .as_ref()
                    .and_then(|launcher| launcher.harnesses.get(launcher.harness_selected))
                    .cloned()
                    .context("no harness profiles are configured")?;
                let profiles = self.config.profiles_for(&harness);
                let launcher = self.launcher.as_mut().expect("launcher exists");
                launcher.profile_selected = launcher
                    .project_preferences
                    .as_ref()
                    .filter(|preferences| {
                        preferences
                            .harness
                            .as_deref()
                            .is_none_or(|saved| saved.eq_ignore_ascii_case(&harness))
                    })
                    .and_then(|preferences| preferences.profile.as_deref())
                    .and_then(|saved| {
                        profiles
                            .iter()
                            .position(|profile| profile.name.eq_ignore_ascii_case(saved))
                    })
                    .unwrap_or(0);
                launcher.profiles = profiles;
                launcher.stage = LaunchStage::Profile;
            }
            Some(LaunchStage::Profile) => {
                let modes = self
                    .launcher
                    .as_ref()
                    .and_then(|launcher| launcher.profiles.get(launcher.profile_selected))
                    .map(|profile| profile.modes.clone())
                    .context("no agent profile selected")?;
                let launcher = self.launcher.as_mut().expect("launcher exists");
                launcher.modes = modes;
                launcher.mode_selected = 0;
                if launcher.modes.len() > 1 {
                    launcher.stage = LaunchStage::Mode;
                    return Ok(());
                }
                self.open_launch_name()?;
            }
            Some(LaunchStage::Mode) => self.open_launch_name()?,
            Some(LaunchStage::Name) => self.finish_launch()?,
            None => self.mode = Mode::Normal,
        }
        Ok(())
    }

    fn open_launch_name(&mut self) -> Result<()> {
        let directory = self
            .launcher
            .as_ref()
            .and_then(|launcher| launcher.directory.as_ref())
            .context("no project directory selected")?;
        let proposed = self
            .launcher
            .as_ref()
            .and_then(|launcher| launcher.project_preferences.as_ref())
            .and_then(|preferences| preferences.session_name.as_deref())
            .map(slugify)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| {
                directory
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map_or_else(|| "agent".to_owned(), slugify)
            });
        let name = self.available_session_name(&proposed);
        let launcher = self.launcher.as_mut().expect("launcher exists");
        launcher.name = name;
        launcher.name_pristine = true;
        launcher.stage = LaunchStage::Name;
        Ok(())
    }

    fn launch_back(&mut self) {
        let Some(launcher) = &mut self.launcher else {
            self.mode = Mode::Normal;
            return;
        };
        launcher.stage = match launcher.stage {
            LaunchStage::Directory => {
                self.launcher = None;
                self.mode = Mode::Normal;
                return;
            }
            LaunchStage::Harness => LaunchStage::Directory,
            LaunchStage::Profile => LaunchStage::Harness,
            LaunchStage::Name if launcher.modes.len() > 1 => LaunchStage::Mode,
            LaunchStage::Mode | LaunchStage::Name => LaunchStage::Profile,
        };
    }

    fn finish_launch(&mut self) -> Result<()> {
        let launcher = self.launcher.as_ref().context("launcher is not open")?;
        let name = launcher.name.trim().to_owned();
        if name.is_empty() {
            bail!("session name cannot be empty");
        }
        if self.sessions.iter().any(|session| session.name == name) {
            bail!("a tmux session named {name} already exists");
        }
        let directory = launcher
            .directory
            .as_ref()
            .context("no project directory selected")?
            .clone();
        let profile = launcher
            .profiles
            .get(launcher.profile_selected)
            .context("no agent profile selected")?
            .clone();
        let mode = launcher.modes.get(launcher.mode_selected).cloned();
        project::remember_launch(&directory, &name, &profile)?;
        self.tmux
            .launch(&name, &directory, &profile, mode.as_ref())?;
        self.message = Some((
            format!(
                "launched {} · {} · {}{}",
                name,
                profile.harness,
                profile.name,
                mode.map_or_else(String::new, |mode| format!(" · {}", mode.display_label()))
            ),
            false,
        ));
        self.launcher = None;
        self.mode = Mode::Normal;
        self.filter.clear();
        self.refresh()?;
        self.restore_selection(Some(&name));
        if self.config.general.switch_on_launch {
            self.activate_named(name)?;
        }
        Ok(())
    }

    fn activate_selected(&mut self) -> Result<()> {
        if let Some(name) = self.selected_session().map(|session| session.name.clone()) {
            self.activate_named(name)?;
        }
        Ok(())
    }

    fn quick_edit_selected(&mut self) -> Result<()> {
        let name = self
            .selected_session()
            .map(|session| session.name.clone())
            .context("no tmux session selected")?;
        if self.current_session.as_deref() == Some(&name) {
            bail!("atmux is already running in this session");
        }
        self.tmux.popup(&name)?;
        self.refresh()?;
        Ok(())
    }

    fn activate_named(&mut self, name: String) -> Result<()> {
        if Tmux::inside_tmux() {
            self.tmux.switch(&name)?;
        } else {
            self.next_attach = Some(name);
            self.should_quit = true;
        }
        Ok(())
    }

    fn move_session(&mut self, delta: isize) {
        let len = self.filtered_indices().len();
        let current = self.list_state.selected().unwrap_or(0);
        self.list_state
            .select((len > 0).then(|| move_index(current, delta, len)));
        self.preview_scroll = 0;
        self.refresh_preview();
    }

    fn select_edge(&mut self, end: bool) {
        let len = self.filtered_indices().len();
        self.list_state
            .select((len > 0).then_some(if end { len - 1 } else { 0 }));
        self.preview_scroll = 0;
        self.refresh_preview();
    }

    fn available_session_name(&self, proposed: &str) -> String {
        if !self.sessions.iter().any(|session| session.name == proposed) {
            return proposed.to_owned();
        }
        for suffix in 2..=u32::MAX {
            let candidate = format!("{proposed}-{suffix}");
            if !self
                .sessions
                .iter()
                .any(|session| session.name == candidate)
            {
                return candidate;
            }
        }
        format!("{proposed}-new")
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if !matches!(self.mode, Mode::Normal | Mode::Filter) {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_session(1),
            MouseEventKind::ScrollUp => self.move_session(-1),
            MouseEventKind::Down(crossterm::event::MouseButton::Left)
                if self.mode == Mode::Normal
                    && mouse.column > self.session_area.x
                    && mouse.column < self.session_area.right().saturating_sub(1)
                    && mouse.row > self.session_area.y
                    && mouse.row < self.session_area.bottom().saturating_sub(1) =>
            {
                let offset = self.list_state.offset();
                let row = usize::from(mouse.row - self.session_area.y - 1);
                let index = offset + row;
                if index < self.filtered_indices().len() {
                    self.list_state.select(Some(index));
                    self.preview_scroll = 0;
                    self.refresh_preview();
                }
            }
            _ => {}
        }
    }
}

fn move_index(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    if delta < 0 {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current
            .saturating_add(usize::try_from(delta).unwrap_or_default())
            .min(len - 1)
    }
}

fn slugify(value: &str) -> String {
    let mut output = String::new();
    let mut last_was_dash = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || character == '_' {
            output.push(character.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash && !output.is_empty() {
            output.push('-');
            last_was_dash = true;
        }
    }
    while output.ends_with('-') {
        output.pop();
    }
    if output.is_empty() {
        "agent".to_owned()
    } else {
        output
    }
}

const fn is_session_name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugifies_project_names() {
        assert_eq!(slugify("HeroDevs API / AI"), "herodevs-api-ai");
        assert_eq!(slugify("---"), "agent");
    }

    #[test]
    fn clamps_list_navigation() {
        assert_eq!(move_index(0, -1, 3), 0);
        assert_eq!(move_index(1, 1, 3), 2);
        assert_eq!(move_index(2, 1, 3), 2);
    }
}
