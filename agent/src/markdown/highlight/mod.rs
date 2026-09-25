//! Syntect-based code highlighting, ported from the reference
//! `craft-highlight` crate. All syntect work runs on the single dedicated
//! thread in [`pool`] to bound regex-automata cache memory.

pub mod pool;

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::mem::size_of;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use syntect::highlighting::{
    FontStyle, HighlightIterator, HighlightState, Highlighter as SynHighlighter, Style as SynStyle,
    Theme,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

const TOKEN_ALIASES: &[(&str, &str)] = &[("jsx", "js")];
pub const TAB_SPACES: &str = "  ";
const BLOCK_CACHE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Bat encodes ANSI palette semantics in the syntect alpha channel: `a = 0`
/// means `r` holds a palette index, `a = 1` means the terminal default.
const ANSI_ALPHA_INDEX: u8 = 0x00;
const ANSI_ALPHA_DEFAULT: u8 = 0x01;

pub type BlockSegments = Arc<Vec<Vec<StyledSegment>>>;

/// Resolved foreground color of a highlighted segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SegmentColor {
    Rgb(u8, u8, u8),
    Ansi(u8),
    Default,
}

impl SegmentColor {
    pub fn from_syntect(c: syntect::highlighting::Color) -> Self {
        match c.a {
            ANSI_ALPHA_INDEX => Self::Ansi(c.r),
            ANSI_ALPHA_DEFAULT => Self::Default,
            _ => Self::Rgb(c.r, c.g, c.b),
        }
    }

    pub fn to_syntect(self) -> syntect::highlighting::Color {
        let (r, g, b, a) = match self {
            Self::Rgb(r, g, b) => (r, g, b, 0xFF),
            Self::Ansi(i) => (i, 0, 0, ANSI_ALPHA_INDEX),
            Self::Default => (0, 0, 0, ANSI_ALPHA_DEFAULT),
        };
        syntect::highlighting::Color { r, g, b, a }
    }
}

/// One highlighted run of text within a code line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StyledSegment {
    pub text: String,
    pub fg: SegmentColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl StyledSegment {
    fn from_syntect(style: SynStyle, text: String) -> Self {
        Self {
            text,
            fg: SegmentColor::from_syntect(style.foreground),
            bold: style.font_style.contains(FontStyle::BOLD),
            italic: style.font_style.contains(FontStyle::ITALIC),
            underline: style.font_style.contains(FontStyle::UNDERLINE),
        }
    }

    fn fallback(text: String) -> Self {
        Self {
            text,
            fg: SegmentColor::Default,
            bold: false,
            italic: false,
            underline: false,
        }
    }
}

/// A theme whose only rule colors comments gray, so tests can observe
/// scope-driven coloring without shipping a real theme set (task 75).
#[cfg(test)]
fn comment_theme() -> Theme {
    use syntect::highlighting::{Color, StyleModifier, ThemeItem};
    let mut theme = Theme::default();
    theme.scopes.push(ThemeItem {
        scope: "comment".parse().unwrap(),
        style: StyleModifier {
            foreground: Some(Color {
                r: 0x88,
                g: 0x88,
                b: 0x88,
                a: 0xff,
            }),
            ..StyleModifier::default()
        },
    });
    theme
}

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static THEME: OnceLock<RwLock<Arc<Theme>>> = OnceLock::new();
static BLOCK_CACHE: OnceLock<RwLock<BlockCache>> = OnceLock::new();
static THEME_GEN: AtomicU64 = AtomicU64::new(0);

/// The built-in theme so code blocks are colored before any user theme is
/// installed. Base16 Ocean Dark matches the ink-toned TUI palette.
fn default_theme() -> Theme {
    two_face::theme::extra()
        .get(two_face::theme::EmbeddedThemeName::Base16OceanDark)
        .clone()
}

fn theme_lock() -> &'static RwLock<Arc<Theme>> {
    THEME.get_or_init(|| RwLock::new(Arc::new(default_theme())))
}

fn block_cache_lock() -> &'static RwLock<BlockCache> {
    BLOCK_CACHE.get_or_init(RwLock::default)
}

pub fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

/// Loads the syntax set and runs one warmup highlight on the pool thread.
pub fn warmup() {
    syntax_set();
    theme_lock();
    pool::run(|| {
        let mut hl = Highlighter::for_token("bash");
        hl.highlight_line("x");
    });
}

pub fn is_ready() -> bool {
    SYNTAX_SET.get().is_some()
}

pub fn set_theme(theme: Theme) {
    // Bump first: a highlight already in flight read the old generation, so its
    // insert lands under a key nobody will look up again.
    THEME_GEN.fetch_add(1, Ordering::Release);
    block_cache_lock()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    *theme_lock().write().unwrap_or_else(|e| e.into_inner()) = Arc::new(theme);
}

/// Bumped by every [`set_theme`]. Anything derived from the theme, like an
/// incremental [`CodeHighlighter`] or a bag of painted lines, is stale once
/// this changes.
pub fn theme_generation() -> u64 {
    THEME_GEN.load(Ordering::Acquire)
}

pub fn theme() -> Arc<Theme> {
    theme_lock()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The byte length rides along with the hash, so an accidental hit needs both
/// to collide. A miss only costs a re-highlight, a false hit costs wrong colors.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct BlockKey {
    hash: u64,
    code_bytes: usize,
    theme_gen: u64,
}

impl BlockKey {
    fn new(lang: &str, code: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        lang.hash(&mut hasher);
        code.hash(&mut hasher);
        Self {
            hash: hasher.finish(),
            code_bytes: code.len(),
            theme_gen: theme_generation(),
        }
    }
}

fn segments_bytes(segments: &[Vec<StyledSegment>]) -> usize {
    segments
        .iter()
        .map(|line| {
            size_of::<Vec<StyledSegment>>()
                + line
                    .iter()
                    .map(|seg| size_of::<StyledSegment>() + seg.text.len())
                    .sum::<usize>()
        })
        .sum()
}

/// Budgeted in bytes, since entries range from a one-liner to a whole file.
///
/// Eviction picks an arbitrary entry on purpose. A resize walks the transcript
/// in order, so an LRU (or dropping a whole generation) always throws out
/// exactly what the walk asks for next and misses every time once the
/// transcript outgrows the budget. Arbitrary eviction keeps a stable subset
/// instead, and the hit rate settles near `budget / working set`.
#[derive(Default)]
struct BlockCache {
    entries: HashMap<BlockKey, (BlockSegments, usize)>,
    bytes: usize,
}

impl BlockCache {
    fn get(&self, key: BlockKey) -> Option<BlockSegments> {
        self.entries.get(&key).map(|(segs, _)| Arc::clone(segs))
    }

    /// Evicts before inserting, so the incoming entry is never its own victim.
    fn insert(&mut self, key: BlockKey, segments: BlockSegments, budget: usize) {
        let bytes = segments_bytes(&segments);
        if bytes > budget {
            return;
        }
        self.remove(key);
        while self.bytes + bytes > budget
            && let Some(&victim) = self.entries.keys().next()
        {
            self.remove(victim);
        }
        self.entries.insert(key, (segments, bytes));
        self.bytes += bytes;
    }

    fn remove(&mut self, key: BlockKey) {
        if let Some((_, bytes)) = self.entries.remove(&key) {
            self.bytes -= bytes;
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

pub fn normalize_text(text: &str) -> String {
    text.trim_end_matches('\n').replace('\t', TAB_SPACES)
}

pub fn syntax_for_path(path: &str) -> &'static SyntaxReference {
    syntax_set()
        .find_syntax_for_file(path)
        .ok()
        .flatten()
        .unwrap_or_else(|| {
            let ext = path.rsplit('.').next().unwrap_or(path);
            syntax_for_token(ext)
        })
}

pub fn syntax_for_token(lang: &str) -> &'static SyntaxReference {
    let ss = syntax_set();
    ss.find_syntax_by_token(lang)
        .or_else(|| {
            TOKEN_ALIASES
                .iter()
                .find(|(from, _)| *from == lang)
                .and_then(|(_, to)| ss.find_syntax_by_token(to))
        })
        .unwrap_or_else(|| ss.find_syntax_plain_text())
}

pub struct Highlighter {
    theme: Arc<Theme>,
    parse_state: ParseState,
    highlight_state: HighlightState,
}

impl Highlighter {
    fn new(syntax: &SyntaxReference, theme: Arc<Theme>) -> Self {
        let syn_hl = SynHighlighter::new(&theme);
        Self {
            highlight_state: HighlightState::new(&syn_hl, ScopeStack::new()),
            parse_state: ParseState::new(syntax),
            theme,
        }
    }

    fn from_state(
        theme: Arc<Theme>,
        highlight_state: HighlightState,
        parse_state: ParseState,
    ) -> Self {
        Self {
            theme,
            highlight_state,
            parse_state,
        }
    }

    pub fn for_path(path: &str) -> Self {
        Self::new(syntax_for_path(path), theme())
    }

    pub fn for_syntax(syntax: &'static SyntaxReference) -> Self {
        Self::new(syntax, theme())
    }

    pub fn for_token(lang: &str) -> Self {
        Self::new(syntax_for_token(lang), theme())
    }

    fn raw_highlight_line<'a>(
        &mut self,
        text: &'a str,
    ) -> Result<Vec<(SynStyle, &'a str)>, syntect::Error> {
        let ops = self.parse_state.parse_line(text, syntax_set())?;
        let syn_hl = SynHighlighter::new(&self.theme);
        let iter = HighlightIterator::new(&mut self.highlight_state, &ops, text, &syn_hl);
        Ok(iter.collect())
    }

    pub fn highlight_line(&mut self, text: &str) -> Vec<StyledSegment> {
        match self.raw_highlight_line(text) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(style, text)| StyledSegment::from_syntect(style, normalize_text(text)))
                .collect(),
            Err(_) => vec![StyledSegment::fallback(normalize_text(text))],
        }
    }

    pub fn advance(&mut self, text: &str) {
        let _ = self.raw_highlight_line(text);
    }

    pub fn state(self) -> (HighlightState, ParseState) {
        (self.highlight_state, self.parse_state)
    }
}

/// Highlights a whole block, memoized on its content. Callers streaming a
/// growing block stay on [`CodeHighlighter`]: it is incremental, and would
/// miss this cache on every token.
pub fn highlight_block(lang: &str, code: &str) -> Vec<Vec<StyledSegment>> {
    let key = BlockKey::new(lang, code);
    if let Some(hit) = block_cache_lock()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
    {
        return (*hit).clone();
    }
    let (lang, code) = (lang.to_owned(), code.to_owned());
    let segments: BlockSegments =
        pool::run(move || Arc::new(highlight_code_inline(&lang, &code, "")));
    block_cache_lock()
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, Arc::clone(&segments), BLOCK_CACHE_MAX_BYTES);
    (*segments).clone()
}

pub fn highlight_code(lang: &str, code: &str, prefix: &str) -> Vec<Vec<StyledSegment>> {
    let (lang, code, prefix) = (lang.to_owned(), code.to_owned(), prefix.to_owned());
    pool::run(move || highlight_code_inline(&lang, &code, &prefix))
}

fn highlight_code_inline(lang: &str, code: &str, prefix: &str) -> Vec<Vec<StyledSegment>> {
    let mut hl = Highlighter::for_token(lang);
    if !prefix.is_empty() {
        for line in LinesWithEndings::from(prefix) {
            hl.advance(line);
        }
    }
    LinesWithEndings::from(code)
        .map(|raw| hl.highlight_line(raw))
        .collect()
}

/// Incremental code highlighter for streaming input.
///
/// Processes completed lines once (using syntect parse/checkpoint state) and
/// re-highlights the trailing partial line on each call. All syntect work
/// happens on the highlight thread.
pub struct CodeHighlighter {
    syntax: &'static SyntaxReference,
    checkpoint_parse: ParseState,
    checkpoint_highlight: HighlightState,
    completed_lines: usize,
    cached_segments: Vec<Vec<StyledSegment>>,
}

impl CodeHighlighter {
    pub fn new(lang: &str) -> Self {
        let syntax = syntax_for_token(lang);
        let t = theme();
        let syn_hl = SynHighlighter::new(&t);
        Self {
            syntax,
            checkpoint_parse: ParseState::new(syntax),
            checkpoint_highlight: HighlightState::new(&syn_hl, ScopeStack::new()),
            completed_lines: 0,
            cached_segments: Vec::new(),
        }
    }

    fn placeholder() -> Self {
        Self::new("txt")
    }

    fn reset_checkpoints(&mut self) {
        let t = theme();
        let syn_hl = SynHighlighter::new(&t);
        self.checkpoint_parse = ParseState::new(self.syntax);
        self.checkpoint_highlight = HighlightState::new(&syn_hl, ScopeStack::new());
        self.completed_lines = 0;
        self.cached_segments.clear();
    }

    fn set_or_push(&mut self, index: usize, segments: Vec<StyledSegment>) {
        if index < self.cached_segments.len() {
            self.cached_segments[index] = segments;
        } else {
            self.cached_segments.push(segments);
        }
    }

    /// Feed the current full text of the block; returns segments for every
    /// line, including the (possibly still-streaming) final line.
    pub fn update(&mut self, code: &str) -> Vec<Vec<StyledSegment>> {
        let code = code.to_owned();
        let mut this = std::mem::replace(self, Self::placeholder());
        let (segments, this) = pool::run(move || {
            let segments = this.update_on_thread(&code);
            (segments, this)
        });
        *self = this;
        segments
    }

    fn update_on_thread(&mut self, code: &str) -> Vec<Vec<StyledSegment>> {
        let raw_lines: Vec<&str> = LinesWithEndings::from(code).collect();
        let total = raw_lines.len();
        if total == 0 {
            self.cached_segments.clear();
            self.completed_lines = 0;
            return Vec::new();
        }

        let new_completed = if code.ends_with('\n') {
            total
        } else {
            total - 1
        };

        if new_completed < self.completed_lines {
            self.reset_checkpoints();
        }

        if new_completed > self.completed_lines {
            let mut hl = Highlighter::from_state(
                theme(),
                self.checkpoint_highlight.clone(),
                self.checkpoint_parse.clone(),
            );

            for raw in &raw_lines[self.completed_lines..new_completed] {
                self.set_or_push(self.completed_lines, hl.highlight_line(raw));
                self.completed_lines += 1;
            }

            let (hs, ps) = hl.state();
            self.checkpoint_parse = ps;
            self.checkpoint_highlight = hs;
        }

        let line_count = new_completed + usize::from(new_completed < total);
        self.cached_segments.truncate(line_count);

        if new_completed < total {
            let mut hl = Highlighter::from_state(
                theme(),
                self.checkpoint_highlight.clone(),
                self.checkpoint_parse.clone(),
            );
            self.set_or_push(new_completed, hl.highlight_line(raw_lines[new_completed]));
        }

        self.cached_segments.clone()
    }
}

/// Test-only lock over the process-global theme and block cache, so tests
/// that touch them (here or in sibling modules) cannot interleave under the
/// shared-process test harness.
#[cfg(test)]
static TEST_GLOBALS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn lock_test_globals() -> std::sync::MutexGuard<'static, ()> {
    TEST_GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Test-only: takes the global lock and installs the built-in theme, so
/// tests in other modules compare highlight output under a known theme.
#[cfg(test)]
pub(crate) fn pin_default_theme_for_tests() -> std::sync::MutexGuard<'static, ()> {
    let guard = lock_test_globals();
    set_theme(default_theme());
    guard
}

#[cfg(test)]
mod tests {
    use std::sync::MutexGuard;

    use super::*;
    use test_case::test_case;

    const BUDGETED_ENTRIES: usize = 32;
    const RUST: &str = "rust";
    const PYTHON: &str = "python";
    const OPENS_A_STRING: &str = "x = '''";

    /// The theme and the block cache are process globals; tests that swap the
    /// theme or read the cache take this first and cannot be tripped up by a
    /// sibling running under `cargo test`'s shared-process harness.
    fn exclusive_globals() -> MutexGuard<'static, ()> {
        super::lock_test_globals()
    }

    /// Like [`exclusive_globals`], but also installs the built-in theme, so
    /// output-comparing tests see the same colors regardless of what a
    /// sibling test last installed.
    fn pinned_theme() -> MutexGuard<'static, ()> {
        let guard = exclusive_globals();
        set_theme(default_theme());
        guard
    }

    fn segments_text(segs: &[StyledSegment]) -> String {
        segs.iter().map(|s| s.text.as_str()).collect()
    }

    fn lines_text(lines: &[Vec<StyledSegment>]) -> Vec<String> {
        lines.iter().map(|l| segments_text(l)).collect()
    }

    #[test]
    fn one_shot_matches_incremental_final_update() {
        warmup();
        let _theme = pinned_theme();
        let code = "fn a() {}\nlet x = 1;\n";
        let mut h = CodeHighlighter::new(RUST);
        assert_eq!(h.update(code), highlight_block(RUST, code));
    }

    #[test]
    fn update_over_prefixes_converges() {
        warmup();
        let _theme = pinned_theme();
        let code = "fn main() {\n    let x = 1;\n}";
        let mut h = CodeHighlighter::new(RUST);
        for end in 1..code.len() {
            if code.is_char_boundary(end) {
                let _ = h.update(&code[..end]);
            }
        }
        assert_eq!(h.update(code), highlight_block(RUST, code));
    }

    #[test]
    fn empty_input_yields_no_lines() {
        warmup();
        assert!(highlight_block("x", "").is_empty());
        assert!(CodeHighlighter::new("x").update("").is_empty());
    }

    #[test]
    fn trailing_newline_has_no_streaming_line() {
        warmup();
        let lines = CodeHighlighter::new("x").update("a\nb\n");
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn default_theme_colors_code() {
        let _globals = pinned_theme();
        warmup();
        let lines = highlight_block(RUST, "let s = \"abc\";\n");
        let colors: Vec<_> = lines[0].iter().map(|s| s.fg).collect();
        assert!(
            colors.iter().any(|c| *c != colors[0]),
            "the built-in theme must distinguish scopes, got one color for all of {colors:?}"
        );
    }

    #[test]
    fn highlight_produces_colors_and_keeps_text() {
        warmup();
        let lines = highlight_block(RUST, "let s = \"abc\";\n");
        assert_eq!(segments_text(&lines[0]), "let s = \"abc\";");
        assert!(
            lines[0].iter().any(|s| s.fg != SegmentColor::Default),
            "syntect must color the line, not render it plain"
        );
    }

    #[test]
    fn highlight_code_line_handling() {
        warmup();
        let _theme = pinned_theme();
        let single = highlight_code(RUST, "fn main() {}\n", "");
        assert_eq!(single.len(), 1);
        assert_eq!(segments_text(&single[0]), "fn main() {}");

        let no_newline = highlight_code(RUST, "let x = 1;", "");
        assert_eq!(no_newline.len(), 1);
        assert_eq!(segments_text(&no_newline[0]), "let x = 1;");

        let trailing = highlight_code(RUST, "let x = 1;\n\n\n", "");
        assert_eq!(trailing.len(), 3);
        assert_eq!(segments_text(&trailing[1]), "");
    }

    #[test]
    fn highlight_code_prefix_builds_state() {
        warmup();
        let _theme = pinned_theme();
        let prefix = "/* start of block comment\n";
        let code = "let x = 42;\n";

        let without_prefix = highlight_code(RUST, code, "");
        let with_prefix = highlight_code(RUST, code, prefix);

        assert_eq!(without_prefix.len(), with_prefix.len());
        assert_ne!(
            without_prefix[0], with_prefix[0],
            "prefix should feed the block comment state so the target line is colored differently",
        );
    }

    #[test]
    fn syntax_for_token_fallback() {
        warmup();
        let plain = syntax_set().find_syntax_plain_text();
        assert_eq!(
            syntax_for_token("nonexistent_language_xyz").name,
            plain.name
        );
    }

    #[test_case("test.rs" => "Rust"; "rust_extension")]
    #[test_case("test.py" => "Python"; "python_extension")]
    #[test_case("test.go" => "Go"; "go_extension")]
    #[test_case("Makefile" => "Makefile"; "makefile_no_ext")]
    fn syntax_for_path_resolves(path: &str) -> String {
        warmup();
        syntax_for_path(path).name.to_string()
    }

    #[test]
    fn syntax_for_path_unknown_falls_back() {
        warmup();
        let plain = syntax_set().find_syntax_plain_text();
        assert_eq!(syntax_for_path("file.totally_unknown_xyz").name, plain.name);
    }

    #[test_case("jsx", "js"; "jsx_alias")]
    fn token_alias_resolves(alias: &str, canonical: &str) {
        warmup();
        let aliased = syntax_for_token(alias);
        let canonical_syntax = syntax_set().find_syntax_by_token(canonical).unwrap();
        assert_eq!(aliased.name, canonical_syntax.name);
    }

    #[test]
    fn set_theme_applies_without_panic() {
        let _globals = exclusive_globals();
        warmup();
        for _ in 0..3 {
            set_theme(Theme::default());
        }
        let mut hl = Highlighter::for_token(RUST);
        assert!(!hl.highlight_line("let x = 1;\n").is_empty());
    }

    #[test]
    fn set_theme_bumps_the_generation_and_clears_the_cache() {
        let _globals = exclusive_globals();
        warmup();
        let before = theme_generation();
        set_theme(Theme::default());
        assert_eq!(theme_generation(), before + 1);
        assert_eq!(block_cache_lock().read().unwrap().bytes, 0);
    }

    /// A closing fence takes the newline before it, so the block's last line
    /// arrives complete and then turns back into the partial tail. Painting it
    /// from the checkpoint that already ate it opens the string twice, and the
    /// line keeps the string's color for the rest of the turn.
    #[test]
    fn the_last_line_of_a_block_keeps_its_colors_when_the_fence_closes() {
        let _guard = pinned_theme();
        warmup();
        let mut ch = CodeHighlighter::new(PYTHON);
        let colors = |lines: &[Vec<StyledSegment>]| {
            lines
                .iter()
                .flatten()
                .filter(|s| !s.text.is_empty())
                .map(|s| (s.text.clone(), s.fg))
                .collect::<Vec<_>>()
        };
        let while_streaming = colors(&ch.update(&format!("{OPENS_A_STRING}\n")));
        assert_eq!(while_streaming, colors(&ch.update(OPENS_A_STRING)));
    }

    #[test]
    fn code_highlighter_streaming_consistency() {
        warmup();
        let _theme = pinned_theme();
        let full_code = "fn main() {\n    let x = 42;\n    println!(\"{}\", x);\n}\n";
        let full = highlight_code(RUST, full_code, "");

        let mut ch = CodeHighlighter::new(RUST);
        ch.update("fn main() {\n");
        ch.update("fn main() {\n    let x = 42;\n");
        let result = ch.update(full_code);

        assert_eq!(lines_text(&full), lines_text(&result));
    }

    #[test]
    fn code_highlighter_partial_line() {
        warmup();
        let _theme = pinned_theme();
        let mut ch = CodeHighlighter::new(RUST);

        ch.update("let x");
        let text1 = segments_text(&ch.update("let x")[0]);

        let text2 = segments_text(&ch.update("let x = 42")[0]);
        assert_ne!(
            text1, text2,
            "partial line should be re-highlighted as content changes"
        );
    }

    #[test]
    fn code_highlighter_shrinks() {
        warmup();
        let mut ch = CodeHighlighter::new(RUST);
        ch.update("let a = 1;\nlet b = 2;\nlet c = 3;\n");
        let segs = ch.update("let a = 1;\n");
        assert_eq!(segs.len(), 1);
        assert_eq!(segments_text(&segs[0]), "let a = 1;");
    }

    #[test]
    fn code_highlighter_shrink_then_regrow() {
        warmup();
        let mut ch = CodeHighlighter::new(RUST);
        ch.update("let a = 1;\nlet b = 2;\n");
        let segs = ch.update("let a = 1;\n");
        assert_eq!(segs.len(), 1);
        let segs = ch.update("let a = 1;\nlet b = 2;\nlet c = 3;\n");
        assert_eq!(segs.len(), 3);
        assert_eq!(segments_text(&segs[0]), "let a = 1;");
        assert_eq!(segments_text(&segs[1]), "let b = 2;");
        assert_eq!(segments_text(&segs[2]), "let c = 3;");
    }

    #[test]
    fn highlighter_advance_and_state_roundtrip() {
        warmup();
        let _theme = pinned_theme();
        let mut hl = Highlighter::for_token(RUST);
        hl.advance("fn main() {\n");
        let (hs, ps) = hl.state();

        let mut from_state = Highlighter::from_state(theme(), hs, ps);
        let seg_from_state = from_state.highlight_line("    let x = 1;\n");

        let mut fresh = Highlighter::for_token(RUST);
        fresh.advance("fn main() {\n");
        let seg_fresh = fresh.highlight_line("    let x = 1;\n");

        assert_eq!(seg_from_state, seg_fresh);
    }

    #[test]
    fn normalize_text_tabs_and_newlines() {
        assert_eq!(normalize_text("\t\t"), format!("{TAB_SPACES}{TAB_SPACES}"));
        assert_eq!(normalize_text("hello\n"), "hello");
        assert_eq!(normalize_text("a\tb"), format!("a{TAB_SPACES}b"));
        assert_eq!(normalize_text("hello world"), "hello world");
        assert_eq!(normalize_text(""), "");
    }

    /// The language mixup pin from the reference: the same body under two
    /// fence languages must paint differently once a theme has scope rules
    /// (rust's `//` is a comment, python's is not).
    #[test]
    fn languages_paint_the_same_body_differently() {
        let _globals = pinned_theme();
        warmup();
        set_theme(comment_theme());
        let body = "fn x() { let y = 1; } // note\n";
        assert_ne!(
            highlight_code("rust", body, ""),
            highlight_code("python", body, ""),
            "rust sees a comment where python sees operators; the comment rule must split them"
        );
        set_theme(default_theme());
    }

    #[test_case(RUST, "fn main() {\n    let x = 1;\n}\n"; "rust_block")]
    #[test_case(RUST, ""; "empty_code")]
    #[test_case("totally_unknown_xyz", "!!! not a language !!!\n"; "unknown_language")]
    fn highlight_block_matches_an_uncached_highlight(lang: &str, code: &str) {
        let _globals = exclusive_globals();
        warmup();
        assert_eq!(highlight_block(lang, code), highlight_code(lang, code, ""));
    }

    /// Hashing the language and the code into one stream would make
    /// `("rust", "x")` and `("rus", "tx")` the same key, and a false hit paints
    /// a block with another block's colors.
    #[test_case((RUST, "x"), ("rus", "tx"); "language_code_boundary")]
    #[test_case((RUST, "let a = 1;\n"), (RUST, "let b = 2;\n"); "equal_length_bodies")]
    fn distinct_blocks_get_distinct_keys(left: (&str, &str), right: (&str, &str)) {
        assert!(BlockKey::new(left.0, left.1) != BlockKey::new(right.0, right.1));
    }

    /// `set_theme` bumps the generation before clearing, so a highlight that
    /// started earlier and finishes after the clear lands under a key no later
    /// lookup can mint.
    #[test]
    fn a_theme_change_orphans_the_blocks_highlighted_before_it() {
        const CODE: &str = "let themed = 1;\n";
        let _globals = exclusive_globals();
        warmup();
        let stale_key = BlockKey::new(RUST, CODE);
        highlight_block(RUST, CODE);

        set_theme(Theme::default());
        assert_eq!(
            block_cache_lock().read().unwrap().bytes,
            0,
            "set_theme must clear the cache"
        );

        let racing: BlockSegments = Arc::new(vec![vec![StyledSegment::fallback("x".into())]]);
        block_cache_lock().write().unwrap().insert(
            stale_key,
            Arc::clone(&racing),
            BLOCK_CACHE_MAX_BYTES,
        );
        assert_ne!(
            (*racing).clone(),
            highlight_block(RUST, CODE),
            "an insert under a key minted before the bump must be unreachable"
        );
    }

    fn block_of(lines: usize) -> BlockSegments {
        Arc::new(vec![vec![StyledSegment::fallback("x".into())]; lines])
    }

    fn one_line() -> BlockSegments {
        block_of(1)
    }

    fn assert_bytes_match_entries(cache: &BlockCache) {
        assert_eq!(
            cache.bytes,
            cache
                .entries
                .values()
                .map(|(segs, _)| segments_bytes(segs))
                .sum::<usize>(),
            "the byte tally must track the entries it holds"
        );
    }

    /// A budget that fits exactly `BUDGETED_ENTRIES` of [`one_line`].
    fn tiny_budget() -> usize {
        segments_bytes(&one_line()) * BUDGETED_ENTRIES
    }

    /// Replays what a resize does: walk every block in order, over and over.
    /// Returns hits per pass.
    fn tiny_cache_scan(blocks: usize, passes: usize) -> (Vec<usize>, BlockCache) {
        let budget = tiny_budget();
        let mut cache = BlockCache::default();
        let mut hits = Vec::new();
        for _ in 0..passes {
            let mut pass_hits = 0;
            for i in 0..blocks {
                let key = BlockKey::new(RUST, &i.to_string());
                if cache.get(key).is_some() {
                    pass_hits += 1;
                } else {
                    cache.insert(key, one_line(), budget);
                }
            }
            hits.push(pass_hits);
        }
        (hits, cache)
    }

    /// Arbitrary eviction is the whole point: an LRU or a generational rotation
    /// would score a flat zero once the walk outgrows the budget.
    #[test_case(BUDGETED_ENTRIES, BUDGETED_ENTRIES; "a_scan_that_fits_stays_fully_warm")]
    #[test_case(BUDGETED_ENTRIES * 4, 1; "a_scan_that_overflows_still_hits")]
    fn a_repeated_scan_keeps_hitting_within_budget(blocks: usize, min_hits: usize) {
        const PASSES: usize = 3;
        let (hits, cache) = tiny_cache_scan(blocks, PASSES);
        assert_eq!(hits[0], 0, "nothing is warm on the first pass");
        assert!(
            hits[1..].iter().all(|&pass| pass >= min_hits),
            "a scan of {blocks} blocks must keep serving at least {min_hits}, got {hits:?}"
        );
        assert!(cache.bytes <= tiny_budget(), "{} over budget", cache.bytes);
        assert_bytes_match_entries(&cache);
    }

    #[test]
    fn block_cache_drops_an_entry_bigger_than_the_budget() {
        const OVERSIZED_LINES: usize = BUDGETED_ENTRIES + 1;
        let budget = tiny_budget();
        let mut cache = BlockCache::default();
        let resident = BlockKey::new(RUST, "resident");
        cache.insert(resident, one_line(), budget);
        let bytes_before = cache.bytes;

        cache.insert(
            BlockKey::new(RUST, "oversized"),
            block_of(OVERSIZED_LINES),
            budget,
        );

        assert!(
            cache.get(BlockKey::new(RUST, "oversized")).is_none(),
            "an entry over the whole budget must not be cached"
        );
        assert!(
            cache.get(resident).is_some(),
            "a rejected insert must not evict what already fits"
        );
        assert_eq!(cache.bytes, bytes_before);
    }

    #[test]
    fn reinserting_a_key_does_not_double_count_its_bytes() {
        const GROWN_LINES: usize = 4;
        let budget = tiny_budget();
        let mut cache = BlockCache::default();
        let key = BlockKey::new(RUST, "same");

        cache.insert(key, one_line(), budget);
        cache.insert(key, block_of(GROWN_LINES), budget);
        cache.insert(key, one_line(), budget);

        assert_eq!(cache.entries.len(), 1);
        assert_bytes_match_entries(&cache);
    }

    #[test]
    fn eviction_spares_the_entry_being_inserted() {
        let budget = tiny_budget();
        let mut cache = BlockCache::default();
        for i in 0..BUDGETED_ENTRIES {
            cache.insert(BlockKey::new(RUST, &i.to_string()), one_line(), budget);
        }
        assert_eq!(cache.bytes, budget, "the cache must start out exactly full");

        let incoming = BlockKey::new(RUST, "incoming");
        cache.insert(incoming, block_of(BUDGETED_ENTRIES), budget);

        assert!(
            cache.get(incoming).is_some(),
            "the entry that forced the eviction must survive it"
        );
        assert_eq!(cache.entries.len(), 1, "it takes the budget on its own");
        assert_bytes_match_entries(&cache);
    }
}
