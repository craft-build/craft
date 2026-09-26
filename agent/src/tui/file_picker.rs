//! File picker (F.3, Ctrl-S): a fuzzy path matcher over an async walkdir of
//! the session's cwd. Ported from the reference
//! `craft-ui/src/components/file_picker.rs`, adapted to this repo: the
//! reference's `nucleo` matcher harness is replaced by `nucleo-matcher` run
//! synchronously over the walked corpus (as the search modal does), and the
//! render lives with the other overlays (`ui::overlays`).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use tracing::warn;

pub(crate) const MODAL_TITLE: &str = " Files ";
pub(crate) const MODAL_TITLE_WALKING: &str = " Files (scanning…) ";
const SEARCH_ROW: u16 = 1;
const SEARCH_PREFIX: &str = "/ ";
const NO_MATCHES: &str = "  No matches";
const LABEL_INDENT: &str = "  ";
/// Not "empty": a directory full of ignored files walks up just as short.
const NOTHING_TO_PICK_MSG: &str = "Nothing to pick in the current directory";
pub(crate) const UNREADABLE_DIR_MSG: &str = "Cannot list the current directory";
const WALKER_CRASHED_MSG: &str = "File scanner crashed";
const PENDING_DEBOUNCE_MS: u128 = 100;
const MAX_MATERIALIZED: usize = 640;

enum WalkerMsg {
    Path(String),
    End(WalkEnd),
}

/// The walker's verdict on the root, which only matters when the list came
/// up empty: an empty directory, a fully ignored one and one we could not
/// open look identical from the injected paths alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WalkEnd {
    Listed,
    Unreadable,
}

impl WalkEnd {
    /// What to tell the user when the walk is over and the list is still
    /// empty. Total over the state, so no ending can be forgotten.
    fn nothing_found_msg(self) -> &'static str {
        match self {
            Self::Listed => NOTHING_TO_PICK_MSG,
            Self::Unreadable => UNREADABLE_DIR_MSG,
        }
    }
}

pub enum FilePickerAction {
    Consumed,
    Select(String),
    Close,
}

pub(crate) struct Match {
    pub(crate) path: String,
    pub(crate) indices: Vec<u32>,
}

pub(crate) struct Session {
    pub(crate) files: Vec<String>,
    pub(crate) matches: Vec<Match>,
    pub(crate) total_matches: usize,
    matcher: Matcher,

    pub(crate) query: String,
    /// Cursor position in chars from the start of `query`.
    pub(crate) cursor: usize,
    pub(crate) selected: usize,
    pub(crate) scroll_offset: usize,
    pub(crate) viewport_height: usize,

    pub(crate) visible: bool,
    pub(crate) started_at: Instant,
    pub(crate) walking: bool,
    rx: std::sync::mpsc::Receiver<WalkerMsg>,
    cancel: Arc<AtomicBool>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub struct FilePicker {
    session: Option<Session>,
}

impl Default for FilePicker {
    fn default() -> Self {
        Self::new()
    }
}

impl FilePicker {
    pub fn new() -> Self {
        Self { session: None }
    }

    pub fn open(&mut self, cwd: &str) {
        self.close();

        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<WalkerMsg>();
        let root = PathBuf::from(cwd);
        let cancel_thread = Arc::clone(&cancel);
        if let Err(e) = thread::Builder::new()
            .name("file-walker".into())
            .spawn(move || {
                walk_root(&root, &cancel_thread, &tx);
            })
        {
            warn!("{WALKER_CRASHED_MSG}: failed to spawn thread: {e}");
            return;
        }

        self.session = Some(Session {
            files: Vec::new(),
            matches: Vec::new(),
            total_matches: 0,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            query: String::new(),
            cursor: 0,
            selected: 0,
            scroll_offset: 0,
            viewport_height: 0,
            visible: false,
            started_at: Instant::now(),
            walking: true,
            rx,
            cancel,
        });
    }

    pub fn close(&mut self) {
        self.session = None;
    }

    pub fn is_open(&self) -> bool {
        self.session.is_some()
    }

    /// The walk is still running (spinner title + a cadence that keeps the
    /// loop coming back for the paths streaming in).
    pub fn walking(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.walking)
    }

    /// Whether the modal draws yet: only once files have arrived or the
    /// walk has dragged past the debounce.
    pub fn visible(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.visible)
    }

    /// The live session, mutably, for the renderer (viewport sizing and
    /// scroll clamping happen at draw time like the search modal's).
    pub(crate) fn session_mut(&mut self) -> Option<&mut Session> {
        self.session.as_mut()
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        let Some(s) = &mut self.session else {
            return false;
        };
        s.query.insert_str(s.query.chars().count(), text);
        s.cursor = s.query.chars().count();
        refresh_matches(s);
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> FilePickerAction {
        let Some(s) = &mut self.session else {
            return FilePickerAction::Close;
        };
        match key.code {
            KeyCode::Esc => return FilePickerAction::Close,
            KeyCode::Enter => {
                if !s.visible {
                    return FilePickerAction::Consumed;
                }
                if let Some(m) = s.matches.get(s.selected) {
                    return FilePickerAction::Select(m.path.clone());
                }
                return FilePickerAction::Close;
            }
            KeyCode::Up => move_selection(s, -1),
            KeyCode::Down => move_selection(s, 1),
            KeyCode::Char('w') | KeyCode::Backspace
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                delete_word_back(s);
                refresh_matches(s);
            }
            KeyCode::Backspace => {
                if s.cursor > 0 {
                    s.cursor -= 1;
                    let byte = byte_of_cursor(&s.query, s.cursor);
                    let end = next_char_boundary(&s.query, byte);
                    s.query.drain(byte..end);
                    refresh_matches(s);
                }
            }
            KeyCode::Left => s.cursor = s.cursor.saturating_sub(1),
            KeyCode::Right => {
                if s.cursor < s.query.chars().count() {
                    s.cursor += 1;
                }
            }
            KeyCode::Home => s.cursor = 0,
            KeyCode::End => s.cursor = s.query.chars().count(),
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL) => {
                let byte = byte_of_cursor(&s.query, s.cursor);
                s.query.insert(byte, c);
                s.cursor += 1;
                refresh_matches(s);
            }
            // Other chords are swallowed: the picker owns the keyboard.
            _ => {}
        }
        FilePickerAction::Consumed
    }

    /// Polls the walker: paths stream in while the list is on screen, and
    /// the walk's ending decides what an empty list means. Returns the
    /// frame owed plus a message to flash if the picker gave up.
    pub fn tick(&mut self) -> (crate::tui::repaint::Dirty, Option<String>) {
        use crate::tui::repaint::Dirty;
        let Some(s) = self.session.as_mut() else {
            return (Dirty::NO, None);
        };

        let mut dirty = Dirty::NO;
        let mut files_arrived = false;
        let mut ended: Option<WalkEnd> = None;
        loop {
            match s.rx.try_recv() {
                Ok(WalkerMsg::Path(path)) => {
                    files_arrived = true;
                    dirty = Dirty::YES;
                    s.files.push(path);
                }
                Ok(WalkerMsg::End(end)) => ended = Some(end),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    // The walker drops its sender right after `End`; a
                    // disconnect with the verdict already in hand is the
                    // normal end of stream, only a bare one is a crash.
                    if ended.is_none() && s.walking {
                        warn!("{WALKER_CRASHED_MSG}: walker thread panicked");
                        self.session = None;
                        return (Dirty::YES, Some(WALKER_CRASHED_MSG.into()));
                    }
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }

        if ended.is_some() {
            s.walking = false;
            dirty = Dirty::YES; // the title drops "scanning…"
        }

        let has_files = !s.files.is_empty();
        // A walk slow enough to cross the debounce is already on screen
        // when it answers, so this close cannot sit behind the visibility
        // gate below.
        if !has_files && !s.walking {
            let msg = ended.map(WalkEnd::nothing_found_msg);
            self.session = None;
            return (Dirty::YES, msg.map(String::from));
        }

        if !s.visible && (has_files || s.started_at.elapsed().as_millis() >= PENDING_DEBOUNCE_MS) {
            s.visible = true;
            dirty = Dirty::YES;
        }

        if files_arrived {
            refresh_matches(s);
            clamp_selection(s);
        }

        (dirty, None)
    }
}

/// Walks `root` (gitignore-aware, hidden files included, `.git` excluded),
/// depth >= 1, directories carrying a trailing separator, and reports the
/// root's verdict at the end.
fn walk_root(root: &std::path::Path, cancel: &AtomicBool, tx: &std::sync::mpsc::Sender<WalkerMsg>) {
    use ignore::WalkBuilder;
    use ignore::overrides::OverrideBuilder;

    let overrides = OverrideBuilder::new(root)
        .add("!.git")
        .expect("statically valid glob")
        .build()
        .expect("statically valid glob");
    let mut walker = WalkBuilder::new(root)
        .hidden(false)
        .overrides(overrides)
        // Depth 0 is the root, which strips to an empty name: a bare
        // separator at the top of every list, selected by default.
        .min_depth(Some(1))
        .build();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        match walker.next() {
            Some(Ok(entry)) => {
                if !entry
                    .file_type()
                    .is_some_and(|ft| ft.is_file() || ft.is_dir() || ft.is_symlink())
                {
                    continue;
                }
                let path = entry.path().strip_prefix(root).unwrap_or(entry.path());
                let mut name = path.to_string_lossy().into_owned();
                if entry.file_type().is_some_and(|ft| ft.is_dir()) {
                    name.push(std::path::MAIN_SEPARATOR);
                }
                if tx.send(WalkerMsg::Path(name)).is_err() {
                    return; // picker closed
                }
            }
            Some(Err(_)) => continue,
            None => break,
        }
    }
    // Only the directory itself can say whether an empty walk means
    // "nothing to pick" or "I could not even look".
    let end = match root.read_dir() {
        Ok(_) => WalkEnd::Listed,
        Err(e) => {
            warn!("{UNREADABLE_DIR_MSG}: {}: {e}", root.display());
            WalkEnd::Unreadable
        }
    };
    let _ = tx.send(WalkerMsg::End(end));
}

fn refresh_matches(s: &mut Session) {
    s.matches.clear();
    s.selected = 0;
    s.scroll_offset = 0;

    if s.query.trim().is_empty() {
        s.total_matches = s.files.len();
        s.matches = s.files[..s.total_matches.min(MAX_MATERIALIZED)]
            .iter()
            .map(|path| Match {
                path: path.clone(),
                indices: Vec::new(),
            })
            .collect();
        return;
    }

    let atom = Atom::new(
        &s.query,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );
    let mut buf = Vec::new();
    let mut scored: Vec<(u16, String, Vec<u32>)> = Vec::new();
    for path in &s.files {
        let haystack = Utf32Str::new(path, &mut buf);
        let mut indices = Vec::new();
        if let Some(score) = atom.indices(haystack, &mut s.matcher, &mut indices) {
            scored.push((score, path.clone(), indices));
        }
    }
    scored.sort_by_key(|(score, _, _)| std::cmp::Reverse(*score));
    s.total_matches = scored.len();
    s.matches = scored
        .into_iter()
        .take(MAX_MATERIALIZED)
        .map(|(_, path, indices)| Match { path, indices })
        .collect();
}

fn move_selection(s: &mut Session, delta: isize) {
    if s.matches.is_empty() {
        return;
    }
    let new = (s.selected as isize + delta).clamp(0, s.matches.len() as isize - 1);
    s.selected = new as usize;
    ensure_visible(s);
}

fn clamp_selection(s: &mut Session) {
    if s.matches.is_empty() {
        s.selected = 0;
        s.scroll_offset = 0;
    } else {
        s.selected = s.selected.min(s.matches.len() - 1);
        ensure_visible(s);
    }
}

pub(crate) fn ensure_visible(s: &Session) -> usize {
    // Pure helper returning the clamped scroll offset so layout stays
    // testable; the session's own copy is updated by the caller.
    let len = s.matches.len();
    let mut scroll = s.scroll_offset;
    if len > s.viewport_height {
        scroll = scroll.min(len - s.viewport_height);
    } else {
        scroll = 0;
    }
    if s.selected < scroll {
        scroll = s.selected;
    } else if s.viewport_height > 0 && s.selected >= scroll + s.viewport_height {
        scroll = s.selected + 1 - s.viewport_height;
    }
    scroll
}

fn delete_word_back(s: &mut Session) {
    let chars: Vec<char> = s.query.chars().collect();
    let mut i = s.cursor;
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    s.query = chars[..i].iter().collect::<String>() + &chars[s.cursor..].iter().collect::<String>();
    s.cursor = i;
}

fn byte_of_cursor(query: &str, cursor: usize) -> usize {
    query
        .char_indices()
        .nth(cursor)
        .map(|(b, _)| b)
        .unwrap_or(query.len())
}

fn next_char_boundary(s: &str, byte: usize) -> usize {
    s[byte..]
        .char_indices()
        .nth(1)
        .map(|(b, _)| byte + b)
        .unwrap_or(s.len())
}

impl Session {
    /// Clamp the scroll window around the selection (called from render).
    pub(crate) fn apply_scroll(&mut self) {
        self.scroll_offset = ensure_visible(self);
    }
}

pub(crate) const fn search_row_height() -> u16 {
    SEARCH_ROW
}

pub(crate) fn no_matches_label() -> &'static str {
    NO_MATCHES
}

pub(crate) fn search_prefix() -> &'static str {
    SEARCH_PREFIX
}

pub(crate) fn label_indent() -> &'static str {
    LABEL_INDENT
}

pub(crate) const fn max_materialized() -> usize {
    MAX_MATERIALIZED
}

/// One spinner glyph per 80ms, matching the status spinner's table.
pub(crate) fn spinner_frame(elapsed: std::time::Duration) -> char {
    const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    SPINNER[(elapsed.as_millis() / 80) as usize % SPINNER.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::repaint::Dirty;
    use crossterm::event::{KeyEventKind, KeyEventState};
    use std::time::Duration;

    const CONVERGE_TIMEOUT: Duration = Duration::from_secs(30);
    const DEBOUNCE_HELD_OFF: Duration = Duration::from_secs(60);
    const NEVER_CONVERGED: &str = "picker never rebuilt its matches from later ticks";

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// A picker with no walker thread at all: paths and endings are fed
    /// straight into the channel, so every branch of `tick` is drivable.
    fn hand_fed_picker() -> (FilePicker, std::sync::mpsc::Sender<WalkerMsg>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut picker = FilePicker::new();
        picker.session = Some(Session {
            files: Vec::new(),
            matches: Vec::new(),
            total_matches: 0,
            matcher: Matcher::new(Config::DEFAULT.match_paths()),
            query: String::new(),
            cursor: 0,
            selected: 0,
            scroll_offset: 0,
            viewport_height: 0,
            visible: false,
            started_at: Instant::now(),
            walking: true,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
        });
        (picker, tx)
    }

    fn inject_file(picker: &mut FilePicker, path: &str) {
        let s = picker.session.as_mut().unwrap();
        s.files.push(path.to_string());
        refresh_matches(s);
    }

    fn tick_until(picker: &mut FilePicker, ready: impl Fn(&Session) -> bool) -> Option<Dirty> {
        let deadline = Instant::now() + CONVERGE_TIMEOUT;
        let mut dirty = Dirty::NO;
        while Instant::now() < deadline {
            let (owed, _) = picker.tick();
            dirty |= owed;
            if picker.session.as_ref().is_some_and(&ready) {
                return Some(dirty);
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        None
    }

    /// The visibility gate is checked against the wall clock, so park
    /// `started_at` in the future to hold the debounce off however long the
    /// test is descheduled for.
    fn hold_off_debounce(picker: &mut FilePicker) {
        picker.session.as_mut().unwrap().started_at = Instant::now() + DEBOUNCE_HELD_OFF;
    }

    #[test]
    fn walk_root_offers_files_not_the_root_and_excludes_git() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("main.rs"), "").unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src").join("lib.rs"), "").unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".git").join("config"), "").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        walk_root(tmp.path(), &AtomicBool::new(false), &tx);
        drop(tx);
        let msgs: Vec<WalkerMsg> = rx.into_iter().collect();

        let mut paths: Vec<String> = msgs
            .iter()
            .filter_map(|m| match m {
                WalkerMsg::Path(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        paths.sort();
        assert_eq!(paths, ["main.rs", "src/", "src/lib.rs"]);
        assert!(matches!(msgs.last(), Some(WalkerMsg::End(WalkEnd::Listed))));
    }

    #[test]
    fn walk_end_tells_missing_from_listed() {
        let tmp = tempfile::tempdir().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        walk_root(&tmp.path().join("gone"), &AtomicBool::new(false), &tx);
        drop(tx);
        assert!(matches!(
            rx.into_iter().last(),
            Some(WalkerMsg::End(WalkEnd::Unreadable))
        ));
    }

    #[test]
    fn a_real_walk_of_an_empty_directory_closes_the_picker() {
        let tmp = tempfile::tempdir().unwrap();
        let mut picker = FilePicker::new();
        picker.open(&tmp.path().to_string_lossy());

        let deadline = Instant::now() + CONVERGE_TIMEOUT;
        let flash = loop {
            if let (_, Some(flash)) = picker.tick() {
                break flash;
            }
            assert!(
                Instant::now() < deadline,
                "picker never closed on empty walk"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        };
        assert_eq!(flash, NOTHING_TO_PICK_MSG);
        assert!(!picker.is_open());
    }

    #[test]
    fn a_real_walk_offers_the_files_and_typing_filters() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("main.rs"), "").unwrap();
        std::fs::create_dir(tmp.path().join("docs")).unwrap();
        std::fs::write(tmp.path().join("docs").join("readme.md"), "").unwrap();

        let mut picker = FilePicker::new();
        picker.open(&tmp.path().to_string_lossy());
        // Wait for the whole walk: the first path already un-empties the
        // list, but the assertion wants everything the walk found.
        let _ = tick_until(&mut picker, |s| !s.walking).expect(NEVER_CONVERGED);

        {
            let s = picker.session.as_ref().unwrap();
            let mut paths: Vec<&str> = s.matches.iter().map(|m| m.path.as_str()).collect();
            paths.sort();
            assert_eq!(paths, ["docs/", "docs/readme.md", "main.rs"]);
        }

        for c in "readme".chars() {
            picker.handle_key(key(KeyCode::Char(c)));
        }
        let s = picker.session.as_ref().unwrap();
        assert_eq!(s.matches.len(), 1);
        assert_eq!(s.matches[0].path, "docs/readme.md");
        assert!(
            !s.matches[0].indices.is_empty(),
            "matched chars highlighted"
        );
    }

    #[test]
    fn walking_picker_hides_until_files_or_debounce() {
        let (mut picker, _tx) = hand_fed_picker();
        hold_off_debounce(&mut picker);
        let _ = picker.tick();
        assert!(!picker.session.as_ref().unwrap().visible, "hidden so far");

        inject_file(&mut picker, "src/main.rs");
        let _ = picker.tick();
        assert!(
            picker.session.as_ref().unwrap().visible,
            "shown once files arrive"
        );

        let (mut picker, _tx) = hand_fed_picker();
        hold_off_debounce(&mut picker);
        let _ = picker.tick();
        picker.session.as_mut().unwrap().started_at = Instant::now() - DEBOUNCE_HELD_OFF;
        let _ = picker.tick();
        assert!(
            picker.session.as_ref().unwrap().visible,
            "shown once the walk drags on"
        );
    }

    #[test]
    fn empty_walk_endings_flash_and_close() {
        for end in [WalkEnd::Listed, WalkEnd::Unreadable] {
            let (mut picker, tx) = hand_fed_picker();
            tx.send(WalkerMsg::End(end)).unwrap();
            let (dirty, flash) = picker.tick();
            assert!(picker.session.is_none());
            assert_eq!(dirty, Dirty::YES);
            assert_eq!(flash.as_deref(), Some(end.nothing_found_msg()));
            assert_eq!(picker.tick(), (Dirty::NO, None), "quiet afterwards");
        }
    }

    #[test]
    fn walker_death_flashes_and_closes() {
        let (mut picker, tx) = hand_fed_picker();
        drop(tx);
        let (dirty, flash) = picker.tick();
        assert!(picker.session.is_none());
        assert_eq!(dirty, Dirty::YES);
        assert_eq!(flash.as_deref(), Some(WALKER_CRASHED_MSG));
    }

    #[test]
    fn settled_picker_owes_no_frame() {
        let (mut picker, tx) = hand_fed_picker();
        inject_file(&mut picker, "src/main.rs");
        tx.send(WalkerMsg::End(WalkEnd::Listed)).unwrap();
        let _ = tick_until(&mut picker, |s| !s.walking);
        assert_eq!(picker.tick(), (Dirty::NO, None), "nothing changed");
    }

    #[test]
    fn esc_closes_enter_selects_and_enter_on_no_matches_closes() {
        let (mut picker, _tx) = hand_fed_picker();
        assert!(matches!(
            picker.handle_key(key(KeyCode::Esc)),
            FilePickerAction::Close
        ));

        let (mut picker, _tx) = hand_fed_picker();
        inject_file(&mut picker, "file_a.rs");
        picker.session.as_mut().unwrap().visible = true;
        match picker.handle_key(key(KeyCode::Enter)) {
            FilePickerAction::Select(path) => assert_eq!(path, "file_a.rs"),
            _ => panic!("expected Select"),
        }

        let (mut picker, _tx) = hand_fed_picker();
        picker.session.as_mut().unwrap().visible = true;
        assert!(matches!(
            picker.handle_key(key(KeyCode::Enter)),
            FilePickerAction::Close
        ));
    }

    #[test]
    fn enter_during_pending_is_consumed_and_typing_buffers() {
        let (mut picker, _tx) = hand_fed_picker();
        assert!(matches!(
            picker.handle_key(key(KeyCode::Enter)),
            FilePickerAction::Consumed
        ));
        picker.handle_key(key(KeyCode::Char('m')));
        picker.handle_key(key(KeyCode::Char('a')));
        assert_eq!(picker.session.as_ref().unwrap().query, "ma");
    }

    #[test]
    fn paste_appends_and_false_when_closed() {
        let (mut picker, _tx) = hand_fed_picker();
        picker.handle_key(key(KeyCode::Char('a')));
        assert!(picker.handle_paste("bc"));
        assert_eq!(picker.session.as_ref().unwrap().query, "abc");

        let mut closed = FilePicker::new();
        assert!(!closed.handle_paste("test"));
    }

    #[test]
    fn backspace_and_ctrl_w_edit_the_query() {
        let (mut picker, _tx) = hand_fed_picker();
        picker.handle_key(key(KeyCode::Char('a')));
        picker.handle_key(key(KeyCode::Char('b')));
        picker.handle_key(key(KeyCode::Backspace));
        assert_eq!(picker.session.as_ref().unwrap().query, "a");

        {
            let s = picker.session.as_mut().unwrap();
            s.query = "foo bar".into();
            s.cursor = 7;
        }
        let mut ctrl_w = key(KeyCode::Char('w'));
        ctrl_w.modifiers = KeyModifiers::CONTROL;
        picker.handle_key(ctrl_w);
        assert_eq!(picker.session.as_ref().unwrap().query, "foo ");
    }

    #[test]
    fn matches_capped_at_max_materialized() {
        let (mut picker, _tx) = hand_fed_picker();
        for i in 0..MAX_MATERIALIZED + 50 {
            picker
                .session
                .as_mut()
                .unwrap()
                .files
                .push(format!("file_{i:04}.rs"));
        }
        refresh_matches(picker.session.as_mut().unwrap());
        let s = picker.session.as_ref().unwrap();
        assert_eq!(s.total_matches, MAX_MATERIALIZED + 50);
        assert_eq!(s.matches.len(), MAX_MATERIALIZED);
    }

    #[test]
    fn selection_moves_and_clamps() {
        let (mut picker, _tx) = hand_fed_picker();
        {
            let s = picker.session.as_mut().unwrap();
            s.matches = (0..5)
                .map(|i| Match {
                    path: format!("file_{i}.rs"),
                    indices: Vec::new(),
                })
                .collect();
            s.total_matches = 5;
            s.viewport_height = 10;
        }

        picker.handle_key(key(KeyCode::Down));
        picker.handle_key(key(KeyCode::Down));
        assert_eq!(picker.session.as_ref().unwrap().selected, 2);
        picker.handle_key(key(KeyCode::Up));
        assert_eq!(picker.session.as_ref().unwrap().selected, 1);
        for _ in 0..10 {
            picker.handle_key(key(KeyCode::Down));
        }
        assert_eq!(
            picker.session.as_ref().unwrap().selected,
            4,
            "clamped at end"
        );
        for _ in 0..10 {
            picker.handle_key(key(KeyCode::Up));
        }
        assert_eq!(
            picker.session.as_ref().unwrap().selected,
            0,
            "clamped at start"
        );
    }

    #[test]
    fn ensure_visible_tracks_the_selection() {
        let (mut picker, _tx) = hand_fed_picker();
        {
            let s = picker.session.as_mut().unwrap();
            s.matches = (0..20)
                .map(|i| Match {
                    path: format!("file_{i:02}.rs"),
                    indices: Vec::new(),
                })
                .collect();
            s.total_matches = 20;
            s.viewport_height = 5;
            s.selected = 10;
            s.scroll_offset = 0;
            s.apply_scroll();
        }
        assert_eq!(picker.session.as_ref().unwrap().scroll_offset, 6);

        // A shrunk viewport clamps the scroll window back inside the list.
        let s = picker.session.as_mut().unwrap();
        s.viewport_height = 20;
        s.apply_scroll();
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn ensure_visible_zero_viewport_does_not_panic() {
        let (mut picker, _tx) = hand_fed_picker();
        let s = picker.session.as_mut().unwrap();
        s.matches = vec![Match {
            path: "x".into(),
            indices: Vec::new(),
        }];
        s.selected = 0;
        s.viewport_height = 0;
        s.apply_scroll();
    }
}
