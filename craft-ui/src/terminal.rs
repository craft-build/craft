use shell_words::split;
use std::io::{Write, stdout};
use std::path::Path;
use std::time::Instant;

use color_eyre::Result;
use crossterm::Command;
use crossterm::ExecutableCommand;
use crossterm::clipboard::CopyToClipboard;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

/// CSI ?2026h — begin synchronized output. Terminals that support it
/// batch all subsequent writes until the matching end, eliminating the
/// partial-frame flicker a full-buffer diff flush can otherwise cause
/// during rapid redraws. Unknown `?` private modes are ignored by spec,
/// so unsupported terminals are unaffected.
const SYNC_BEGIN: &str = "\u{1b}[?2026h";
const SYNC_END: &str = "\u{1b}[?2026l";

/// Mouse tracking modes. We enable only what the UI uses — normal +
/// button-event tracking under SGR encoding — and deliberately omit
/// `?1003` (any-motion) tracking.
///
/// crossterm's bundled `EnableMouseCapture` turns on `?1003`, which makes
/// the terminal emit a report on every pointer move. On a fast macOS
/// trackpad a scroll burst produces a high-density stream of SGR reports
/// that crossterm can split across two `read(2)` calls; the half it fails
/// to parse is then surfaced as raw `KeyEvent`s and leaks into the input
/// bar as literal `ESC[<…M` text (crossterm-rs/crossterm#668). The UI never
/// needs motion-without-button, so dropping `?1003` removes the trigger
/// without losing click, drag, or scroll-wheel support. `?1015` is skipped
/// as redundant with the SGR (`?1006`) encoding.
const MOUSE_ENABLE: &str = "\u{1b}[?1000h\u{1b}[?1002h\u{1b}[?1006h";
const MOUSE_DISABLE: &str = "\u{1b}[?1006l\u{1b}[?1002l\u{1b}[?1000l";

/// Emits the synchronized-output begin sequence and flushes, so the
/// terminal enters batched-update mode before the next frame diff.
pub(crate) fn begin_synchronized_output() {
    let _ = write_and_flush(SYNC_BEGIN);
}

/// Emits the synchronized-output end sequence and flushes, releasing
/// the batched frame for display.
pub(crate) fn end_synchronized_output() {
    let _ = write_and_flush(SYNC_END);
}

fn write_and_flush(bytes: &str) -> std::io::Result<()> {
    let mut out = stdout().lock();
    out.write_all(bytes.as_bytes())?;
    out.flush()
}

pub(crate) struct TerminalGuard;

enum TerminalMux {
    Zellij,
    Tmux,
    Screen,
    None,
}

impl TerminalMux {
    fn detect() -> Self {
        if std::env::var_os("ZELLIJ").is_some() {
            Self::Zellij
        } else if std::env::var_os("TMUX").is_some() {
            Self::Tmux
        } else if std::env::var_os("STY").is_some() {
            Self::Screen
        } else {
            Self::None
        }
    }

    // tmux and screen need DCS-passthrough with every internal ESC doubled.
    // Without doubling, the `ESC \` in an OSC52 ST terminator would close
    // the DCS wrapper early and truncate the payload.
    // Zellij intercepts OSC52 natively, so we just emit the raw sequence.
    fn wrap_for_mux(&self, sequence: String) -> String {
        match self {
            Self::Zellij | Self::None => sequence,
            Self::Tmux => {
                let escaped = sequence.replace('\u{1b}', "\u{1b}\u{1b}");
                format!("\u{1b}Ptmux;{escaped}\u{1b}\\")
            }
            Self::Screen => {
                let escaped = sequence.replace('\u{1b}', "\u{1b}\u{1b}");
                format!("\u{1b}P{escaped}\u{1b}\\")
            }
        }
    }
}

impl TerminalGuard {
    pub(crate) fn init() -> Result<(Self, ratatui::DefaultTerminal)> {
        let terminal = ratatui::init();
        stdout().execute(EnableBracketedPaste)?;
        write_and_flush(MOUSE_ENABLE)?;
        push_keyboard_enhancement();
        Ok((Self, terminal))
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let started = Instant::now();
        pop_terminal_modes();
        ratatui::restore();
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "terminal restored"
        );
    }
}

pub(crate) fn suspend(terminal: &mut ratatui::DefaultTerminal) {
    teardown();
    #[cfg(unix)]
    unsafe {
        libc::raise(libc::SIGTSTP);
    }
    resume(terminal);
}

fn teardown() {
    pop_terminal_modes();
    terminal::disable_raw_mode().ok();
    stdout().execute(LeaveAlternateScreen).ok();
    stdout().flush().ok();
}

fn pop_terminal_modes() {
    stdout().execute(crossterm::cursor::Show).ok();
    stdout().execute(PopKeyboardEnhancementFlags).ok();
    let _ = write_and_flush(MOUSE_DISABLE);
    stdout().execute(DisableBracketedPaste).ok();
}

fn resume(terminal: &mut ratatui::DefaultTerminal) {
    stdout().execute(EnterAlternateScreen).ok();
    stdout().execute(EnableBracketedPaste).ok();
    let _ = write_and_flush(MOUSE_ENABLE);
    terminal::enable_raw_mode().ok();
    push_keyboard_enhancement();
    let _ = terminal.clear();
}

fn push_keyboard_enhancement() {
    if let Err(e) = stdout().execute(PushKeyboardEnhancementFlags(
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES,
    )) {
        tracing::warn!(error = %e, "failed to enable keyboard enhancement (Kitty protocol)");
    }
}

pub(crate) fn edit_temp_content(
    content: &str,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<String, String> {
    let tmp = tempfile::Builder::new()
        .prefix("craft-input-")
        .suffix(".md")
        .tempfile()
        .map_err(|e| format!("Failed to create temp file: {e}"))?;

    std::fs::write(tmp.path(), content).map_err(|e| format!("Failed to write temp file: {e}"))?;

    open_in_editor(tmp.path(), terminal)?;

    std::fs::read_to_string(tmp.path()).map_err(|e| format!("Failed to read edited content: {e}"))
}

pub(crate) fn open_in_editor(
    path: &Path,
    terminal: &mut ratatui::DefaultTerminal,
) -> Result<i32, String> {
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .map_err(|_| "Set $VISUAL or $EDITOR to open files".to_string())?;

    let args = split(&editor).map_err(|e| format!("Failed to parse $VISUAL or $EDITOR: {e}"))?;

    if args.is_empty() {
        return Err("Empty $VISUAL or $EDITOR".to_string());
    }

    teardown();

    let result = std::process::Command::new(&args[0])
        .args(&args[1..])
        .arg(path)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    resume(terminal);

    match result {
        Ok(status) => Ok(status.code().unwrap_or(-1)),
        Err(e) => Err(format!(
            "Failed to open {editor}: {e} - set $VISUAL or $EDITOR"
        )),
    }
}

pub(crate) fn copy_to_clipboard(text: &str) -> Result<(), String> {
    let mut sequence = String::new();
    CopyToClipboard::to_clipboard_from(text)
        .write_ansi(&mut sequence)
        .map_err(|e| e.to_string())?;
    let sequence = TerminalMux::detect().wrap_for_mux(sequence);
    let mut stdout = stdout().lock();
    stdout
        .write_all(sequence.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|e| e.to_string())
}

/// True if running inside tmux or screen. Kitty/iTerm2 graphics escapes
/// don't survive their DCS passthrough, so callers should downgrade to
/// halfblocks when this returns true. Zellij intercepts OSC52 natively
/// and is treated as not-muxed.
pub(crate) fn is_muxed() -> bool {
    !matches!(
        TerminalMux::detect(),
        TerminalMux::None | TerminalMux::Zellij
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_sequences_match_spec() {
        assert_eq!(SYNC_BEGIN, "\u{1b}[?2026h");
        assert_eq!(SYNC_END, "\u{1b}[?2026l");
    }

    #[test]
    fn sync_sequences_are_private_mode_csi() {
        assert!(SYNC_BEGIN.starts_with("\u{1b}[?"));
        assert!(SYNC_BEGIN.ends_with('h'));
        assert!(SYNC_END.ends_with('l'));
        assert_eq!(&SYNC_BEGIN[3..SYNC_BEGIN.len() - 1], "2026");
    }

    // Simulates DCS-passthrough parsing: `ESC ESC` becomes one ESC,
    // `ESC \` ends the DCS. Panics on bad input so tests fail loudly.
    fn parse_dcs_passthrough(wrapped: &str, prefix: &str) -> String {
        let body = wrapped
            .strip_prefix(prefix)
            .unwrap_or_else(|| panic!("missing DCS prefix {prefix:?} in {wrapped:?}"));
        let bytes = body.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        loop {
            match bytes.get(i) {
                None => panic!("DCS body missing ST terminator: {body:?}"),
                Some(&0x1B) => match bytes.get(i + 1) {
                    Some(&0x1B) => {
                        out.push(0x1B);
                        i += 2;
                    }
                    Some(&b'\\') => {
                        assert_eq!(
                            i + 2,
                            bytes.len(),
                            "unexpected trailing bytes after DCS ST: {:?}",
                            &bytes[i + 2..]
                        );
                        return String::from_utf8(out).expect("utf-8 body");
                    }
                    Some(b) => panic!("unexpected byte 0x{b:02x} after ESC inside DCS"),
                    None => panic!("lone trailing ESC in DCS body"),
                },
                Some(&b) => {
                    out.push(b);
                    i += 1;
                }
            }
        }
    }

    // Uses the ST terminator that crossterm emits, which puts ESC bytes
    // at the start and in the middle of the payload.
    const OSC52_WITH_ST: &str = "\u{1b}]52;c;SGVsbG8=\u{1b}\\";

    #[test]
    fn none_is_identity() {
        assert_eq!(
            TerminalMux::None.wrap_for_mux(OSC52_WITH_ST.to_string()),
            OSC52_WITH_ST
        );
    }

    #[test]
    fn zellij_is_identity_because_it_intercepts_osc52() {
        // Zellij handles OSC52 itself; DCS-wrapping would eat the sequence.
        assert_eq!(
            TerminalMux::Zellij.wrap_for_mux(OSC52_WITH_ST.to_string()),
            OSC52_WITH_ST
        );
    }

    #[test]
    fn tmux_wrap_survives_tmux_passthrough_parser() {
        let wrapped = TerminalMux::Tmux.wrap_for_mux(OSC52_WITH_ST.to_string());
        assert_eq!(
            parse_dcs_passthrough(&wrapped, "\u{1b}Ptmux;"),
            OSC52_WITH_ST
        );
    }

    #[test]
    fn screen_wrap_survives_screen_passthrough_parser() {
        let wrapped = TerminalMux::Screen.wrap_for_mux(OSC52_WITH_ST.to_string());
        assert_eq!(parse_dcs_passthrough(&wrapped, "\u{1b}P"), OSC52_WITH_ST);
    }

    // Multiple interior ESC bytes: if any gets left undoubled, the first
    // bare `ESC \` would close the DCS early and truncate everything after.
    #[test]
    fn tmux_preserves_payload_with_multiple_interior_esc_bytes() {
        let payload = "\u{1b}A\u{1b}B\u{1b}C\u{1b}\\";
        let wrapped = TerminalMux::Tmux.wrap_for_mux(payload.to_string());
        assert_eq!(parse_dcs_passthrough(&wrapped, "\u{1b}Ptmux;"), payload);
    }

    #[test]
    fn tmux_wrap_roundtrips_crossterm_osc52_output() {
        let mut sequence = String::new();
        CopyToClipboard::to_clipboard_from("hello, world!")
            .write_ansi(&mut sequence)
            .expect("crossterm write_ansi");
        let wrapped = TerminalMux::Tmux.wrap_for_mux(sequence.clone());
        assert_eq!(parse_dcs_passthrough(&wrapped, "\u{1b}Ptmux;"), sequence);
    }
}
