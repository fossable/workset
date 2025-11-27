mod app;
mod metadata;
mod tree;

use app::{App, AppMode, Section};
use metadata::{format_size, format_time_ago, get_repo_modification_time, get_repo_size};
use tree::RepoInfo;

#[cfg(feature = "github")]
use crate::remote::github;
#[cfg(feature = "gitlab")]
use crate::remote::gitlab;
use crate::{Workspace, check_repo_status, find_git_repositories};
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use fuzzy_matcher::FuzzyMatcher;
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
    AddRepo(String),
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
        let workspace_repos: Vec<RepoInfo> = find_git_repositories(&workspace.path)?
            .into_iter()
            .map(|path| {
                let display_name = path
                    .strip_prefix(&workspace.path)
                    .unwrap_or(&path)
                    .display()
                    .to_string()
                    .trim_start_matches('/')
                    .to_string();

                // Check repo status in a single pass for performance
                let status = check_repo_status(&path).unwrap_or_default();
                // A repo is only clean if it has commits, no changes, and no unpushed commits
                let is_clean = status.has_commits && !status.has_changes && !status.has_unpushed;

                // Get modification time
                let modification_time = get_repo_modification_time(&path, is_clean).ok();

                // Get size (only for workspace repos, not computed for library to save time)
                let size_bytes = None;

                RepoInfo {
                    path,
                    display_name,
                    is_clean,
                    modification_time,
                    size_bytes,
                }
            })
            .collect();

        // Collect library repositories
        let library_repos: Vec<RepoInfo> = if let Some(library) = &workspace.library {
            library
                .list()
                .unwrap_or_default()
                .into_iter()
                .map(|repo_path| {
                    let full_path = PathBuf::from(&library.path).join(&repo_path);
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
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

        // Setup terminal
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // Create app
        let mut app = App::new(workspace_repos, library_repos);

        // Run app
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
            Action::DropToLibrary(repo_paths) => {
                use crate::RepoPattern;

                let count = repo_paths.len();
                log_capture.set_message(format!("📦 Dropping {} repo(s) to library...", count));
                // Force a redraw to show the message immediately
                app.last_log_message = log_capture.get_message();
                terminal.draw(|f| ui(f, &mut app))?;

                let mut success_count = 0;
                let mut error_count = 0;

                for repo_path in repo_paths {
                    let pattern: RepoPattern = match repo_path.parse() {
                        Ok(p) => p,
                        Err(e) => {
                            log_capture.set_message(format!(
                                "✗ Failed to parse pattern {}: {}",
                                repo_path, e
                            ));
                            error_count += 1;
                            continue;
                        }
                    };

                    match workspace.drop(workspace.library.as_ref(), &pattern, false, false) {
                        Ok(_) => success_count += 1,
                        Err(e) => {
                            log_capture
                                .set_message(format!("✗ Failed to drop {}: {}", repo_path, e));
                            error_count += 1;
                        }
                    }
                }

                if error_count == 0 {
                    log_capture
                        .set_message(format!("✓ Dropped {} repo(s) to library", success_count));
                } else {
                    log_capture.set_message(format!(
                        "⚠ Dropped {} repo(s), {} failed",
                        success_count, error_count
                    ));
                }

                // Loop back to refresh TUI
                continue;
            }
            Action::RestoreFromLibrary(repo_paths) => {
                let count = repo_paths.len();
                log_capture.set_message(format!("📦 Restoring {} repo(s) from library...", count));
                // Force a redraw to show the message immediately
                app.last_log_message = log_capture.get_message();
                terminal.draw(|f| ui(f, &mut app))?;

                let mut success_count = 0;
                let mut error_count = 0;

                for repo_path in repo_paths {
                    let result = if let Some(library) = &workspace.library {
                        library.restore_to_workspace(&workspace.path, &repo_path)
                    } else {
                        Ok(())
                    };

                    match result {
                        Ok(_) => success_count += 1,
                        Err(e) => {
                            log_capture
                                .set_message(format!("✗ Failed to restore {}: {}", repo_path, e));
                            error_count += 1;
                        }
                    }
                }

                if error_count == 0 {
                    log_capture
                        .set_message(format!("✓ Restored {} repo(s) from library", success_count));
                } else {
                    log_capture.set_message(format!(
                        "⚠ Restored {} repo(s), {} failed",
                        success_count, error_count
                    ));
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
                    Ok(_) => {
                        log_capture.set_message(format!("✓ Added repository {}", repo_pattern))
                    }
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

        // Use poll with timeout to allow checking for metadata updates periodically
        if event::poll(std::time::Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            match app.mode {
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
                        // Ctrl+A = add repo dialog
                        app.mode = AppMode::AddRepo;
                        app.add_repo_input.clear();

                        // Fetch suggestions in background (blocking for now)
                        let mut suggestions = Vec::new();
                        #[cfg(feature = "github")]
                        suggestions.extend(github::get_suggestions());
                        #[cfg(feature = "gitlab")]
                        suggestions.extend(gitlab::get_suggestions());
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
                AppMode::AddRepo => match key.code {
                    KeyCode::Esc => {
                        app.mode = AppMode::Normal;
                        app.add_repo_input.clear();
                    }
                    KeyCode::Enter => {
                        // Use selected suggestion or manual input
                        let repo = if let Some(idx) = app.add_repo_state.selected() {
                            // Filter suggestions by current input
                            let filtered: Vec<_> = app
                                .add_repo_suggestions
                                .iter()
                                .filter(|s| {
                                    s.to_lowercase()
                                        .contains(&app.add_repo_input.to_lowercase())
                                })
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
                        let filtered: Vec<_> = app
                            .add_repo_suggestions
                            .iter()
                            .filter(|s| {
                                s.to_lowercase()
                                    .contains(&app.add_repo_input.to_lowercase())
                            })
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
                        let filtered: Vec<_> = app
                            .add_repo_suggestions
                            .iter()
                            .filter(|s| {
                                s.to_lowercase()
                                    .contains(&app.add_repo_input.to_lowercase())
                            })
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
            Span::raw(" add  "),
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
                if repo.is_clean {
                    spans.push(Span::styled("✓ ", Style::default().fg(Color::Green)));
                } else {
                    spans.push(Span::styled("⚠ ", Style::default().fg(Color::Yellow)));
                }
            }

            // Add name with search highlighting
            if !app.search_query.is_empty() {
                // For directory nodes, check if the search query contains this directory as a path component
                let should_highlight_dir = node.repo_info.is_none()
                    && app.search_query.contains(&format!("{}/", node.name));

                // Try to match against the full path for this node
                if should_highlight_dir
                    || app
                        .matcher
                        .fuzzy_indices(full_path, &app.search_query)
                        .is_some()
                {
                    if let Some((_, indices)) =
                        app.matcher.fuzzy_indices(full_path, &app.search_query)
                    {
                        let mut last_pos = 0;
                        let chars: Vec<(usize, char)> = node.name.char_indices().collect();

                        // Find which indices apply to just the node name (not full path)
                        let path_offset = full_path.len() - node.name.len();

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
                                spans.push(Span::raw(&node.name[start_byte..end_byte]));
                            }

                            // Add highlighted character
                            let char_byte_start = chars[local_idx].0;
                            let char_byte_end = if local_idx + 1 < chars.len() {
                                chars[local_idx + 1].0
                            } else {
                                node.name.len()
                            };
                            spans.push(Span::styled(
                                &node.name[char_byte_start..char_byte_end],
                                Style::default().fg(Color::Black).bg(Color::Yellow),
                            ));

                            last_pos = local_idx + 1;
                        }

                        // Add remaining text
                        if last_pos < chars.len() {
                            let start_byte = chars[last_pos].0;
                            spans.push(Span::raw(&node.name[start_byte..]));
                        } else if last_pos == 0 {
                            // No matches in name portion, show normally
                            spans.push(Span::raw(&node.name));
                        }
                    } else if should_highlight_dir {
                        // Directory is part of search path, highlight entire name
                        spans.push(Span::styled(
                            &node.name,
                            Style::default().fg(Color::Black).bg(Color::Yellow),
                        ));
                    }
                } else {
                    spans.push(Span::raw(&node.name));
                }
            } else {
                spans.push(Span::raw(&node.name));
            }

            // Calculate current text width (accounting for unicode characters)
            let mut text_width = 0;
            for span in &spans {
                text_width += span.content.chars().count();
            }

            // Add modification time for workspace repos (right-aligned if available)
            if let Some(ref repo) = node.repo_info
                && let Some(mod_time) = repo.modification_time
            {
                let time_str = format_time_ago(mod_time);
                let metadata_width = time_str.chars().count();
                // Calculate padding needed to right-align (ensure at least 1 space)
                let padding_needed = workspace_width
                    .saturating_sub(text_width)
                    .saturating_sub(metadata_width)
                    .max(1);
                spans.push(Span::raw(" ".repeat(padding_needed)));
                spans.push(Span::styled(time_str, Style::default().fg(Color::DarkGray)));
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
                if should_highlight_dir
                    || app
                        .matcher
                        .fuzzy_indices(full_path, &app.search_query)
                        .is_some()
                {
                    if let Some((_, indices)) =
                        app.matcher.fuzzy_indices(full_path, &app.search_query)
                    {
                        let mut last_pos = 0;
                        let chars: Vec<(usize, char)> = node.name.char_indices().collect();

                        // Find which indices apply to just the node name (not full path)
                        let path_offset = full_path.len() - node.name.len();

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
                                spans.push(Span::raw(&node.name[start_byte..end_byte]));
                            }

                            // Add highlighted character
                            let char_byte_start = chars[local_idx].0;
                            let char_byte_end = if local_idx + 1 < chars.len() {
                                chars[local_idx + 1].0
                            } else {
                                node.name.len()
                            };
                            spans.push(Span::styled(
                                &node.name[char_byte_start..char_byte_end],
                                Style::default().fg(Color::Black).bg(Color::Yellow),
                            ));

                            last_pos = local_idx + 1;
                        }

                        // Add remaining text
                        if last_pos < chars.len() {
                            let start_byte = chars[last_pos].0;
                            spans.push(Span::raw(&node.name[start_byte..]));
                        } else if last_pos == 0 {
                            // No matches in name portion, show normally
                            spans.push(Span::raw(&node.name));
                        }
                    } else if should_highlight_dir {
                        // Directory is part of search path, highlight entire name
                        spans.push(Span::styled(
                            &node.name,
                            Style::default().fg(Color::Black).bg(Color::Yellow),
                        ));
                    }
                } else {
                    spans.push(Span::raw(&node.name));
                }
            } else {
                spans.push(Span::raw(&node.name));
            }

            // Calculate current text width (accounting for unicode characters)
            let mut text_width = 0;
            for span in &spans {
                text_width += span.content.chars().count();
            }

            // Add size for library repos (right-aligned if available)
            if let Some(ref repo) = node.repo_info
                && let Some(size_bytes) = repo.size_bytes
            {
                let size_str = format_size(size_bytes);
                let metadata_width = size_str.chars().count();
                // Calculate padding needed to right-align (ensure at least 1 space)
                let padding_needed = library_width
                    .saturating_sub(text_width)
                    .saturating_sub(metadata_width)
                    .max(1);
                spans.push(Span::raw(" ".repeat(padding_needed)));
                spans.push(Span::styled(size_str, Style::default().fg(Color::DarkGray)));
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
            Constraint::Length(3), // Input box
            Constraint::Min(0),    // Suggestions
        ])
        .split(inner);

    // Input box
    let input_text = format!("{}_", app.add_repo_input);
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
        .add_repo_suggestions
        .iter()
        .filter(|s| {
            if app.add_repo_input.is_empty() {
                true
            } else {
                s.to_lowercase()
                    .contains(&app.add_repo_input.to_lowercase())
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
    state.select(app.add_repo_state.selected());
    f.render_stateful_widget(suggestions, chunks[1], &mut state);
}
