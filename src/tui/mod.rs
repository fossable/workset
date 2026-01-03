mod app;
mod metadata;
mod tree;

use app::{App, AppMode, Section};
use metadata::{format_size, format_time_ago_verbose, get_repo_modification_time, get_repo_size};
use tree::RepoInfo;

use crate::{Workspace, find_git_repositories};
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use fuzzy_matcher::FuzzyMatcher;
use notify::{Event as NotifyEvent, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
        self.last_message
            .lock()
            .ok()
            .map(|m| m.clone())
            .unwrap_or_default()
    }
}

enum Action {
    None,
    OpenShell(PathBuf),
    DropToLibrary(Vec<String>),
    RestoreFromLibrary(Vec<String>),
    CloneRepo(String),
    RefreshData,
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
            if let Ok(output) = std::process::Command::new("which").arg(shell_name).output()
                && output.status.success()
            {
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

pub fn run_tui(workspace: &Workspace) -> Result<()> {
    let log_capture = LogCapture::new();

    loop {
        // Collect workspace repositories
        let workspace_repos = collect_workspace_repos(workspace);
        let library_repos = collect_library_repos(workspace);

        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // Create app
        let mut app = App::new(workspace_repos, library_repos);

        // Setup filesystem watcher with debouncing
        let (watcher_tx, watcher_rx) = channel();
        let workspace_path = PathBuf::from(&workspace.path);

        // Shared state for debouncing
        let last_event = Arc::new(Mutex::new(Instant::now()));
        let debounce_duration = Duration::from_millis(500);

        let watcher_tx_clone = watcher_tx.clone();
        let last_event_clone = last_event.clone();

        let mut watcher = RecommendedWatcher::new(
            move |res: Result<NotifyEvent, notify::Error>| {
                if let Ok(event) = res {
                    // Ignore events if ANY path is in .git or .workset directories
                    let should_ignore = event.paths.iter().any(|path| {
                        let path_str = path.to_string_lossy();

                        // Ignore .git and .workset directories
                        path_str.contains("/.git/")
                            || path_str.contains("/.workset/")
                            || path_str.ends_with("/.git")
                            || path_str.ends_with("/.workset")
                    });

                    if !should_ignore {
                        let mut last = last_event_clone.lock().unwrap();
                        let now = Instant::now();

                        // Only send if enough time has passed since last event
                        if now.duration_since(*last) > debounce_duration {
                            *last = now;
                            let _ = watcher_tx_clone.send(());
                        }
                    }
                }
            },
            notify::Config::default(),
        )?;

        // Watch the workspace directory recursively
        watcher.watch(&workspace_path, RecursiveMode::Recursive)?;

        // Inner loop to handle actions without tearing down terminal
        loop {
            // Run app with the watcher receiver
            let action = run_app(&mut terminal, &mut app, log_capture.clone(), &watcher_rx)?;

            // Handle the action
            match action {
                Action::None => {
                    // Drain any pending events before cleanup to avoid issues
                    while event::poll(Duration::from_millis(0))? {
                        let _ = event::read()?;
                    }

                    // Restore terminal before exiting
                    disable_raw_mode()?;
                    execute!(
                        terminal.backend_mut(),
                        LeaveAlternateScreen,
                        DisableMouseCapture
                    )?;
                    terminal.show_cursor()?;
                    return Ok(()); // Exit completely
                }
                Action::OpenShell(path) => {
                    // Restore terminal before opening shell
                    disable_raw_mode()?;
                    execute!(
                        terminal.backend_mut(),
                        LeaveAlternateScreen,
                        DisableMouseCapture
                    )?;
                    terminal.show_cursor()?;

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

                    // After shell exits, break inner loop to restart outer loop (recreate terminal)
                    break;
                }
                Action::DropToLibrary(repo_paths) => {
                    use crate::RepoPattern;

                    let mut success_count = 0;
                    let mut error_count = 0;

                    for repo_path in &repo_paths {
                        // Mark as dropping
                        app.update_repo_status(repo_path, tree::RepoOperationStatus::Dropping);
                        terminal.draw(|f| ui(f, &mut app))?;

                        // Small delay so user can see the status change
                        std::thread::sleep(std::time::Duration::from_millis(100));

                        let pattern: RepoPattern = match repo_path.parse() {
                            Ok(p) => p,
                            Err(e) => {
                                app.update_repo_status(
                                    repo_path,
                                    tree::RepoOperationStatus::Failed(format!(
                                        "Parse error: {}",
                                        e
                                    )),
                                );
                                terminal.draw(|f| ui(f, &mut app))?;
                                error_count += 1;
                                continue;
                            }
                        };

                        match workspace.drop(&pattern, false, false) {
                            Ok(_) => {
                                app.update_repo_status(
                                    repo_path,
                                    tree::RepoOperationStatus::Success,
                                );
                                success_count += 1;
                            }
                            Err(e) => {
                                app.update_repo_status(
                                    repo_path,
                                    tree::RepoOperationStatus::Failed(e.to_string()),
                                );
                                error_count += 1;
                            }
                        }
                        terminal.draw(|f| ui(f, &mut app))?;
                    }

                    // Set final message
                    if error_count == 0 {
                        log_capture
                            .set_message(format!("Dropped {} repo(s) to library", success_count));
                    } else {
                        log_capture.set_message(format!(
                            "Dropped {} repo(s), {} failed",
                            success_count, error_count
                        ));
                    }

                    // Wait a moment for user to see the result
                    std::thread::sleep(std::time::Duration::from_millis(500));

                    // Reload repository data and rebuild the app state
                    let workspace_repos = collect_workspace_repos(workspace);
                    let library_repos = collect_library_repos(workspace);

                    // Rebuild app with fresh data
                    app = App::new(workspace_repos, library_repos);
                    app.last_log_message = log_capture.get_message();
                }
                Action::RestoreFromLibrary(repo_paths) => {
                    let mut success_count = 0;
                    let mut error_count = 0;

                    for repo_path in &repo_paths {
                        // Mark as restoring
                        app.update_repo_status(repo_path, tree::RepoOperationStatus::Restoring);
                        terminal.draw(|f| ui(f, &mut app))?;

                        // Small delay so user can see the status change
                        std::thread::sleep(std::time::Duration::from_millis(100));

                        let result = workspace.restore_from_library(repo_path);

                        match result {
                            Ok(_) => {
                                app.update_repo_status(
                                    repo_path,
                                    tree::RepoOperationStatus::Success,
                                );
                                success_count += 1;
                            }
                            Err(e) => {
                                app.update_repo_status(
                                    repo_path,
                                    tree::RepoOperationStatus::Failed(e.to_string()),
                                );
                                error_count += 1;
                            }
                        }
                        terminal.draw(|f| ui(f, &mut app))?;
                    }

                    // Set final message
                    if error_count == 0 {
                        log_capture.set_message(format!(
                            "Restored {} repo(s) from library",
                            success_count
                        ));
                    } else {
                        log_capture.set_message(format!(
                            "Restored {} repo(s), {} failed",
                            success_count, error_count
                        ));
                    }

                    // Wait a moment for user to see the result
                    std::thread::sleep(std::time::Duration::from_millis(500));

                    // Reload repository data and rebuild the app state
                    let workspace_repos = collect_workspace_repos(workspace);
                    let library_repos = collect_library_repos(workspace);

                    // Rebuild app with fresh data
                    app = App::new(workspace_repos, library_repos);
                    app.last_log_message = log_capture.get_message();
                }
                Action::CloneRepo(repo_pattern) => {
                    use crate::RepoPattern;

                    log_capture.set_message(format!("Cloning repository {}...", repo_pattern));
                    // Force a redraw to show the message immediately
                    app.last_log_message = log_capture.get_message();
                    terminal.draw(|f| ui(f, &mut app))?;

                    let pattern: RepoPattern = repo_pattern
                        .parse()
                        .map_err(|e| anyhow::anyhow!("Failed to parse pattern: {}", e))?;

                    // Clone the repository
                    match workspace.open(&pattern) {
                        Ok(_) => {
                            log_capture.set_message(format!("Cloned repository {}", repo_pattern))
                        }
                        Err(e) => log_capture.set_message(format!("Failed to clone: {}", e)),
                    }
                }
                Action::RefreshData => {
                    // Filesystem changed - reload repository data
                    let workspace_repos = collect_workspace_repos(workspace);
                    let library_repos = collect_library_repos(workspace);

                    // Rebuild app with fresh data
                    app = App::new(workspace_repos, library_repos);
                    app.last_log_message = log_capture.get_message();
                }
            } // End of action match
        } // End of inner loop
    } // End of outer loop
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    log_capture: LogCapture,
    watcher_rx: &Receiver<()>,
) -> Result<Action> {
    loop {
        // Update the app with the latest log message
        app.last_log_message = log_capture.get_message();

        terminal.draw(|f| ui(f, app))?;

        // Check for filesystem changes
        match watcher_rx.try_recv() {
            Ok(_) => {
                // Filesystem changed - signal refresh needed
                return Ok(Action::RefreshData);
            }
            Err(TryRecvError::Disconnected) => {
                // Watcher died - continue without watching
            }
            Err(TryRecvError::Empty) => {
                // No changes - continue
            }
        }

        // Use poll with timeout to allow checking for filesystem updates periodically
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => match app.mode {
                    AppMode::Normal => match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(Action::None);
                        }
                        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            // Ctrl+D = drop workspace repo(s) to library
                            if app.active_section == Section::Workspace
                                && let Some(node) = app.selected_workspace_node()
                            {
                                let repo_paths = node.collect_repo_paths();
                                if !repo_paths.is_empty() {
                                    return Ok(Action::DropToLibrary(repo_paths));
                                }
                            }
                        }
                        KeyCode::Right => {
                            // Right arrow = expand node
                            app.toggle_expand();
                        }
                        KeyCode::Left => {
                            // Left arrow = collapse node
                            app.toggle_expand();
                        }
                        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            // Ctrl+A = clone repo dialog
                            app.mode = AppMode::CloneRepo;
                            app.clone_repo_input.clear();

                            // Fetch suggestions in background (blocking for now)
                            let mut suggestions = Vec::new();
                            suggestions.extend(get_github_suggestions());
                            suggestions.extend(get_gitlab_suggestions());
                            suggestions.sort();
                            suggestions.dedup();

                            // Filter out repos that already exist in workspace or library
                            let existing_repos: std::collections::HashSet<String> = app
                                .get_flattened_workspace()
                                .iter()
                                .chain(app.get_flattened_library().iter())
                                .filter_map(|(node, _, _, _)| {
                                    node.repo_info.as_ref().map(|r| r.display_name.clone())
                                })
                                .collect();

                            suggestions.retain(|s| {
                                // Extract the repo name from the suggestion (e.g., "github.com/user/repo" -> "github.com/user/repo")
                                !existing_repos.contains(s)
                            });

                            app.clone_repo_suggestions = suggestions;

                            if !app.clone_repo_suggestions.is_empty() {
                                app.clone_repo_state.select(Some(0));
                            }
                        }
                        KeyCode::Esc => {
                            // Return None to exit - cleanup happens in the outer loop
                            return Ok(Action::None);
                        }
                        KeyCode::Tab => {
                            // Tab switches between workspace and library
                            match app.active_section {
                                Section::Workspace => {
                                    let library_items = app.get_flattened_library();
                                    if !library_items.is_empty() {
                                        app.workspace_state.select(None);
                                        app.library_state.select(Some(0));
                                        app.active_section = Section::Library;
                                    }
                                }
                                Section::Library => {
                                    let workspace_items = app.get_flattened_workspace();
                                    if !workspace_items.is_empty() {
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
                                    if let Some(node) = app.selected_workspace_node() {
                                        if let Some(repo) = node.repo_info {
                                            Action::OpenShell(repo.path.clone())
                                        } else {
                                            // Just a directory node, toggle expand
                                            app.toggle_expand();
                                            Action::None
                                        }
                                    } else {
                                        Action::None
                                    }
                                }
                                Section::Library => {
                                    if let Some(node) = app.selected_library_node() {
                                        let repo_paths = node.collect_repo_paths();
                                        if !repo_paths.is_empty() {
                                            Action::RestoreFromLibrary(repo_paths)
                                        } else {
                                            // Just a directory node, toggle expand
                                            app.toggle_expand();
                                            Action::None
                                        }
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
                    AppMode::CloneRepo => match key.code {
                        KeyCode::Esc => {
                            app.mode = AppMode::Normal;
                            app.clone_repo_input.clear();
                        }
                        KeyCode::Enter => {
                            // Use selected suggestion or manual input
                            let repo = if let Some(idx) = app.clone_repo_state.selected() {
                                // Filter suggestions by current input
                                let filtered: Vec<_> = app
                                    .clone_repo_suggestions
                                    .iter()
                                    .filter(|s| {
                                        s.to_lowercase()
                                            .contains(&app.clone_repo_input.to_lowercase())
                                    })
                                    .collect();

                                filtered.get(idx).map(|s| (*s).to_string())
                            } else {
                                None
                            };

                            let repo = repo.unwrap_or_else(|| app.clone_repo_input.clone());

                            if !repo.is_empty() {
                                app.mode = AppMode::Normal;
                                return Ok(Action::CloneRepo(repo));
                            }
                        }
                        KeyCode::Down => {
                            let filtered: Vec<_> = app
                                .clone_repo_suggestions
                                .iter()
                                .filter(|s| {
                                    s.to_lowercase()
                                        .contains(&app.clone_repo_input.to_lowercase())
                                })
                                .collect();

                            if !filtered.is_empty() {
                                let next = match app.clone_repo_state.selected() {
                                    Some(i) if i >= filtered.len() - 1 => 0,
                                    Some(i) => i + 1,
                                    None => 0,
                                };
                                app.clone_repo_state.select(Some(next));
                            }
                        }
                        KeyCode::Up => {
                            let filtered: Vec<_> = app
                                .clone_repo_suggestions
                                .iter()
                                .filter(|s| {
                                    s.to_lowercase()
                                        .contains(&app.clone_repo_input.to_lowercase())
                                })
                                .collect();

                            if !filtered.is_empty() {
                                let prev = match app.clone_repo_state.selected() {
                                    Some(0) => filtered.len() - 1,
                                    Some(i) => i - 1,
                                    None => 0,
                                };
                                app.clone_repo_state.select(Some(prev));
                            }
                        }
                        KeyCode::Char(c) => {
                            app.clone_repo_input.push(c);
                            app.clone_repo_state.select(Some(0));
                        }
                        KeyCode::Backspace => {
                            app.clone_repo_input.pop();
                        }
                        _ => {}
                    },
                },
                // Ignore other event types (Mouse, Resize, etc.)
                _ => {}
            }
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    if app.mode == AppMode::CloneRepo {
        render_clone_repo_dialog(f, app);
        return;
    }

    // Split vertically into rows
    let vertical_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Help (at top)
            Constraint::Min(0),    // Main area (workspace + library side by side)
            Constraint::Length(3), // Search box
            Constraint::Length(1), // Status/log message (at bottom)
        ])
        .split(f.area());

    // Split the main area horizontally into workspace (left) and library (right)
    let horizontal_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(50), // Workspace (left)
            Constraint::Percentage(50), // Library (right)
        ])
        .split(vertical_chunks[1]);

    // Help text (at top)
    let help_spans = if app.active_section == Section::Workspace {
        vec![
            Span::styled(
                "Tab",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" switch  "),
            Span::styled(
                "↑/↓",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" navigate  "),
            Span::styled(
                "←/→",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" expand/collapse  "),
            Span::styled(
                "Enter",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" open  "),
            Span::styled(
                "Ctrl+D",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" drop  "),
            Span::styled(
                "Ctrl+A",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" clone  "),
            Span::styled(
                "Esc",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" quit"),
        ]
    } else if app.active_section == Section::Library {
        vec![
            Span::styled(
                "Tab",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" switch  "),
            Span::styled(
                "↑/↓",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" navigate  "),
            Span::styled(
                "←/→",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" expand/collapse  "),
            Span::styled(
                "Enter",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" restore  "),
            Span::styled(
                "Ctrl+A",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" clone  "),
            Span::styled(
                "Esc",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" quit"),
        ]
    } else {
        vec![
            Span::styled(
                "Esc",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" quit"),
        ]
    };
    let help = Paragraph::new(Line::from(help_spans)).alignment(Alignment::Center);
    f.render_widget(help, vertical_chunks[0]);

    // Calculate available width for workspace panel (minus borders and padding)
    // Account for: 2 for borders, 2 for highlight symbol ">> ", 1 for padding on right
    let workspace_width = horizontal_chunks[0].width.saturating_sub(5) as usize;

    // Workspace repositories (tree)
    let workspace_flat = app.get_flattened_workspace();
    let workspace_items: Vec<ListItem> = workspace_flat
        .iter()
        .map(|(node, depth, _, full_path)| {
            let mut spans = vec![];

            // Add tree structure indicators
            if *depth > 0 {
                spans.push(Span::raw("  ".repeat(*depth)));
            }

            // Add expand/collapse indicator
            if !node.children.is_empty() {
                spans.push(Span::styled(
                    if node.expanded { "▼ " } else { "▶ " },
                    Style::default().fg(Color::Cyan),
                ));
            } else if *depth > 0 {
                spans.push(Span::raw("  "));
            }

            // Add status icon for repos only
            if let Some(ref repo) = node.repo_info {
                if repo.is_submodule {
                    // Submodule indicator
                    if repo.submodule_initialized {
                        spans.push(Span::styled("S ", Style::default().fg(Color::Magenta)));
                    } else {
                        spans.push(Span::styled("S ", Style::default().fg(Color::DarkGray)));
                        spans.push(Span::styled(
                            "(uninit) ",
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                } else {
                    // Regular repo status
                    if repo.is_clean {
                        spans.push(Span::styled("+ ", Style::default().fg(Color::Green)));
                    } else {
                        spans.push(Span::styled("* ", Style::default().fg(Color::Yellow)));
                    }
                }
            }

            // Add name with search highlighting
            if !app.search_query.is_empty() {
                // For directory nodes, check if the search query contains this directory as a path component
                let should_highlight_dir = node.repo_info.is_none()
                    && app.search_query.contains(&format!("{}/", node.name));

                // Try to match against the full path for this node
                let indices = if should_highlight_dir
                    || app
                        .matcher
                        .fuzzy_indices(full_path, &app.search_query)
                        .is_some()
                {
                    app.matcher
                        .fuzzy_indices(full_path, &app.search_query)
                        .map(|(_, idx)| idx)
                } else {
                    None
                };

                spans.extend(render_highlighted_name(
                    &node.name,
                    full_path,
                    should_highlight_dir,
                    indices,
                ));
            } else {
                spans.push(Span::raw(&node.name));
            }

            // Calculate current text width (accounting for unicode characters)
            let mut text_width = 0;
            for span in &spans {
                text_width += span.content.chars().count();
            }

            // Add operation status or modification time for workspace repos
            if let Some(ref repo) = node.repo_info {
                let (status_text, status_color) = match &repo.operation_status {
                    tree::RepoOperationStatus::None => {
                        // Show modification time if available
                        if let Some(mod_time) = repo.modification_time {
                            (format_time_ago_verbose(mod_time), Color::DarkGray)
                        } else {
                            (String::new(), Color::DarkGray)
                        }
                    }
                    tree::RepoOperationStatus::Dropping => {
                        ("dropping...".to_string(), Color::Yellow)
                    }
                    tree::RepoOperationStatus::Restoring => {
                        ("restoring...".to_string(), Color::Cyan)
                    }
                    tree::RepoOperationStatus::Success => ("done".to_string(), Color::Green),
                    tree::RepoOperationStatus::Failed(err) => {
                        (format!("failed: {}", err), Color::Red)
                    }
                };

                spans.extend(render_metadata_span(
                    text_width,
                    workspace_width,
                    status_text,
                    status_color,
                ));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let workspace_repo_count = app.count_workspace_repos();
    let workspace_list = List::new(workspace_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Workspace ({})", workspace_repo_count))
                .border_style(if app.active_section == Section::Workspace {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                }),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    // Create a custom state wrapper for rendering
    let mut ws_list_state = ratatui::widgets::ListState::default();
    ws_list_state.select(app.workspace_state.selected());
    f.render_stateful_widget(workspace_list, horizontal_chunks[0], &mut ws_list_state);

    // Calculate available width for library panel (minus borders and padding)
    // Account for: 2 for borders, 2 for highlight symbol ">> ", 1 for padding on right
    let library_width = horizontal_chunks[1].width.saturating_sub(5) as usize;

    // Library repositories (tree)
    let library_flat = app.get_flattened_library();
    let library_items: Vec<ListItem> = library_flat
        .iter()
        .map(|(node, depth, _, full_path)| {
            let mut spans = vec![];

            // Add tree structure indicators
            if *depth > 0 {
                spans.push(Span::raw("  ".repeat(*depth)));
            }

            // Add expand/collapse indicator
            if !node.children.is_empty() {
                spans.push(Span::styled(
                    if node.expanded { "▼ " } else { "▶ " },
                    Style::default().fg(Color::Cyan),
                ));
            } else if *depth > 0 {
                spans.push(Span::raw("  "));
            }

            // No icons for library items

            // Add name with search highlighting
            if !app.search_query.is_empty() {
                // For directory nodes, check if the search query contains this directory as a path component
                let should_highlight_dir = node.repo_info.is_none()
                    && app.search_query.contains(&format!("{}/", node.name));

                // Try to match against the full path for this node
                let indices = if should_highlight_dir
                    || app
                        .matcher
                        .fuzzy_indices(full_path, &app.search_query)
                        .is_some()
                {
                    app.matcher
                        .fuzzy_indices(full_path, &app.search_query)
                        .map(|(_, idx)| idx)
                } else {
                    None
                };

                spans.extend(render_highlighted_name(
                    &node.name,
                    full_path,
                    should_highlight_dir,
                    indices,
                ));
            } else {
                spans.push(Span::raw(&node.name));
            }

            // Calculate current text width (accounting for unicode characters)
            let mut text_width = 0;
            for span in &spans {
                text_width += span.content.chars().count();
            }

            // Add operation status or size for library repos
            if let Some(ref repo) = node.repo_info {
                let (status_text, status_color) = match &repo.operation_status {
                    tree::RepoOperationStatus::None => {
                        // Show size if available
                        if let Some(size_bytes) = repo.size_bytes {
                            (format_size(size_bytes), Color::DarkGray)
                        } else {
                            (String::new(), Color::DarkGray)
                        }
                    }
                    tree::RepoOperationStatus::Dropping => {
                        ("dropping...".to_string(), Color::Yellow)
                    }
                    tree::RepoOperationStatus::Restoring => {
                        ("restoring...".to_string(), Color::Cyan)
                    }
                    tree::RepoOperationStatus::Success => ("done".to_string(), Color::Green),
                    tree::RepoOperationStatus::Failed(err) => {
                        (format!("failed: {}", err), Color::Red)
                    }
                };

                spans.extend(render_metadata_span(
                    text_width,
                    library_width,
                    status_text,
                    status_color,
                ));
            }

            ListItem::new(Line::from(spans))
        })
        .collect();

    let library_repo_count = app.count_library_repos();
    let library_list = List::new(library_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Library ({})", library_repo_count))
                .border_style(if app.active_section == Section::Library {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                }),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    // Create a custom state wrapper for rendering
    let mut lib_list_state = ratatui::widgets::ListState::default();
    lib_list_state.select(app.library_state.selected());
    f.render_stateful_widget(library_list, horizontal_chunks[1], &mut lib_list_state);

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
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(search_border_style)
                .title("Search"),
        );
    f.render_widget(search, vertical_chunks[2]);

    // Status/log message (at bottom)
    let status = Paragraph::new(app.last_log_message.as_str())
        .style(Style::default().fg(Color::Gray))
        .alignment(Alignment::Left);
    f.render_widget(status, vertical_chunks[3]);
}

fn render_clone_repo_dialog(f: &mut Frame, app: &App) {
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
        .title("Clone Repository")
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
            Constraint::Length(3), // Input box
            Constraint::Min(0),    // Suggestions
        ])
        .split(inner);

    // Input box
    let input_text = format!("{}_", app.clone_repo_input);
    let input = Paragraph::new(input_text)
        .style(Style::default().fg(Color::Yellow))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Repository (e.g. github.com/user/repo)"),
        );
    f.render_widget(input, chunks[0]);

    // Suggestions list
    let filtered_suggestions: Vec<_> = app
        .clone_repo_suggestions
        .iter()
        .filter(|s| {
            if app.clone_repo_input.is_empty() {
                true
            } else {
                s.to_lowercase()
                    .contains(&app.clone_repo_input.to_lowercase())
            }
        })
        .collect();

    let suggestion_items: Vec<ListItem> = filtered_suggestions
        .iter()
        .map(|s| ListItem::new(s.as_str()))
        .collect();

    let suggestions = List::new(suggestion_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("Suggestions ({})", filtered_suggestions.len())),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    let mut state = ratatui::widgets::ListState::default();
    state.select(app.clone_repo_state.selected());
    f.render_stateful_widget(suggestions, chunks[1], &mut state);
}

/// Render right-aligned metadata (status or size) with padding
fn render_metadata_span<'a>(
    text_width: usize,
    available_width: usize,
    metadata_text: String,
    metadata_color: Color,
) -> Vec<Span<'a>> {
    let mut spans = vec![];

    if !metadata_text.is_empty() {
        let metadata_width = metadata_text.chars().count();
        // Calculate padding needed to right-align (ensure at least 1 space)
        let padding_needed = available_width
            .saturating_sub(text_width)
            .saturating_sub(metadata_width)
            .max(1);
        spans.push(Span::raw(" ".repeat(padding_needed)));
        spans.push(Span::styled(
            metadata_text,
            Style::default().fg(metadata_color),
        ));
    }

    spans
}

/// Render highlighted name with character-by-character fuzzy match highlighting
fn render_highlighted_name<'a>(
    node_name: &'a str,
    full_path: &str,
    should_highlight_dir: bool,
    indices: Option<Vec<usize>>,
) -> Vec<Span<'a>> {
    let mut spans = vec![];

    if let Some(indices) = indices {
        let mut last_pos = 0;
        let chars: Vec<(usize, char)> = node_name.char_indices().collect();

        // Find which indices apply to just the node name (not full path)
        let path_offset = full_path.len() - node_name.len();

        for &match_idx in &indices {
            if match_idx < path_offset {
                continue; // Skip matches in path prefix
            }
            let local_idx = match_idx - path_offset;

            if local_idx >= chars.len() {
                continue;
            }

            // Add unmatched text before this character
            if local_idx > last_pos {
                let start_byte = chars[last_pos].0;
                let end_byte = chars[local_idx].0;
                spans.push(Span::raw(&node_name[start_byte..end_byte]));
            }

            // Add highlighted character
            let char_byte_start = chars[local_idx].0;
            let char_byte_end = if local_idx + 1 < chars.len() {
                chars[local_idx + 1].0
            } else {
                node_name.len()
            };
            spans.push(Span::styled(
                &node_name[char_byte_start..char_byte_end],
                Style::default().fg(Color::Black).bg(Color::Yellow),
            ));

            last_pos = local_idx + 1;
        }

        // Add remaining text
        if last_pos < chars.len() {
            let start_byte = chars[last_pos].0;
            spans.push(Span::raw(&node_name[start_byte..]));
        } else if last_pos == 0 {
            // No matches in name portion, show normally
            spans.push(Span::raw(node_name));
        }
    } else if should_highlight_dir {
        // Directory is part of search path, highlight entire name
        spans.push(Span::styled(
            node_name,
            Style::default().fg(Color::Black).bg(Color::Yellow),
        ));
    } else {
        spans.push(Span::raw(node_name));
    }

    spans
}

/// Collect workspace repositories with metadata
fn collect_workspace_repos(workspace: &Workspace) -> Vec<RepoInfo> {
    let repos = find_git_repositories(&workspace.path).unwrap_or_default();
    let mut repo_infos = Vec::new();

    for path in repos {
        let display_name = path
            .strip_prefix(&workspace.path)
            .unwrap_or(&path)
            .display()
            .to_string()
            .trim_start_matches('/')
            .to_string();

        // Check repo status and get modification time in a single repo open for performance
        let (status, modification_time) = crate::check_repo_status_and_modification_time(&path)
            .unwrap_or((crate::RepoStatus::NoCommits, None));

        // A repo is only clean if it has commits, no changes, and no unpushed commits
        let is_clean = matches!(status, crate::RepoStatus::Clean);

        // Size not computed for workspace repos to save time
        let size_bytes = None;

        // Add the main repository
        repo_infos.push(RepoInfo {
            path: path.clone(),
            display_name: display_name.clone(),
            is_clean,
            modification_time,
            size_bytes,
            operation_status: tree::RepoOperationStatus::None,
            is_submodule: false,
            submodule_initialized: false,
            parent_repo_path: None,
        });

        // Find and add submodules
        if let Ok(submodules) = crate::find_submodules_in_repo(&path) {
            for submodule in submodules {
                let submodule_display_name = if display_name.is_empty() {
                    submodule.path.display().to_string()
                } else {
                    format!("{}/{}", display_name, submodule.path.display())
                };

                repo_infos.push(RepoInfo {
                    path: path.join(&submodule.path),
                    display_name: submodule_display_name,
                    is_clean: true, // Submodule status computed separately
                    modification_time: None,
                    size_bytes: None,
                    operation_status: tree::RepoOperationStatus::None,
                    is_submodule: true,
                    submodule_initialized: submodule.initialized,
                    parent_repo_path: Some(path.clone()),
                });
            }
        }
    }

    repo_infos
}

/// Collect library repositories with metadata
fn collect_library_repos(workspace: &Workspace) -> Vec<RepoInfo> {
    workspace
        .list_library()
        .unwrap_or_default()
        .into_iter()
        .map(|repo_path| {
            let full_path = PathBuf::from(&workspace.library_path()).join(&repo_path);
            // Get modification time for library repos
            let modification_time = get_repo_modification_time(&full_path, true).ok();
            // Get size for library repos
            let size_bytes = get_repo_size(&full_path).ok();

            RepoInfo {
                path: full_path,
                display_name: repo_path,
                is_clean: true, // Library repos are always clean
                modification_time,
                size_bytes,
                operation_status: tree::RepoOperationStatus::None,
                is_submodule: false,
                submodule_initialized: false,
                parent_repo_path: None,
            }
        })
        .collect()
}

/// Get the configured GitHub hostname from gh CLI
fn get_github_hostname() -> String {
    if let Ok(output) = std::process::Command::new("gh")
        .args(["auth", "status", "--active", "--json", "hosts"])
        .output()
        && output.status.success()
    {
        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) {
            if let Some(hosts) = json.get("hosts").and_then(|h| h.as_object()) {
                if let Some(hostname) = hosts.keys().next() {
                    return hostname.clone();
                }
            }
        }
    }
    // Default to github.com if we can't determine the hostname
    "github.com".to_string()
}

/// Fetch repository suggestions from GitHub CLI for TUI autocomplete
fn get_github_suggestions() -> Vec<String> {
    if let Ok(output) = std::process::Command::new("gh")
        .args([
            "repo",
            "list",
            "--limit",
            "100",
            "--json",
            "nameWithOwner",
            "-q",
            ".[].nameWithOwner",
        ])
        .output()
        && output.status.success()
    {
        let hostname = get_github_hostname();
        return String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| format!("{}/{}", hostname, line.trim()))
            .collect();
    }
    Vec::new()
}

/// Get the configured GitLab hostname from glab CLI
fn get_gitlab_hostname() -> String {
    if let Ok(output) = std::process::Command::new("glab")
        .args(["config", "get", "host"])
        .output()
        && output.status.success()
    {
        let hostname = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !hostname.is_empty() {
            return hostname;
        }
    }
    // Default to gitlab.com if we can't determine the hostname
    "gitlab.com".to_string()
}

/// Fetch repository suggestions from GitLab CLI for TUI autocomplete
fn get_gitlab_suggestions() -> Vec<String> {
    if let Ok(output) = std::process::Command::new("glab")
        .args(["repo", "list", "--all", "--per-page", "100"])
        .output()
        && output.status.success()
    {
        let hostname = get_gitlab_hostname();
        return String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                // glab output format is: "namespace/project"
                let parts: Vec<&str> = line.split_whitespace().collect();
                parts.first().map(|repo| format!("{}/{}", hostname, repo))
            })
            .collect();
    }
    Vec::new()
}
