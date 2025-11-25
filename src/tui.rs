use crate::{Workspace, check_uncommitted_changes, check_unpushed_commits, find_git_repositories};
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Alignment},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Shared state for capturing log messages
#[derive(Clone)]
pub struct LogCapture {
    pub last_message: Arc<Mutex<String>>,
}

impl Default for LogCapture {
    fn default() -> Self {
        Self {
            last_message: Arc::new(Mutex::new(String::new())),
        }
    }
}

impl LogCapture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_message(&self, msg: String) {
        if let Ok(mut last) = self.last_message.lock() {
            *last = msg;
        }
    }

    pub fn get_message(&self) -> String {
        self.last_message.lock().ok().map(|m| m.clone()).unwrap_or_default()
    }
}

/// Detect the parent shell by reading /proc/self/status
fn detect_parent_shell() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        // Read parent PID from /proc/self/status
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let ppid_line = status.lines().find(|line| line.starts_with("PPid:"))?;
        let ppid: u32 = ppid_line.split_whitespace().nth(1)?.parse().ok()?;

        // Read the command name of the parent process
        let cmdline = std::fs::read_to_string(format!("/proc/{}/comm", ppid)).ok()?;
        let shell_name = cmdline.trim();

        // Check if it's a known shell
        if matches!(shell_name, "fish" | "bash" | "zsh" | "sh" | "dash" | "ksh") {
            // Find the full path to this shell
            if let Ok(output) = std::process::Command::new("which")
                .arg(shell_name)
                .output()
                && output.status.success() {
                return Some(String::from_utf8_lossy(&output.stdout).trim().to_string());
            }
            // Fallback to just the shell name
            return Some(shell_name.to_string());
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

pub struct App {
    workspace_repos: Vec<RepoInfo>,
    library_repos: Vec<String>,
    filtered_workspace: Vec<(usize, Vec<usize>)>, // (index, match_positions)
    filtered_library: Vec<(usize, Vec<usize>)>,   // (index, match_positions)
    workspace_state: ListState,
    library_state: ListState,
    search_query: String,
    active_section: Section,
    matcher: SkimMatcherV2,
    last_log_message: String,
    mode: AppMode,
    add_repo_input: String,
    add_repo_suggestions: Vec<String>,
    add_repo_state: ListState,
}

#[derive(PartialEq)]
enum AppMode {
    Normal,
    AddRepo,
}

struct RepoInfo {
    path: PathBuf,
    display_name: String,
    is_clean: bool,
}

#[derive(PartialEq)]
enum Section {
    Workspace,
    Library,
}

impl App {
    fn new(workspace_repos: Vec<RepoInfo>, library_repos: Vec<String>) -> Self {
        let filtered_workspace: Vec<(usize, Vec<usize>)> =
            (0..workspace_repos.len()).map(|i| (i, Vec::new())).collect();
        let filtered_library: Vec<(usize, Vec<usize>)> =
            (0..library_repos.len()).map(|i| (i, Vec::new())).collect();

        let mut workspace_state = ListState::default();
        let mut library_state = ListState::default();

        // Select first item in whichever section has items
        let active_section = if !workspace_repos.is_empty() {
            workspace_state.select(Some(0));
            Section::Workspace
        } else if !library_repos.is_empty() {
            library_state.select(Some(0));
            Section::Library
        } else {
            Section::Workspace
        };

        Self {
            workspace_repos,
            library_repos,
            filtered_workspace,
            filtered_library,
            workspace_state,
            library_state,
            search_query: String::new(),
            active_section,
            matcher: SkimMatcherV2::default(),
            last_log_message: String::new(),
            mode: AppMode::Normal,
            add_repo_input: String::new(),
            add_repo_suggestions: Vec::new(),
            add_repo_state: ListState::default(),
        }
    }

    fn filter_repos(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_workspace = (0..self.workspace_repos.len())
                .map(|i| (i, Vec::new()))
                .collect();
            self.filtered_library = (0..self.library_repos.len())
                .map(|i| (i, Vec::new()))
                .collect();
        } else {
            // Filter and score workspace repos
            let mut workspace_matches: Vec<(usize, i64, Vec<usize>)> = self.workspace_repos
                .iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    self.matcher
                        .fuzzy_indices(&r.display_name, &self.search_query)
                        .map(|(score, indices)| (i, score, indices))
                })
                .collect();

            // Sort by score (higher is better)
            workspace_matches.sort_by(|a, b| b.1.cmp(&a.1));
            self.filtered_workspace = workspace_matches
                .into_iter()
                .map(|(i, _, indices)| (i, indices))
                .collect();

            // Filter and score library repos
            let mut library_matches: Vec<(usize, i64, Vec<usize>)> = self.library_repos
                .iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    self.matcher
                        .fuzzy_indices(r, &self.search_query)
                        .map(|(score, indices)| (i, score, indices))
                })
                .collect();

            // Sort by score (higher is better)
            library_matches.sort_by(|a, b| b.1.cmp(&a.1));
            self.filtered_library = library_matches
                .into_iter()
                .map(|(i, _, indices)| (i, indices))
                .collect();
        }

        // Reset selection
        if !self.filtered_workspace.is_empty() {
            self.workspace_state.select(Some(0));
            self.library_state.select(None);
            self.active_section = Section::Workspace;
        } else if !self.filtered_library.is_empty() {
            self.workspace_state.select(None);
            self.library_state.select(Some(0));
            self.active_section = Section::Library;
        } else {
            self.workspace_state.select(None);
            self.library_state.select(None);
        }
    }

    fn next(&mut self) {
        match self.active_section {
            Section::Workspace => {
                if self.filtered_workspace.is_empty() {
                    return;
                }
                let i = match self.workspace_state.selected() {
                    Some(i) => {
                        if i >= self.filtered_workspace.len() - 1 {
                            // Move to library section if available
                            if !self.filtered_library.is_empty() {
                                self.workspace_state.select(None);
                                self.library_state.select(Some(0));
                                self.active_section = Section::Library;
                                return;
                            }
                            0
                        } else {
                            i + 1
                        }
                    }
                    None => 0,
                };
                self.workspace_state.select(Some(i));
            }
            Section::Library => {
                if self.filtered_library.is_empty() {
                    return;
                }
                let i = match self.library_state.selected() {
                    Some(i) => {
                        if i >= self.filtered_library.len() - 1 {
                            // Wrap to workspace section if available
                            if !self.filtered_workspace.is_empty() {
                                self.library_state.select(None);
                                self.workspace_state.select(Some(0));
                                self.active_section = Section::Workspace;
                                return;
                            }
                            0
                        } else {
                            i + 1
                        }
                    }
                    None => 0,
                };
                self.library_state.select(Some(i));
            }
        }
    }

    fn previous(&mut self) {
        match self.active_section {
            Section::Workspace => {
                if self.filtered_workspace.is_empty() {
                    return;
                }
                let i = match self.workspace_state.selected() {
                    Some(i) => {
                        if i == 0 {
                            // Move to library section if available
                            if !self.filtered_library.is_empty() {
                                self.workspace_state.select(None);
                                self.library_state.select(Some(self.filtered_library.len() - 1));
                                self.active_section = Section::Library;
                                return;
                            }
                            self.filtered_workspace.len() - 1
                        } else {
                            i - 1
                        }
                    }
                    None => 0,
                };
                self.workspace_state.select(Some(i));
            }
            Section::Library => {
                if self.filtered_library.is_empty() {
                    return;
                }
                let i = match self.library_state.selected() {
                    Some(i) => {
                        if i == 0 {
                            // Wrap to workspace section if available
                            if !self.filtered_workspace.is_empty() {
                                self.library_state.select(None);
                                self.workspace_state.select(Some(self.filtered_workspace.len() - 1));
                                self.active_section = Section::Workspace;
                                return;
                            }
                            self.filtered_library.len() - 1
                        } else {
                            i - 1
                        }
                    }
                    None => 0,
                };
                self.library_state.select(Some(i));
            }
        }
    }

    fn selected_workspace_repo(&self) -> Option<&RepoInfo> {
        self.workspace_state.selected().and_then(|i| {
            self.filtered_workspace.get(i).and_then(|(idx, _)| {
                self.workspace_repos.get(*idx)
            })
        })
    }

    fn selected_library_repo(&self) -> Option<&String> {
        self.library_state.selected().and_then(|i| {
            self.filtered_library.get(i).and_then(|(idx, _)| {
                self.library_repos.get(*idx)
            })
        })
    }
}

enum Action {
    None,
    OpenShell(PathBuf),
    DropToLibrary(String),
    RestoreFromLibrary(String),
    AddRepo(String),
}

/// Fetch repository suggestions from GitHub CLI
fn get_github_suggestions() -> Vec<String> {
    if let Ok(output) = std::process::Command::new("gh")
        .args(["repo", "list", "--limit", "100", "--json", "nameWithOwner", "-q", ".[].nameWithOwner"])
        .output()
        && output.status.success() {
        return String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| format!("github.com/{}", line.trim()))
            .collect();
    }
    Vec::new()
}

/// Fetch repository suggestions from GitLab CLI
fn get_gitlab_suggestions() -> Vec<String> {
    if let Ok(output) = std::process::Command::new("glab")
        .args(["repo", "list", "--all", "--per-page", "100"])
        .output()
        && output.status.success() {
        return String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                // glab output format is: "namespace/project"
                let parts: Vec<&str> = line.split_whitespace().collect();
                parts.first().map(|repo| format!("gitlab.com/{}", repo))
            })
            .collect();
    }
    Vec::new()
}

pub fn run_tui(workspace: &Workspace) -> Result<()> {
    let log_capture = LogCapture::new();

    // Note: We could install a custom tracing layer to capture logs automatically,
    // but since tracing_subscriber is already initialized in main, we use manual
    // log capture via LogCapture instead

    loop {
        // Collect workspace repositories
        let workspace_repos = find_git_repositories(&workspace.path)?
            .into_iter()
            .map(|path| {
                let display_name = path
                    .strip_prefix(&workspace.path)
                    .unwrap_or(&path)
                    .display()
                    .to_string()
                    .trim_start_matches('/')
                    .to_string();

                let has_changes = check_uncommitted_changes(&path).unwrap_or(false);
                let has_unpushed = check_unpushed_commits(&path).unwrap_or(false);
                let is_clean = !has_changes && !has_unpushed;

                RepoInfo {
                    path,
                    display_name,
                    is_clean,
                }
            })
            .collect();

        // Collect library repositories
        let library_repos = if let Some(library) = &workspace.library {
            library.list().unwrap_or_default()
        } else {
            Vec::new()
        };

        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // Create app and run
        let mut app = App::new(workspace_repos, library_repos);
        let action = run_app(&mut terminal, &mut app, log_capture.clone())?;

        // Restore terminal
        disable_raw_mode()?;
        execute!(
            terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        )?;
        terminal.show_cursor()?;

        // Handle the action
        match action {
            Action::None => break,
            Action::OpenShell(path) => {
                log_capture.set_message(format!("Opening shell at: {}", path.display()));

                // Try to detect the actual parent shell
                let shell = detect_parent_shell().unwrap_or_else(|| {
                    // Fallback to SHELL env var
                    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
                });

                // Spawn an interactive shell in the repository directory
                std::process::Command::new(&shell)
                    .current_dir(&path)
                    .status()?;

                // After shell exits, loop back to show TUI again
                continue;
            }
            Action::DropToLibrary(repo_path) => {
                use crate::RepoPattern;

                log_capture.set_message(format!("📦 Dropping {} to library...", repo_path));
                // Force a redraw to show the message immediately
                app.last_log_message = log_capture.get_message();
                terminal.draw(|f| ui(f, &mut app))?;

                let pattern: RepoPattern = repo_path
                    .parse()
                    .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;

                // Perform the operation
                match workspace.drop(workspace.library.as_ref(), &pattern, false, false) {
                    Ok(_) => log_capture.set_message(format!("✓ Dropped {} to library", repo_path)),
                    Err(e) => log_capture.set_message(format!("✗ Failed to drop: {}", e)),
                }

                // Loop back to refresh TUI
                continue;
            }
            Action::RestoreFromLibrary(repo_path) => {
                log_capture.set_message(format!("📦 Restoring {} from library...", repo_path));
                // Force a redraw to show the message immediately
                app.last_log_message = log_capture.get_message();
                terminal.draw(|f| ui(f, &mut app))?;

                // Perform the operation
                let result = if let Some(library) = &workspace.library {
                    library.restore_to_workspace(&workspace.path, &repo_path)
                } else {
                    Ok(())
                };

                match result {
                    Ok(_) => log_capture.set_message(format!("✓ Restored {} from library", repo_path)),
                    Err(e) => log_capture.set_message(format!("✗ Failed to restore: {}", e)),
                }

                // Loop back to refresh TUI
                continue;
            }
            Action::AddRepo(repo_pattern) => {
                use crate::RepoPattern;

                log_capture.set_message(format!("🔄 Adding repository {}...", repo_pattern));
                // Force a redraw to show the message immediately
                app.last_log_message = log_capture.get_message();
                terminal.draw(|f| ui(f, &mut app))?;

                let pattern: RepoPattern = repo_pattern
                    .parse()
                    .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;

                // Add the repository
                match workspace.open(workspace.library.as_ref(), &pattern) {
                    Ok(_) => log_capture.set_message(format!("✓ Added repository {}", repo_pattern)),
                    Err(e) => log_capture.set_message(format!("✗ Failed to add: {}", e)),
                }

                // Loop back to refresh TUI
                continue;
            }
        }
    }

    Ok(())
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    log_capture: LogCapture,
) -> Result<Action> {
    loop {
        // Update the app with the latest log message
        app.last_log_message = log_capture.get_message();

        terminal.draw(|f| ui(f, app))?;

        if let Event::Key(key) = event::read()? {
            match app.mode {
                AppMode::Normal => match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(Action::None)
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // Ctrl+D = drop workspace repo to library
                        if app.active_section == Section::Workspace
                            && let Some(repo) = app.selected_workspace_repo() {
                            return Ok(Action::DropToLibrary(repo.display_name.clone()));
                        }
                    }
                    KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // Ctrl+A = add repo dialog
                        app.mode = AppMode::AddRepo;
                        app.add_repo_input.clear();

                        // Fetch suggestions in background (blocking for now)
                        let mut suggestions = Vec::new();
                        suggestions.extend(get_github_suggestions());
                        suggestions.extend(get_gitlab_suggestions());
                        suggestions.sort();
                        suggestions.dedup();
                        app.add_repo_suggestions = suggestions;

                        if !app.add_repo_suggestions.is_empty() {
                            app.add_repo_state.select(Some(0));
                        }
                    }
                    KeyCode::Esc => return Ok(Action::None),
                    KeyCode::Tab => {
                        // Tab switches between workspace and library
                        match app.active_section {
                            Section::Workspace => {
                                if !app.filtered_library.is_empty() {
                                    app.workspace_state.select(None);
                                    app.library_state.select(Some(0));
                                    app.active_section = Section::Library;
                                }
                            }
                            Section::Library => {
                                if !app.filtered_workspace.is_empty() {
                                    app.library_state.select(None);
                                    app.workspace_state.select(Some(0));
                                    app.active_section = Section::Workspace;
                                }
                            }
                        }
                    }
                    KeyCode::Down => app.next(),
                    KeyCode::Up => app.previous(),
                    KeyCode::Enter => {
                        // Enter on workspace repo = open shell
                        // Enter on library repo = restore from library
                        return Ok(match app.active_section {
                            Section::Workspace => {
                                if let Some(repo) = app.selected_workspace_repo() {
                                    Action::OpenShell(repo.path.clone())
                                } else {
                                    Action::None
                                }
                            }
                            Section::Library => {
                                if let Some(repo) = app.selected_library_repo() {
                                    Action::RestoreFromLibrary(repo.clone())
                                } else {
                                    Action::None
                                }
                            }
                        });
                    }
                    KeyCode::Char(c) => {
                        app.search_query.push(c);
                        app.filter_repos();
                    }
                    KeyCode::Backspace => {
                        app.search_query.pop();
                        app.filter_repos();
                    }
                    _ => {}
                },
                AppMode::AddRepo => match key.code {
                    KeyCode::Esc => {
                        app.mode = AppMode::Normal;
                        app.add_repo_input.clear();
                    }
                    KeyCode::Enter => {
                        // Use selected suggestion or manual input
                        let repo = if let Some(idx) = app.add_repo_state.selected() {
                            // Filter suggestions by current input
                            let filtered: Vec<_> = app.add_repo_suggestions
                                .iter()
                                .filter(|s| s.to_lowercase().contains(&app.add_repo_input.to_lowercase()))
                                .collect();

                            filtered.get(idx).map(|s| s.to_string())
                        } else {
                            None
                        };

                        let repo = repo.unwrap_or_else(|| app.add_repo_input.clone());

                        if !repo.is_empty() {
                            app.mode = AppMode::Normal;
                            return Ok(Action::AddRepo(repo));
                        }
                    }
                    KeyCode::Down => {
                        let filtered: Vec<_> = app.add_repo_suggestions
                            .iter()
                            .filter(|s| s.to_lowercase().contains(&app.add_repo_input.to_lowercase()))
                            .collect();

                        if !filtered.is_empty() {
                            let next = match app.add_repo_state.selected() {
                                Some(i) if i >= filtered.len() - 1 => 0,
                                Some(i) => i + 1,
                                None => 0,
                            };
                            app.add_repo_state.select(Some(next));
                        }
                    }
                    KeyCode::Up => {
                        let filtered: Vec<_> = app.add_repo_suggestions
                            .iter()
                            .filter(|s| s.to_lowercase().contains(&app.add_repo_input.to_lowercase()))
                            .collect();

                        if !filtered.is_empty() {
                            let prev = match app.add_repo_state.selected() {
                                Some(0) => filtered.len() - 1,
                                Some(i) => i - 1,
                                None => 0,
                            };
                            app.add_repo_state.select(Some(prev));
                        }
                    }
                    KeyCode::Char(c) => {
                        app.add_repo_input.push(c);
                        app.add_repo_state.select(Some(0));
                    }
                    KeyCode::Backspace => {
                        app.add_repo_input.pop();
                    }
                    _ => {}
                },
            }
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    if app.mode == AppMode::AddRepo {
        render_add_repo_dialog(f, app);
        return;
    }

    // Split vertically into rows
    let vertical_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),       // Help (at top)
            Constraint::Min(0),          // Main area (workspace + library side by side)
            Constraint::Length(3),       // Search box
            Constraint::Length(1),       // Status/log message (at bottom)
        ])
        .split(f.area());

    // Split the main area horizontally into workspace (left) and library (right)
    let horizontal_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50),  // Workspace (left)
            Constraint::Percentage(50),  // Library (right)
        ])
        .split(vertical_chunks[1]);

    // Help text (at top)
    let help_spans = if app.mode == AppMode::AddRepo {
        vec![
            Span::styled("Esc", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" cancel"),
        ]
    } else if app.active_section == Section::Workspace {
        vec![
            Span::styled("Tab", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" switch  "),
            Span::styled("↑/↓", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" navigate  "),
            Span::styled("type", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" search  "),
            Span::styled("Enter", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw(" open shell  "),
            Span::styled("Ctrl+D", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw(" drop  "),
            Span::styled("Ctrl+C/Esc", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" quit"),
        ]
    } else if app.active_section == Section::Library {
        vec![
            Span::styled("Tab", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" switch  "),
            Span::styled("↑/↓", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" navigate  "),
            Span::styled("type", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" search  "),
            Span::styled("Enter", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw(" restore  "),
            Span::styled("Ctrl+A", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
            Span::raw(" add  "),
            Span::styled("Ctrl+C/Esc", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" quit"),
        ]
    } else {
        vec![
            Span::styled("Tab", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" switch  "),
            Span::styled("↑/↓", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" navigate  "),
            Span::styled("type", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
            Span::raw(" search  "),
            Span::styled("Ctrl+A", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
            Span::raw(" add  "),
            Span::styled("Ctrl+C/Esc", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" quit"),
        ]
    };
    let help = Paragraph::new(Line::from(help_spans))
        .alignment(Alignment::Center);
    f.render_widget(help, vertical_chunks[0]);

    // Workspace repositories
    let workspace_items: Vec<ListItem> = app
        .filtered_workspace
        .iter()
        .map(|(idx, match_positions)| {
            let repo = &app.workspace_repos[*idx];
            let status = if repo.is_clean {
                Span::styled("✓ ", Style::default().fg(Color::Green))
            } else {
                Span::styled("⚠ ", Style::default().fg(Color::Yellow))
            };

            // Highlight search matches (fuzzy)
            let mut spans = vec![status];
            if !match_positions.is_empty() {
                let mut last_pos = 0;
                let chars: Vec<(usize, char)> = repo.display_name.char_indices().collect();

                for &match_idx in match_positions {
                    if match_idx >= chars.len() {
                        continue;
                    }

                    // Add unmatched text before this character
                    if match_idx > last_pos {
                        let start_byte = chars[last_pos].0;
                        let end_byte = chars[match_idx].0;
                        spans.push(Span::raw(&repo.display_name[start_byte..end_byte]));
                    }

                    // Add highlighted character
                    let char_byte_start = chars[match_idx].0;
                    let char_byte_end = if match_idx + 1 < chars.len() {
                        chars[match_idx + 1].0
                    } else {
                        repo.display_name.len()
                    };
                    spans.push(Span::styled(
                        &repo.display_name[char_byte_start..char_byte_end],
                        Style::default().fg(Color::Black).bg(Color::Yellow)
                    ));

                    last_pos = match_idx + 1;
                }

                // Add remaining text
                if last_pos < chars.len() {
                    let start_byte = chars[last_pos].0;
                    spans.push(Span::raw(&repo.display_name[start_byte..]));
                }
            } else {
                spans.push(Span::raw(&repo.display_name));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let workspace_list = List::new(workspace_items)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(format!("Workspace ({})", app.filtered_workspace.len()))
            .border_style(if app.active_section == Section::Workspace {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            }))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    f.render_stateful_widget(workspace_list, horizontal_chunks[0], &mut app.workspace_state);

    // Library repositories (second panel)
    let library_items: Vec<ListItem> = app
        .filtered_library
        .iter()
        .map(|(idx, match_positions)| {
            let repo = &app.library_repos[*idx];

            // Highlight search matches (fuzzy)
            let spans = if !match_positions.is_empty() {
                let mut spans = Vec::new();
                let mut last_pos = 0;
                let chars: Vec<(usize, char)> = repo.char_indices().collect();

                for &match_idx in match_positions {
                    if match_idx >= chars.len() {
                        continue;
                    }

                    // Add unmatched text before this character
                    if match_idx > last_pos {
                        let start_byte = chars[last_pos].0;
                        let end_byte = chars[match_idx].0;
                        spans.push(Span::raw(&repo[start_byte..end_byte]));
                    }

                    // Add highlighted character
                    let char_byte_start = chars[match_idx].0;
                    let char_byte_end = if match_idx + 1 < chars.len() {
                        chars[match_idx + 1].0
                    } else {
                        repo.len()
                    };
                    spans.push(Span::styled(
                        &repo[char_byte_start..char_byte_end],
                        Style::default().fg(Color::Black).bg(Color::Yellow)
                    ));

                    last_pos = match_idx + 1;
                }

                // Add remaining text
                if last_pos < chars.len() {
                    let start_byte = chars[last_pos].0;
                    spans.push(Span::raw(&repo[start_byte..]));
                }

                spans
            } else {
                vec![Span::raw(repo)]
            };

            ListItem::new(Line::from(spans))
        })
        .collect();

    let library_list = List::new(library_items)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(format!("Library ({})", app.filtered_library.len()))
            .border_style(if app.active_section == Section::Library {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            }))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    f.render_stateful_widget(library_list, horizontal_chunks[1], &mut app.library_state);

    // Search box
    let search_text = format!("{}_", app.search_query);
    let search_style = if app.search_query.is_empty() {
        Style::default()
    } else {
        Style::default().fg(Color::Yellow)
    };
    let search_border_style = if app.search_query.is_empty() {
        Style::default()
    } else {
        Style::default().fg(Color::Yellow)
    };
    let search = Paragraph::new(search_text)
        .style(search_style)
        .alignment(Alignment::Left)
        .block(Block::default()
            .borders(Borders::ALL)
            .border_style(search_border_style)
            .title("Search"));
    f.render_widget(search, vertical_chunks[2]);

    // Status/log message (at bottom)
    let status = Paragraph::new(app.last_log_message.as_str())
        .style(Style::default().fg(Color::Gray))
        .alignment(Alignment::Left);
    f.render_widget(status, vertical_chunks[3]);
}

fn render_add_repo_dialog(f: &mut Frame, app: &App) {
    use ratatui::layout::Rect;

    // Create a centered dialog
    let area = f.area();
    let dialog_width = area.width.min(80);
    let dialog_height = area.height.min(20);
    let dialog_x = (area.width - dialog_width) / 2;
    let dialog_y = (area.height - dialog_height) / 2;

    let dialog_area = Rect {
        x: dialog_x,
        y: dialog_y,
        width: dialog_width,
        height: dialog_height,
    };

    // Clear the background
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title("Add Repository")
        .style(Style::default().bg(Color::Black));
    f.render_widget(block, dialog_area);

    // Split into input and suggestions
    let inner = dialog_area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 1,
    });

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Input box
            Constraint::Min(0),     // Suggestions
        ])
        .split(inner);

    // Input box
    let input_text = format!("{}_", app.add_repo_input);
    let input = Paragraph::new(input_text)
        .style(Style::default().fg(Color::Yellow))
        .block(Block::default()
            .borders(Borders::ALL)
            .title("Repository (e.g. github.com/user/repo)"));
    f.render_widget(input, chunks[0]);

    // Suggestions list
    let filtered_suggestions: Vec<_> = app.add_repo_suggestions
        .iter()
        .filter(|s| {
            if app.add_repo_input.is_empty() {
                true
            } else {
                s.to_lowercase().contains(&app.add_repo_input.to_lowercase())
            }
        })
        .collect();

    let suggestion_items: Vec<ListItem> = filtered_suggestions
        .iter()
        .map(|s| ListItem::new(s.as_str()))
        .collect();

    let suggestions = List::new(suggestion_items)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(format!("Suggestions ({})", filtered_suggestions.len())))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        )
        .highlight_symbol(">> ");

    let mut state = app.add_repo_state.clone();
    f.render_stateful_widget(suggestions, chunks[1], &mut state);
}
