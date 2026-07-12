mod app;
mod metadata;
mod tree;
mod watcher;

use app::{App, AppMode, Section};
use metadata::{format_size, format_time_ago_verbose, get_repo_modification_time, get_repo_size};
use tree::{RepoInfo, RepoOperationStatus, TreeNode};
use watcher::FileWatcher;

use crate::{RepoPattern, Workspace, find_git_repositories};
use anyhow::{Result, anyhow};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use fuzzy_matcher::FuzzyMatcher;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, mpsc};
use std::time::Duration;

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

enum Action {
    None,
    OpenShell(PathBuf),
    DropToLibrary(Vec<String>),
    RestoreFromLibrary(Vec<String>),
    CloneRepo(String),
    RefreshData,
}

/// Results streamed from the background repo scan
enum LoadEvent {
    Total(usize),
    Workspace(Vec<RepoInfo>),
    Library(RepoInfo),
}

/// A repo scan running on background threads. Results are drained into the
/// `App` from the event loop via `poll`, so the UI stays responsive.
struct RepoLoader {
    rx: mpsc::Receiver<LoadEvent>,
    workspace_repos: Vec<RepoInfo>,
    library_repos: Vec<RepoInfo>,
    done: usize,
    total: usize,
    last_name: String,
    /// Show partial results as they arrive (initial load); otherwise the app
    /// keeps its current data until the scan completes (background refresh).
    progressive: bool,
}

impl RepoLoader {
    fn start(workspace: &Workspace, progressive: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        let workspace = workspace.clone();
        std::thread::spawn(move || scan_all_repos(&workspace, tx));
        Self {
            rx,
            workspace_repos: Vec::new(),
            library_repos: Vec::new(),
            done: 0,
            total: 0,
            last_name: String::new(),
            progressive,
        }
    }

    /// Drain any newly scanned repos into the app without blocking.
    /// Returns false once the scan has finished.
    fn poll(&mut self, app: &mut App) -> bool {
        let mut received = false;
        let finished = loop {
            match self.rx.try_recv() {
                Ok(LoadEvent::Total(total)) => self.total = total,
                Ok(LoadEvent::Workspace(infos)) => {
                    if let Some(info) = infos.first() {
                        self.last_name = info.display_name.clone();
                    }
                    self.workspace_repos.extend(infos);
                    self.done += 1;
                    received = true;
                }
                Ok(LoadEvent::Library(info)) => {
                    self.last_name = info.display_name.clone();
                    self.library_repos.push(info);
                    self.done += 1;
                    received = true;
                }
                Err(mpsc::TryRecvError::Empty) => break false,
                Err(mpsc::TryRecvError::Disconnected) => break true,
            }
        };

        if finished {
            app.update_repos(
                std::mem::take(&mut self.workspace_repos),
                std::mem::take(&mut self.library_repos),
            );
            if self.progressive {
                app.last_log_message.clear();
            }
        } else if received && self.progressive {
            app.update_repos(self.workspace_repos.clone(), self.library_repos.clone());
            let spinner = SPINNER_FRAMES[self.done % SPINNER_FRAMES.len()];
            app.last_log_message = format!(
                "{} Loading: {} ({}/{})",
                spinner, self.last_name, self.done, self.total
            );
        }

        !finished
    }
}

/// Handles to work running on background threads, drained by the event loop
struct BackgroundTasks {
    loader: Option<RepoLoader>,
    suggestions: Option<mpsc::Receiver<Vec<String>>>,
    clone_result: Option<mpsc::Receiver<String>>,
    /// Delivers the file watcher once its (potentially slow) recursive
    /// registration of the workspace tree completes
    watcher: Option<mpsc::Receiver<Result<FileWatcher, notify::Error>>>,
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
    loop {
        // Setup terminal FIRST so we can show progress
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // Create app with empty data; repos stream in from the background loader
        let mut app = App::new(Vec::new(), Vec::new());
        app.last_log_message = "Loading repositories...".to_string();

        // Setup the debounced filesystem watcher on a background thread, since
        // recursively registering a large workspace can take a while and would
        // delay the first frame
        let workspace_path = PathBuf::from(&workspace.path);
        let (watcher_tx, watcher_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = watcher_tx.send(FileWatcher::new(
                &workspace_path,
                Duration::from_millis(500),
            ));
        });
        let mut file_watcher: Option<FileWatcher> = None;

        let mut background = BackgroundTasks {
            loader: Some(RepoLoader::start(workspace, true)),
            suggestions: None,
            clone_result: None,
            watcher: Some(watcher_rx),
        };

        // Inner loop to handle actions without tearing down terminal
        loop {
            let action = run_app(&mut terminal, &mut app, &mut file_watcher, &mut background)?;

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

                    // Use $SHELL or try to detect the actual parent shell
                    let shell = std::env::var("SHELL").unwrap_or_else(|_| {
                        detect_parent_shell().unwrap_or_else(|| "/bin/sh".to_string())
                    });

                    // Spawn an interactive shell in the repository directory
                    std::process::Command::new(&shell)
                        .current_dir(&path)
                        .status()?;

                    // After shell exits, break inner loop to restart outer loop (recreate terminal)
                    break;
                }
                Action::DropToLibrary(repo_paths) => {
                    let message = run_repo_operation(
                        &mut terminal,
                        &mut app,
                        &repo_paths,
                        RepoOperationStatus::Dropping,
                        |repo_path| {
                            let Ok(pattern) = repo_path.parse::<RepoPattern>();
                            workspace.drop(&pattern, false, false)
                        },
                        "Dropped",
                    )?;
                    app.last_log_message = message;
                    background.loader = Some(RepoLoader::start(workspace, false));
                }
                Action::RestoreFromLibrary(repo_paths) => {
                    let message = run_repo_operation(
                        &mut terminal,
                        &mut app,
                        &repo_paths,
                        RepoOperationStatus::Restoring,
                        |repo_path| workspace.restore_from_library(repo_path),
                        "Restored",
                    )?;
                    app.last_log_message = message;
                    background.loader = Some(RepoLoader::start(workspace, false));
                }
                Action::CloneRepo(repo_pattern) => {
                    app.last_log_message = format!("Cloning repository {}...", repo_pattern);

                    // Clone on a background thread; the file watcher picks up
                    // the new repo and triggers a refresh when it lands
                    let (tx, rx) = mpsc::channel();
                    background.clone_result = Some(rx);
                    let workspace = workspace.clone();
                    std::thread::spawn(move || {
                        let Ok(pattern) = repo_pattern.parse::<RepoPattern>();
                        let message = match workspace.open(&pattern) {
                            Ok(_) => format!("Cloned repository {}", repo_pattern),
                            Err(e) => format!("Failed to clone: {}", e),
                        };
                        let _ = tx.send(message);
                    });
                }
                Action::RefreshData => {
                    // Filesystem changed - reload repository data in the background
                    background.loader = Some(RepoLoader::start(workspace, false));
                    if let Some(watcher) = file_watcher.as_mut() {
                        watcher.drain_pending();
                    }
                }
            }
        }
    }
}

/// Run a drop/restore operation over the given repos, updating each repo's
/// status in the UI as it progresses. Returns the summary message.
fn run_repo_operation<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    repo_paths: &[String],
    in_progress: RepoOperationStatus,
    mut operation: impl FnMut(&str) -> Result<()>,
    verb: &str,
) -> Result<String> {
    let mut success_count = 0;
    let mut error_count = 0;
    let draw = |terminal: &mut Terminal<B>, app: &mut App| -> Result<()> {
        terminal
            .draw(|f| ui(f, app))
            .map_err(|e| anyhow!("Failed to render frame: {}", e))?;
        Ok(())
    };

    for repo_path in repo_paths {
        app.update_repo_status(repo_path, in_progress.clone());
        draw(terminal, app)?;

        // Small delay so user can see the status change
        std::thread::sleep(Duration::from_millis(100));

        match operation(repo_path) {
            Ok(_) => {
                app.update_repo_status(repo_path, RepoOperationStatus::Success);
                success_count += 1;
            }
            Err(e) => {
                app.update_repo_status(repo_path, RepoOperationStatus::Failed(e.to_string()));
                error_count += 1;
            }
        }
        draw(terminal, app)?;
    }

    // Wait a moment for user to see the result
    std::thread::sleep(Duration::from_millis(500));

    Ok(if error_count == 0 {
        format!("{} {} repo(s)", verb, success_count)
    } else {
        format!("{} {} repo(s), {} failed", verb, success_count, error_count)
    })
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    file_watcher: &mut Option<FileWatcher>,
    background: &mut BackgroundTasks,
) -> Result<Action> {
    loop {
        // Apply results from background work before drawing
        if let Some(rx) = &background.watcher {
            match rx.try_recv() {
                Ok(Ok(watcher)) => {
                    *file_watcher = Some(watcher);
                    background.watcher = None;
                }
                Ok(Err(e)) => {
                    app.last_log_message = format!("File watching disabled: {}", e);
                    background.watcher = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => background.watcher = None,
            }
        }
        if let Some(loader) = background.loader.as_mut()
            && !loader.poll(app)
        {
            background.loader = None;
            // Drain events generated by the scan to prevent a feedback loop
            if let Some(watcher) = file_watcher.as_mut() {
                watcher.drain_pending();
            }
        }
        poll_suggestions(app, background);
        if let Some(rx) = &background.clone_result {
            match rx.try_recv() {
                Ok(message) => {
                    app.last_log_message = message;
                    background.clone_result = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => background.clone_result = None,
            }
        }

        terminal
            .draw(|f| ui(f, app))
            .map_err(|e| anyhow!("Failed to render frame: {}", e))?;

        // Check for filesystem changes; skip while a reload is already running
        if background.loader.is_none()
            && let Some(watcher) = file_watcher.as_mut()
            && watcher.poll_refresh()
        {
            return Ok(Action::RefreshData);
        }

        // Use poll with timeout to allow checking for filesystem updates periodically
        if event::poll(Duration::from_millis(100))? {
            // Ignore other event types (Mouse, Resize, etc.)
            let Event::Key(key) = event::read()? else {
                continue;
            };
            match app.mode {
                AppMode::Normal => match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(Action::None);
                    }
                    KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // Ctrl+D = drop workspace repo(s) to library
                        if app.active_section == Section::Workspace
                            && let Some(node) = app.selected_node()
                        {
                            let repo_paths = node.collect_repo_paths();
                            if !repo_paths.is_empty() {
                                return Ok(Action::DropToLibrary(repo_paths));
                            }
                        }
                    }
                    KeyCode::Right | KeyCode::Left => {
                        app.toggle_expand();
                    }
                    KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        // Ctrl+A = clone repo dialog; suggestions arrive from a
                        // background thread since gh/glab may hit the network
                        app.mode = AppMode::CloneRepo;
                        app.clone_repo_input.clear();
                        app.clone_repo_suggestions.clear();
                        app.clone_repo_state.select(None);
                        app.suggestions_loading = true;

                        let (tx, rx) = mpsc::channel();
                        background.suggestions = Some(rx);
                        std::thread::spawn(move || {
                            let mut suggestions = get_github_suggestions();
                            suggestions.extend(get_gitlab_suggestions());
                            suggestions.sort();
                            suggestions.dedup();
                            let _ = tx.send(suggestions);
                        });
                    }
                    KeyCode::Esc => {
                        // Return None to exit - cleanup happens in the outer loop
                        return Ok(Action::None);
                    }
                    KeyCode::Tab => {
                        // Tab switches between workspace and library
                        app.switch_section();
                    }
                    KeyCode::Down => app.next(),
                    KeyCode::Up => app.previous(),
                    KeyCode::Enter => {
                        // Enter on workspace repo = open shell
                        // Enter on library repo = restore from library
                        // Enter on a directory node = toggle expansion
                        let selected = app.selected_node().map(|node| {
                            (
                                node.repo_info.as_ref().map(|r| r.path.clone()),
                                node.collect_repo_paths(),
                            )
                        });

                        if let Some((repo_dir, repo_paths)) = selected {
                            match app.active_section {
                                Section::Workspace => match repo_dir {
                                    Some(path) => return Ok(Action::OpenShell(path)),
                                    None => app.toggle_expand(),
                                },
                                Section::Library => {
                                    if !repo_paths.is_empty() {
                                        return Ok(Action::RestoreFromLibrary(repo_paths));
                                    }
                                    app.toggle_expand();
                                }
                            }
                        }
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
                        let repo = app
                            .clone_repo_state
                            .selected()
                            .and_then(|idx| {
                                app.filtered_suggestions().get(idx).map(|s| s.to_string())
                            })
                            .unwrap_or_else(|| app.clone_repo_input.clone());

                        if !repo.is_empty() {
                            app.mode = AppMode::Normal;
                            return Ok(Action::CloneRepo(repo));
                        }
                    }
                    KeyCode::Down => {
                        let len = app.filtered_suggestions().len();
                        if len > 0 {
                            let next = match app.clone_repo_state.selected() {
                                Some(i) if i + 1 >= len => 0,
                                Some(i) => i + 1,
                                None => 0,
                            };
                            app.clone_repo_state.select(Some(next));
                        }
                    }
                    KeyCode::Up => {
                        let len = app.filtered_suggestions().len();
                        if len > 0 {
                            let prev = match app.clone_repo_state.selected() {
                                Some(0) => len - 1,
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
            }
        }
    }
}

/// Apply clone-dialog suggestions once the background fetch completes
fn poll_suggestions(app: &mut App, background: &mut BackgroundTasks) {
    let Some(rx) = &background.suggestions else {
        return;
    };
    match rx.try_recv() {
        Ok(mut suggestions) => {
            // Filter out repos that already exist in workspace or library
            let existing_repos: std::collections::HashSet<String> = app
                .get_flattened_workspace()
                .iter()
                .chain(app.get_flattened_library().iter())
                .filter_map(|(node, _, _, _)| {
                    node.repo_info.as_ref().map(|r| r.display_name.clone())
                })
                .collect();
            suggestions.retain(|s| !existing_repos.contains(s));

            app.clone_repo_suggestions = suggestions;
            if !app.clone_repo_suggestions.is_empty() && app.clone_repo_state.selected().is_none() {
                app.clone_repo_state.select(Some(0));
            }
            app.suggestions_loading = false;
            background.suggestions = None;
        }
        Err(mpsc::TryRecvError::Empty) => {}
        Err(mpsc::TryRecvError::Disconnected) => {
            app.suggestions_loading = false;
            background.suggestions = None;
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

    render_help_line(f, app, vertical_chunks[0]);
    render_tree_panel(f, app, horizontal_chunks[0], Section::Workspace);
    render_tree_panel(f, app, horizontal_chunks[1], Section::Library);

    // Search box
    let search_text = format!("{}_", app.search_query);
    let search_style = if app.search_query.is_empty() {
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
                .border_style(search_style)
                .title("Search"),
        );
    f.render_widget(search, vertical_chunks[2]);

    // Status/log message (at bottom)
    let status = Paragraph::new(app.last_log_message.as_str())
        .style(Style::default().fg(Color::Gray))
        .alignment(Alignment::Left);
    f.render_widget(status, vertical_chunks[3]);
}

/// Render the key-binding help line at the top of the screen
fn render_help_line(f: &mut Frame, app: &App, area: Rect) {
    let enter_action = match app.active_section {
        Section::Workspace => " open  ",
        Section::Library => " restore  ",
    };

    let mut bindings: Vec<(&str, Color, &str)> = vec![
        ("Tab", Color::Cyan, " switch  "),
        ("↑/↓", Color::Cyan, " navigate  "),
        ("←/→", Color::Cyan, " expand/collapse  "),
        ("Enter", Color::Green, enter_action),
    ];
    if app.active_section == Section::Workspace {
        bindings.push(("Ctrl+D", Color::Yellow, " drop  "));
    }
    bindings.push(("Ctrl+A", Color::Magenta, " clone  "));
    bindings.push(("Esc", Color::Red, " quit"));

    let mut help_spans = Vec::new();
    for (key, color, description) in bindings {
        help_spans.push(Span::styled(
            key,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        help_spans.push(Span::raw(description));
    }

    let help = Paragraph::new(Line::from(help_spans)).alignment(Alignment::Center);
    f.render_widget(help, area);
}

/// Render the workspace or library tree panel
fn render_tree_panel(f: &mut Frame, app: &App, area: Rect, section: Section) {
    // Account for: 2 for borders, 2 for highlight symbol ">> ", 1 for padding on right
    let available_width = area.width.saturating_sub(5) as usize;

    // Workspace repos show status icons and modification time; library repos show size
    type IdleMetadata = fn(&RepoInfo) -> String;
    let (items, state, title, show_status_icons, idle_metadata) = match section {
        Section::Workspace => (
            app.get_flattened_workspace(),
            &app.workspace_state,
            format!("Workspace ({})", app.count_workspace_repos()),
            true,
            (|repo| {
                repo.modification_time
                    .map(format_time_ago_verbose)
                    .unwrap_or_default()
            }) as IdleMetadata,
        ),
        Section::Library => (
            app.get_flattened_library(),
            &app.library_state,
            format!("Library ({})", app.count_library_repos()),
            false,
            (|repo| repo.size_bytes.map(format_size).unwrap_or_default()) as IdleMetadata,
        ),
    };

    let list_items: Vec<ListItem> = items
        .iter()
        .map(|(node, depth, _, full_path)| {
            tree_list_item(
                node,
                *depth,
                full_path,
                app,
                available_width,
                show_status_icons,
                idle_metadata,
            )
        })
        .collect();

    let list = List::new(list_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(if app.active_section == section {
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
    let mut list_state = ratatui::widgets::ListState::default();
    list_state.select(state.selected());
    f.render_stateful_widget(list, area, &mut list_state);
}

/// Render a single tree node as a list item
fn tree_list_item<'a>(
    node: &'a TreeNode,
    depth: usize,
    full_path: &str,
    app: &App,
    available_width: usize,
    show_status_icons: bool,
    idle_metadata: fn(&RepoInfo) -> String,
) -> ListItem<'a> {
    let mut spans = vec![];

    // Add tree structure indicators
    if depth > 0 {
        spans.push(Span::raw("  ".repeat(depth)));
    }

    // Add expand/collapse indicator
    if !node.children.is_empty() {
        let is_git_submodule = node
            .repo_info
            .as_ref()
            .map(|r| r.is_submodule)
            .unwrap_or(false);
        let indicator = match (is_git_submodule, node.expanded) {
            (true, true) => "◇ ",
            (true, false) => "◆ ",
            (false, true) => "▼ ",
            (false, false) => "▶ ",
        };
        spans.push(Span::styled(indicator, Style::default().fg(Color::Cyan)));
    } else if depth > 0 {
        spans.push(Span::raw("  "));
    }

    // Add status icon for repos only
    if show_status_icons && let Some(ref repo) = node.repo_info {
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
        } else if repo.is_clean {
            spans.push(Span::styled("✓ ", Style::default().fg(Color::Green)));
        } else {
            spans.push(Span::styled("* ", Style::default().fg(Color::Yellow)));
        }
    }

    // Add name with search highlighting
    if !app.search_query.is_empty() {
        // For directory nodes, check if the search query contains this directory as a path component
        let should_highlight_dir =
            node.repo_info.is_none() && app.search_query.contains(&format!("{}/", node.name));

        // Try to match against the full path for this node
        let indices = app
            .matcher
            .fuzzy_indices(full_path, &app.search_query)
            .map(|(_, indices)| indices);

        spans.extend(render_highlighted_name(
            &node.name,
            full_path,
            should_highlight_dir,
            indices,
        ));
    } else {
        spans.push(Span::raw(&node.name));
    }

    // Add right-aligned operation status or idle metadata for repos
    if let Some(ref repo) = node.repo_info {
        // Current text width (accounting for unicode characters)
        let text_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();

        let (status_text, status_color) = match &repo.operation_status {
            RepoOperationStatus::None => (idle_metadata(repo), Color::DarkGray),
            RepoOperationStatus::Dropping => ("dropping...".to_string(), Color::Yellow),
            RepoOperationStatus::Restoring => ("restoring...".to_string(), Color::Cyan),
            RepoOperationStatus::Success => ("done".to_string(), Color::Green),
            RepoOperationStatus::Failed(err) => (format!("failed: {}", err), Color::Red),
        };

        spans.extend(render_metadata_span(
            text_width,
            available_width,
            status_text,
            status_color,
        ));
    }

    ListItem::new(Line::from(spans))
}

fn render_clone_repo_dialog(f: &mut Frame, app: &App) {
    // Create a centered dialog
    let area = f.area();
    let dialog_width = area.width.min(80);
    let dialog_height = area.height.min(20);

    let dialog_area = Rect {
        x: (area.width - dialog_width) / 2,
        y: (area.height - dialog_height) / 2,
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
    let filtered_suggestions = app.filtered_suggestions();
    let suggestion_items: Vec<ListItem> = filtered_suggestions
        .iter()
        .map(|s| ListItem::new(*s))
        .collect();

    let suggestions_title = if app.suggestions_loading {
        format!("Suggestions ({}) - loading...", filtered_suggestions.len())
    } else {
        format!("Suggestions ({})", filtered_suggestions.len())
    };
    let suggestions = List::new(suggestion_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(suggestions_title),
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

/// Scan a single workspace repository, returning it and any submodules
fn scan_workspace_repo(workspace_path: &str, path: PathBuf) -> Vec<RepoInfo> {
    let display_name = path
        .strip_prefix(workspace_path)
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

    let mut infos = vec![RepoInfo {
        path: path.clone(),
        display_name: display_name.clone(),
        is_clean,
        modification_time,
        size_bytes: None, // Size not computed for workspace repos to save time
        operation_status: RepoOperationStatus::None,
        is_submodule: false,
        submodule_initialized: false,
        parent_repo_path: None,
    }];

    // Find and add submodules
    if let Ok(submodules) = crate::find_submodules_in_repo(&path) {
        for submodule in submodules {
            let submodule_display_name = if display_name.is_empty() {
                submodule.path.display().to_string()
            } else {
                format!("{}/{}", display_name, submodule.path.display())
            };

            infos.push(RepoInfo {
                path: path.join(&submodule.path),
                display_name: submodule_display_name,
                is_clean: true, // Submodule status computed separately
                modification_time: None,
                size_bytes: None,
                operation_status: RepoOperationStatus::None,
                is_submodule: true,
                submodule_initialized: submodule.initialized,
                parent_repo_path: Some(path.clone()),
            });
        }
    }

    infos
}

/// Scan a single library repository for its metadata
fn scan_library_repo(library_path: &str, repo_path: String) -> RepoInfo {
    let full_path = PathBuf::from(library_path).join(&repo_path);
    RepoInfo {
        modification_time: get_repo_modification_time(&full_path).ok(),
        size_bytes: get_repo_size(&full_path).ok(),
        path: full_path,
        display_name: repo_path,
        is_clean: true, // Library repos are always clean
        operation_status: RepoOperationStatus::None,
        is_submodule: false,
        submodule_initialized: false,
        parent_repo_path: None,
    }
}

/// Enumerate and scan all workspace and library repositories on worker
/// threads, streaming results to the UI thread. Runs on a background thread;
/// exits early if the receiver is dropped.
fn scan_all_repos(workspace: &Workspace, tx: mpsc::Sender<LoadEvent>) {
    enum ScanTask {
        Workspace(PathBuf),
        Library(String),
    }

    let workspace_paths = find_git_repositories(Path::new(&workspace.path)).unwrap_or_default();
    let library_paths = workspace.list_library().unwrap_or_default();
    let library_path = workspace.library_path();

    if tx
        .send(LoadEvent::Total(
            workspace_paths.len() + library_paths.len(),
        ))
        .is_err()
    {
        return;
    }

    let tasks: Mutex<Vec<ScanTask>> = Mutex::new(
        workspace_paths
            .into_iter()
            .map(ScanTask::Workspace)
            .chain(library_paths.into_iter().map(ScanTask::Library))
            .collect(),
    );

    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(8);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let tasks = &tasks;
            let workspace_path = workspace.path.as_str();
            let library_path = library_path.as_str();
            scope.spawn(move || {
                loop {
                    let task = tasks.lock().unwrap().pop();
                    let Some(task) = task else {
                        break;
                    };
                    let event = match task {
                        ScanTask::Workspace(path) => {
                            LoadEvent::Workspace(scan_workspace_repo(workspace_path, path))
                        }
                        ScanTask::Library(repo_path) => {
                            LoadEvent::Library(scan_library_repo(library_path, repo_path))
                        }
                    };
                    if tx.send(event).is_err() {
                        break;
                    }
                }
            });
        }
    });
}

/// Get the configured GitHub hostname from gh CLI
fn get_github_hostname() -> String {
    if let Ok(output) = std::process::Command::new("gh")
        .args(["auth", "status", "--active", "--json", "hosts"])
        .output()
        && output.status.success()
        && let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        && let Some(hosts) = json.get("hosts").and_then(|h| h.as_object())
        && let Some(hostname) = hosts.keys().next()
    {
        return hostname.clone();
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
