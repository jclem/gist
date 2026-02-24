use std::collections::HashSet;
use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;
use syntect::highlighting::{self, ThemeSet};
use syntect::parsing::SyntaxSet;
use tokio::sync::mpsc;

use crate::api::{self, Gist};
use crate::error::CliError;

enum BgMessage {
    /// Initial gist list loaded (metadata only, content may be truncated)
    GistList(Vec<Gist>),
    /// Full gist content fetched on-demand
    GistDetail(Gist),
    Error(String),
}

#[derive(Clone, PartialEq)]
enum EntryKind {
    Gist { id: String, public: bool },
    File { gist_id: String, filename: String },
}

struct Entry {
    label: String,
    kind: EntryKind,
    indent: u16,
}

#[derive(PartialEq)]
enum Focus {
    Sidebar,
    Content,
}

struct App {
    gists: Vec<Gist>,
    entries: Vec<Entry>,
    selected: usize,
    expanded: HashSet<String>,
    focus: Focus,
    content_scroll: u16,
    content_hscroll: u16,
    status: String,
    loading: bool,
    confirm_delete: Option<String>,
    file_content: Option<(String, String)>,
    highlighted_lines: Vec<Line<'static>>,
    syntax_set: SyntaxSet,
    theme: highlighting::Theme,
    /// Gist IDs we've already fetched full detail for
    fetched_detail: HashSet<String>,
    /// (gist_id, filename) waiting for detail fetch to complete
    pending_file: Option<(String, String)>,
}

impl App {
    fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme = ThemeSet::load_defaults().themes["base16-eighties.dark"].clone();
        Self {
            gists: Vec::new(),
            entries: Vec::new(),
            selected: 0,
            expanded: HashSet::new(),
            focus: Focus::Sidebar,
            content_scroll: 0,
            content_hscroll: 0,
            status: String::new(),
            loading: true,
            confirm_delete: None,
            file_content: None,
            highlighted_lines: Vec::new(),
            syntax_set,
            theme,
            fetched_detail: HashSet::new(),
            pending_file: None,
        }
    }

    fn set_gists(&mut self, gists: Vec<Gist>) {
        self.gists = gists;
        self.loading = false;
        self.rebuild_entries();
        self.preview_selected();
    }

    fn rebuild_entries(&mut self) {
        self.entries.clear();

        let mut sorted_gists: Vec<&Gist> = self.gists.iter().collect();
        sorted_gists.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));

        for gist in sorted_gists {
            let mut filenames: Vec<&String> = gist.files.keys().collect();
            filenames.sort();

            let label = filenames
                .first()
                .map(|s| s.as_str())
                .unwrap_or("(empty)");
            let visibility = if gist.public { "public" } else { "secret" };
            let expanded = self.expanded.contains(&gist.id);
            let arrow = if expanded { "▼" } else { "▶" };

            self.entries.push(Entry {
                label: format!("{arrow} {label} ({visibility})"),
                kind: EntryKind::Gist {
                    id: gist.id.clone(),
                    public: gist.public,
                },
                indent: 0,
            });

            if expanded {
                for filename in &filenames {
                    self.entries.push(Entry {
                        label: filename.to_string(),
                        kind: EntryKind::File {
                            gist_id: gist.id.clone(),
                            filename: filename.to_string(),
                        },
                        indent: 2,
                    });
                }
            }
        }
    }

    fn toggle_expand(&mut self) {
        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };

        match &entry.kind {
            EntryKind::Gist { id, .. } => {
                let id = id.clone();
                if self.expanded.contains(&id) {
                    self.expanded.remove(&id);
                } else {
                    self.expanded.insert(id);
                }
                self.rebuild_entries();
            }
            EntryKind::File {
                gist_id, filename, ..
            } => {
                let gist_id = gist_id.clone();
                let filename = filename.clone();
                self.select_file(&gist_id, &filename);
            }
        }
    }

    /// Try to show file content. If we already have it, display immediately.
    /// If not, set pending_file so the event loop can fetch it.
    fn select_file(&mut self, gist_id: &str, filename: &str) {
        let content = self
            .gists
            .iter()
            .find(|g| g.id == gist_id)
            .and_then(|g| g.files.get(filename))
            .and_then(|f| f.content.clone());

        if let Some(content) = content {
            if !content.is_empty() || self.fetched_detail.contains(gist_id) {
                // We have content (or we fetched detail and it's genuinely empty)
                self.show_content(filename, &content);
                return;
            }
        }

        // Need to fetch full gist detail
        self.pending_file = Some((gist_id.to_string(), filename.to_string()));
        self.status = "Fetching content...".into();
    }

    fn show_content(&mut self, filename: &str, content: &str) {
        self.highlighted_lines =
            highlight_content(&self.syntax_set, &self.theme, filename, content);
        self.file_content = Some((filename.to_string(), content.to_string()));
        self.content_scroll = 0;
        self.content_hscroll = 0;
        self.pending_file = None;
    }

    /// Called when a full gist detail arrives. Merges it into our gist list
    /// and shows the pending file if it matches.
    fn apply_gist_detail(&mut self, gist: Gist) {
        let gist_id = gist.id.clone();
        self.fetched_detail.insert(gist_id.clone());

        // Replace the gist in our list with the full version
        if let Some(existing) = self.gists.iter_mut().find(|g| g.id == gist_id) {
            *existing = gist;
        }

        // If we were waiting for this gist, show the file now
        if let Some((pending_gist_id, pending_filename)) = self.pending_file.take() {
            if pending_gist_id == gist_id {
                let content = self
                    .gists
                    .iter()
                    .find(|g| g.id == gist_id)
                    .and_then(|g| g.files.get(&pending_filename))
                    .and_then(|f| f.content.clone())
                    .unwrap_or_default();
                self.show_content(&pending_filename, &content);
                self.status = String::new();
            }
        }
    }

    fn collapse_or_back(&mut self) {
        if self.focus == Focus::Content {
            self.focus = Focus::Sidebar;
            return;
        }

        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };

        match &entry.kind {
            EntryKind::Gist { id, .. } => {
                let id = id.clone();
                self.expanded.remove(&id);
                self.rebuild_entries();
            }
            EntryKind::File { gist_id, .. } => {
                let gist_id = gist_id.clone();
                self.expanded.remove(&gist_id);
                self.rebuild_entries();
                for (i, e) in self.entries.iter().enumerate() {
                    if let EntryKind::Gist { id, .. } = &e.kind {
                        if *id == gist_id {
                            self.selected = i;
                            break;
                        }
                    }
                }
            }
        }
    }

    fn move_up(&mut self) {
        if self.focus == Focus::Content {
            self.content_scroll = self.content_scroll.saturating_sub(1);
            return;
        }
        if self.selected > 0 {
            self.selected -= 1;
            self.preview_selected();
        }
    }

    fn move_down(&mut self) {
        if self.focus == Focus::Content {
            self.content_scroll = self.content_scroll.saturating_add(1);
            return;
        }
        if self.selected + 1 < self.entries.len() {
            self.selected += 1;
            self.preview_selected();
        }
    }

    /// Show content for the currently selected entry without changing focus.
    fn preview_selected(&mut self) {
        let Some(entry) = self.entries.get(self.selected) else {
            return;
        };

        let (gist_id, filename) = match &entry.kind {
            EntryKind::File { gist_id, filename } => (gist_id.clone(), filename.clone()),
            EntryKind::Gist { id, .. } => {
                // Preview the first file of the gist
                let Some(gist) = self.gists.iter().find(|g| g.id == *id) else {
                    return;
                };
                let mut filenames: Vec<&String> = gist.files.keys().collect();
                filenames.sort();
                let Some(first) = filenames.first() else {
                    return;
                };
                (id.clone(), first.to_string())
            }
        };

        self.select_file(&gist_id, &filename);
    }

    fn selected_gist_id(&self) -> Option<String> {
        let entry = self.entries.get(self.selected)?;
        match &entry.kind {
            EntryKind::Gist { id, .. } => Some(id.clone()),
            EntryKind::File { gist_id, .. } => Some(gist_id.clone()),
        }
    }

    /// Returns (gist_id, filename) for the file to edit.
    /// If focused on content pane, use the currently viewed file.
    /// If on a File entry, use that file.
    /// If on a Gist entry, use its first file.
    fn resolve_edit_target(&self) -> Option<(String, String)> {
        // If viewing content, edit that file
        if self.focus == Focus::Content {
            if let Some((filename, _)) = &self.file_content {
                // Find which gist this file belongs to
                for gist in &self.gists {
                    if gist.files.contains_key(filename) {
                        return Some((gist.id.clone(), filename.clone()));
                    }
                }
            }
            return None;
        }

        let entry = self.entries.get(self.selected)?;
        match &entry.kind {
            EntryKind::File { gist_id, filename } => {
                Some((gist_id.clone(), filename.clone()))
            }
            EntryKind::Gist { id, .. } => {
                let gist = self.gists.iter().find(|g| g.id == *id)?;
                let mut filenames: Vec<&String> = gist.files.keys().collect();
                filenames.sort();
                let filename = filenames.first()?;
                Some((id.clone(), filename.to_string()))
            }
        }
    }

    fn clamp_selection(&mut self) {
        if self.selected >= self.entries.len() && !self.entries.is_empty() {
            self.selected = self.entries.len() - 1;
        }
    }
}

pub async fn execute(client: &reqwest::Client) -> Result<(), CliError> {
    let mut app = App::new();

    terminal::enable_raw_mode().map_err(|e| CliError::Io {
        context: "failed to enable raw mode".into(),
        source: e,
    })?;

    crossterm::execute!(io::stderr(), EnterAlternateScreen).map_err(|e| CliError::Io {
        context: "failed to enter alternate screen".into(),
        source: e,
    })?;

    let backend = CrosstermBackend::new(io::stderr());
    let mut terminal = Terminal::new(backend).map_err(|e| CliError::Io {
        context: "failed to create terminal".into(),
        source: e,
    })?;

    let (tx, rx) = mpsc::channel(8);
    let load_client = client.clone();
    let list_tx = tx.clone();
    tokio::spawn(async move {
        match api::list_gists(&load_client).await {
            Ok(gists) => {
                let _ = list_tx.send(BgMessage::GistList(gists)).await;
            }
            Err(e) => {
                let _ = list_tx.send(BgMessage::Error(e.to_string())).await;
            }
        }
    });

    let result = run_loop(&mut terminal, &mut app, client, rx, tx).await;

    crossterm::execute!(io::stderr(), LeaveAlternateScreen).ok();
    terminal::disable_raw_mode().ok();

    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    app: &mut App,
    client: &reqwest::Client,
    mut bg_rx: mpsc::Receiver<BgMessage>,
    bg_tx: mpsc::Sender<BgMessage>,
) -> Result<(), CliError> {
    loop {
        // Check for background messages
        while let Ok(msg) = bg_rx.try_recv() {
            match msg {
                BgMessage::GistList(gists) => {
                    app.set_gists(gists);
                }
                BgMessage::GistDetail(gist) => {
                    app.apply_gist_detail(gist);
                }
                BgMessage::Error(e) => {
                    app.loading = false;
                    app.pending_file = None;
                    app.status = format!("Error: {e}");
                }
            }
        }

        // If there's a pending file fetch, kick it off
        if let Some((ref gist_id, _)) = app.pending_file {
            if !app.fetched_detail.contains(gist_id) {
                let fetch_id = gist_id.clone();
                app.fetched_detail.insert(fetch_id.clone());
                let fetch_client = client.clone();
                let fetch_tx = bg_tx.clone();
                tokio::spawn(async move {
                    match api::get_gist(&fetch_client, &fetch_id).await {
                        Ok(gist) => {
                            let _ = fetch_tx.send(BgMessage::GistDetail(gist)).await;
                        }
                        Err(e) => {
                            let _ = fetch_tx.send(BgMessage::Error(e.to_string())).await;
                        }
                    }
                });
            }
        }

        terminal
            .draw(|frame| render(frame, app))
            .map_err(|e| CliError::Io {
                context: "failed to draw frame".into(),
                source: e,
            })?;

        // Yield to let spawned tasks (e.g. background gist loading) run
        tokio::task::yield_now().await;

        if !event::poll(std::time::Duration::from_millis(50)).map_err(|e| CliError::Io {
            context: "failed to poll events".into(),
            source: e,
        })? {
            continue;
        }

        let ev = event::read().map_err(|e| CliError::Io {
            context: "failed to read event".into(),
            source: e,
        })?;

        let Event::Key(key) = ev else {
            continue;
        };

        if key.kind != KeyEventKind::Press {
            continue;
        }

        // Handle delete confirmation
        if let Some(gist_id) = app.confirm_delete.take() {
            match key.code {
                KeyCode::Char('y') => {
                    app.status = "Deleting...".into();
                    terminal
                        .draw(|frame| render(frame, app))
                        .map_err(|e| CliError::Io {
                            context: "failed to draw frame".into(),
                            source: e,
                        })?;

                    api::delete_gist(client, &gist_id).await?;
                    app.gists.retain(|g| g.id != gist_id);
                    app.fetched_detail.remove(&gist_id);
                    app.file_content = None;
                    app.rebuild_entries();
                    app.clamp_selection();
                    app.status = "Gist deleted.".into();
                }
                _ => {
                    app.status = String::new();
                }
            }
            continue;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => break,
            (KeyCode::Char('j'), _) | (KeyCode::Down, _) => app.move_down(),
            (KeyCode::Char('k'), _) | (KeyCode::Up, _) => app.move_up(),
            (KeyCode::Char('l'), KeyModifiers::NONE) => {
                if app.focus == Focus::Content {
                    app.content_hscroll = app.content_hscroll.saturating_add(32);
                } else {
                    app.toggle_expand();
                }
            }
            (KeyCode::Char('h'), KeyModifiers::NONE) => {
                if app.focus == Focus::Content {
                    app.content_hscroll = app.content_hscroll.saturating_sub(32);
                } else {
                    app.collapse_or_back();
                }
            }
            (KeyCode::Enter, _) | (KeyCode::Right, _) => {
                app.toggle_expand();
            }
            (KeyCode::Left, _) | (KeyCode::Esc, _) => {
                app.collapse_or_back();
            }
            (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
                app.focus = Focus::Sidebar;
            }
            (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                app.focus = Focus::Content;
            }
            (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
                app.focus = if app.focus == Focus::Sidebar {
                    Focus::Content
                } else {
                    Focus::Sidebar
                };
            }
            (KeyCode::Char('o'), _) => {
                if let Some(id) = app.selected_gist_id() {
                    if let Some(gist) = app.gists.iter().find(|g| g.id == id) {
                        let _ = std::process::Command::new("open")
                            .arg(&gist.html_url)
                            .spawn();
                    }
                }
            }
            (KeyCode::Char('e'), _) => {
                edit_gist_via_editor(terminal, app, client).await?;
            }
            (KeyCode::Char('n'), _) => {
                create_gist_via_editor(terminal, app, client).await?;
            }
            (KeyCode::Char('d'), _) => {
                if let Some(id) = app.selected_gist_id() {
                    app.confirm_delete = Some(id);
                    app.status = "Delete this gist? (y/n)".into();
                }
            }
            (KeyCode::Char('r'), _) => {
                app.loading = true;
                app.status = String::new();
                app.fetched_detail.clear();
                app.file_content = None;
                app.pending_file = None;

                let refresh_client = client.clone();
                let refresh_tx = bg_tx.clone();
                tokio::spawn(async move {
                    match api::list_gists(&refresh_client).await {
                        Ok(gists) => {
                            let _ = refresh_tx.send(BgMessage::GistList(gists)).await;
                        }
                        Err(e) => {
                            let _ = refresh_tx.send(BgMessage::Error(e.to_string())).await;
                        }
                    }
                });
            }
            (_, _) => {}
        }
    }

    Ok(())
}

fn render(frame: &mut ratatui::Frame, app: &App) {
    let area = frame.area();

    let [main_area, status_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);

    let [sidebar_area, content_area] =
        Layout::horizontal([Constraint::Length(40), Constraint::Fill(1)]).areas(main_area);

    render_sidebar(frame, app, sidebar_area);
    render_content(frame, app, content_area);
    render_status(frame, app, status_area);
}

fn render_sidebar(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let title = if app.loading {
        " Gists (loading...) "
    } else {
        " Gists "
    };

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(if app.focus == Focus::Sidebar {
            Style::default().fg(Color::Blue)
        } else {
            Style::default().fg(Color::DarkGray)
        });

    if app.entries.is_empty() {
        let msg = if app.loading {
            "Loading..."
        } else {
            "No gists found. Press 'n' to create one."
        };
        let paragraph = Paragraph::new(msg)
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, area);
        return;
    }

    let items: Vec<ListItem> = app
        .entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let indent = " ".repeat(entry.indent as usize);
            let style = if i == app.selected && app.focus == Focus::Sidebar {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                match &entry.kind {
                    EntryKind::Gist { .. } => Style::default().fg(Color::Cyan),
                    EntryKind::File { .. } => Style::default().fg(Color::White),
                }
            };

            ListItem::new(Line::from(vec![
                Span::raw(indent),
                Span::styled(&entry.label, style),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(app.selected));

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(Color::DarkGray));

    frame.render_stateful_widget(list, area, &mut state);
}

fn render_content(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let block = Block::default()
        .title(match &app.file_content {
            Some((filename, _)) => format!(" {filename} "),
            None => match &app.pending_file {
                Some((_, filename)) => format!(" {filename} (loading...) "),
                None => " Content ".to_string(),
            },
        })
        .borders(Borders::ALL)
        .border_style(if app.focus == Focus::Content {
            Style::default().fg(Color::Blue)
        } else {
            Style::default().fg(Color::DarkGray)
        });

    if app.pending_file.is_some() && app.file_content.is_none() {
        let paragraph = Paragraph::new("Loading...")
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, area);
        return;
    }

    if app.file_content.is_none() {
        let paragraph = Paragraph::new("Select a file to view its content.")
            .block(block)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(paragraph, area);
        return;
    }

    let lines: Vec<Line> = app.highlighted_lines.clone();
    let paragraph = Paragraph::new(lines)
        .block(block)
        .scroll((app.content_scroll, app.content_hscroll));

    frame.render_widget(paragraph, area);
}

fn highlight_content(
    syntax_set: &SyntaxSet,
    theme: &highlighting::Theme,
    filename: &str,
    content: &str,
) -> Vec<Line<'static>> {
    let syntax = syntax_set
        .find_syntax_for_file(filename)
        .ok()
        .flatten()
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text());

    let mut highlighter = syntect::easy::HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();

    for line in syntect::util::LinesWithEndings::from(content) {
        let ranges = highlighter
            .highlight_line(line, syntax_set)
            .unwrap_or_default();

        let spans: Vec<Span<'static>> = ranges
            .into_iter()
            .map(|(style, text)| {
                let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
                Span::styled(text.to_string(), Style::default().fg(fg))
            })
            .collect();

        lines.push(Line::from(spans));
    }

    lines
}

fn render_status(frame: &mut ratatui::Frame, app: &App, area: Rect) {
    let text = if !app.status.is_empty() {
        app.status.clone()
    } else {
        " j/k: navigate  h/l: collapse/expand  tab: switch pane  o: open  n: new  e: edit  d: delete  r: refresh  q: quit".to_string()
    };

    let style = if app.confirm_delete.is_some() {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if !app.status.is_empty() {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let paragraph = Paragraph::new(text).style(style);
    frame.render_widget(paragraph, area);
}

async fn create_gist_via_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    app: &mut App,
    client: &reqwest::Client,
) -> Result<(), CliError> {
    let editor = std::env::var("EDITOR")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| CliError::api("EDITOR is not set"))?;

    // Leave TUI
    crossterm::execute!(io::stderr(), LeaveAlternateScreen).ok();
    terminal::disable_raw_mode().ok();

    // Prompt for filename
    eprint!("Filename: ");
    let mut filename = String::new();
    io::stdin().read_line(&mut filename).map_err(|e| CliError::Io {
        context: "failed to read filename".into(),
        source: e,
    })?;
    let filename = filename.trim().to_string();

    if filename.is_empty() {
        // Re-enter TUI and bail
        terminal::enable_raw_mode().ok();
        crossterm::execute!(io::stderr(), EnterAlternateScreen).ok();
        terminal.clear().ok();
        app.status = "Cancelled.".into();
        return Ok(());
    }

    let tmp = std::env::temp_dir().join(&filename);
    std::fs::write(&tmp, "").map_err(|e| CliError::Io {
        context: "failed to create temp file".into(),
        source: e,
    })?;

    let status = std::process::Command::new(&editor)
        .arg(&tmp)
        .status()
        .map_err(|e| CliError::Io {
            context: format!("failed to launch editor ({editor})"),
            source: e,
        })?;

    // Re-enter TUI
    terminal::enable_raw_mode().map_err(|e| CliError::Io {
        context: "failed to re-enable raw mode".into(),
        source: e,
    })?;
    crossterm::execute!(io::stderr(), EnterAlternateScreen).map_err(|e| CliError::Io {
        context: "failed to re-enter alternate screen".into(),
        source: e,
    })?;
    terminal.clear().map_err(|e| CliError::Io {
        context: "failed to clear terminal".into(),
        source: e,
    })?;

    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        app.status = "Editor exited without saving.".into();
        return Ok(());
    }

    let content = std::fs::read_to_string(&tmp).map_err(|e| CliError::Io {
        context: "failed to read temp file".into(),
        source: e,
    })?;
    let _ = std::fs::remove_file(&tmp);

    if content.trim().is_empty() {
        app.status = "Empty content, gist not created.".into();
        return Ok(());
    }

    app.status = "Creating gist...".into();
    terminal
        .draw(|frame| render(frame, app))
        .map_err(|e| CliError::Io {
            context: "failed to draw frame".into(),
            source: e,
        })?;

    let gist = api::create_gist(client, &filename, &content, false, "").await?;
    let full = api::get_gist(client, &gist.id).await?;
    let gist_id = full.id.clone();
    app.fetched_detail.insert(gist_id.clone());
    app.gists.insert(0, full);
    app.expanded.insert(gist_id.clone());
    app.rebuild_entries();
    app.selected = 0;
    app.show_content(&filename, &content);
    app.status = format!("Created: {}", gist.html_url);

    Ok(())
}

async fn edit_gist_via_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    app: &mut App,
    client: &reqwest::Client,
) -> Result<(), CliError> {
    let Some((gist_id, filename)) = app.resolve_edit_target() else {
        app.status = "No file selected to edit.".into();
        return Ok(());
    };

    let editor = std::env::var("EDITOR")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| CliError::api("EDITOR is not set"))?;

    // Get current content — may need to fetch it first
    let current_content = app
        .gists
        .iter()
        .find(|g| g.id == gist_id)
        .and_then(|g| g.files.get(&filename))
        .and_then(|f| f.content.clone());

    let content = if let Some(c) = current_content {
        c
    } else {
        // Fetch full gist to get content
        let full = api::get_gist(client, &gist_id).await?;
        let c = full
            .files
            .get(&filename)
            .and_then(|f| f.content.clone())
            .unwrap_or_default();
        // Store it
        if let Some(existing) = app.gists.iter_mut().find(|g| g.id == gist_id) {
            *existing = full;
        }
        app.fetched_detail.insert(gist_id.clone());
        c
    };

    // Leave TUI
    crossterm::execute!(io::stderr(), LeaveAlternateScreen).ok();
    terminal::disable_raw_mode().ok();

    // Use the original extension so EDITOR gets syntax highlighting
    let ext = std::path::Path::new(&filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt");
    let tmp = std::env::temp_dir().join(format!("gist-edit-{}.{ext}", std::process::id()));

    std::fs::write(&tmp, &content).map_err(|e| CliError::Io {
        context: "failed to create temp file".into(),
        source: e,
    })?;

    let status = std::process::Command::new(&editor)
        .arg(&tmp)
        .status()
        .map_err(|e| CliError::Io {
            context: format!("failed to launch editor ({editor})"),
            source: e,
        })?;

    // Re-enter TUI
    terminal::enable_raw_mode().map_err(|e| CliError::Io {
        context: "failed to re-enable raw mode".into(),
        source: e,
    })?;
    crossterm::execute!(io::stderr(), EnterAlternateScreen).map_err(|e| CliError::Io {
        context: "failed to re-enter alternate screen".into(),
        source: e,
    })?;
    terminal.clear().map_err(|e| CliError::Io {
        context: "failed to clear terminal".into(),
        source: e,
    })?;

    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        app.status = "Editor exited without saving.".into();
        return Ok(());
    }

    let new_content = std::fs::read_to_string(&tmp).map_err(|e| CliError::Io {
        context: "failed to read temp file".into(),
        source: e,
    })?;
    let _ = std::fs::remove_file(&tmp);

    if new_content == content {
        app.status = "No changes.".into();
        return Ok(());
    }

    app.status = "Saving...".into();
    terminal
        .draw(|frame| render(frame, app))
        .map_err(|e| CliError::Io {
            context: "failed to draw frame".into(),
            source: e,
        })?;

    let updated = api::update_gist_file(client, &gist_id, &filename, &new_content).await?;
    app.fetched_detail.insert(gist_id.clone());
    if let Some(existing) = app.gists.iter_mut().find(|g| g.id == gist_id) {
        *existing = updated;
    }

    // Refresh the content pane if we were viewing this file
    app.show_content(&filename, &new_content);
    app.status = "Saved.".into();

    Ok(())
}
