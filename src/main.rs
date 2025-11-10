use std::{
    collections::VecDeque,
    fs, io,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use chrono::{Local, Timelike};
use crossterm::{
    event::{self, Event as CEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use humansize::{format_size, DECIMAL};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame, Terminal,
};
use rayon::prelude::*;
use thousands::Separable;
use walkdir::WalkDir;

// ====== Data types ======

#[derive(Debug, Clone)]
struct DirStats {
    path: PathBuf,
    total_bytes: u128,
    file_count: u64,
    dir_count: u64,
    // last_scanned: Instant,
}

#[derive(Debug)]
enum Msg {
    RecomputeNow, // manual or scheduled refresh
    Tick,         // UI timer tick
    #[allow(dead_code)]
    Error(String), // error message for the log pane
    ScanFinished(Vec<DirStats>), // new results
    DeleteFinished(PathBuf, Result<(), String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    Normal,
    ConfirmDelete(PathBuf),
}

// ====== App state ======

struct App {
    cwd: PathBuf,
    selected: usize,
    entries: Vec<DirStats>,
    messages: VecDeque<String>,
    last_error: Option<String>,
    last_scan_started: Option<Instant>,
    is_scanning: bool,
    mode: Mode,
}

impl App {
    fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            selected: 0,
            entries: Vec::new(),
            messages: VecDeque::with_capacity(200),
            last_error: None,
            last_scan_started: None,
            is_scanning: false,
            mode: Mode::Normal,
        }
    }

    fn log<S: Into<String>>(&mut self, s: S) {
        if self.messages.len() == self.messages.capacity() {
            self.messages.pop_front();
        }
        self.messages.push_back(s.into());
    }

    fn selected_entry(&self) -> Option<&DirStats> {
        self.entries.get(self.selected)
    }

    fn set_entries(&mut self, mut list: Vec<DirStats>) {
        list.sort_by(|a, b| b.total_bytes.cmp(&a.total_bytes));
        self.entries = list;
        if self.selected >= self.entries.len() && !self.entries.is_empty() {
            self.selected = self.entries.len() - 1;
        } else if self.entries.is_empty() {
            self.selected = 0;
        }
    }
}

// ====== Scanning logic ======

fn immediate_subdirs(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                .map(|e| e.path())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn compute_stats_for_dir(dir: &Path) -> DirStats {
    let mut total_bytes: u128 = 0;
    let mut file_count: u64 = 0;
    let mut dir_count: u64 = 0;

    for entry in WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            if let Ok(md) = entry.metadata() {
                total_bytes = total_bytes.saturating_add(md.len() as u128);
                file_count = file_count.saturating_add(1);
            }
        } else if entry.file_type().is_dir() {
            dir_count = dir_count.saturating_add(1);
        }
    }

    DirStats {
        path: dir.to_path_buf(),
        total_bytes,
        file_count,
        dir_count,
        // last_scanned: Instant::now(),
    }
}

fn spawn_scan_thread(cwd: PathBuf, tx: Sender<Msg>) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let child_dirs = immediate_subdirs(&cwd);
        let results: Vec<DirStats> = child_dirs
            .par_iter()
            .map(|d| compute_stats_for_dir(d))
            .collect();
        let _ = tx.send(Msg::ScanFinished(results));
    })
}

fn spawn_delete_thread(target: PathBuf, tx: Sender<Msg>) {
    thread::spawn(move || {
        // Safety: attempt to delete recursively; report back
        let res = match fs::remove_dir_all(&target) {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("{e}")),
        };
        let _ = tx.send(Msg::DeleteFinished(target, res));
        // Afterwards, trigger a rescan so UI updates
        let _ = tx.send(Msg::RecomputeNow);
    });
}

// ====== UI ======

fn draw_ui(f: &mut Frame, app: &App) {
    let root_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(f.size());

    let left = root_chunks[0];
    let right = root_chunks[1];

    draw_left(f, app, left);
    draw_right(f, app, right);

    // Modal confirm for deletion
    if let Mode::ConfirmDelete(path) = &app.mode {
        draw_confirm_modal(f, path);
    }
}

fn draw_left(f: &mut Frame, app: &App, area: Rect) {
    let title = format!(
        "Directories under {}{}",
        app.cwd.display(),
        if app.is_scanning {
            "  [scanning…]"
        } else {
            ""
        }
    );

    let items: Vec<ListItem> = app
        .entries
        .iter()
        .map(|ds| {
            let name = ds
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("<unknown>");
            let size = format_size(ds.total_bytes as u64, DECIMAL);
            let files = ds.file_count.separate_with_spaces();
            let line = format!("{name:<30}  {size:>10}  ({files} files)");
            ListItem::new(Line::from(Span::raw(line)))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    f.render_stateful_widget(list, area, &mut list_state(app));
}

fn list_state(app: &App) -> ratatui::widgets::ListState {
    let mut st = ratatui::widgets::ListState::default();
    if !app.entries.is_empty() {
        st.select(Some(app.selected));
    }
    st
}

fn convert_bytes(bytes: u128) -> (f64, String) {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    const TB: f64 = GB * 1024.0;

    let bytes_f64 = bytes as f64;

    if bytes_f64 >= TB {
        (bytes_f64 / TB, "TB".to_string())
    } else if bytes_f64 >= GB {
        (bytes_f64 / GB, "GB".to_string())
    } else if bytes_f64 >= MB {
        (bytes_f64 / MB, "MB".to_string())
    } else if bytes_f64 >= KB {
        (bytes_f64 / KB, "KB".to_string())
    } else {
        (bytes_f64, "Bytes".to_string())
    }
}

fn draw_right(f: &mut Frame, app: &App, area: Rect) {
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(9), // Info
            Constraint::Min(6),    // Messages (grows with vertical space)
            Constraint::Length(9), // Help
        ])
        .split(area);

    // Info about selected directory
    let info = if let Some(sel) = app.selected_entry() {
        let name = sel
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>");
        // let size = format_size(sel.total_bytes as u64, DECIMAL);
        let size = convert_bytes(sel.total_bytes).0.round();
        let size_end = convert_bytes(sel.total_bytes).1;
        let info_lines = vec![
            Line::from(vec![
                Span::raw("Selected: "),
                Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::from(format!("Path: {}", sel.path.display())),
            Line::from(format!("Total size: {size} {size_end}")),
            Line::from(format!("Files: {}", sel.file_count.separate_with_spaces())),
            Line::from(format!("Dirs: {}", sel.dir_count.separate_with_spaces())),
            Line::from(""),
        ];
        Paragraph::new(info_lines)
            .block(Block::default().borders(Borders::ALL).title("Info"))
            .wrap(Wrap { trim: true })
    } else if app.is_scanning {
        Paragraph::new("Scanning.").block(Block::default().borders(Borders::ALL).title("Info"))
    } else {
        Paragraph::new("No subdirectories in this location.")
            .block(Block::default().borders(Borders::ALL).title("Info"))
    };
    f.render_widget(info, right_chunks[0]);

    // Messages / Errors
    let mut lines: Vec<Line> = app
        .messages
        .iter()
        .rev()
        .take(200)
        .map(|m| Line::from(m.as_str()))
        .collect();
    if let Some(err) = &app.last_error {
        lines.insert(
            0,
            Line::from(Span::styled(
                format!("ERROR: {err}"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )),
        );
    }
    let msg = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Messages & Errors"),
        )
        .wrap(Wrap { trim: true });
    f.render_widget(msg, right_chunks[1]);

    // Help / Keys
    let help = Paragraph::new(vec![
        Line::from("Keys:"),
        Line::from("  ↑/↓       — Move selection"),
        Line::from("  Enter     — Drill into selected directory"),
        Line::from("  Backspace — Go to parent directory"),
        Line::from("  d         — Delete selected directory (asks for confirmation)"),
        Line::from("  r         — Refresh now"),
        Line::from("  q         — Quit"),
    ])
    .block(Block::default().borders(Borders::ALL).title("Help"));
    f.render_widget(help, right_chunks[2]);
}

fn draw_confirm_modal(f: &mut Frame, target: &Path) {
    // Centered box
    let area = f.size();
    let w = (area.width as f32 * 0.7) as u16;
    let h = 7u16;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    let msg = vec![
        Line::from(Span::styled(
            "WARNING: This will permanently and recursively delete the selected directory.",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("Target: {}", target.display())),
        Line::from(""),
        Line::from("Press 'y' to confirm, 'n' or Esc to cancel."),
    ];

    f.render_widget(Clear, popup);
    let block = Paragraph::new(msg).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Confirm Deletion"),
    );
    f.render_widget(block, popup);
}

// ====== Event loop ======

fn main() -> Result<()> {
    let cwd = std::env::current_dir().context("Unable to get current directory")?;
    let mut app = App::new(cwd.clone());

    // Channels
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = mpsc::channel();

    // UI timer (tick) thread
    {
        let tx = tx.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_millis(200));
            let _ = tx.send(Msg::Tick);
        });
    }

    // Periodic rescanner (every 15 seconds)
    {
        let tx = tx.clone();
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(60 * 15));
            let _ = tx.send(Msg::RecomputeNow);
        });
    }

    // Kick off initial scan
    {
        let tx = tx.clone();
        let _ = tx.send(Msg::RecomputeNow);
    }

    // TUI setup
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Main loop
    let result = run_loop(&mut terminal, &mut app, rx, tx.clone());

    // Restore terminal
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    // Return result
    if let Err(e) = result {
        eprintln!("Fatal error: {e:?}");
        std::process::exit(1);
    }
    Ok(())
}

fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    rx: Receiver<Msg>,
    tx: Sender<Msg>,
) -> Result<()> {
    loop {
        terminal.draw(|f| draw_ui(f, app))?;

        // Poll keyboard with small timeout so we can also process messages
        if event::poll(Duration::from_millis(50))? {
            if let CEvent::Key(key) = event::read()? {
                if handle_key(key, app, &tx)? {
                    // true => quit
                    return Ok(());
                }
            }
        }

        // Drain messages
        while let Ok(msg) = rx.try_recv() {
            match msg {
                Msg::Tick => { /* no-op */ }
                Msg::RecomputeNow => {
                    if !app.is_scanning {
                        let now = Local::now();

                        // Extract hours, minutes, and seconds
                        let hour = now.hour();
                        let minute = now.minute();
                        // let second = now.second();
                        let now = format!("{hour}:{minute}");

                        app.log(format!("{now} - scan started "));
                        app.is_scanning = true;
                        app.last_scan_started = Some(Instant::now());
                        let _ = spawn_scan_thread(app.cwd.clone(), tx.clone());
                    }
                }
                Msg::Error(e) => {
                    app.last_error = Some(e.clone());
                    app.log(format!("Error: {e}"));
                }
                Msg::ScanFinished(list) => {
                    app.is_scanning = false;
                    app.set_entries(list);
                    if let Some(started) = app.last_scan_started.take() {
                        let elapsed = started.elapsed().as_secs();
                        let now = Local::now();

                        // Extract hours, minutes, and seconds
                        let hour = now.hour();
                        let minute = now.minute();
                        // let second = now.second();
                        let now = format!("{hour}:{minute}");

                        app.log(format!("{now} - scan completed ({elapsed}s)"));
                    } else {
                        app.log("Scan completed");
                    }
                }
                Msg::DeleteFinished(path, res) => match res {
                    Ok(()) => app.log(format!("Deleted: {}", path.display())),
                    Err(e) => {
                        app.last_error = Some(format!("Failed to delete {}: {e}", path.display()));
                        app.log(format!("Failed to delete {}: {e}", path.display()));
                    }
                },
            }
        }
    }
}

fn handle_key(key: KeyEvent, app: &mut App, tx: &Sender<Msg>) -> Result<bool> {
    if key.kind != KeyEventKind::Press {
        return Ok(false);
    }
    match &app.mode {
        Mode::Normal => match (key.code, key.modifiers) {
            (KeyCode::Char('q'), _) => return Ok(true),

            // Refresh
            (KeyCode::Char('r'), _) => {
                let _ = tx.send(Msg::RecomputeNow);
            }

            // Move selection
            (KeyCode::Up, KeyModifiers::NONE) => {
                if !app.entries.is_empty() {
                    app.selected = app.selected.saturating_sub(1);
                }
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                if !app.entries.is_empty() {
                    app.selected = (app.selected + 1).min(app.entries.len().saturating_sub(1));
                }
            }

            // Drill in
            (KeyCode::Enter, _) => {
                if let Some(sel) = app.selected_entry() {
                    app.cwd = sel.path.clone();
                    app.selected = 0;
                    app.log(format!("Entered {}", app.cwd.display()));
                    let _ = tx.send(Msg::RecomputeNow);
                }
            }

            // Go up to parent
            (KeyCode::Backspace, _) => {
                if let Some(parent) = app.cwd.parent() {
                    app.cwd = parent.to_path_buf();
                    app.selected = 0;
                    app.log(format!("Up to {}", app.cwd.display()));
                    let _ = tx.send(Msg::RecomputeNow);
                } else {
                    app.log("Already at filesystem root");
                }
            }

            // Delete selected directory (ask confirmation)
            (KeyCode::Char('d'), _) => {
                if let Some(sel) = app.selected_entry() {
                    app.mode = Mode::ConfirmDelete(sel.path.clone());
                }
            }

            _ => {}
        },

        Mode::ConfirmDelete(target) => match (key.code, key.modifiers) {
            (KeyCode::Char('y'), _) => {
                let target = target.clone();
                let _ = tx.send(Msg::RecomputeNow); // kick off scan after deletion completes too
                spawn_delete_thread(target.clone(), tx.clone());
                // Exit modal
                app.mode = Mode::Normal;
            }
            (KeyCode::Char('n'), _) | (KeyCode::Esc, _) => {
                app.mode = Mode::Normal;
                app.log("Deletion cancelled");
            }
            _ => {}
        },
    }

    Ok(false)
}
