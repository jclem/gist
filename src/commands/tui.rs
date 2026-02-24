use std::collections::HashSet;
use std::io::{self, Write};
use std::path::PathBuf;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;
use syntect::highlighting::{self, ThemeSet};
use syntect::parsing::SyntaxSet;
use tokio::sync::mpsc;
use tui_term::widget::PseudoTerminal;

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

struct PtyEditor {
    master: Box<dyn MasterPty + Send>,
    pty_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    parser: vt100::Parser,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    filename: String,
    gist_id: String,
    tmp_path: PathBuf,
    original_content: String,
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
    /// Embedded PTY editor state
    pty_editor: Option<PtyEditor>,
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
            pty_editor: None,
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

/// Convert a crossterm KeyEvent into raw bytes suitable for writing to a PTY.
fn key_event_to_bytes(key: &crossterm::event::KeyEvent) -> Vec<u8> {
    let mods = key.modifiers;
    match key.code {
        KeyCode::Char(c) => {
            if mods.contains(KeyModifiers::CONTROL) {
                // Ctrl+a = 0x01, Ctrl+z = 0x1a
                let byte = (c.to_ascii_lowercase() as u8).wrapping_sub(b'a').wrapping_add(1);
                if byte <= 26 {
                    return vec![byte];
                }
            }
            if mods.contains(KeyModifiers::ALT) {
                let mut bytes = vec![0x1b];
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                return bytes;
            }
            let mut buf = [0u8; 4];
            c.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::F(n) => match n {
            1 => vec![0x1b, b'O', b'P'],
            2 => vec![0x1b, b'O', b'Q'],
            3 => vec![0x1b, b'O', b'R'],
            4 => vec![0x1b, b'O', b'S'],
            5 => vec![0x1b, b'[', b'1', b'5', b'~'],
            6 => vec![0x1b, b'[', b'1', b'7', b'~'],
            7 => vec![0x1b, b'[', b'1', b'8', b'~'],
            8 => vec![0x1b, b'[', b'1', b'9', b'~'],
            9 => vec![0x1b, b'[', b'2', b'0', b'~'],
            10 => vec![0x1b, b'[', b'2', b'1', b'~'],
            11 => vec![0x1b, b'[', b'2', b'3', b'~'],
            12 => vec![0x1b, b'[', b'2', b'4', b'~'],
            _ => vec![],
        },
        _ => vec![],
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

/// Compute the content pane area (the inner area where the editor renders).
fn content_pane_size(terminal_size: Rect) -> (u16, u16) {
    let [main_area, _] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(terminal_size);
    let [_, content_area] =
        Layout::horizontal([Constraint::Length(40), Constraint::Fill(1)]).areas(main_area);
    // Account for the block border (1 on each side)
    let rows = content_area.height.saturating_sub(2);
    let cols = content_area.width.saturating_sub(2);
    (rows, cols)
}

fn start_pty_editor(app: &mut App, content_area: Rect) -> Result<(), CliError> {
    let Some((gist_id, filename)) = app.resolve_edit_target() else {
        app.status = "No file selected to edit.".into();
        return Ok(());
    };

    let editor = std::env::var("EDITOR")
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| CliError::api("EDITOR is not set"))?;

    // Get current content
    let content = app
        .gists
        .iter()
        .find(|g| g.id == gist_id)
        .and_then(|g| g.files.get(&filename))
        .and_then(|f| f.content.clone())
        .unwrap_or_default();

    // Write to temp file preserving extension for editor syntax highlighting
    let ext = std::path::Path::new(&filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("txt");
    let tmp_path = std::env::temp_dir().join(format!("gist-edit-{}.{ext}", std::process::id()));
    std::fs::write(&tmp_path, &content).map_err(|e| CliError::Io {
        context: "failed to create temp file".into(),
        source: e,
    })?;

    // Size the pty to the content pane (minus block borders)
    let rows = content_area.height.saturating_sub(2).max(1);
    let cols = content_area.width.saturating_sub(2).max(1);

    let pty_system = native_pty_system();
    let pty_pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| CliError::api(format!("failed to open pty: {e}")))?;

    let mut cmd = CommandBuilder::new(&editor);
    cmd.arg(&tmp_path);

    let child = pty_pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| CliError::api(format!("failed to spawn editor: {e}")))?;

    // Drop the slave — we only interact through the master
    drop(pty_pair.slave);

    let mut reader = pty_pair
        .master
        .try_clone_reader()
        .map_err(|e| CliError::api(format!("failed to get pty reader: {e}")))?;
    let writer = pty_pair
        .master
        .take_writer()
        .map_err(|e| CliError::api(format!("failed to get pty writer: {e}")))?;

    // Spawn a thread to read pty output and send it through a channel
    let (pty_tx, pty_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if pty_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let parser = vt100::Parser::new(rows, cols, 0);

    app.pty_editor = Some(PtyEditor {
        master: pty_pair.master,
        pty_rx,
        writer,
        parser,
        child,
        filename,
        gist_id,
        tmp_path,
        original_content: content,
    });

    Ok(())
}

async fn finish_pty_editor(app: &mut App, client: &reqwest::Client) -> Result<(), CliError> {
    let editor = app.pty_editor.take().expect("no pty editor to finish");

    let new_content = std::fs::read_to_string(&editor.tmp_path).map_err(|e| CliError::Io {
        context: "failed to read temp file".into(),
        source: e,
    })?;
    let _ = std::fs::remove_file(&editor.tmp_path);

    if new_content == editor.original_content {
        app.status = "No changes.".into();
        return Ok(());
    }

    app.status = "Saving...".into();
    let updated =
        api::update_gist_file(client, &editor.gist_id, &editor.filename, &new_content).await?;
    app.fetched_detail.insert(editor.gist_id.clone());
    if let Some(existing) = app.gists.iter_mut().find(|g| g.id == editor.gist_id) {
        *existing = updated;
    }

    app.show_content(&editor.filename, &new_content);
    app.status = "Saved.".into();
    Ok(())
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    app: &mut App,
    client: &reqwest::Client,
    mut bg_rx: mpsc::Receiver<BgMessage>,
    bg_tx: mpsc::Sender<BgMessage>,
) -> Result<(), CliError> {
    let mut last_content_area = Rect::default();

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

        // Read pty output if editor is active
        if let Some(ref mut pty) = app.pty_editor {
            // Drain all available output from the reader thread
            while let Ok(bytes) = pty.pty_rx.try_recv() {
                pty.parser.process(&bytes);
            }

            // Check if child exited
            if let Some(_status) = pty.child.try_wait().ok().flatten() {
                finish_pty_editor(app, client).await?;
            }
        }

        terminal
            .draw(|frame| {
                render(frame, app);
                // Capture the content area for resize handling
                let area = frame.area();
                let [main_area, _] =
                    Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
                let [_, content_area] =
                    Layout::horizontal([Constraint::Length(40), Constraint::Fill(1)])
                        .areas(main_area);
                last_content_area = content_area;
            })
            .map_err(|e| CliError::Io {
                context: "failed to draw frame".into(),
                source: e,
            })?;

        // Yield to let spawned tasks (e.g. background gist loading) run
        tokio::task::yield_now().await;

        if !event::poll(std::time::Duration::from_millis(16)).map_err(|e| CliError::Io {
            context: "failed to poll events".into(),
            source: e,
        })? {
            continue;
        }

        let ev = event::read().map_err(|e| CliError::Io {
            context: "failed to read event".into(),
            source: e,
        })?;

        // Handle resize events for the pty editor
        if let Event::Resize(_, _) = &ev {
            if let Some(ref mut pty) = app.pty_editor {
                let (rows, cols) = content_pane_size(terminal.get_frame().area());
                let rows = rows.max(1);
                let cols = cols.max(1);
                let _ = pty.master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                pty.parser.screen_mut().set_size(rows, cols);
            }
        }

        let Event::Key(key) = ev else {
            continue;
        };

        if key.kind != KeyEventKind::Press {
            continue;
        }

        // If pty editor is active, forward all keys to it
        if app.pty_editor.is_some() {
            let bytes = key_event_to_bytes(&key);
            if !bytes.is_empty() {
                if let Some(ref mut pty) = app.pty_editor {
                    let _ = pty.writer.write_all(&bytes);
                    let _ = pty.writer.flush();
                }
            }
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
                start_pty_editor(app, last_content_area)?;
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
    // If pty editor is active, render the terminal widget
    if let Some(ref pty) = app.pty_editor {
        let block = Block::default()
            .title(format!(" {} (editing) ", pty.filename))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow));

        let pseudo_term = PseudoTerminal::new(pty.parser.screen()).block(block);
        frame.render_widget(pseudo_term, area);
        return;
    }

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
    let text = if app.pty_editor.is_some() {
        " editing — save & quit from your editor to return".to_string()
    } else if !app.status.is_empty() {
        app.status.clone()
    } else {
        " j/k: navigate  h/l: collapse/expand  tab: switch pane  o: open  n: new  e: edit  d: delete  r: refresh  q: quit".to_string()
    };

    let style = if app.pty_editor.is_some() {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if app.confirm_delete.is_some() {
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

