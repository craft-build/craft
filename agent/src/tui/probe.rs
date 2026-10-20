//! Startup terminal probes that own raw stdin (`F.6`).
//!
//! Every pre-`slt` query lives here: probes must run before slt's input
//! pump takes over the terminal, they bound their wait so a silent
//! terminal cannot stall startup, and they park and restore the line
//! discipline themselves. [`ui::image`]'s cell-size probe (`CSI 16 t`)
//! borrows [`read_tty_reply`].
//!
//! [`probe_truecolor`] closes the SSH color gap the environment cannot:
//! SSH forwards `TERM` but never `COLORTERM`, so a truecolor terminal
//! that reports a boring `TERM` (`xterm-256color` — iTerm2, Terminal.app,
//! VS Code out of the box) is indistinguishable from a genuinely
//! 256-color one. The XTGETTCAP query asks the terminal itself for the
//! terminfo direct-color capability `Tc` — the reply travels the same
//! connection back — and a positive ack upgrades the render depth. A
//! silent or 256-color-only terminal keeps the env-derived depth, so the
//! probe can only improve fidelity, never speculate.

use std::io::{IsTerminal, Read, Write};
use std::sync::OnceLock;
use std::time::Duration;

use slt::ColorDepth;

use crate::tui::ui::messages::is_muxed;

/// Time the XTGETTCAP reply may take to arrive. Local terminals answer in
/// single-digit milliseconds; SSH adds a round trip. Paid only by sessions
/// whose env-derived depth is already downsampled (`ack_may_upgrade` gates
/// the query), i.e. exactly the sessions with something to gain.
const TRUECOLOR_PROBE_TIMEOUT: Duration = Duration::from_millis(350);

/// XTGETTCAP request for the `Tc` (direct color) capname: DCS + q with the
/// hex encoding of the capability name ("Tc" → `54 63`), terminated by ST.
const XTGETTCAP_TC_QUERY: &str = "\x1bP+q5463\x1b\\";

/// Read a terminal reply from stdin for up to `timeout`, with raw mode (and
/// its restore) owned by this call so nothing the pump owns is disturbed.
///
/// Returns the raw bytes as UTF-8 (terminal replies here are ASCII), or
/// `None` when the write failed, nothing arrived in time, or the bytes were
/// not UTF-8. `complete` is consulted after every byte so a well-formed
/// reply can end the read early.
pub(crate) fn read_tty_reply(
    query: &str,
    timeout: Duration,
    complete: impl Fn(&[u8]) -> bool,
) -> Option<String> {
    use std::os::fd::AsRawFd;

    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    // Park the line discipline: the reply must be readable byte by byte.
    let mut saved = unsafe { std::mem::zeroed::<libc::termios>() };
    let raw_ok = unsafe { libc::tcgetattr(fd, &mut saved) } == 0;
    if raw_ok {
        let mut raw = saved;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
    }

    let mut out = std::io::stdout();
    let wrote = out
        .write_all(query.as_bytes())
        .and_then(|()| out.flush())
        .is_ok();

    let mut buf: Vec<u8> = Vec::new();
    let deadline = std::time::Instant::now() + timeout;
    if wrote {
        let mut stdin = stdin.lock();
        let mut byte = [0u8; 1];
        while deadline.duration_since(std::time::Instant::now()) > Duration::ZERO {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe {
                libc::poll(
                    &mut pfd,
                    1,
                    remaining.as_millis().min(i32::MAX as u128) as i32,
                )
            };
            if ready <= 0 {
                break; // timeout or poll error
            }
            match stdin.read(&mut byte) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    buf.push(byte[0]);
                    if complete(&buf) {
                        break;
                    }
                }
            }
        }
    }

    if raw_ok {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    }
    String::from_utf8(buf).ok()
}

/// Outcome of the truecolor probe, resolved at most once per process.
///
/// * `None` — not probed: the env chain already resolved NoColor/TrueColor
///   (the answer cannot change the depth), the session is muxed, stdio is
///   not an interactive terminal, or the binary runs under tests.
/// * `Some(true)` — the terminal answered XTGETTCAP advertising `Tc`.
/// * `Some(false)` — probed, and the terminal stayed silent or disclaimed
///   the capability; the env-derived depth stands.
static TRUECOLOR_ACK: OnceLock<Option<bool>> = OnceLock::new();

/// Whether an XTGETTCAP ack could improve on `env_depth`. Only
/// downsampled depths benefit: NoColor is an explicit user demand and
/// TrueColor already renders at full fidelity.
pub(crate) fn ack_may_upgrade(env_depth: ColorDepth) -> bool {
    matches!(env_depth, ColorDepth::EightBit | ColorDepth::Basic)
}

/// Query the terminal's `Tc` capability over the wire.
///
/// Must run before slt takes over stdin — `drive_slt` calls it next to
/// [`ui::image::probe`], in the same pre-pump window. The env-derived
/// depth decides whether firing the query is worth the bounded wait; the
/// outcome is cached and read back through [`truecolor_ack`].
pub fn probe_truecolor() {
    TRUECOLOR_ACK.get_or_init(|| {
        let var = |name: &str| std::env::var(name).ok();
        let env_depth = super::color_depth_for(
            var("NO_COLOR").as_deref(),
            var("COLORTERM").as_deref(),
            var("TERM").as_deref(),
            None,
        );
        if !ack_may_upgrade(env_depth) {
            return None; // The env chain already decided; no query needed.
        }
        query_truecolor()
    });
}

/// Send the XTGETTCAP request and classify the reply. `None`-producing
/// conditions mirror the cell-size probe's guards: tests must never touch
/// stdio, a muxer re-escapes replies it forwards, and a pipe cannot
/// answer.
fn query_truecolor() -> Option<bool> {
    if cfg!(test) || is_muxed() {
        return None;
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return None;
    }
    let reply = read_tty_reply(XTGETTCAP_TC_QUERY, TRUECOLOR_PROBE_TIMEOUT, xtgettcap_complete)?;
    Some(parse_xtgettcap_tc(&reply))
}

/// Whether the read buffer holds a complete XTGETTCAP reply: any of the
/// recognized DCS response prefixes (`1 + r` for a capability the terminal
/// has, `0 + r` for one it doesn't) followed by the ST terminator. The
/// prefix may be preceded by other replies, so the check scans the whole
/// buffer — but the ST must come after the prefix.
fn xtgettcap_complete(buf: &[u8]) -> bool {
    for prefix in [b"\x1bP1+r" as &[u8], b"\x1bP0+r"] {
        if let Some(pos) = buf.windows(prefix.len()).position(|w| w == prefix) {
            let rest = &buf[pos + prefix.len()..];
            if rest.windows(2).any(|w| w == b"\x1b\\") {
                return true;
            }
        }
    }
    false
}

/// Parse an XTGETTCAP reply for the `Tc` (truecolor) capname. A reply takes
/// the form `DCS 1 + r <hex(capname)>[=<hex(value)>] ST`; the leading `1`
/// means the capability is present, `5463` is the hex encoding of `Tc`.
/// Mirrors slt's `parse_xtgettcap_truecolor` byte for byte so the two
/// parsers cannot drift apart.
fn parse_xtgettcap_tc(reply: &str) -> bool {
    if let Some(pos) = reply.find("\x1bP1+r") {
        let body = &reply[pos + 5..];
        body.to_ascii_lowercase()
            .split([';', '\x1b'])
            .any(|seg| seg.starts_with("5463"))
    } else {
        false
    }
}

/// The cached probe outcome; read-only, safe to call after slt owns stdin.
/// Never fires the query itself.
pub(crate) fn truecolor_ack() -> Option<bool> {
    *TRUECOLOR_ACK.get_or_init(|| None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply_acks(bytes: &[u8]) -> bool {
        let s = String::from_utf8(bytes.to_vec()).unwrap();
        parse_xtgettcap_tc(&s)
    }

    #[test]
    fn xtgettcap_tc_ack_parses() {
        // xterm-style positive reply: DCS 1 + r 5463 ST.
        assert!(reply_acks(b"\x1bP1+r5463\x1b\\"));
        // With an (empty) value list for the boolean capability.
        assert!(reply_acks(b"\x1bP1+r5463=\x1b\\"));
        // Multiple capnames in one reply: any segment naming Tc acks.
        assert!(reply_acks(b"\x1bP1+r5463;7370\x1b\\"));
    }

    #[test]
    fn xtgettcap_negative_or_foreign_replies_do_not_ack() {
        // 0 + r: the terminal answered but does not have the capability.
        assert!(!reply_acks(b"\x1bP0+r5463\x1b\\"));
        // Ack for a different capname.
        assert!(!reply_acks(b"\x1bP1+r1234=\x1b\\"));
        // Silence / unrelated traffic (e.g. a late CSI-16t reply alone).
        assert!(!reply_acks(b"\x1b[6;37;17t"));
    }

    #[test]
    fn junk_before_the_ack_still_acks() {
        // A late CSI-16t reply lands ahead of the ack; the parser scans,
        // it does not anchor.
        assert!(reply_acks(b"\x1b[6;37;17t\x1bP1+r5463\x1b\\"));
    }

    #[test]
    fn completion_requires_suffix_then_st() {
        assert!(xtgettcap_complete(b"\x1bP1+r5463\x1b\\"));
        assert!(xtgettcap_complete(b"\x1b[6;37;17t\x1bP0+r5463\x1b\\"));
        // Prefix without terminator: keep waiting for the rest.
        assert!(!xtgettcap_complete(b"\x1bP1+r54"));
        // No reply at all.
        assert!(!xtgettcap_complete(b"\x1b[6;37;17t"));
    }

    #[test]
    fn only_downsampled_depths_can_upgrade() {
        assert!(ack_may_upgrade(ColorDepth::EightBit));
        assert!(ack_may_upgrade(ColorDepth::Basic));
        assert!(!ack_may_upgrade(ColorDepth::TrueColor));
        assert!(!ack_may_upgrade(ColorDepth::NoColor));
    }
}
