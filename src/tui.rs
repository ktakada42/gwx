//! Interactive worktree picker.
//!
//! The picker draws to `/dev/tty` rather than stdout, because stdout is how the
//! chosen path gets back to the shell function that performs the `cd` — it is
//! captured in a command substitution and never reaches the terminal.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, queue, style, terminal};

use crate::commands::clean::{
    Candidate, NAME_HEADER as CLEAN_NAME_HEADER, NOTE_HEADER, VERDICT_HEADER,
};
use crate::commands::remove::{removal_blocker, remove_worktree, RemoveOptions};
use crate::git::{self, Worktree};
use crate::repo::Repo;

/// How the picker ended.
pub enum Outcome {
    /// Move to this worktree.
    Selected(PathBuf),
    /// Nothing was chosen; the shell should stay where it is.
    Cancelled,
}

/// `true` when a terminal is available to draw on.
pub fn is_available() -> bool {
    open_tty().is_ok()
}

fn open_tty() -> Result<File> {
    File::options()
        .write(true)
        .open("/dev/tty")
        .context("no terminal available")
}

/// The shape of the screen a frame is being drawn for.
///
/// Passed in rather than asked for inside the drawing code: `terminal::size`
/// answers for the process's own terminal, which a test has none of, and the
/// layout is the part worth testing. The event loops own the terminal, so they
/// are where the question gets asked — once per frame, which is also what keeps
/// a resize taking effect on the next redraw.
#[derive(Debug, Clone, Copy)]
struct Size {
    cols: usize,
    rows: u16,
}

impl Size {
    fn current() -> Result<Self> {
        let (cols, rows) = terminal::size()?;
        Ok(Self {
            cols: cols as usize,
            rows,
        })
    }
}

/// Whether a command should open the picker instead of printing.
///
/// Without a terminal — a script, a pipeline, CI — the caller keeps its plain
/// behaviour. A repository whose only worktree is the main one has nothing
/// worth picking from either.
pub fn should_pick(repo: &Repo) -> Result<bool> {
    if !is_available() {
        return Ok(false);
    }
    Ok(repo.worktrees()?.len() > 1)
}

/// One row of the list.
struct Item {
    name: String,
    path: PathBuf,
    head: String,
    /// `None` until [`StatusFeed`] reports back.
    ///
    /// Empty and unknown are different answers, and the column has to be able
    /// to say which one it is showing.
    note: Option<String>,
    is_current: bool,
    is_main: bool,
    worktree: Worktree,
}

/// What a Backspace did, since the key has two jobs.
#[derive(Debug, PartialEq, Eq)]
enum Backspace {
    /// Removed a character from the filter.
    ErasedFilter,
    /// Swallowed, because the filter had only just been emptied.
    Absorbed,
    /// Asked to remove the selected worktree.
    Delete,
}

/// List state: which rows exist, what was typed, where the cursor is.
struct Picker {
    items: Vec<Item>,
    filter: String,
    /// Index into the *filtered* list.
    cursor: usize,
    /// First visible row, for lists taller than the terminal.
    offset: usize,
    /// Set while Backspace is being used to erase the filter.
    ///
    /// Holding the key to clear what you typed sends a burst of Backspaces;
    /// without this, the burst would run past the empty filter and open the
    /// delete dialog. One press is swallowed at the boundary, so reaching the
    /// dialog always takes a deliberate keystroke.
    erasing: bool,
}

impl Picker {
    fn new(items: Vec<Item>) -> Self {
        Self {
            items,
            filter: String::new(),
            cursor: 0,
            offset: 0,
            erasing: false,
        }
    }

    /// Resolves what Backspace means right now.
    fn backspace(&mut self) -> Backspace {
        if !self.filter.is_empty() {
            self.pop_filter();
            self.erasing = true;
            return Backspace::ErasedFilter;
        }
        if self.erasing {
            self.erasing = false;
            return Backspace::Absorbed;
        }
        Backspace::Delete
    }

    /// Any other key ends the erasing streak.
    fn note_other_key(&mut self) {
        self.erasing = false;
    }

    /// Indices of the items matching the filter.
    ///
    /// Matching is a case-insensitive substring test over the name and the
    /// path, which is what makes typing a fragment of a branch name work.
    fn matches(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.items.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.name.to_lowercase().contains(&needle)
                    || item.path.to_string_lossy().to_lowercase().contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn selected(&self) -> Option<&Item> {
        self.matches().get(self.cursor).map(|&i| &self.items[i])
    }

    fn move_down(&mut self) {
        let len = self.matches().len();
        if len > 0 {
            self.cursor = (self.cursor + 1) % len;
        }
    }

    fn move_up(&mut self) {
        let len = self.matches().len();
        if len > 0 {
            self.cursor = (self.cursor + len - 1) % len;
        }
    }

    fn push_filter(&mut self, c: char) {
        self.filter.push(c);
        self.clamp();
    }

    fn pop_filter(&mut self) {
        self.filter.pop();
        self.clamp();
    }

    /// Keeps the cursor inside the filtered list after it shrinks or grows.
    fn clamp(&mut self) {
        let len = self.matches().len();
        if len == 0 {
            self.cursor = 0;
        } else if self.cursor >= len {
            self.cursor = len - 1;
        }
    }

    /// Scrolls so the cursor stays visible in a window of `height` rows.
    fn scroll_into_view(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + height {
            self.offset = self.cursor + 1 - height;
        }
    }
}

/// Restores the terminal no matter how the picker exits, including on panic.
struct Screen {
    tty: File,
}

impl Screen {
    fn open() -> Result<Self> {
        let mut tty = open_tty()?;
        terminal::enable_raw_mode().context("failed to switch the terminal to raw mode")?;
        queue!(tty, terminal::EnterAlternateScreen, cursor::Hide)?;
        tty.flush()?;
        Ok(Self { tty })
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        let _ = queue!(self.tty, cursor::Show, terminal::LeaveAlternateScreen);
        let _ = self.tty.flush();
        let _ = terminal::disable_raw_mode();
    }
}

/// How often the loop looks for status results while they are still coming.
const STATUS_POLL: std::time::Duration = std::time::Duration::from_millis(60);

/// Runs the picker until the user chooses a worktree or gives up.
pub fn pick(repo: &Repo) -> Result<Outcome> {
    let mut screen = Screen::open()?;
    let mut picker = Picker::new(load(repo)?);
    let mut status = StatusFeed::spawn(repo, &picker.items);
    let mut message: Option<String> = None;

    loop {
        draw(
            &mut screen.tty,
            &mut picker,
            message.as_deref(),
            Size::current()?,
        )?;

        let Some(key) = next_key(&mut picker, &mut status)? else {
            // Status arrived rather than a key; show it.
            continue;
        };
        // Windows reports both press and release; act on press only.
        if key.kind != KeyEventKind::Press {
            continue;
        }
        message = None;

        match step(&mut picker, key_action(&key)) {
            Step::Done(outcome) => return Ok(outcome),
            Step::Stay => {}
            Step::Delete => {
                message = delete_selected(repo, &mut screen.tty, &mut picker)?;
                status = StatusFeed::spawn(repo, &picker.items);
            }
        }
    }
}

/// What the loop does after a key has been applied to the list.
enum Step {
    /// Redraw and wait for the next key.
    Stay,
    /// The picker is finished.
    Done(Outcome),
    /// Open the confirmation dialog for the selected row.
    Delete,
}

/// Applies one key to the list, leaving anything that needs the terminal to
/// the caller.
///
/// Splitting the decision from the I/O is what makes the keyboard testable:
/// every branch here is reachable from a `KeyEvent` alone, with no terminal to
/// draw on and no repository to delete from.
fn step(picker: &mut Picker, action: Action) -> Step {
    if action != Action::Backspace {
        picker.note_other_key();
    }
    match action {
        Action::Cancel => Step::Done(Outcome::Cancelled),
        Action::Confirm => match picker.selected() {
            Some(item) => Step::Done(Outcome::Selected(item.path.clone())),
            // Nothing matches the filter, so there is nowhere to go.
            None => Step::Stay,
        },
        Action::Down => {
            picker.move_down();
            Step::Stay
        }
        Action::Up => {
            picker.move_up();
            Step::Stay
        }
        Action::Backspace => match picker.backspace() {
            Backspace::Delete => Step::Delete,
            _ => Step::Stay,
        },
        Action::Insert(c) => {
            picker.push_filter(c);
            Step::Stay
        }
        Action::Delete => Step::Delete,
        Action::None => Step::Stay,
    }
}

/// Waits for a key, redrawing when the status column fills in first.
///
/// Returns `None` when something other than a key needs the screen redrawn.
fn next_key(picker: &mut Picker, status: &mut StatusFeed) -> Result<Option<KeyEvent>> {
    loop {
        if status.is_done() {
            // Nothing left to wait for, so stop waking up to check.
            return Ok(match event::read()? {
                Event::Key(key) => Some(key),
                _ => None,
            });
        }
        if event::poll(STATUS_POLL)? {
            return Ok(match event::read()? {
                Event::Key(key) => Some(key),
                _ => None,
            });
        }
        if status.drain(&mut picker.items) {
            return Ok(None);
        }
    }
}

/// Opens the confirmation dialog and removes the worktree if confirmed.
///
/// Returns the line to show in the status area.
fn delete_selected(
    repo: &Repo,
    out: &mut impl Write,
    picker: &mut Picker,
) -> Result<Option<String>> {
    let subject = match pending(repo, picker) {
        None => return Ok(None),
        Some(Pending::Blocked(line)) => return Ok(Some(line)),
        Some(Pending::Ask(subject)) => subject,
    };

    let Some(with_branch) = confirm(out, &subject.worktree, subject.dirty, subject.merged)? else {
        return Ok(None);
    };

    // Removing can take a moment. Say so before starting, or the dialog just
    // sits there after the answer and invites a second, harder press.
    working(
        out,
        &format!("Removing {}...", subject.worktree.path.display()),
        Size::current()?,
    )?;

    let outcome = remove_picked(repo, picker, &subject, with_branch)?;

    // Anything typed while git was working was meant for the dialog, not for
    // the list that is about to replace it.
    discard_pending_input()?;
    Ok(Some(outcome))
}

/// The row Backspace landed on, and what removing it would cost.
struct Subject {
    worktree: Worktree,
    label: String,
    dirty: bool,
    merged: bool,
}

/// Whether a deletion can be offered at all, and on what terms.
enum Pending {
    /// git will not remove this one; the dialog would only ask a question with
    /// no good answer, so the reason goes to the status line instead.
    Blocked(String),
    Ask(Subject),
}

/// Works out what the dialog should say, without drawing it.
fn pending(repo: &Repo, picker: &Picker) -> Option<Pending> {
    let item = picker.selected()?;
    let worktree = item.worktree.clone();
    let label = item.name.clone();

    if let Some(reason) = removal_blocker(repo, &worktree, true) {
        return Some(Pending::Blocked(format!(
            "cannot remove `{label}`: {reason}"
        )));
    }
    let dirty = git::is_dirty(&worktree.path).unwrap_or(false);
    let merged = worktree
        .branch
        .as_deref()
        .zip(git::MergeState::read(&repo.main).ok())
        .is_some_and(|(b, merges)| merges.of(b) != git::Merged::No);

    Some(Pending::Ask(Subject {
        worktree,
        label,
        dirty,
        merged,
    }))
}

/// Removes a confirmed worktree and reloads the list. Returns the status line.
fn remove_picked(
    repo: &Repo,
    picker: &mut Picker,
    subject: &Subject,
    with_branch: bool,
) -> Result<String> {
    let label = &subject.label;
    let opts = RemoveOptions {
        // The dialog already spelled out the risk, so honour the answer.
        force: true,
        with_branch,
        quiet: true,
        // Deleting from the picker is still deleting: a hook that tears down
        // containers or caches has to run whichever way it was asked for.
        no_hooks: false,
    };
    match remove_worktree(repo, &subject.worktree, opts) {
        Ok(()) => {
            picker.items = load(repo)?;
            picker.clamp();
            let extra = if with_branch { " and its branch" } else { "" };
            Ok(format!("removed `{label}`{extra}"))
        }
        // `{e:#}` rather than `{e}`: the top of the chain says only which hook
        // failed, and what it printed before giving up is further down. The
        // line is cut to the terminal width anyway.
        Err(e) => Ok(format!("failed to remove `{label}`: {e:#}")),
    }
}

/// Clears the screen and states what is happening, for work that blocks.
fn working(out: &mut impl Write, what: &str, size: Size) -> Result<()> {
    queue!(
        out,
        terminal::Clear(terminal::ClearType::All),
        cursor::MoveTo(0, 0),
        style::Print(what),
        cursor::MoveTo(0, size.rows.saturating_sub(1)),
    )?;
    out.flush()?;
    Ok(())
}

fn discard_pending_input() -> Result<()> {
    while event::poll(std::time::Duration::from_millis(0))? {
        let _ = event::read()?;
    }
    Ok(())
}

/// Asks for confirmation. `None` means cancelled, `Some(with_branch)` confirms.
fn confirm(
    out: &mut impl Write,
    worktree: &Worktree,
    dirty: bool,
    merged: bool,
) -> Result<Option<bool>> {
    loop {
        draw_confirm(out, worktree, dirty, merged, Size::current()?)?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        match reply(&key, worktree.branch.is_some()) {
            Some(Reply::Remove { with_branch }) => return Ok(Some(with_branch)),
            Some(Reply::Cancel) => return Ok(None),
            // Anything else leaves the question on screen.
            None => {}
        }
    }
}

/// Draws the confirmation dialog.
fn draw_confirm(
    out: &mut impl Write,
    worktree: &Worktree,
    dirty: bool,
    merged: bool,
    size: Size,
) -> Result<()> {
    let branch = worktree.branch.as_deref();
    queue!(
        out,
        terminal::Clear(terminal::ClearType::All),
        cursor::MoveTo(0, 0),
        style::Print("Remove this worktree?"),
        cursor::MoveTo(0, 2),
        style::Print(format!("  {}", worktree.path.display())),
    )?;

    let mut row = 4;
    if dirty {
        queue!(
            out,
            cursor::MoveTo(0, row),
            style::Print("  ! uncommitted changes will be lost"),
        )?;
        row += 1;
    }
    if let Some(b) = branch {
        let state = if merged { "merged" } else { "NOT merged" };
        queue!(
            out,
            cursor::MoveTo(0, row),
            style::Print(format!("  branch `{b}` ({state})")),
        )?;
    }

    queue!(
        out,
        cursor::MoveTo(0, size.rows.saturating_sub(1)),
        style::Print(confirm_keys(branch.is_some()))
    )?;
    out.flush()?;
    Ok(())
}

/// The key line under the dialog. Without a branch there is nothing for `b` to
/// remove, so offering it would be a key that does nothing.
fn confirm_keys(has_branch: bool) -> &'static str {
    if has_branch {
        "[y] remove worktree   [b] remove worktree and branch   [n] cancel"
    } else {
        "[y] remove worktree   [n] cancel"
    }
}

/// An answer to the confirmation dialog.
#[derive(Debug, PartialEq, Eq)]
enum Reply {
    Remove { with_branch: bool },
    Cancel,
}

/// Reads one key as an answer. `None` means the dialog is still waiting.
fn reply(key: &KeyEvent, has_branch: bool) -> Option<Reply> {
    // Windows reports both press and release; act on press only.
    if key.kind != KeyEventKind::Press {
        return None;
    }
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => Some(Reply::Remove { with_branch: false }),
        KeyCode::Char('b') | KeyCode::Char('B') if has_branch => {
            Some(Reply::Remove { with_branch: true })
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Some(Reply::Cancel),
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Some(Reply::Cancel),
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Confirm,
    Cancel,
    Up,
    Down,
    Delete,
    Backspace,
    Insert(char),
    None,
}

/// Maps a key to an action.
///
/// `Delete` is bound to Ctrl-D as well: on Mac keyboards the key labelled
/// "delete" sends Backspace, which the filter needs.
fn key_action(key: &KeyEvent) -> Action {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter => Action::Confirm,
        KeyCode::Esc => Action::Cancel,
        KeyCode::Char('c') | KeyCode::Char('g') if ctrl => Action::Cancel,
        KeyCode::Down => Action::Down,
        KeyCode::Up => Action::Up,
        KeyCode::Char('n') if ctrl => Action::Down,
        KeyCode::Char('p') if ctrl => Action::Up,
        KeyCode::Char('d') if ctrl => Action::Delete,
        KeyCode::Delete => Action::Delete,
        KeyCode::Backspace => Action::Backspace,
        // Terminals told to send 0x08 for the erase key surface it as Ctrl-H.
        KeyCode::Char('h') if ctrl => Action::Backspace,
        KeyCode::Char(c) if !ctrl => Action::Insert(c),
        _ => Action::None,
    }
}

/// Builds the rows without asking git anything slow.
///
/// The status column is left unknown here and filled in by [`StatusFeed`]: it
/// costs a `git status` per worktree, which is fast in a small repository and
/// seconds across ten of them. The list has to be on screen before that.
fn load(repo: &Repo) -> Result<Vec<Item>> {
    let worktrees = repo.worktrees()?;
    let Some(main) = worktrees.first().cloned() else {
        return Ok(Vec::new());
    };

    Ok(worktrees
        .into_iter()
        .map(|wt| Item {
            name: repo.display_name(&wt, &main),
            head: wt.short_head(),
            note: None,
            is_main: wt.path == main.path,
            is_current: repo.cwd.starts_with(&wt.path),
            path: wt.path.clone(),
            worktree: wt,
        })
        .collect())
}

/// Fills in the status column without blocking the list.
///
/// One background thread asks which branches are merged, then fans out a
/// thread per worktree for the `git status` calls, so ten worktrees cost about
/// as long as the slowest one rather than the sum of all ten. The content test
/// a squash merge needs is per branch, so it rides along in the same fan-out.
struct StatusFeed {
    rx: Option<std::sync::mpsc::Receiver<Vec<(usize, String)>>>,
}

/// Everything a worker needs to describe one worktree, owned so it can move
/// to another thread.
struct StatusJob {
    index: usize,
    path: PathBuf,
    branch: Option<String>,
    is_main: bool,
    flags: Vec<&'static str>,
}

impl StatusFeed {
    fn spawn(repo: &Repo, items: &[Item]) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let main = repo.main.clone();
        let subjects: Vec<StatusJob> = items
            .iter()
            .enumerate()
            .map(|(index, item)| StatusJob {
                index,
                path: item.path.clone(),
                branch: item.worktree.branch.clone(),
                is_main: item.is_main,
                flags: flags(&item.worktree),
            })
            .collect();

        std::thread::spawn(move || {
            let merges = git::MergeState::read(&main).ok();
            let mut workers = Vec::new();
            for subject in subjects {
                let merges = merges.clone();
                workers.push(std::thread::spawn(move || {
                    let note = note(
                        &subject.path,
                        subject.branch.as_deref(),
                        merges.as_ref(),
                        subject.is_main,
                        &subject.flags,
                    );
                    (subject.index, note)
                }));
            }
            let notes = workers.into_iter().filter_map(|w| w.join().ok()).collect();
            // The receiver is gone once the picker closes; nothing to report.
            let _ = tx.send(notes);
        });

        Self { rx: Some(rx) }
    }

    /// Applies whatever has arrived. `true` when the screen needs redrawing.
    fn drain(&mut self, items: &mut [Item]) -> bool {
        use std::sync::mpsc::TryRecvError;
        let Some(rx) = &self.rx else {
            return false;
        };
        match rx.try_recv() {
            Ok(notes) => {
                for (index, note) in notes {
                    if let Some(item) = items.get_mut(index) {
                        item.note = Some(note);
                    }
                }
                self.rx = None;
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.rx = None;
                false
            }
        }
    }

    /// Once nothing more is coming, the loop can go back to blocking on keys.
    fn is_done(&self) -> bool {
        self.rx.is_none()
    }
}

/// What is worth knowing about a worktree before acting on it.
///
/// The path used to sit here, but it is derived from the branch name for every
/// worktree gwx creates, so it repeated what the name already said. What is
/// missing at a glance is whether removing this one would lose anything.
///
/// "merged" is skipped for the main worktree: a branch is always merged into
/// itself, and the main worktree cannot be removed anyway.
fn note(
    path: &std::path::Path,
    branch: Option<&str>,
    merges: Option<&git::MergeState>,
    is_main: bool,
    flags: &[&str],
) -> String {
    let mut notes: Vec<String> = Vec::new();

    if git::is_dirty(path).unwrap_or(false) {
        notes.push("dirty".to_string());
    }
    if !is_main {
        if let Some((branch, merges)) = branch.zip(merges) {
            // Squash-merged and plainly merged both read as "merged" here.
            // The column is a hint about what a removal would cost, and the
            // cost is the same; `gwx clean` is where the difference is spelt
            // out.
            if merges.of(branch) != git::Merged::No {
                notes.push("merged".to_string());
            }
        }
    }
    notes.extend(flags.iter().map(|f| f.to_string()));
    notes.join(", ")
}

fn flags(wt: &Worktree) -> Vec<&'static str> {
    let mut notes = Vec::new();
    if wt.bare {
        notes.push("bare");
    }
    if wt.detached {
        notes.push("detached");
    }
    if wt.locked {
        notes.push("locked");
    }
    if wt.prunable {
        notes.push("broken");
    }
    notes
}

/// One entry of the help line: the key, then what it does.
///
/// The key is drawn in reverse video, which gives the boundary between key and
/// label a shape rather than leaving it to whitespace. It reuses the attribute
/// the cursor row already uses, so it holds up on any colour theme.
struct Hint {
    keys: &'static [&'static str],
    label: &'static str,
    /// Dropped first when the terminal is too narrow for the full line.
    optional: bool,
}

/// Keys are spelled out: `bksp` is keycap shorthand, not something a help line
/// can assume its reader knows.
///
/// Everything here is plain ASCII on purpose — arrows and the return symbol
/// land in ranges that terminals disagree about, and U+25B6 in particular has
/// an emoji presentation that renders double width and shifts the whole row.
const HINTS_IDLE: &[Hint] = &[
    Hint {
        keys: &["up/down"],
        label: "move",
        optional: true,
    },
    Hint {
        keys: &["enter"],
        label: "cd",
        optional: false,
    },
    Hint {
        keys: &["ctrl-d", "backspace"],
        label: "delete",
        optional: false,
    },
    Hint {
        keys: &["esc"],
        label: "cancel",
        optional: false,
    },
];

/// While a filter is typed, Backspace edits it instead of deleting.
const HINTS_FILTERING: &[Hint] = &[
    Hint {
        keys: &["up/down"],
        label: "move",
        optional: true,
    },
    Hint {
        keys: &["enter"],
        label: "cd",
        optional: false,
    },
    Hint {
        keys: &["ctrl-d"],
        label: "delete",
        optional: false,
    },
    Hint {
        keys: &["backspace"],
        label: "erase",
        optional: false,
    },
    Hint {
        keys: &["esc"],
        label: "cancel",
        optional: false,
    },
];

/// The hints matching what Backspace does right now.
fn hints_for(picker: &Picker) -> &'static [Hint] {
    if picker.filter.is_empty() {
        HINTS_IDLE
    } else {
        HINTS_FILTERING
    }
}

/// Columns a set of hints needs, badges included.
fn hints_width(hints: &[&Hint]) -> usize {
    hints
        .iter()
        .map(|h| {
            // Each key sits in a badge padded by one space on either side.
            let keys: usize = h.keys.iter().map(|k| k.chars().count() + 2).sum();
            let between_keys = h.keys.len() - 1;
            keys + between_keys + 1 + h.label.chars().count()
        })
        .sum::<usize>()
        + hints.len().saturating_sub(1) * 2
}

/// Drops optional hints until the line fits, rather than truncating mid-word.
fn hints_that_fit(hints: &'static [Hint], width: usize) -> Vec<&'static Hint> {
    let mut kept: Vec<&Hint> = hints.iter().collect();
    while hints_width(&kept) > width && kept.iter().any(|h| h.optional) {
        let index = kept.iter().position(|h| h.optional).unwrap();
        kept.remove(index);
    }
    kept
}

/// Truncates to `width` and pads to it, so a highlighted row spans the line.
fn fit(line: &str, width: usize) -> String {
    let mut out: String = line.chars().take(width).collect();
    let len = out.chars().count();
    if len < width {
        out.extend(std::iter::repeat_n(' ', width - len));
    }
    out
}

const PLACEHOLDER: &str = "type to filter";

/// Stands in for a status that has not been worked out yet.
///
/// An empty column is an answer of its own — nothing about this worktree is
/// worth saying — so the wait needs a mark, or a row still being measured and
/// a row with nothing to report look exactly alike and neither can be read.
const PENDING: &str = "...";

/// Column labels. `HEAD` keeps git's own name for the short commit id.
const NAME_HEADER: &str = "WORKTREE";
const HEAD_HEADER: &str = "HEAD   ";
const STATUS_HEADER: &str = "STATUS";

/// What the right of the prompt says about the list below it.
///
/// While filtering, this is the only place the size of the list is stated —
/// without it, typing something that matches nothing just empties the screen
/// with no explanation.
fn count_label(filter: &str, matched: usize, total: usize) -> String {
    if !filter.is_empty() {
        return format!("{matched} of {total}");
    }
    match total {
        1 => "1 worktree".to_string(),
        n => format!("{n} worktrees"),
    }
}

/// Draws the filter line: prompt, what has been typed, a block cursor, and —
/// while nothing is typed — what typing would do.
///
/// The terminal's own cursor is hidden for the whole picker, so the block is
/// drawn by hand as a space in reverse video. Without it, an empty filter line
/// is a lone `>` that gives no sign it accepts input.
fn draw_prompt(out: &mut impl Write, picker: &Picker, matched: usize, cols: usize) -> Result<()> {
    let placeholder = if picker.filter.is_empty() {
        PLACEHOLDER
    } else {
        ""
    };
    let count = count_label(&picker.filter, matched, picker.items.len());
    let left = 2 + picker.filter.chars().count() + 1 + placeholder.chars().count();

    queue!(out, style::Print("> "), style::Print(&picker.filter))?;
    queue!(
        out,
        style::SetAttribute(style::Attribute::Reverse),
        style::Print(" "),
        style::SetAttribute(style::Attribute::Reset),
    )?;
    if !placeholder.is_empty() {
        queue!(
            out,
            style::SetAttribute(style::Attribute::Dim),
            style::Print(placeholder),
            style::SetAttribute(style::Attribute::Reset),
        )?;
    }

    // Right-aligned, and dropped rather than wrapped when the line is full.
    let gap = cols.saturating_sub(left + count.chars().count());
    if gap > 0 {
        queue!(
            out,
            style::Print(" ".repeat(gap)),
            style::SetAttribute(style::Attribute::Dim),
            style::Print(count),
            style::SetAttribute(style::Attribute::Reset),
        )?;
    }
    Ok(())
}

fn draw(
    out: &mut impl Write,
    picker: &mut Picker,
    message: Option<&str>,
    size: Size,
) -> Result<()> {
    let Size { cols, rows } = size;
    // The prompt, the header, the help line, and any message.
    let reserved = if message.is_some() { 4 } else { 3 };
    let height = (rows as usize).saturating_sub(reserved);
    picker.scroll_into_view(height);

    let matches = picker.matches();
    let name_width = matches
        .iter()
        .map(|&i| picker.items[i].name.chars().count())
        .max()
        .unwrap_or(0)
        // Never narrower than the column it is labelled with.
        .max(NAME_HEADER.len());

    // The table starts at the top, so the header sits against the rows it
    // labels; the filter joins the help line at the bottom, where the things
    // you operate live. Butting the filter against the header, as this used
    // to, made the two read as one block of chrome.
    //
    // Bold and underlined, not dimmed. Dim is what the filter placeholder
    // uses, and a header that shares it reads as another piece of hint text
    // instead of the structure of the table. The underline runs the full width
    // so it doubles as the rule between the header and the rows; carrying both
    // attributes means the header still stands out on a terminal that renders
    // only one of them.
    let header = fit(
        &format!("  {NAME_HEADER:<name_width$}  {HEAD_HEADER}  {STATUS_HEADER}"),
        cols,
    );
    queue!(
        out,
        terminal::Clear(terminal::ClearType::All),
        cursor::MoveTo(0, 0),
        style::SetAttribute(style::Attribute::Bold),
        style::SetAttribute(style::Attribute::Underlined),
        style::Print(header),
        style::SetAttribute(style::Attribute::Reset),
    )?;

    for (row, &index) in matches.iter().skip(picker.offset).take(height).enumerate() {
        let item = &picker.items[index];
        let is_cursor = picker.offset + row == picker.cursor;
        // The marker column says "you are standing here"; the cursor is the
        // highlight, so the two never compete for the same character.
        let marker = if item.is_current { "*" } else { " " };
        let line = format!(
            "{marker} {:<name_width$}  {}  {}",
            item.name,
            item.head,
            item.note.as_deref().unwrap_or(PENDING)
        );
        let line = fit(&line, cols);
        queue!(out, cursor::MoveTo(0, row as u16 + 1))?;
        if is_cursor {
            // Reverse video rather than a colour: it stays readable on any
            // theme, and highlights the row edge to edge.
            queue!(
                out,
                style::SetAttribute(style::Attribute::Reverse),
                style::Print(line),
                style::SetAttribute(style::Attribute::Reset),
            )?;
        } else {
            queue!(out, style::Print(line))?;
        }
    }

    if let Some(message) = message {
        queue!(
            out,
            cursor::MoveTo(0, rows.saturating_sub(3)),
            style::Print(message.chars().take(cols).collect::<String>()),
        )?;
    }
    queue!(out, cursor::MoveTo(0, rows.saturating_sub(2)))?;
    draw_prompt(out, picker, matches.len(), cols)?;

    queue!(out, cursor::MoveTo(0, rows.saturating_sub(1)))?;
    draw_hints(out, &hints_that_fit(hints_for(picker), cols))?;
    out.flush()?;
    Ok(())
}

/// Draws the help line, each key as a reverse-video badge.
fn draw_hints(out: &mut impl Write, hints: &[&Hint]) -> Result<()> {
    for (i, hint) in hints.iter().enumerate() {
        if i > 0 {
            queue!(out, style::Print("  "))?;
        }
        for (k, key) in hint.keys.iter().enumerate() {
            if k > 0 {
                queue!(out, style::Print(" "))?;
            }
            queue!(
                out,
                style::SetAttribute(style::Attribute::Reverse),
                style::Print(format!(" {key} ")),
                style::SetAttribute(style::Attribute::Reset),
            )?;
        }
        queue!(out, style::Print(format!(" {}", hint.label)))?;
    }
    Ok(())
}

/// The multi-select screen behind `gwx clean`.
///
/// Deliberately not the picker: that one exists to go somewhere and takes a
/// single choice, this one exists to remove things and takes many. Sharing one
/// widget would mean a mode flag threaded through every keystroke, and a list
/// where Enter sometimes moves you and sometimes deletes.
///
/// Returns the indices to remove, or `None` when the user backed out.
pub fn choose_to_clean(candidates: &[Candidate], with_branch: bool) -> Result<Option<Vec<usize>>> {
    let mut screen = Screen::open()?;
    let mut ticked: Vec<bool> = candidates.iter().map(|c| c.state.preselected()).collect();
    let mut cursor_at = 0usize;

    loop {
        draw_clean(
            &mut screen.tty,
            candidates,
            &ticked,
            cursor_at,
            with_branch,
            Size::current()?,
        )?;

        let Event::Key(key) = event::read()? else {
            continue;
        };
        match clean_step(&key, &mut ticked, &mut cursor_at) {
            CleanStep::Stay => {}
            CleanStep::Cancel => return Ok(None),
            CleanStep::Accept => {
                return Ok(Some(
                    ticked
                        .iter()
                        .enumerate()
                        .filter(|(_, on)| **on)
                        .map(|(i, _)| i)
                        .collect(),
                ))
            }
        }
    }
}

/// What the clean screen does after a key.
#[derive(Debug, PartialEq, Eq)]
enum CleanStep {
    Stay,
    Cancel,
    Accept,
}

/// Moves the cursor and ticks boxes; the caller owns the screen and the answer.
fn clean_step(key: &KeyEvent, ticked: &mut [bool], cursor_at: &mut usize) -> CleanStep {
    // Windows reports both press and release; act on press only.
    if key.kind != KeyEventKind::Press {
        return CleanStep::Stay;
    }
    let last = ticked.len().saturating_sub(1);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    // Both ends wrap, so a list you have scrolled past is one key away from the
    // other end rather than a dead stop.
    let down = |at: usize| if at == last { 0 } else { at + 1 };
    let up = |at: usize| at.checked_sub(1).unwrap_or(last);
    match key.code {
        KeyCode::Esc => return CleanStep::Cancel,
        KeyCode::Char('c') | KeyCode::Char('g') if ctrl => return CleanStep::Cancel,
        KeyCode::Enter => return CleanStep::Accept,
        KeyCode::Down => *cursor_at = down(*cursor_at),
        KeyCode::Char('n') if ctrl => *cursor_at = down(*cursor_at),
        KeyCode::Up => *cursor_at = up(*cursor_at),
        KeyCode::Char('p') if ctrl => *cursor_at = up(*cursor_at),
        KeyCode::Char(' ') => ticked[*cursor_at] = !ticked[*cursor_at],
        _ => {}
    }
    CleanStep::Stay
}

/// One `gwx clean` frame: header, rows, and the help line.
fn draw_clean(
    out: &mut impl Write,
    candidates: &[Candidate],
    ticked: &[bool],
    cursor_at: usize,
    with_branch: bool,
    size: Size,
) -> Result<()> {
    let Size { cols, rows } = size;
    let verdicts: Vec<String> = candidates
        .iter()
        .map(|c| c.state.verdict(with_branch))
        .collect();
    let width = candidates
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(CLEAN_NAME_HEADER.len());
    let verdict_width = verdicts
        .iter()
        .map(|v| v.chars().count())
        .max()
        .unwrap_or(0)
        .max(VERDICT_HEADER.len());

    let header = format!(
        "      {CLEAN_NAME_HEADER:<width$}  {VERDICT_HEADER:<verdict_width$}  {NOTE_HEADER}"
    );
    queue!(
        out,
        terminal::Clear(terminal::ClearType::All),
        cursor::MoveTo(0, 0),
        style::Print("Select worktrees to remove"),
        cursor::MoveTo(0, 2),
        style::SetAttribute(style::Attribute::Bold),
        style::SetAttribute(style::Attribute::Underlined),
        style::Print(fit(&header, cols)),
        style::SetAttribute(style::Attribute::Reset),
    )?;

    for (i, candidate) in candidates.iter().enumerate() {
        let row = 3 + i as u16;
        if row >= rows.saturating_sub(2) {
            break;
        }
        let line = format!(
            "{} [{}] {:<width$}  {:<verdict_width$}  {}",
            if i == cursor_at { ">" } else { " " },
            if ticked[i] { "x" } else { " " },
            candidate.name,
            verdicts[i],
            candidate.note,
        );
        queue!(out, cursor::MoveTo(0, row))?;
        if i == cursor_at {
            queue!(
                out,
                style::SetAttribute(style::Attribute::Reverse),
                style::Print(fit(&format!("{line:<cols$}"), cols)),
                style::SetAttribute(style::Attribute::Reset),
            )?;
        } else {
            queue!(out, style::Print(fit(&line, cols)))?;
        }
    }

    let count = ticked.iter().filter(|on| **on).count();
    queue!(
        out,
        cursor::MoveTo(0, rows.saturating_sub(2)),
        style::SetAttribute(style::Attribute::Dim),
        style::Print(fit(
            &format!("{count} of {} selected", candidates.len()),
            cols
        )),
        style::SetAttribute(style::Attribute::Reset),
        cursor::MoveTo(0, rows.saturating_sub(1)),
    )?;
    draw_hints(out, &hints_that_fit(CLEAN_HINTS, cols))?;
    out.flush()?;
    Ok(())
}

/// The help line for `gwx clean`.
///
/// Four keys, none of them optional: the line fits an eighty-column terminal
/// whole, so nothing here is a shortcut somebody has to be told about. Bulk
/// selection is deliberately absent — with a handful of worktrees, space is
/// enough, and the key that ticks everything is the one this screen should
/// think hardest before adding.
const CLEAN_HINTS: &[Hint] = &[
    Hint {
        keys: &["up/down"],
        label: "move",
        optional: false,
    },
    Hint {
        keys: &["space"],
        label: "toggle",
        optional: false,
    },
    Hint {
        keys: &["enter"],
        label: "remove",
        optional: false,
    },
    Hint {
        keys: &["esc"],
        label: "cancel",
        optional: false,
    },
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::*;
    use crate::commands::clean::State;
    use crate::config::Config;

    /// What a terminal would have shown for one frame.
    ///
    /// The drawing code writes escape sequences to move the cursor and switch
    /// attributes, so a frame captured in a buffer is one run-on line until
    /// those are replayed. This replays the three that carry the layout —
    /// clear, cursor move, reverse video — which is enough to assert on what a
    /// person would have seen, and nothing about how it was encoded.
    #[derive(Debug, Default)]
    struct Frame {
        rows: BTreeMap<u16, String>,
        /// Every run drawn in reverse video: the cursor row and the key badges.
        highlighted: Vec<String>,
    }

    impl Frame {
        fn parse(bytes: &[u8]) -> Self {
            let text = String::from_utf8(bytes.to_vec()).unwrap();
            let mut frame = Frame::default();
            let mut row = 0u16;
            let mut reversed = false;
            let mut chars = text.chars().peekable();

            while let Some(c) = chars.next() {
                if c != '\u{1b}' {
                    frame.rows.entry(row).or_default().push(c);
                    if reversed {
                        frame.highlighted.last_mut().unwrap().push(c);
                    }
                    continue;
                }
                if chars.peek() != Some(&'[') {
                    continue;
                }
                chars.next();
                let mut params = String::new();
                let mut end = ' ';
                for c in chars.by_ref() {
                    if c.is_ascii_digit() || c == ';' || c == '?' {
                        params.push(c);
                    } else {
                        end = c;
                        break;
                    }
                }
                match end {
                    // Rows are 1-based on the wire; everything is drawn at
                    // column 0, so the column is not worth tracking.
                    'H' => {
                        let line = params.split(';').next().unwrap_or("1");
                        row = line.parse::<u16>().unwrap_or(1).saturating_sub(1);
                    }
                    'J' => frame.rows.clear(),
                    'm' => match params.as_str() {
                        "7" => {
                            reversed = true;
                            frame.highlighted.push(String::new());
                        }
                        "0" => reversed = false,
                        _ => {}
                    },
                    _ => {}
                }
            }
            frame
        }

        fn row(&self, n: u16) -> &str {
            self.rows.get(&n).map(String::as_str).unwrap_or("")
        }

        /// Every row, so a test can ask whether something is on screen at all.
        fn text(&self) -> String {
            self.rows.values().cloned().collect::<Vec<_>>().join("\n")
        }
    }

    fn frame_of(f: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> Frame {
        let mut buf = Vec::new();
        f(&mut buf).unwrap();
        Frame::parse(&buf)
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn item(name: &str, path: &str) -> Item {
        Item {
            name: name.to_string(),
            path: PathBuf::from(path),
            head: "abc1234".to_string(),
            note: None,
            is_current: false,
            is_main: false,
            worktree: Worktree {
                path: PathBuf::from(path),
                head: None,
                branch: Some(name.to_string()),
                bare: false,
                detached: false,
                locked: false,
                prunable: false,
            },
        }
    }

    fn picker() -> Picker {
        Picker::new(vec![
            item("@", "/repo"),
            item("feature/auth", "/wt/feature/auth"),
            item("feature/billing", "/wt/feature/billing"),
            item("hotfix", "/wt/hotfix"),
        ])
    }

    #[test]
    fn the_interface_is_ascii_only() {
        // Non-ASCII risks missing glyphs, and emoji-presentation characters
        // render double width and break the column alignment.
        for hint in HINTS_IDLE.iter().chain(HINTS_FILTERING) {
            for key in hint.keys {
                assert!(key.is_ascii(), "{key}");
            }
            assert!(hint.label.is_ascii(), "{}", hint.label);
        }
        for label in [
            "Remove this worktree?",
            "  ! uncommitted changes will be lost",
            "[y] remove worktree   [b] remove worktree and branch   [n] cancel",
        ] {
            assert!(label.is_ascii(), "{label}");
        }
    }

    #[test]
    fn keys_are_spelled_out() {
        // `^d` and `bksp` are shorthand a help line cannot assume its reader
        // knows, so both are written in full.
        let keys: Vec<&str> = HINTS_IDLE
            .iter()
            .chain(HINTS_FILTERING)
            .flat_map(|h| h.keys.iter().copied())
            .collect();
        assert!(keys.contains(&"ctrl-d"), "{keys:?}");
        assert!(keys.contains(&"backspace"), "{keys:?}");
        assert!(!keys.iter().any(|k| k.contains("bksp") || k.contains('^')));
    }

    #[test]
    fn the_help_says_what_backspace_will_do() {
        let mut p = picker();
        let idle = hints_for(&p);
        let delete = idle.iter().find(|h| h.label == "delete").unwrap();
        assert!(delete.keys.contains(&"backspace"), "{:?}", delete.keys);

        p.push_filter('a');
        let filtering = hints_for(&p);
        let erase = filtering.iter().find(|h| h.label == "erase").unwrap();
        assert_eq!(erase.keys, &["backspace"]);
        // While filtering, deleting is ctrl-d only.
        let delete = filtering.iter().find(|h| h.label == "delete").unwrap();
        assert_eq!(delete.keys, &["ctrl-d"]);
    }

    #[test]
    fn a_narrow_terminal_drops_optional_hints_instead_of_cutting_words() {
        let full = hints_width(&HINTS_IDLE.iter().collect::<Vec<_>>());
        assert_eq!(hints_that_fit(HINTS_IDLE, full).len(), HINTS_IDLE.len());

        // One column short: the optional "move" hint goes, the rest survive.
        let narrowed = hints_that_fit(HINTS_IDLE, full - 1);
        assert_eq!(narrowed.len(), HINTS_IDLE.len() - 1);
        assert!(!narrowed.iter().any(|h| h.label == "move"));
        assert!(narrowed.iter().any(|h| h.label == "cancel"));

        // Nothing optional left to drop: the required hints are kept.
        assert!(!hints_that_fit(HINTS_IDLE, 1).is_empty());
    }

    #[test]
    fn the_count_explains_an_empty_list() {
        // Nothing typed: the size of the list.
        assert_eq!(count_label("", 4, 4), "4 worktrees");
        assert_eq!(count_label("", 1, 1), "1 worktree");
        // Typing: how much of it survived, so filtering everything away reads
        // as `0 of 4` instead of an unexplained blank screen.
        assert_eq!(count_label("bill", 1, 4), "1 of 4");
        assert_eq!(count_label("zzz", 0, 4), "0 of 4");
    }

    #[test]
    fn the_column_headers_are_ascii() {
        for header in [NAME_HEADER, HEAD_HEADER, STATUS_HEADER, PENDING] {
            assert!(header.is_ascii(), "{header}");
        }
    }

    #[test]
    fn rows_start_without_a_status() {
        // The list is drawn before git is asked anything slow, so the status
        // column starts empty and fills in later.
        let p = picker();
        assert!(p.items.iter().all(|i| i.note.is_none()));
    }

    #[test]
    fn the_placeholder_is_ascii_and_says_what_typing_does() {
        assert!(PLACEHOLDER.is_ascii(), "{PLACEHOLDER}");
        assert!(PLACEHOLDER.contains("filter"), "{PLACEHOLDER}");
    }

    #[test]
    fn backspace_removes_a_worktree_when_nothing_is_typed() {
        let mut p = picker();
        assert_eq!(p.backspace(), Backspace::Delete);
    }

    #[test]
    fn backspace_edits_the_filter_while_there_is_one() {
        let mut p = picker();
        p.push_filter('a');
        p.push_filter('u');
        assert_eq!(p.backspace(), Backspace::ErasedFilter);
        assert_eq!(p.backspace(), Backspace::ErasedFilter);
        assert_eq!(p.filter, "");
    }

    #[test]
    fn holding_backspace_to_clear_cannot_run_into_a_deletion() {
        let mut p = picker();
        for c in "auth".chars() {
            p.push_filter(c);
        }
        // The burst that empties the filter.
        for _ in 0..4 {
            assert_eq!(p.backspace(), Backspace::ErasedFilter);
        }
        // The overshoot from key repeat is swallowed instead of deleting.
        assert_eq!(p.backspace(), Backspace::Absorbed);
        // A deliberate press after that still works.
        assert_eq!(p.backspace(), Backspace::Delete);
    }

    #[test]
    fn any_other_key_ends_the_erasing_streak() {
        let mut p = picker();
        p.push_filter('a');
        assert_eq!(p.backspace(), Backspace::ErasedFilter);
        // Moving the cursor means the next Backspace is a fresh intent.
        p.note_other_key();
        assert_eq!(p.backspace(), Backspace::Delete);
    }

    #[test]
    fn rows_are_padded_so_a_highlight_covers_the_line() {
        assert_eq!(fit("abc", 6), "abc   ");
        assert_eq!(fit("abcdefgh", 4), "abcd");
        assert_eq!(fit("", 3), "   ");
    }

    #[test]
    fn filter_matches_name_and_path_case_insensitively() {
        let mut p = picker();
        p.filter = "AUTH".to_string();
        assert_eq!(p.matches().len(), 1);
        assert_eq!(p.selected().unwrap().name, "feature/auth");

        p.filter = "feature".to_string();
        assert_eq!(p.matches().len(), 2);

        p.filter = "/wt/hot".to_string();
        assert_eq!(p.selected().unwrap().name, "hotfix");
    }

    #[test]
    fn cursor_wraps_around() {
        let mut p = picker();
        p.move_up();
        assert_eq!(p.selected().unwrap().name, "hotfix");
        p.move_down();
        assert_eq!(p.selected().unwrap().name, "@");
    }

    #[test]
    fn cursor_stays_inside_a_shrinking_list() {
        let mut p = picker();
        p.cursor = 3;
        for c in "feature".chars() {
            p.push_filter(c);
        }
        // Only the two `feature/*` rows remain, so the cursor cannot stay at 3.
        assert_eq!(p.matches().len(), 2);
        assert_eq!(p.cursor, 1);
        assert_eq!(p.selected().unwrap().name, "feature/billing");
    }

    #[test]
    fn no_match_leaves_nothing_selected() {
        let mut p = picker();
        p.filter = "zzz".to_string();
        p.clamp();
        assert!(p.selected().is_none());
    }

    #[test]
    fn backspace_restores_matches() {
        let mut p = picker();
        p.push_filter('z');
        assert!(p.selected().is_none());
        p.pop_filter();
        assert_eq!(p.matches().len(), 4);
    }

    #[test]
    fn scrolling_follows_the_cursor() {
        let mut p = picker();
        p.cursor = 3;
        p.scroll_into_view(2);
        assert_eq!(p.offset, 2);
        p.cursor = 0;
        p.scroll_into_view(2);
        assert_eq!(p.offset, 0);
    }

    #[test]
    fn a_terminal_with_no_room_for_the_list_does_not_scroll_it() {
        // Three rows of chrome on a three-row terminal leaves a window of
        // nothing; scrolling into it would put the offset past the last item.
        let mut p = picker();
        p.cursor = 3;
        p.scroll_into_view(0);
        assert_eq!(p.offset, 0);
    }

    #[test]
    fn delete_is_bound_to_both_delete_and_ctrl_d() {
        let del = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
        let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(matches!(key_action(&del), Action::Delete));
        assert!(matches!(key_action(&ctrl_d), Action::Delete));
        // Backspace must stay with the filter, not delete a worktree.
        let backspace = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(matches!(key_action(&backspace), Action::Backspace));
    }

    #[test]
    fn plain_characters_type_into_the_filter() {
        let a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(matches!(key_action(&a), Action::Insert('a')));
        // Ctrl-n/p navigate instead of typing.
        let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert!(matches!(key_action(&ctrl_n), Action::Down));
    }

    #[test]
    fn every_way_out_of_the_picker_is_bound() {
        assert_eq!(key_action(&key(KeyCode::Enter)), Action::Confirm);
        assert_eq!(key_action(&key(KeyCode::Esc)), Action::Cancel);
        assert_eq!(key_action(&ctrl('c')), Action::Cancel);
        assert_eq!(key_action(&ctrl('g')), Action::Cancel);
        assert_eq!(key_action(&key(KeyCode::Up)), Action::Up);
        assert_eq!(key_action(&key(KeyCode::Down)), Action::Down);
        assert_eq!(key_action(&ctrl('p')), Action::Up);
        // A terminal told to send 0x08 for the erase key surfaces it as Ctrl-H,
        // which has to reach the filter rather than doing nothing.
        assert_eq!(key_action(&ctrl('h')), Action::Backspace);
        // Anything unbound is inert rather than typed into the filter.
        assert_eq!(key_action(&key(KeyCode::F(1))), Action::None);
        assert_eq!(key_action(&ctrl('z')), Action::None);
    }

    // --- what a key does to the list ------------------------------------

    #[test]
    fn enter_hands_back_the_selected_path() {
        let mut p = picker();
        p.cursor = 1;
        match step(&mut p, Action::Confirm) {
            Step::Done(Outcome::Selected(path)) => {
                assert_eq!(path, PathBuf::from("/wt/feature/auth"))
            }
            _ => panic!("Enter should have chosen the row under the cursor"),
        }
    }

    #[test]
    fn enter_on_nothing_leaves_the_picker_open() {
        // Filtering everything away must not be a way to exit with no answer.
        let mut p = picker();
        p.filter = "zzz".to_string();
        p.clamp();
        assert!(matches!(step(&mut p, Action::Confirm), Step::Stay));
    }

    #[test]
    fn escape_ends_the_picker_without_moving() {
        let mut p = picker();
        assert!(matches!(
            step(&mut p, Action::Cancel),
            Step::Done(Outcome::Cancelled)
        ));
    }

    #[test]
    fn typing_and_moving_only_change_the_list() {
        let mut p = picker();
        assert!(matches!(step(&mut p, Action::Insert('f')), Step::Stay));
        assert_eq!(p.filter, "f");
        assert!(matches!(step(&mut p, Action::Down), Step::Stay));
        assert_eq!(p.selected().unwrap().name, "feature/billing");
        assert!(matches!(step(&mut p, Action::Up), Step::Stay));
        assert_eq!(p.selected().unwrap().name, "feature/auth");
        assert!(matches!(step(&mut p, Action::None), Step::Stay));
    }

    #[test]
    fn only_a_deliberate_backspace_reaches_the_dialog() {
        let mut p = picker();
        // While there is a filter, Backspace edits it.
        assert!(matches!(step(&mut p, Action::Insert('a')), Step::Stay));
        assert!(matches!(step(&mut p, Action::Backspace), Step::Stay));
        // The press that overshoots the empty filter is swallowed.
        assert!(matches!(step(&mut p, Action::Backspace), Step::Stay));
        // The one after it is meant.
        assert!(matches!(step(&mut p, Action::Backspace), Step::Delete));
        // Ctrl-D never has to wait for that, since it has only one job.
        assert!(matches!(step(&mut p, Action::Delete), Step::Delete));
    }

    #[test]
    fn moving_the_cursor_ends_the_erasing_streak() {
        // `step` is where the streak is cleared, so the swallowed press cannot
        // survive a key that was clearly not part of the burst.
        let mut p = picker();
        step(&mut p, Action::Insert('a'));
        step(&mut p, Action::Backspace);
        step(&mut p, Action::Down);
        assert!(matches!(step(&mut p, Action::Backspace), Step::Delete));
    }

    // --- the frame the picker draws -------------------------------------

    const WIDE: Size = Size { cols: 80, rows: 24 };

    #[test]
    fn the_frame_puts_the_header_over_the_rows_it_labels() {
        let mut p = picker();
        p.items[0].is_current = true;
        p.items[1].note = Some("dirty, merged".to_string());
        let frame = frame_of(|out| draw(out, &mut p, None, WIDE));

        assert!(frame.row(0).starts_with("  WORKTREE"), "{:?}", frame.row(0));
        assert!(frame.row(0).contains("HEAD"));
        assert!(frame.row(0).contains("STATUS"));
        // The four rows follow it, in order, one per line.
        assert!(frame.row(1).starts_with("* @"), "{:?}", frame.row(1));
        assert!(frame.row(2).contains("feature/auth"));
        assert!(frame.row(2).contains("dirty, merged"));
        assert!(frame.row(3).contains("feature/billing"));
        assert!(frame.row(4).contains("hotfix"));
        // The marker column says where you are standing; only one row can.
        assert!(!frame.row(2).starts_with('*'), "{:?}", frame.row(2));
    }

    #[test]
    fn a_status_still_being_worked_out_says_so() {
        let mut p = picker();
        // What the first frame shows: nothing has come back from the feed yet.
        let frame = frame_of(|out| draw(out, &mut p, None, WIDE));
        assert!(frame.row(1).contains(PENDING), "{:?}", frame.row(1));

        // And once it has: an answer of "nothing to report" leaves the column
        // blank, which is what the placeholder was there to keep apart from
        // the wait.
        p.items[0].note = Some(String::new());
        p.items[1].note = Some("dirty".to_string());
        let frame = frame_of(|out| draw(out, &mut p, None, WIDE));
        assert!(!frame.row(1).contains(PENDING), "{:?}", frame.row(1));
        assert_eq!(frame.row(1).trim_end(), "  @                abc1234");
        assert!(frame.row(2).contains("dirty"), "{:?}", frame.row(2));
    }

    #[test]
    fn the_cursor_row_is_highlighted_edge_to_edge() {
        let mut p = picker();
        p.cursor = 2;
        let frame = frame_of(|out| draw(out, &mut p, None, WIDE));

        let row = frame
            .highlighted
            .iter()
            .find(|h| h.contains("feature/billing"))
            .expect("the row under the cursor is drawn in reverse video");
        // Padded to the full width, so the highlight is a bar and not a
        // ragged patch the length of the branch name.
        assert_eq!(row.chars().count(), WIDE.cols);
    }

    #[test]
    fn a_narrow_terminal_cuts_the_rows_rather_than_wrapping_them() {
        // Wrapping would push the prompt and the help line off the bottom.
        let mut p = picker();
        let size = Size { cols: 24, rows: 24 };
        let frame = frame_of(|out| draw(out, &mut p, None, size));

        for row in 0..=4 {
            assert_eq!(
                frame.row(row).chars().count(),
                size.cols,
                "row {row}: {:?}",
                frame.row(row)
            );
        }
    }

    #[test]
    fn a_message_sits_above_the_prompt_and_is_cut_to_the_width() {
        let mut p = picker();
        let long = "failed to remove `feature/auth`: ".to_string() + &"x".repeat(200);
        let frame = frame_of(|out| draw(out, &mut p, Some(&long), WIDE));

        // Rows 21, 22, 23 of a 24-row terminal: message, prompt, hints.
        assert!(frame.row(21).starts_with("failed to remove"));
        assert_eq!(frame.row(21).chars().count(), WIDE.cols);
        assert!(frame.row(22).starts_with("> "));
        assert!(frame.row(23).contains("cancel"));
    }

    #[test]
    fn the_list_shrinks_to_leave_room_for_a_message() {
        // Four rows of terminal: header, one row, prompt, hints — and with a
        // message, the list gives up its last row rather than the help line.
        let mut p = picker();
        let size = Size { cols: 80, rows: 5 };
        let frame = frame_of(|out| draw(out, &mut p, None, size));
        assert!(frame.row(2).contains("feature/auth"));

        let frame = frame_of(|out| draw(out, &mut p, Some("removed `hotfix`"), size));
        assert!(!frame.row(2).contains("feature/auth"), "{:?}", frame.row(2));
        assert!(frame.row(2).starts_with("removed `hotfix`"));
    }

    #[test]
    fn the_view_scrolls_to_keep_the_cursor_on_screen() {
        let mut p = picker();
        p.cursor = 3;
        // Two rows of list, so the first two items have to scroll away.
        let size = Size { cols: 80, rows: 5 };
        let frame = frame_of(|out| draw(out, &mut p, None, size));

        assert!(
            frame.row(1).contains("feature/billing"),
            "{:?}",
            frame.row(1)
        );
        assert!(frame.row(2).contains("hotfix"), "{:?}", frame.row(2));
        assert!(!frame.text().contains(" @ "), "{}", frame.text());
    }

    #[test]
    fn the_prompt_offers_the_placeholder_and_the_size_of_the_list() {
        let p = picker();
        let frame = frame_of(|out| draw_prompt(out, &p, 4, 80));
        let line = frame.row(0);
        assert!(line.starts_with("> "), "{line:?}");
        assert!(line.contains(PLACEHOLDER), "{line:?}");
        // The count is right-aligned against the width it was given.
        assert!(line.ends_with("4 worktrees"), "{line:?}");
        assert_eq!(line.chars().count(), 80);
    }

    #[test]
    fn typing_replaces_the_placeholder_with_what_survived() {
        let mut p = picker();
        p.push_filter('f');
        let frame = frame_of(|out| draw_prompt(out, &p, 2, 80));
        let line = frame.row(0);
        assert!(line.starts_with("> f"), "{line:?}");
        assert!(!line.contains(PLACEHOLDER), "{line:?}");
        assert!(line.ends_with("2 of 4"), "{line:?}");
    }

    #[test]
    fn the_count_is_dropped_rather_than_wrapped_onto_the_next_line() {
        let p = picker();
        let frame = frame_of(|out| draw_prompt(out, &p, 4, 10));
        assert!(!frame.row(0).contains("worktrees"), "{:?}", frame.row(0));
    }

    #[test]
    fn the_filter_line_always_shows_a_block_cursor() {
        // The terminal's own cursor is hidden for the whole picker, so without
        // this the empty filter line is a lone `>` with no sign it takes input.
        let p = picker();
        let frame = frame_of(|out| draw_prompt(out, &p, 4, 80));
        assert!(frame.highlighted.contains(&" ".to_string()));
    }

    #[test]
    fn each_key_in_the_help_line_gets_its_own_badge() {
        let hints: Vec<&Hint> = HINTS_IDLE.iter().collect();
        let frame = frame_of(|out| draw_hints(out, &hints));

        assert_eq!(frame.row(0).trim_start(), frame.row(0).trim_start());
        assert!(frame.row(0).contains(" enter  cd"), "{:?}", frame.row(0));
        // Two keys for one label, each in a badge of its own.
        assert!(frame.highlighted.contains(&" ctrl-d ".to_string()));
        assert!(frame.highlighted.contains(&" backspace ".to_string()));
    }

    // --- the confirmation dialog ----------------------------------------

    fn worktree(path: &str, branch: Option<&str>) -> Worktree {
        Worktree {
            path: PathBuf::from(path),
            head: Some("abc1234".to_string()),
            branch: branch.map(str::to_string),
            bare: false,
            detached: branch.is_none(),
            locked: false,
            prunable: false,
        }
    }

    #[test]
    fn the_dialog_names_the_worktree_and_the_state_of_its_branch() {
        let wt = worktree("/wt/feature/auth", Some("feature/auth"));
        let frame = frame_of(|out| draw_confirm(out, &wt, false, true, WIDE));

        assert_eq!(frame.row(0), "Remove this worktree?");
        assert_eq!(frame.row(2), "  /wt/feature/auth");
        assert_eq!(frame.row(4), "  branch `feature/auth` (merged)");
        assert!(frame.row(23).contains("[b] remove worktree and branch"));
    }

    #[test]
    fn an_unmerged_branch_says_so_in_capitals() {
        // The word is the whole warning, so it has to survive being skimmed.
        let wt = worktree("/wt/hotfix", Some("hotfix"));
        let frame = frame_of(|out| draw_confirm(out, &wt, false, false, WIDE));
        assert_eq!(frame.row(4), "  branch `hotfix` (NOT merged)");
    }

    #[test]
    fn uncommitted_work_is_warned_about_above_the_branch() {
        let wt = worktree("/wt/hotfix", Some("hotfix"));
        let frame = frame_of(|out| draw_confirm(out, &wt, true, false, WIDE));

        assert_eq!(frame.row(4), "  ! uncommitted changes will be lost");
        // The branch line moves down rather than being written over.
        assert_eq!(frame.row(5), "  branch `hotfix` (NOT merged)");
    }

    #[test]
    fn a_detached_worktree_is_not_offered_a_branch_to_delete() {
        let wt = worktree("/wt/detached", None);
        let frame = frame_of(|out| draw_confirm(out, &wt, false, false, WIDE));

        assert!(!frame.text().contains("branch"), "{}", frame.text());
        assert_eq!(frame.row(23), "[y] remove worktree   [n] cancel");
    }

    #[test]
    fn the_dialog_takes_yes_in_either_case() {
        for c in ['y', 'Y'] {
            assert_eq!(
                reply(&key(KeyCode::Char(c)), true),
                Some(Reply::Remove { with_branch: false })
            );
        }
        for c in ['b', 'B'] {
            assert_eq!(
                reply(&key(KeyCode::Char(c)), true),
                Some(Reply::Remove { with_branch: true })
            );
        }
    }

    #[test]
    fn without_a_branch_the_branch_key_does_nothing() {
        // The key line does not offer `b`, so pressing it is a typo, and a typo
        // must not be read as a bigger yes than the one on offer.
        assert_eq!(reply(&key(KeyCode::Char('b')), false), None);
        assert_eq!(
            reply(&key(KeyCode::Char('y')), false),
            Some(Reply::Remove { with_branch: false })
        );
    }

    #[test]
    fn every_way_of_backing_out_of_the_dialog_cancels() {
        for k in [
            key(KeyCode::Char('n')),
            key(KeyCode::Char('N')),
            key(KeyCode::Esc),
            ctrl('c'),
        ] {
            assert_eq!(reply(&k, true), Some(Reply::Cancel), "{k:?}");
        }
    }

    #[test]
    fn an_unrelated_key_leaves_the_question_on_screen() {
        assert_eq!(reply(&key(KeyCode::Char('q')), true), None);
        assert_eq!(reply(&key(KeyCode::Enter), true), None);

        // Windows reports a release for every press; answering on both would
        // count one keystroke twice.
        let mut release = key(KeyCode::Char('y'));
        release.kind = KeyEventKind::Release;
        assert_eq!(reply(&release, true), None);
    }

    #[test]
    fn the_working_line_says_what_is_taking_so_long() {
        let frame = frame_of(|out| working(out, "Removing /wt/hotfix...", WIDE));
        assert_eq!(frame.row(0), "Removing /wt/hotfix...");
    }

    // --- the clean screen -----------------------------------------------

    fn candidate(name: &str, state: State, note: &str) -> Candidate {
        Candidate {
            worktree: worktree(&format!("/wt/{name}"), Some(name)),
            name: name.to_string(),
            state,
            note: note.to_string(),
        }
    }

    fn candidates() -> Vec<Candidate> {
        vec![
            candidate("feature/auth", State::Done, "merged into main"),
            candidate("feature/billing", State::Local, "3 commits nowhere else"),
            candidate("hotfix", State::Dirty, "uncommitted changes"),
        ]
    }

    #[test]
    fn the_clean_screen_shows_a_verdict_and_a_box_per_row() {
        let list = candidates();
        let ticked = [true, false, false];
        let frame = frame_of(|out| draw_clean(out, &list, &ticked, 0, false, WIDE));

        assert_eq!(frame.row(0).trim_end(), "Select worktrees to remove");
        assert!(frame.row(2).contains("WORKTREE"), "{:?}", frame.row(2));
        assert!(frame.row(2).contains("SAFE TO REMOVE"));
        assert!(
            frame.row(3).contains("[x] feature/auth"),
            "{:?}",
            frame.row(3)
        );
        assert!(frame.row(4).contains("[ ] feature/billing"));
        assert!(frame.row(4).contains("yes (local)"));
        assert!(frame.row(5).contains("no (dirty)"), "{:?}", frame.row(5));
        assert!(frame.row(5).contains("uncommitted changes"));
        // The tally is what tells you Enter is about to remove one thing.
        assert!(frame.row(22).starts_with("1 of 3 selected"));
        assert!(frame.row(23).contains("toggle"));
    }

    #[test]
    fn with_branch_turns_a_local_branch_into_a_no() {
        // Removing the branch takes its commits with it, which nothing else
        // on the screen would have warned about.
        let list = candidates();
        let ticked = [false, false, false];
        let frame = frame_of(|out| draw_clean(out, &list, &ticked, 0, true, WIDE));
        assert!(frame.row(4).contains("no (local)"), "{:?}", frame.row(4));
    }

    #[test]
    fn the_clean_cursor_row_is_marked_and_highlighted() {
        let list = candidates();
        let ticked = [false, false, false];
        let frame = frame_of(|out| draw_clean(out, &list, &ticked, 1, false, WIDE));

        assert!(frame.row(4).starts_with("> "), "{:?}", frame.row(4));
        assert!(frame.row(3).starts_with("  "), "{:?}", frame.row(3));
        let row = frame
            .highlighted
            .iter()
            .find(|h| h.contains("feature/billing"))
            .expect("the row under the cursor is drawn in reverse video");
        assert_eq!(row.chars().count(), WIDE.cols);
    }

    #[test]
    fn a_short_terminal_stops_before_the_tally_it_would_overwrite() {
        let list = candidates();
        let ticked = [false, false, false];
        let size = Size { cols: 80, rows: 6 };
        let frame = frame_of(|out| draw_clean(out, &list, &ticked, 0, false, size));

        assert!(frame.row(3).contains("feature/auth"));
        // Row 4 is the last one that fits; the tally owns row 4 of a 6-row
        // terminal, so the third candidate is dropped rather than drawn over it.
        assert!(!frame.text().contains("hotfix"), "{}", frame.text());
        assert!(frame.row(4).starts_with("0 of 3 selected"));
    }

    #[test]
    fn space_ticks_the_row_under_the_cursor() {
        let mut ticked = vec![false, false, false];
        let mut at = 1;
        assert_eq!(
            clean_step(&key(KeyCode::Char(' ')), &mut ticked, &mut at),
            CleanStep::Stay
        );
        assert_eq!(ticked, [false, true, false]);
        // And unticks it, so a mis-selection costs one keystroke.
        clean_step(&key(KeyCode::Char(' ')), &mut ticked, &mut at);
        assert_eq!(ticked, [false, false, false]);
    }

    #[test]
    fn the_clean_cursor_wraps_at_both_ends() {
        let mut ticked = vec![false, false, false];
        let mut at = 0;
        clean_step(&key(KeyCode::Up), &mut ticked, &mut at);
        assert_eq!(at, 2);
        clean_step(&key(KeyCode::Down), &mut ticked, &mut at);
        assert_eq!(at, 0);
        clean_step(&ctrl('n'), &mut ticked, &mut at);
        assert_eq!(at, 1);
        clean_step(&ctrl('p'), &mut ticked, &mut at);
        assert_eq!(at, 0);
    }

    #[test]
    fn enter_accepts_the_selection_and_escape_throws_it_away() {
        let mut ticked = vec![true, false, true];
        let mut at = 0;
        assert_eq!(
            clean_step(&key(KeyCode::Enter), &mut ticked, &mut at),
            CleanStep::Accept
        );
        for k in [key(KeyCode::Esc), ctrl('c'), ctrl('g')] {
            assert_eq!(
                clean_step(&k, &mut ticked, &mut at),
                CleanStep::Cancel,
                "{k:?}"
            );
        }
        // An unbound key changes nothing at all.
        assert_eq!(
            clean_step(&key(KeyCode::Char('x')), &mut ticked, &mut at),
            CleanStep::Stay
        );
        assert_eq!(ticked, [true, false, true]);
        assert_eq!(at, 0);

        let mut release = key(KeyCode::Enter);
        release.kind = KeyEventKind::Release;
        assert_eq!(
            clean_step(&release, &mut ticked, &mut at),
            CleanStep::Stay,
            "a release must not confirm what the press already did"
        );
    }

    // --- against a real repository ---------------------------------------

    /// Runs git in `dir`, deaf to whatever repository the test runner is in.
    ///
    /// `cargo test` from `.githooks/pre-commit` already has `GIT_DIR` exported,
    /// and git reads it before the working directory — so without this the
    /// fixtures would reconfigure the real checkout.
    fn git_in(dir: &Path, args: &[&str]) {
        let mut cmd = std::process::Command::new("git");
        cmd.current_dir(dir);
        for var in git::REPO_ENV {
            cmd.env_remove(var);
        }
        for var in ["GIT_CONFIG", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
            cmd.env_remove(var);
        }
        let out = cmd.args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repository with one commit on `main`, plus a worktree per branch.
    struct Fixture {
        _tmp: tempfile::TempDir,
        main: PathBuf,
    }

    impl Fixture {
        fn new(branches: &[&str]) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            // macOS hands out /var/... symlinks; git reports the resolved path.
            let root = tmp.path().canonicalize().unwrap();
            let main = root.join("repo");

            git_in(&root, &["init", "--initial-branch=main", "repo"]);
            git_in(&main, &["config", "user.email", "test@example.com"]);
            git_in(&main, &["config", "user.name", "Test"]);
            std::fs::write(main.join("README.md"), "hello\n").unwrap();
            git_in(&main, &["add", "."]);
            git_in(&main, &["commit", "-m", "init"]);

            for branch in branches {
                let path = root.join("worktrees").join(branch);
                git_in(
                    &main,
                    &["worktree", "add", "-b", branch, path.to_str().unwrap()],
                );
            }
            Self { _tmp: tmp, main }
        }

        fn repo(&self) -> Repo {
            Repo {
                cwd: self.main.clone(),
                main: self.main.clone(),
                config: Config::default(),
            }
        }

        fn worktree(&self, branch: &str) -> PathBuf {
            self.main.parent().unwrap().join("worktrees").join(branch)
        }
    }

    #[test]
    fn the_list_leads_with_the_main_worktree_and_marks_where_you_are() {
        let fixture = Fixture::new(&["feature/auth"]);
        let items = load(&fixture.repo()).unwrap();

        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "@");
        assert!(items[0].is_main);
        assert!(items[0].is_current, "cwd is the main worktree");
        assert_eq!(items[1].name, "feature/auth");
        assert!(!items[1].is_main);
        assert!(!items[1].is_current);
        // Seven characters of the real commit, not a placeholder.
        assert_eq!(items[0].head.len(), 7);
        // The status column is filled in later, off the drawing path.
        assert!(items.iter().all(|i| i.note.is_none()));
    }

    #[test]
    fn a_worktree_you_are_standing_in_is_the_current_one() {
        let fixture = Fixture::new(&["feature/auth"]);
        let mut repo = fixture.repo();
        repo.cwd = fixture.worktree("feature/auth");
        let items = load(&repo).unwrap();

        assert!(!items[0].is_current);
        assert!(items[1].is_current);
    }

    #[test]
    fn the_status_column_reports_what_removing_would_cost() {
        let fixture = Fixture::new(&["feature/auth"]);
        let merges = git::MergeState::read(&fixture.main).unwrap();

        // A branch off main with no commits of its own is already merged.
        assert_eq!(
            note(
                &fixture.worktree("feature/auth"),
                Some("feature/auth"),
                Some(&merges),
                false,
                &[]
            ),
            "merged"
        );
        // Uncommitted work leads, since it is the only thing a removal loses.
        std::fs::write(fixture.worktree("feature/auth").join("wip.txt"), "wip\n").unwrap();
        assert_eq!(
            note(
                &fixture.worktree("feature/auth"),
                Some("feature/auth"),
                Some(&merges),
                false,
                &["locked"]
            ),
            "dirty, merged, locked"
        );
        // The main worktree's branch is merged into itself, which says nothing.
        assert_eq!(
            note(&fixture.main, Some("main"), Some(&merges), true, &[]),
            ""
        );
    }

    #[test]
    fn flags_repeat_only_what_git_says_about_the_worktree() {
        let plain = worktree("/wt/hotfix", Some("hotfix"));
        assert!(flags(&plain).is_empty());

        let mut odd = plain.clone();
        odd.bare = true;
        odd.detached = true;
        odd.locked = true;
        assert_eq!(flags(&odd), ["bare", "detached", "locked"]);
    }

    #[test]
    fn the_status_feed_fills_the_column_in_the_background() {
        let fixture = Fixture::new(&["feature/auth"]);
        let repo = fixture.repo();
        let mut items = load(&repo).unwrap();
        let mut feed = StatusFeed::spawn(&repo, &items);

        // Nothing is waited for on the drawing path, so this is the loop's
        // poll, bounded so a hang fails the test instead of it.
        let start = std::time::Instant::now();
        while !feed.drain(&mut items) {
            assert!(!feed.is_done(), "the feed finished without reporting");
            assert!(start.elapsed() < std::time::Duration::from_secs(30));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(feed.is_done());
        // Drained once and only once; a second call has nothing to redraw for.
        assert!(!feed.drain(&mut items));

        // The main worktree is clean and nothing else is worth saying about
        // it, which is an answer — not the absence of one.
        assert_eq!(items[0].note.as_deref(), Some(""));
        assert_eq!(items[1].note.as_deref(), Some("merged"));
    }

    #[test]
    fn the_dialog_is_not_opened_for_a_worktree_git_would_refuse() {
        let fixture = Fixture::new(&["feature/auth"]);
        let repo = fixture.repo();
        let mut p = Picker::new(load(&repo).unwrap());

        // The main worktree, and the one you are standing in, cannot go.
        match pending(&repo, &p) {
            Some(Pending::Blocked(line)) => {
                assert_eq!(line, "cannot remove `@`: this is the main worktree")
            }
            _ => panic!("the main worktree must not reach the dialog"),
        }

        // Nothing selected, nothing to ask about.
        p.filter = "zzz".to_string();
        p.clamp();
        assert!(pending(&repo, &p).is_none());
    }

    #[test]
    fn the_dialog_is_told_what_the_removal_would_cost() {
        let fixture = Fixture::new(&["feature/auth"]);
        let repo = fixture.repo();
        let mut p = Picker::new(load(&repo).unwrap());
        p.cursor = 1;

        let Some(Pending::Ask(subject)) = pending(&repo, &p) else {
            panic!("a removable worktree should reach the dialog")
        };
        assert_eq!(subject.label, "feature/auth");
        assert!(subject.merged, "a branch with no commits of its own");
        assert!(!subject.dirty);

        std::fs::write(fixture.worktree("feature/auth").join("wip.txt"), "wip\n").unwrap();
        let Some(Pending::Ask(subject)) = pending(&repo, &p) else {
            panic!("uncommitted work is a warning, not a refusal")
        };
        assert!(subject.dirty);
    }

    #[test]
    fn removing_from_the_picker_reloads_the_list_and_says_what_went() {
        let fixture = Fixture::new(&["feature/auth", "hotfix"]);
        let repo = fixture.repo();
        let mut p = Picker::new(load(&repo).unwrap());
        p.cursor = 2;
        let Some(Pending::Ask(subject)) = pending(&repo, &p) else {
            panic!("hotfix should be removable")
        };

        let line = remove_picked(&repo, &mut p, &subject, true).unwrap();
        assert_eq!(line, "removed `hotfix` and its branch");
        assert!(!git::local_branch_exists(&repo.main, "hotfix"));

        // The list is reloaded, and the cursor pulled back inside it.
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.cursor, 1);
        assert_eq!(p.selected().unwrap().name, "feature/auth");
    }

    #[test]
    fn keeping_the_branch_is_said_out_loud_too() {
        let fixture = Fixture::new(&["feature/auth"]);
        let repo = fixture.repo();
        let mut p = Picker::new(load(&repo).unwrap());
        p.cursor = 1;
        let Some(Pending::Ask(subject)) = pending(&repo, &p) else {
            panic!("feature/auth should be removable")
        };

        let line = remove_picked(&repo, &mut p, &subject, false).unwrap();
        assert_eq!(line, "removed `feature/auth`");
        assert!(git::local_branch_exists(&repo.main, "feature/auth"));
        assert_eq!(p.items.len(), 1);
    }

    #[test]
    fn a_failed_removal_reports_it_instead_of_ending_the_picker() {
        // The picker is a list you are browsing; a git failure has to land in
        // the status line, not close the screen out from under you.
        let fixture = Fixture::new(&["feature/auth"]);
        let repo = fixture.repo();
        let mut p = Picker::new(load(&repo).unwrap());
        p.cursor = 1;
        let Some(Pending::Ask(mut subject)) = pending(&repo, &p) else {
            panic!("feature/auth should be removable")
        };
        // A path git knows nothing about: the removal fails, the list stands.
        subject.worktree.path = fixture.main.parent().unwrap().join("gone");

        let line = remove_picked(&repo, &mut p, &subject, false).unwrap();
        assert!(
            line.starts_with("failed to remove `feature/auth`:"),
            "{line}"
        );
        assert_eq!(p.items.len(), 2);
    }
}
