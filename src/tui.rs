//! Interactive browser using a model/update/view split.

use std::io::{self, Stdout};
use std::path::PathBuf;

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
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState};

use crate::format::{SizeFormat, escape_name, format_count, format_number, format_size};
use crate::tree::{Dir, Flag, Order, SortBy};

/// Width of the usage bar.
const BAR_WIDTH: usize = 24;

/// What the browser is currently doing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Browsing,
    /// Typing a name filter.
    Filtering,
    Help,
}

/// One rendered child line.
#[derive(Debug, Clone)]
pub struct Child {
    pub name: String,
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
        let mut path = PathBuf::from(self.tree.name.as_ref());
        let mut dir = &self.tree;
        for &index in &self.path {
            dir = &dir.dirs[index];
            path.push(dir.name.as_ref());
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
        }
    }

    fn browse_key(&mut self, key: KeyEvent) {
        let rows = self.rows();
        let last = rows.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,

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

            KeyCode::Char('/') => {
                self.mode = Mode::Filtering;
                self.filter.clear();
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => {}
        }
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
pub fn run(tree: Dir, apparent: bool, size_format: SizeFormat) -> io::Result<()> {
    let mut model = Model::new(tree);
    model.apparent = apparent;
    model.size_format = size_format;

    let mut terminal = enter()?;
    let result = event_loop(&mut terminal, &mut model);
    leave(&mut terminal)?;
    result
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
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| view(frame, model))?;

        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            model.on_key(key);
        }
        if model.should_quit() {
            return Ok(());
        }
    }
}

fn view(frame: &mut ratatui::Frame, model: &Model) {
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
    } else {
        " ↑↓ move   → enter   ← up   s/n/C/m sort   a apparent   / filter   ? help   q quit"
            .to_string()
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::Gray)),
        area,
    );
}

fn overlay(frame: &mut ratatui::Frame, title: &str, lines: Vec<Line<'static>>) {
    let width = 60.min(frame.area().width.saturating_sub(4));
    let height = (lines.len() as u16 + 2).min(frame.area().height);
    let area = Rect {
        x: (frame.area().width.saturating_sub(width)) / 2,
        y: (frame.area().height.saturating_sub(height)) / 2,
        width,
        height,
    };

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title.to_string()),
        ),
        area,
    );
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
        "q           quit",
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
