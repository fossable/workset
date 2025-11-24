use crate::Workspace;
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::io;

use crate::remote::ListRepos;

pub struct App {
    repos: Vec<String>,
    filtered_repos: Vec<String>,
    state: ListState,
    search_query: String,
    mode: InputMode,
}

enum InputMode {
    Normal,
    Searching,
}

impl App {
    pub fn new(repos: Vec<String>) -> Self {
        let mut state = ListState::default();
        if !repos.is_empty() {
            state.select(Some(0));
        }

        Self {
            filtered_repos: repos.clone(),
            repos,
            state,
            search_query: String::new(),
            mode: InputMode::Normal,
        }
    }

    fn filter_repos(&mut self) {
        if self.search_query.is_empty() {
            self.filtered_repos = self.repos.clone();
        } else {
            self.filtered_repos = self
                .repos
                .iter()
                .filter(|r| r.to_lowercase().contains(&self.search_query.to_lowercase()))
                .cloned()
                .collect();
        }

        // Reset selection to first item
        if !self.filtered_repos.is_empty() {
            self.state.select(Some(0));
        } else {
            self.state.select(None);
        }
    }

    fn next(&mut self) {
        if self.filtered_repos.is_empty() {
            return;
        }

        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.filtered_repos.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn previous(&mut self) {
        if self.filtered_repos.is_empty() {
            return;
        }

        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.filtered_repos.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn selected_repo(&self) -> Option<String> {
        self.state
            .selected()
            .and_then(|i| self.filtered_repos.get(i).cloned())
    }
}

pub fn run_tui(workspace: &Workspace) -> Result<Option<String>> {
    // Collect repositories from all configured remotes
    let mut all_repos = Vec::new();

    for remote in &workspace.remotes {
        match remote.list_repo_paths() {
            Ok(repos) => all_repos.extend(repos),
            Err(e) => eprintln!("Warning: Failed to fetch repos from remote: {}", e),
        }
    }

    if all_repos.is_empty() {
        anyhow::bail!("No repositories found in configured remotes");
    }

    // Sort repositories
    all_repos.sort();
    all_repos.dedup();

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app and run
    let app = App::new(all_repos);
    let result = run_app(&mut terminal, app);

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    mut app: App,
) -> Result<Option<String>> {
    loop {
        terminal.draw(|f| ui(f, &mut app))?;

        if let Event::Key(key) = event::read()? {
            match app.mode {
                InputMode::Normal => match key.code {
                    KeyCode::Char('q') => return Ok(None),
                    KeyCode::Char('/') => {
                        app.mode = InputMode::Searching;
                    }
                    KeyCode::Down | KeyCode::Char('j') => app.next(),
                    KeyCode::Up | KeyCode::Char('k') => app.previous(),
                    KeyCode::Enter => return Ok(app.selected_repo()),
                    _ => {}
                },
                InputMode::Searching => match key.code {
                    KeyCode::Enter => {
                        app.mode = InputMode::Normal;
                    }
                    KeyCode::Esc => {
                        app.mode = InputMode::Normal;
                        app.search_query.clear();
                        app.filter_repos();
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
            }
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(f.area());

    // Search bar
    let search_style = match app.mode {
        InputMode::Searching => Style::default().fg(Color::Yellow),
        InputMode::Normal => Style::default(),
    };

    let search_text = if matches!(app.mode, InputMode::Searching) {
        format!("Search: {}_", app.search_query)
    } else {
        format!("Search: {} (press '/' to search)", app.search_query)
    };

    let search = Paragraph::new(search_text)
        .style(search_style)
        .block(Block::default().borders(Borders::ALL).title("Filter"));
    f.render_widget(search, chunks[0]);

    // Repository list
    let items: Vec<ListItem> = app
        .filtered_repos
        .iter()
        .map(|repo| ListItem::new(Line::from(vec![Span::raw(repo)])))
        .collect();

    let items = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(
            "Repositories ({}/{})",
            app.filtered_repos.len(),
            app.repos.len()
        )))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(">> ");

    f.render_stateful_widget(items, chunks[1], &mut app.state);

    // Help text
    let help = Paragraph::new("↑/k: up | ↓/j: down | /: search | Enter: select | q: quit")
        .block(Block::default().borders(Borders::ALL).title("Help"));
    f.render_widget(help, chunks[2]);
}
