//! Interactive browser using a model/update/view split.

use std::ffi::OsString;
use std::io::{self, Stdout};
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState, Wrap};

use crate::delete::Batch;
use crate::format::{SizeFormat, escape_name, format_count, format_number, format_size};
use crate::tree::{Dir, Flag, Order, SortBy};

/// Width of the usage bar.
const BAR_WIDTH: usize = 24;

pub type Refresh = Box<dyn FnMut() -> io::Result<Dir> + Send>;

/// What the browser is currently doing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Browsing,
    /// Typing a name filter.
    Filtering,
    Help,
    ConfirmDelete,
    Deleting,
    Message,
}

/// One rendered child line.
#[derive(Debug, Clone)]
pub struct Child {
    pub name: String,
    pub fs_name: OsString,
    pub size: i64,
    pub usage: i64,
    pub items: i64,
    pub flag: Flag,
    /// Index in `current.dirs` when this is a directory.
    pub dir_index: Option<usize>,
}

impl Child {
    fn shown_size(&self, apparent: bool) -> i64 {
        if apparent { self.size } else { self.usage }
    }
}

/// Browser state.
pub struct Model {
    tree: Dir,
    /// Indices from the root to the current directory.
    path: Vec<usize>,
    cursor: usize,
    sort: SortBy,
    order: Order,
    filter: String,
    mode: Mode,
    /// Report apparent size rather than disk usage.
    pub apparent: bool,
    pub size_format: SizeFormat,
    quit: bool,
    batch: Option<Batch>,
    confirmation: String,
    exit_after_delete: bool,
    apply_requested: bool,
    message: String,
    dialog_scroll: u16,
}

impl Model {
    pub fn new(tree: Dir) -> Self {
        Self {
            tree,
            path: Vec::new(),
            cursor: 0,
            sort: SortBy::Usage,
            order: Order::Descending,
            filter: String::new(),
            mode: Mode::Browsing,
            apparent: false,
            size_format: SizeFormat::Binary,
            quit: false,
            batch: None,
            confirmation: String::new(),
            exit_after_delete: false,
            apply_requested: false,
            message: String::new(),
            dialog_scroll: 0,
        }
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Current directory.
    pub fn current(&self) -> &Dir {
        let mut dir = &self.tree;
        for &index in &self.path {
            dir = &dir.dirs[index];
        }
        dir
    }

    /// Current directory path.
    pub fn current_path(&self) -> PathBuf {
        let mut path = PathBuf::from(self.tree.fs_name());
        let mut dir = &self.tree;
        for &index in &self.path {
            dir = &dir.dirs[index];
            path.push(dir.fs_name());
        }
        path
    }

    fn relative_path(&self) -> PathBuf {
        let mut path = PathBuf::new();
        let mut dir = &self.tree;
        for &index in &self.path {
            dir = &dir.dirs[index];
            path.push(dir.fs_name());
        }
        path
    }

    /// Sorted and filtered children.
    pub fn rows(&self) -> Vec<Child> {
        let dir = self.current();
        let needle = self.filter.to_lowercase();
        let sort = match self.sort {
            SortBy::Usage | SortBy::ApparentSize => SortBy::size(self.apparent),
            other => other,
        };

        dir.sorted(sort, self.order)
            .into_iter()
            .filter(|entry| needle.is_empty() || entry.name.to_lowercase().contains(&needle))
            .map(|entry| Child {
                name: entry.name.to_string(),
                fs_name: entry.fs_name.to_owned(),
                size: entry.size,
                usage: entry.usage,
                items: entry.items,
                flag: entry.flag,
                dir_index: entry.dir_index,
            })
            .collect()
    }

    /// Handles one key press.
    pub fn on_key(&mut self, key: KeyEvent) {
        match self.mode {
            Mode::Help => self.mode = Mode::Browsing,
            Mode::Filtering => self.filter_key(key),
            Mode::Browsing => self.browse_key(key),
            Mode::ConfirmDelete => self.confirm_key(key),
            Mode::Deleting => {}
            Mode::Message => match key.code {
                KeyCode::PageDown => self.dialog_scroll = self.dialog_scroll.saturating_add(5),
                KeyCode::PageUp => self.dialog_scroll = self.dialog_scroll.saturating_sub(5),
                _ => self.mode = Mode::Browsing,
            },
        }
    }

    fn browse_key(&mut self, key: KeyEvent) {
        let rows = self.rows();
        let last = rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('q') => self.request_delete(true),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.request_delete(true);
            }

            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = last,
            KeyCode::PageDown => self.cursor = (self.cursor + 10).min(last),
            KeyCode::PageUp => self.cursor = self.cursor.saturating_sub(10),

            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                self.descend(rows.get(self.cursor).and_then(|row| row.dir_index))
            }
            KeyCode::Left | KeyCode::Char('h') => self.ascend(),

            KeyCode::Char('s') => self.set_sort(SortBy::Usage),
            KeyCode::Char('n') => self.set_sort(SortBy::Name),
            KeyCode::Char('C') => self.set_sort(SortBy::ItemCount),
            KeyCode::Char('m') => self.set_sort(SortBy::Mtime),
            KeyCode::Char('a') => self.apparent = !self.apparent,
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(row) = rows.get(self.cursor) {
                    self.toggle_delete(row);
                }
            }
            KeyCode::Char('u') => {
                if let Some(batch) = &mut self.batch {
                    batch.clear();
                }
            }
            KeyCode::Char('x') => self.request_delete(false),

            KeyCode::Char('/') => {
                self.mode = Mode::Filtering;
                self.filter.clear();
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
    }

    fn toggle_delete(&mut self, row: &Child) {
        let path = self.relative_path().join(&row.fs_name);
        match &mut self.batch {
            Some(batch) => {
                let usage = if row.flag == Flag::HardLink {
                    0
                } else {
                    row.usage
                };
                if let Err(err) = batch.toggle(&path, usage) {
                    self.show_message(format!("Cannot mark entry: {err}"));
                }
            }
            None => self.show_message(
                "This browser is read-only; deletion is disabled (saved report or failed rescan)."
                    .into(),
            ),
        }
    }

    fn request_delete(&mut self, exiting: bool) {
        if self.batch.as_ref().is_some_and(|batch| batch.len() > 0) {
            self.confirmation.clear();
            self.dialog_scroll = 0;
            self.exit_after_delete = exiting;
            self.mode = Mode::ConfirmDelete;
        } else if exiting {
            self.quit = true;
        }
    }

    fn confirm_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.mode = Mode::Browsing,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.mode = Mode::Browsing;
            }
            KeyCode::PageDown => self.dialog_scroll = self.dialog_scroll.saturating_add(5),
            KeyCode::PageUp => self.dialog_scroll = self.dialog_scroll.saturating_sub(5),
            KeyCode::Backspace => {
                self.confirmation.pop();
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.confirmation.len() < 32 {
                    self.confirmation.push(c);
                }
            }
            KeyCode::Enter if self.confirmation == "DELETE" => {
                self.apply_requested = true;
                self.mode = Mode::Deleting;
            }
            _ => {}
        }
    }

    fn show_message(&mut self, message: String) {
        self.message = message;
        self.dialog_scroll = 0;
        self.mode = Mode::Message;
    }

    fn replace_tree(&mut self, tree: Dir) {
        let path = self.relative_path();
        self.tree = tree;
        self.path.clear();
        for name in path.components() {
            let Some(index) = self
                .current()
                .dirs
                .iter()
                .position(|dir| dir.fs_name() == name.as_os_str())
            else {
                break;
            };
            self.path.push(index);
        }
        self.cursor = self.cursor.min(self.rows().len().saturating_sub(1));
    }

    fn filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Browsing;
            }
            KeyCode::Enter => self.mode = Mode::Browsing,
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(c) => self.filter.push(c),
            _ => {}
        }
        self.cursor = self.cursor.min(self.rows().len().saturating_sub(1));
    }

    fn set_sort(&mut self, sort: SortBy) {
        if self.sort == sort {
            self.order = self.order.reversed();
        } else {
            self.sort = sort;
            self.order = Order::Descending;
        }
        self.cursor = 0;
    }

    fn descend(&mut self, index: Option<usize>) {
        if let Some(index) = index {
            self.path.push(index);
            self.cursor = 0;
            self.filter.clear();
        }
    }

    fn ascend(&mut self) {
        if let Some(leaving) = self.path.pop() {
            self.filter.clear();
            self.cursor = self
                .rows()
                .iter()
                .position(|row| row.dir_index == Some(leaving))
                .unwrap_or(0);
        }
    }
}

/// Runs the browser and restores the terminal on exit.
pub fn run(
    tree: Dir,
    apparent: bool,
    size_format: SizeFormat,
    refresh: Option<Refresh>,
) -> io::Result<Flag> {
    let mut model = Model::new(tree);
    model.apparent = apparent;
    model.size_format = size_format;
    if refresh.is_some() {
        model.batch = Some(Batch::new(std::path::Path::new(model.tree.fs_name()))?);
    }

    let mut terminal = enter()?;
    let result = event_loop(&mut terminal, &mut model, refresh);
    leave(&mut terminal)?;
    result.map(|()| model.tree.flag)
}

fn enter() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let result = (|| {
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        Terminal::new(CrosstermBackend::new(stdout))
    })();
    if result.is_err() {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
    result
}

fn leave(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    model: &mut Model,
    mut refresh: Option<Refresh>,
) -> io::Result<()> {
    struct Completed {
        batch: Batch,
        deleted: io::Result<usize>,
        tree: io::Result<Dir>,
        refresh: Refresh,
    }
    std::thread::scope(|scope| {
        let mut worker: Option<std::thread::ScopedJoinHandle<'_, Completed>> = None;
        loop {
            if worker.as_ref().is_some_and(|worker| worker.is_finished()) {
                let completed = worker
                    .take()
                    .expect("finished worker")
                    .join()
                    .map_err(|_| {
                        io::Error::other("Deletion worker panicked; filesystem may have changed")
                    })?;
                model.batch = Some(completed.batch);
                refresh = Some(completed.refresh);
                let mut message = match &completed.deleted {
                    Ok(count) => format!("Permanently deleted {count} marked entries."),
                    Err(err) => format!("Deletion failed: {err}"),
                };
                let refreshed = match completed.tree {
                    Ok(tree) => {
                        let complete = !tree.flag.is_incomplete();
                        if let Some(batch) = &mut model.batch {
                            batch.update_estimates(&tree);
                        }
                        model.replace_tree(tree);
                        if !complete {
                            message.push_str("\nRescan incomplete: unreadable paths were skipped.");
                        }
                        complete
                    }
                    Err(err) => {
                        message.push_str(&format!(
                        "\nRescan failed: {err}. Displayed totals are stale; deletion is disabled. Quit and scan again."
                    ));
                        model.batch = None;
                        model.tree.mark_child_unreadable();
                        false
                    }
                };
                if completed.deleted.is_ok() && refreshed && model.exit_after_delete {
                    model.quit = true;
                } else {
                    model.show_message(message);
                }
            }
            if model.should_quit() {
                return Ok(());
            }
            if model.apply_requested {
                model.apply_requested = false;
                let mut batch = model.batch.take().expect("live deletion batch");
                let mut refresh = refresh.take().expect("live scanner");
                worker = Some(scope.spawn(move || {
                    let deleted = batch.apply();
                    let tree = batch
                        .validate_root()
                        .and_then(|()| refresh())
                        .and_then(|tree| {
                            batch.validate_root()?;
                            Ok(tree)
                        });
                    Completed {
                        batch,
                        deleted,
                        tree,
                        refresh,
                    }
                }));
            }
            terminal.draw(|frame| view(frame, model))?;

            if (worker.is_none() || event::poll(Duration::from_millis(50))?)
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                model.on_key(key);
            }
        }
    })
}

fn view(frame: &mut ratatui::Frame, model: &Model) {
    if model.mode == Mode::Deleting {
        overlay(
            frame,
            "Working",
            vec![
                Line::from("Permanently deleting marked entries, then rescanning..."),
                Line::from("Please wait. This operation cannot be undone."),
            ],
        );
        return;
    }
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    header(frame, areas[0], model);
    listing(frame, areas[1], model);
    footer(frame, areas[2], model);

    if model.mode == Mode::Help {
        overlay(frame, "Keys", help_text());
    } else if model.mode == Mode::ConfirmDelete {
        let batch = model.batch.as_ref().expect("confirmation has a batch");
        let mut lines = vec![
            Line::from("PERMANENT deletion. No Trash and no undo."),
            Line::from("Folders include ALL contents, even filtered/unscanned entries."),
            Line::from(format!(
                "{} marked entries; estimated release: ~{}",
                batch.len(),
                format_size(batch.usage(), model.size_format),
            )),
            Line::from("Estimate is scanned allocation, not guaranteed free space."),
            Line::from("Hard links, snapshots and shared blocks can retain storage."),
            Line::from(""),
        ];
        lines.extend(
            batch
                .paths()
                .map(|path| Line::from(escape_name(&format!("{:?}", batch.root().join(path))))),
        );
        overlay_scrolled(
            frame,
            "Review permanent deletion",
            lines,
            model.dialog_scroll,
            Some(format!(
                "Type DELETE: {} | Enter apply | Esc cancel | PgUp/PgDn review",
                model.confirmation,
            )),
        );
    } else if model.mode == Mode::Message {
        overlay_scrolled(
            frame,
            "Result",
            model
                .message
                .lines()
                .map(|line| Line::from(escape_name(line)))
                .collect(),
            model.dialog_scroll,
            Some("PgUp/PgDn scroll | Any other key closes".into()),
        );
    }
}

fn header(frame: &mut ratatui::Frame, area: Rect, model: &Model) {
    let dir = model.current();
    let total = dir.shown_size(model.apparent);
    let sort = match model.sort {
        SortBy::Usage | SortBy::ApparentSize => "size",
        SortBy::Name => "name",
        SortBy::ItemCount => "items",
        SortBy::Mtime => "mtime",
    };
    let arrow = if model.order == Order::Descending {
        "↓"
    } else {
        "↑"
    };

    let left = format!(
        " {} — {}, {} items",
        escape_name(&model.current_path().to_string_lossy()),
        format_size(total, model.size_format),
        format_number(dir.items)
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(left, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled(
                format!("{sort} {arrow}"),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .style(Style::default().bg(Color::Rgb(30, 30, 40))),
        area,
    );
}

fn listing(frame: &mut ratatui::Frame, area: Rect, model: &Model) {
    let rows = model.rows();
    let parent = model.relative_path();
    let largest = rows
        .iter()
        .map(|row| row.shown_size(model.apparent))
        .max()
        .unwrap_or(0)
        .max(1);

    let table_rows: Vec<Row> = rows
        .iter()
        .map(|row| {
            let size = row.shown_size(model.apparent);
            let share = (size as f64 / largest as f64).clamp(0.0, 1.0);
            let filled = (share * BAR_WIDTH as f64).round() as usize;

            let escaped = escape_name(&row.name);
            let marked = model
                .batch
                .as_ref()
                .is_some_and(|batch| batch.covers(&parent.join(&row.fs_name)));
            let marker = if marked {
                Span::styled(
                    "D",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(" ")
            };
            let name = if row.dir_index.is_some() {
                Span::styled(
                    format!("/{escaped}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(escaped)
            };

            Row::new(vec![
                Line::from(marker),
                Line::from(Span::styled(
                    row.flag.as_char().to_string(),
                    Style::default().fg(Color::Red),
                )),
                Line::from(Span::styled(
                    format!("{:>10}", format_size(size, model.size_format)),
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(vec![
                    Span::styled("█".repeat(filled), Style::default().fg(Color::Green)),
                    Span::styled(
                        "░".repeat(BAR_WIDTH - filled),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
                Line::from(Span::styled(
                    format!("{:>4.0}%", share * 100.0),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    format!("{:>7}", format_count(row.items)),
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(name),
            ])
        })
        .collect();

    let mut state = TableState::default().with_selected(Some(model.cursor));
    frame.render_stateful_widget(
        Table::new(
            table_rows,
            [
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(10),
                Constraint::Length(BAR_WIDTH as u16),
                Constraint::Length(5),
                Constraint::Length(7),
                Constraint::Min(10),
            ],
        )
        .row_highlight_style(Style::default().bg(Color::Rgb(50, 50, 70)))
        .column_spacing(1),
        area,
        &mut state,
    );
}

fn footer(frame: &mut ratatui::Frame, area: Rect, model: &Model) {
    let text = if model.mode == Mode::Filtering {
        format!(" /{}", escape_name(&model.filter))
    } else if model.mode == Mode::Browsing {
        match &model.batch {
            Some(batch) if batch.len() > 0 => format!(
                " D: {} marked | ~{} potential release | d toggle  x apply  u clear  q review & quit  ? help",
                batch.len(),
                format_size(batch.usage(), model.size_format),
            ),
            Some(_) => {
                " ↑↓ move  → enter  ← up  d mark delete  x apply  / filter  ? help  q quit".into()
            }
            None => " READ-ONLY | ↑↓ move  → enter  ← up  / filter  ? help  q quit".into(),
        }
    } else {
        " Deletion requires typing DELETE; Esc cancels. Marks alone never delete files.".into()
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::Gray)),
        area,
    );
}

fn overlay(frame: &mut ratatui::Frame, title: &str, lines: Vec<Line<'static>>) {
    overlay_scrolled(frame, title, lines, 0, None);
}

fn overlay_scrolled(
    frame: &mut ratatui::Frame,
    title: &str,
    lines: Vec<Line<'static>>,
    scroll: u16,
    prompt: Option<String>,
) {
    let width = 90.min(frame.area().width.saturating_sub(2));
    let inner_width = usize::from(width.saturating_sub(2)).max(1);
    let content_height: usize = lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(inner_width))
        .sum();
    let prompt_height = prompt.as_ref().map_or(0, |prompt| {
        prompt.chars().count().div_ceil(inner_width) as u16 + 1
    });
    let height = (content_height.saturating_add(2).min(u16::MAX as usize) as u16)
        .saturating_add(prompt_height)
        .min(frame.area().height);
    let area = Rect {
        x: (frame.area().width.saturating_sub(width)) / 2,
        y: (frame.area().height.saturating_sub(height)) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title.to_string());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let parts =
        Layout::vertical([Constraint::Min(0), Constraint::Length(prompt_height)]).split(inner);
    let max_scroll = content_height
        .saturating_sub(parts[0].height as usize)
        .min(u16::MAX as usize) as u16;
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((scroll.min(max_scroll), 0)),
        parts[0],
    );
    if let Some(prompt) = prompt {
        frame.render_widget(
            Paragraph::new(prompt)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(Color::Yellow)),
            parts[1],
        );
    }
}

fn help_text() -> Vec<Line<'static>> {
    [
        "↑/k ↓/j     move          g/G   top/bottom",
        "→/l/Enter   enter dir     ←/h   parent dir",
        "s           sort by size  n     sort by name",
        "C           sort by count m     sort by mtime",
        "            (repeat a sort key to reverse it)",
        "a           apparent size vs disk usage",
        "/           filter by name, Esc clears",
        "d/Delete    mark/unmark for permanent deletion",
        "x           review and apply marked deletions",
        "u           clear all deletion marks",
        "D           marked (or inside a marked folder)",
        "q           review pending deletions, then quit",
        "            Esc cancels; u then q discards marks",
        "            Saved reports are read-only",
    ]
    .into_iter()
    .map(Line::from)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::FileStats;

    fn press(model: &mut Model, code: KeyCode) {
        model.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// ```text
    /// root/           big.bin 8k, small.txt 1k
    ///   docs/         notes.md 4k
    ///     deep/       leaf 2k
    ///   empty/
    /// ```
    fn sample() -> Dir {
        let mut root = Dir::new("/tmp/sample");
        root.push_file("big.bin", FileStats::plain(8192, 8192, 300));
        root.push_file("small.txt", FileStats::plain(1024, 1024, 100));

        let mut docs = Dir::new("docs");
        docs.push_file("notes.md", FileStats::plain(4096, 4096, 200));
        let mut deep = Dir::new("deep");
        deep.push_file("leaf", FileStats::plain(2048, 2048, 150));
        deep.finish(1);
        docs.push_dir(deep);
        docs.finish(2);

        let mut empty = Dir::new("empty");
        empty.finish(0);

        root.push_dir(docs);
        root.push_dir(empty);
        root.finish(4);
        root
    }

    fn model() -> Model {
        Model::new(sample())
    }

    fn live_model(root: &std::path::Path) -> Model {
        let tree = crate::scan::run(
            root,
            &crate::filter::Filters::default(),
            crate::scan::Options::default(),
            &crate::scan::Progress::default(),
            1,
        )
        .unwrap();
        let mut model = Model::new(tree);
        model.batch = Some(Batch::new(root).unwrap());
        model
    }

    fn type_text(model: &mut Model, text: &str) {
        for c in text.chars() {
            press(model, KeyCode::Char(c));
        }
    }

    #[test]
    fn marking_and_cancelling_never_delete_and_confirmation_is_exact() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), "data").unwrap();
        let mut model = live_model(root.path());
        press(&mut model, KeyCode::Char('d'));
        assert_eq!(model.batch.as_ref().unwrap().len(), 1);
        assert!(root.path().join("file").exists());
        let screen = render(&model, 120, 12);
        assert!(
            screen.contains("1 marked") && screen.contains("potential release"),
            "{screen}"
        );
        assert!(
            screen
                .lines()
                .any(|line| line.contains("file") && line.contains('D'))
        );

        press(&mut model, KeyCode::Char('x'));
        assert_eq!(model.mode, Mode::ConfirmDelete);
        press(&mut model, KeyCode::Enter);
        assert!(!model.apply_requested);
        type_text(&mut model, "delete");
        press(&mut model, KeyCode::Enter);
        assert!(!model.apply_requested);
        press(&mut model, KeyCode::Esc);
        assert_eq!(model.mode, Mode::Browsing);
        assert_eq!(model.batch.as_ref().unwrap().len(), 1);

        press(&mut model, KeyCode::Char('x'));
        assert!(model.confirmation.is_empty());
        type_text(&mut model, "DELETE");
        let screen = render(&model, 120, 20);
        assert!(screen.contains("PERMANENT deletion"), "{screen}");
        assert!(screen.contains("ALL contents"), "{screen}");
        assert!(
            screen.contains(&root.path().join("file").to_string_lossy().to_string()),
            "{screen}"
        );
        assert!(screen.contains("Type DELETE: DELETE"), "{screen}");
        press(&mut model, KeyCode::Enter);
        assert!(model.apply_requested);
        assert_eq!(model.mode, Mode::Deleting);
        assert!(root.path().join("file").exists(), "only the worker deletes");
    }

    #[test]
    fn quitting_reviews_marks_and_clearing_them_allows_a_non_destructive_exit() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), "data").unwrap();
        let mut model = live_model(root.path());
        press(&mut model, KeyCode::Char('d'));
        press(&mut model, KeyCode::Char('q'));
        assert!(!model.should_quit());
        assert!(model.exit_after_delete);
        assert_eq!(model.mode, Mode::ConfirmDelete);
        press(&mut model, KeyCode::Esc);
        press(&mut model, KeyCode::Char('u'));
        assert_eq!(model.batch.as_ref().unwrap().len(), 0);
        press(&mut model, KeyCode::Char('q'));
        assert!(model.should_quit());
        assert!(root.path().join("file").exists());
    }

    #[test]
    fn imported_reports_cannot_mark_or_delete() {
        let mut model = model();
        press(&mut model, KeyCode::Char('d'));
        assert_eq!(model.mode, Mode::Message);
        assert!(model.message.contains("read-only"));
        assert!(model.batch.is_none());
        assert!(!model.apply_requested);
        press(&mut model, KeyCode::Esc);
        press(&mut model, KeyCode::Char('x'));
        assert_eq!(model.mode, Mode::Browsing);
    }

    #[test]
    fn marking_uses_filtered_selection_and_preserves_exact_filename_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let root = tempfile::tempdir().unwrap();
        let name = std::ffi::OsStr::from_bytes(b"bad\xff");
        std::fs::write(root.path().join(name), "data").unwrap();
        std::fs::write(root.path().join("keep"), "data").unwrap();
        let mut model = live_model(root.path());
        press(&mut model, KeyCode::Char('/'));
        type_text(&mut model, "bad");
        press(&mut model, KeyCode::Enter);
        press(&mut model, KeyCode::Char('d'));
        let batch = model.batch.as_mut().unwrap();
        assert!(batch.covers(std::path::Path::new(name)));
        batch.apply().unwrap();
        assert!(!root.path().join(name).exists());
        assert!(root.path().join("keep").exists());
    }

    #[test]
    fn marked_directories_cover_children_without_counting_them_twice() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("folder")).unwrap();
        std::fs::write(root.path().join("folder/file"), "data").unwrap();
        let mut model = live_model(root.path());
        let expected_usage = model.rows()[0].usage;
        press(&mut model, KeyCode::Right);
        press(&mut model, KeyCode::Char('d'));
        press(&mut model, KeyCode::Left);
        press(&mut model, KeyCode::Char('d'));
        assert_eq!(model.batch.as_ref().unwrap().len(), 1);
        assert_eq!(model.batch.as_ref().unwrap().usage(), expected_usage);
        press(&mut model, KeyCode::Right);
        let screen = render(&model, 120, 12);
        assert!(
            screen
                .lines()
                .any(|line| line.contains("file") && line.contains('D'))
        );
        model.apparent = true;
        assert_eq!(model.batch.as_ref().unwrap().usage(), expected_usage);
    }

    #[test]
    fn rescanning_recomputes_totals_and_restores_navigation_by_name_not_index() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("folder")).unwrap();
        std::fs::write(root.path().join("folder/file"), "data").unwrap();
        let mut model = live_model(root.path());
        let old_items = model.tree.items;
        press(&mut model, KeyCode::Right);
        press(&mut model, KeyCode::Char('d'));
        model.batch.as_mut().unwrap().apply().unwrap();
        let refreshed = live_model(root.path()).tree;
        model.replace_tree(refreshed);
        assert!(model.current_path().ends_with("folder"));
        assert!(model.rows().is_empty());
        assert_eq!(model.tree.items, old_items - 1);
        assert_eq!(model.tree.usage, 0);
        assert_eq!(model.current().flag, Flag::Empty);
    }

    #[test]
    fn pending_delete_dialogs_render_at_small_sizes_and_keep_the_prompt_visible() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file"), "data").unwrap();
        let mut model = live_model(root.path());
        press(&mut model, KeyCode::Char('d'));
        press(&mut model, KeyCode::Char('x'));
        for (width, height) in [(100, 10), (20, 5), (5, 2), (1, 1)] {
            render(&model, width, height);
        }
        press(&mut model, KeyCode::PageDown);
        let screen = render(&model, 100, 10);
        assert!(screen.contains("Type DELETE:"), "{screen}");
        assert!(!model.apply_requested);
    }

    #[test]
    fn opens_on_the_largest_child() {
        let model = model();
        let rows = model.rows();
        assert_eq!(rows[0].name, "big.bin", "8k beats docs' 6k");
        assert_eq!(model.cursor, 0, "and the cursor starts on it");
    }

    #[test]
    fn navigation_descends_and_returns_to_the_directory_it_left() {
        let mut model = model();
        while model.rows().get(model.cursor).map(|r| r.name.as_str()) != Some("docs") {
            press(&mut model, KeyCode::Down);
        }
        press(&mut model, KeyCode::Right);

        assert!(model.current_path().ends_with("docs"));
        assert!(model.rows().iter().any(|r| r.name == "notes.md"));

        press(&mut model, KeyCode::Left);
        assert_eq!(
            model.rows().get(model.cursor).map(|r| r.name.as_str()),
            Some("docs"),
            "coming back up lands on the directory just left"
        );
    }

    #[test]
    fn entering_a_file_does_nothing() {
        let mut model = model();
        assert_eq!(model.rows()[model.cursor].name, "big.bin");
        press(&mut model, KeyCode::Right);
        assert_eq!(model.current_path(), PathBuf::from("/tmp/sample"));
    }

    #[test]
    fn repeating_a_sort_key_reverses_it() {
        let mut model = model();
        assert_eq!(model.rows()[0].name, "big.bin");

        press(&mut model, KeyCode::Char('s'));
        assert_eq!(model.order, Order::Ascending);
        assert_eq!(model.rows()[0].name, "empty", "smallest first now");

        press(&mut model, KeyCode::Char('n'));
        assert_eq!(model.sort, SortBy::Name);
        assert_eq!(
            model.order,
            Order::Descending,
            "a different sort key resets the direction"
        );
    }

    #[test]
    fn apparent_mode_sorts_by_apparent_size() {
        let mut root = Dir::new("root");
        root.push_file("sparse", FileStats::plain(8192, 0, 0));
        root.push_file("allocated", FileStats::plain(1024, 4096, 0));
        let mut model = Model::new(root);

        assert_eq!(model.rows()[0].name, "allocated");
        press(&mut model, KeyCode::Char('a'));
        assert_eq!(model.rows()[0].name, "sparse");
    }

    #[test]
    fn the_filter_narrows_and_escape_restores() {
        let mut model = model();
        let total = model.rows().len();

        press(&mut model, KeyCode::Char('/'));
        for c in "big".chars() {
            press(&mut model, KeyCode::Char(c));
        }
        assert_eq!(model.rows().len(), 1);
        assert_eq!(model.rows()[0].name, "big.bin");

        press(&mut model, KeyCode::Esc);
        assert_eq!(model.rows().len(), total, "Esc clears the filter");
        assert_eq!(model.mode, Mode::Browsing);
    }

    #[test]
    fn filtering_is_case_insensitive() {
        let mut model = model();
        press(&mut model, KeyCode::Char('/'));
        for c in "BIG".chars() {
            press(&mut model, KeyCode::Char(c));
        }
        assert_eq!(model.rows().len(), 1);
    }

    #[test]
    fn help_opens_and_any_key_closes_it() {
        let mut model = model();
        press(&mut model, KeyCode::Char('?'));
        assert_eq!(model.mode, Mode::Help);
        press(&mut model, KeyCode::Char('x'));
        assert_eq!(model.mode, Mode::Browsing);
    }

    #[test]
    fn the_cursor_never_leaves_the_list() {
        let mut model = model();
        for _ in 0..50 {
            press(&mut model, KeyCode::Down);
        }
        assert_eq!(model.cursor, model.rows().len() - 1);

        for _ in 0..50 {
            press(&mut model, KeyCode::Up);
        }
        assert_eq!(model.cursor, 0);
    }

    /// Renders one frame off-screen as plain text.
    fn render(model: &Model, width: u16, height: u16) -> String {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| view(frame, model)).unwrap();

        terminal
            .backend()
            .buffer()
            .content()
            .chunks(width as usize)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_frame_shows_the_path_totals_and_a_proportional_bar() {
        let screen = render(&model(), 100, 10);

        assert!(
            screen.contains("/tmp/sample"),
            "header path missing:\n{screen}"
        );
        assert!(
            screen.contains("8.0 KiB"),
            "the largest child's size:\n{screen}"
        );
        assert!(
            screen.contains("/docs"),
            "directories are marked:\n{screen}"
        );

        let big = screen.lines().find(|l| l.contains("big.bin")).unwrap();
        let small = screen.lines().find(|l| l.contains("small.txt")).unwrap();
        assert_eq!(
            big.matches('█').count(),
            BAR_WIDTH,
            "largest fills it: {big}"
        );
        assert!(
            small.matches('█').count() < BAR_WIDTH / 2,
            "1 KiB against 8 KiB should be a short bar: {small}"
        );
        assert!(big.contains("100%") && small.contains("12%"), "{screen}");
    }

    #[test]
    fn rendered_names_escape_terminal_controls() {
        let mut root = Dir::new("/tmp/root\n\u{202e}");
        root.push_file("bad\u{1b}\r\u{7f}", FileStats::plain(8, 8, 0));
        let model = Model::new(root);

        let screen = render(&model, 100, 10);
        assert!(screen.contains(r"/tmp/root\n\u{202e}"), "{screen:?}");
        assert!(screen.contains(r"bad\x1b\r\x7f"), "{screen:?}");
        assert!(!screen.contains('\u{1b}'));
    }

    #[test]
    fn the_help_overlay_renders() {
        let mut model = model();
        press(&mut model, KeyCode::Char('?'));
        let screen = render(&model, 100, 16);
        assert!(screen.contains("Keys"), "{screen}");
        assert!(screen.contains("sort by size"), "{screen}");
    }

    #[test]
    fn a_narrow_terminal_still_renders_without_panicking() {
        for (width, height) in [(20, 5), (10, 3), (5, 2), (1, 1)] {
            let mut model = model();
            render(&model, width, height);
            press(&mut model, KeyCode::Char('?'));
            render(&model, width, height);
            press(&mut model, KeyCode::Char('x'));
            render(&model, width, height);
        }
    }

    #[test]
    fn quitting_sets_the_flag() {
        let mut model = model();
        assert!(!model.should_quit());
        press(&mut model, KeyCode::Char('q'));
        assert!(model.should_quit());
    }
}
