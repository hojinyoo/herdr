//! trzsz (`trz` / `tsz`) file-transfer passthrough.
//!
//! A pane whose child emits the trzsz trigger stops being a terminal until the
//! transfer ends: the child's raw bytes go straight to the one attached client
//! that owns the transfer, and that client's raw input goes straight back to the
//! PTY. What the trzsz client upstream needs is the child's bytes, not Herdr's
//! rendered screen, so anything Herdr parses or renders in between is lost.
//!
//! Trigger format, entry guards, and end markers follow trzsz-go at commit
//! 6650842 (`trzsz/comm.go` `detectTrzsz`, `trzsz/relay.go` `wrapInput` /
//! `wrapOutput`). Herdr deliberately does not intercept the `#ACT:` / `#CFG:`
//! handshake the way trzsz-go's relay does: that interception exists to force
//! base64 mode and to tolerate junk tmux injects into the stream, and a byte
//! exact passthrough does neither.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use crate::protocol::{ServerMessage, TerminalFrame};
use crate::server::client_transport::ClientControlWriter;

const TRIGGER: &[u8] = b"::TRZSZ:TRANSFER:";

/// Shortest chunk that can hold a trigger (trzsz-go `detectTrzsz`).
const MIN_TRIGGER_CHUNK: usize = 24;

/// A trigger split across two PTY reads is still one trigger. trzsz-go does not
/// carry a tail because its 32K reads always swallow the child's single
/// `Sync`ed write; Herdr reads 8K and cannot assume that.
const TAIL_CARRY: usize = 96;

/// trzsz-go caps its repeated-id map at 100 entries.
const SEEN_ID_LIMIT: usize = 100;

/// How long a trigger may go unanswered before the pane is released.
///
/// This is not a round-trip bound. trzsz asks the user where to save *before*
/// it sends `#ACT:`, so an unanswered trigger and a real transfer waiting on a
/// file dialog look identical from here, and any bound short enough to catch
/// the first races the human on the second. So it is deliberately longer than
/// a person needs to pick a folder; a lone Ctrl-C is the immediate escape, the
/// same one trzsz itself gives you.
const ARMING_TIMEOUT: Duration = Duration::from_secs(120);

/// A transfer whose client or link died produces no end marker at all. Applies
/// only once armed: before that the link is legitimately silent.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Echoed history or a finished transfer rather than a live trigger
/// (trzsz-go `detectTrzsz`).
const ECHO_GUARDS: [&[u8]; 5] = [b"#CFG:", b"Saved", b"Cancelled", b"Stopped", b"Interrupted"];
const ECHO_GUARD_OFFSET: usize = 40;

/// `#EXIT:` is the normal end and travels client to child; `#FAIL:` / `#fail:`
/// can appear in either direction. trzsz-go's relay scans for exactly these.
const END_MARKERS: [&[u8]; 3] = [b"#EXIT:", b"#FAIL:", b"#fail:"];

static GATE: OnceLock<TransferGate> = OnceLock::new();

pub(crate) fn gate() -> &'static TransferGate {
    GATE.get_or_init(TransferGate::new)
}

/// Why a passthrough session ended, for logging and operator-facing reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndReason {
    /// `#EXIT:` from the client: the transfer completed.
    Completed,
    /// `#FAIL:` / `#fail:` in either direction.
    Failed,
    /// A lone `\x03` from the owning client.
    Cancelled,
    /// Nothing came back from the client: the trigger was not a real transfer.
    FalseTrigger,
    /// Nothing moved in either direction for [`IDLE_TIMEOUT`].
    Idle,
    /// The owning client went away, or the pane died.
    Abandoned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransferEnded {
    pub(crate) pane: u32,
    pub(crate) client_id: u64,
    pub(crate) reason: EndReason,
}

/// What the caller should do with a chunk of pane output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum PaneOutput {
    /// Feed it to the emulator as usual.
    #[default]
    Emulate,
    /// Emulate only the first N bytes. The rest of the chunk was the trigger
    /// and went to the owner raw, so the emulator never records it.
    EmulatePrefix(usize),
    /// It went to the transfer owner raw; the emulator must not see it.
    Consumed,
}

/// What the caller should do with a chunk of client input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClientInput {
    /// No transfer is running, or another client sent it; handle it normally.
    /// Input from other clients still cannot reach the transferring pane: the
    /// PTY write itself is refused while a transfer owns the pane.
    Normal,
    /// Write the bytes to this pane's PTY verbatim.
    Passthrough(u32),
}

pub(crate) struct Trigger {
    pub(crate) unique_id: String,
    /// Offset of the trigger within the scanned buffer. Everything before it,
    /// including the child's DECSC, is ordinary output the emulator still needs.
    pub(crate) start: usize,
}

/// Scans one chunk of pane output for a live trzsz trigger.
///
/// Guards run cheapest-first. The last one is a Herdr addition: the child
/// `Sync`s straight after writing the trigger and then goes quiet waiting for
/// `#ACT:`, so a live trigger closes its chunk, while a trigger sitting inside
/// a `cat`ed log almost never does.
pub(crate) fn find_trigger(buf: &[u8]) -> Option<Trigger> {
    if buf.len() < MIN_TRIGGER_CHUNK {
        return None;
    }
    let idx = memchr::memmem::rfind(buf, TRIGGER)?;
    let sub = &buf[idx..];
    if sub.len() > ECHO_GUARD_OFFSET
        && ECHO_GUARDS
            .iter()
            .any(|guard| memchr::memmem::find(&sub[ECHO_GUARD_OFFSET..], guard).is_some())
    {
        return None;
    }
    let body = &sub[TRIGGER.len()..];
    let (unique_id, consumed) = parse_trigger(body)?;
    if body[consumed..]
        .iter()
        .any(|byte| !matches!(byte, b'\r' | b'\n'))
    {
        return None;
    }
    Some(Trigger {
        unique_id,
        start: idx,
    })
}

/// Parses `<mode>:<major>.<minor>.<patch>[:<uniqueID>][:<port>]`, returning the
/// unique id and how many bytes the trigger occupies.
fn parse_trigger(body: &[u8]) -> Option<(String, usize)> {
    if !matches!(body.first()?, b'S' | b'R' | b'D' | b'F') {
        return None;
    }
    let mut idx = 1;
    if body.get(idx) != Some(&b':') {
        return None;
    }
    idx += 1;
    for part in 0..3 {
        if part > 0 {
            if body.get(idx) != Some(&b'.') {
                return None;
            }
            idx += 1;
        }
        let len = digit_run(body, idx);
        if len == 0 {
            return None;
        }
        idx += len;
    }

    let mut unique_id = String::new();
    if body.get(idx) == Some(&b':') {
        let len = digit_run(body, idx + 1);
        if len > 0 {
            unique_id = String::from_utf8_lossy(&body[idx + 1..idx + 1 + len]).into_owned();
            idx += 1 + len;
        }
    }
    if body.get(idx) == Some(&b':') {
        let len = digit_run(body, idx + 1);
        if len > 0 {
            idx += 1 + len;
        }
    }
    Some((unique_id, idx))
}

fn digit_run(buf: &[u8], from: usize) -> usize {
    if from >= buf.len() {
        return 0;
    }
    buf[from..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count()
}

fn contains_end_marker(buf: &[u8]) -> bool {
    END_MARKERS
        .iter()
        .any(|marker| memchr::memmem::find(buf, marker).is_some())
}

struct Candidate {
    client_id: u64,
    writer: ClientControlWriter,
}

struct Session {
    pane: u32,
    client_id: u64,
    writer: ClientControlWriter,
    seq: u64,
    /// Chunks of client input the gate actually saw. Separates "no client input
    /// reached the gate at all" from "it did, and the transfer still stalled".
    client_seq: u64,
    armed: bool,
    started: Instant,
    last_activity: Instant,
}

#[derive(Default)]
struct GateInner {
    candidate: Option<Candidate>,
    session: Option<Session>,
    tail: Vec<u8>,
    seen_ids: VecDeque<String>,
}

/// Server-wide passthrough state.
///
/// One transfer at a time: the owner is a single client terminal and trzsz
/// drives one transfer per terminal anyway. Keeping it server-wide means the
/// per-byte cost of detection is O(1) in pane count rather than O(panes) — only
/// the eligible pane is ever scanned.
pub(crate) struct TransferGate {
    enabled: AtomicBool,
    /// Pane allowed to start a transfer: the foreground client's focused pane.
    /// `PaneId` never allocates 0, so 0 means none.
    eligible_pane: AtomicU32,
    /// Pane currently in passthrough, or 0.
    active_pane: AtomicU32,
    /// Client that owns the running passthrough, or 0.
    owner_client: AtomicU64,
    inner: Mutex<GateInner>,
}

impl Default for TransferGate {
    fn default() -> Self {
        Self::new()
    }
}

impl TransferGate {
    pub(crate) fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            eligible_pane: AtomicU32::new(0),
            active_pane: AtomicU32::new(0),
            owner_client: AtomicU64::new(0),
            inner: Mutex::new(GateInner::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, GateInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Turning the feature off has to release a running transfer too, or the
    /// pane keeps refusing writes with nothing left to clear the session.
    pub(crate) fn set_enabled(&self, enabled: bool) -> Option<TransferEnded> {
        self.enabled.store(enabled, Ordering::Relaxed);
        if enabled {
            return None;
        }
        self.eligible_pane.store(0, Ordering::Relaxed);
        let mut inner = self.lock();
        inner.candidate = None;
        self.finish(&mut inner, EndReason::Abandoned)
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub(crate) fn active_pane(&self) -> Option<u32> {
        match self.active_pane.load(Ordering::Relaxed) {
            0 => None,
            pane => Some(pane),
        }
    }

    pub(crate) fn owner_client(&self) -> Option<u64> {
        match self.owner_client.load(Ordering::Relaxed) {
            0 => None,
            client_id => Some(client_id),
        }
    }

    /// Writes to a pane in passthrough must come from the owning client only.
    /// Every other writer — keybindings, mouse, `herdr pane input`, agent sends
    /// — would inject bytes into the middle of the protocol stream.
    pub(crate) fn write_blocked(&self, pane: u32) -> bool {
        self.active_pane.load(Ordering::Relaxed) == pane && pane != 0
    }

    /// Records which client and pane may start the next transfer. Called on
    /// focus and foreground changes, never per byte.
    pub(crate) fn set_candidate(
        &self,
        pane: Option<u32>,
        client: Option<(u64, ClientControlWriter)>,
    ) {
        let mut inner = self.lock();
        let pane = pane.unwrap_or(0);
        let eligible = if client.is_some() { pane } else { 0 };
        if self.eligible_pane.swap(eligible, Ordering::Relaxed) != eligible {
            inner.tail.clear();
        }
        inner.candidate = client.map(|(client_id, writer)| Candidate { client_id, writer });
    }

    /// Routes one chunk of pane output.
    ///
    /// Hot path for every pane on every PTY read: two relaxed loads decide it
    /// for panes that are neither active nor eligible.
    pub(crate) fn on_pane_output(&self, pane: u32, bytes: &[u8]) -> PaneOutput {
        if self.active_pane.load(Ordering::Relaxed) == pane {
            return self.forward_output(pane, bytes);
        }
        if self.enabled.load(Ordering::Relaxed)
            && self.eligible_pane.load(Ordering::Relaxed) == pane
        {
            return self.try_start(pane, bytes);
        }
        PaneOutput::Emulate
    }

    fn try_start(&self, pane: u32, bytes: &[u8]) -> PaneOutput {
        let mut inner = self.lock();
        if inner.session.is_some() {
            return PaneOutput::Emulate;
        }

        let carry_len = inner.tail.len();
        let trigger = if carry_len == 0 {
            find_trigger(bytes)
        } else {
            inner.tail.extend_from_slice(bytes);
            find_trigger(&inner.tail)
        };

        let Some(trigger) = trigger else {
            let keep = bytes.len().min(TAIL_CARRY);
            inner.tail.clear();
            inner.tail.extend_from_slice(&bytes[bytes.len() - keep..]);
            return PaneOutput::Emulate;
        };
        inner.tail.clear();

        if !trigger.unique_id.is_empty() && inner.seen_ids.contains(&trigger.unique_id) {
            debug!(pane, "ignored repeated trzsz trigger id");
            return PaneOutput::Emulate;
        }
        let Some(candidate) = inner.candidate.as_ref() else {
            debug!(
                pane,
                "trzsz trigger with no eligible client; rendering as text"
            );
            return PaneOutput::Emulate;
        };

        let client_id = candidate.client_id;
        let writer = candidate.writer.clone();
        // Everything up to the trigger is ordinary output and still has to be
        // emulated, in particular the DECSC the child restores from at the end.
        // Whatever the carried tail already covered was emulated last time.
        let prefix = trigger.start.saturating_sub(carry_len);
        if !trigger.unique_id.is_empty() {
            if inner.seen_ids.len() >= SEEN_ID_LIMIT {
                inner.seen_ids.pop_front();
            }
            inner.seen_ids.push_back(trigger.unique_id);
        }

        let now = Instant::now();
        let mut session = Session {
            pane,
            client_id,
            writer,
            seq: 0,
            client_seq: 0,
            armed: false,
            started: now,
            last_activity: now,
        };

        // Ordering matters: the client must be told to stop interpreting its own
        // stdin before the trigger it is about to act on reaches it.
        if send_message(
            &session.writer,
            &ServerMessage::TransferPassthrough { active: true },
        )
        .is_err()
            || send_passthrough(&mut session, bytes).is_err()
        {
            warn!(pane, client_id, "trzsz owner writer closed while starting");
            return PaneOutput::Emulate;
        }

        self.owner_client.store(client_id, Ordering::Relaxed);
        self.active_pane.store(pane, Ordering::Relaxed);
        inner.session = Some(session);
        info!(pane, client_id, "trzsz passthrough started");
        PaneOutput::EmulatePrefix(prefix)
    }

    fn forward_output(&self, pane: u32, bytes: &[u8]) -> PaneOutput {
        let mut inner = self.lock();
        let Some(session) = inner.session.as_mut() else {
            return PaneOutput::Emulate;
        };
        if session.pane != pane {
            return PaneOutput::Emulate;
        }
        session.last_activity = Instant::now();

        let reason = if send_passthrough(session, bytes).is_err() {
            Some(EndReason::Abandoned)
        } else if contains_end_marker(bytes) {
            Some(EndReason::Failed)
        } else {
            None
        };

        match reason {
            // Emulating the closing chunk lets the child's own `\x1b[u\x1b[0J`
            // restore the pane screen, exactly as it would on a bare terminal.
            Some(reason) => {
                self.finish(&mut inner, reason);
                PaneOutput::Emulate
            }
            None => PaneOutput::Consumed,
        }
    }

    /// Routes one chunk of client input. The bytes are the caller's to write;
    /// the gate only decides where they go and whether the session is over.
    pub(crate) fn on_client_input(&self, client_id: u64, data: &[u8]) -> ClientInput {
        if self.active_pane.load(Ordering::Relaxed) == 0 {
            return ClientInput::Normal;
        }
        let mut inner = self.lock();
        let Some(session) = inner.session.as_mut() else {
            return ClientInput::Normal;
        };
        if session.client_id != client_id {
            return ClientInput::Normal;
        }

        let pane = session.pane;
        session.last_activity = Instant::now();
        session.client_seq = session.client_seq.saturating_add(1);
        // Any byte back from the owner proves a live client is on the other end,
        // which is all `armed` has to mean. Matching `#ACT:` specifically was
        // both narrower and more fragile: the answer can split across reads the
        // same way a trigger can, and a scan that misses it kills a working
        // transfer at the arming timeout.
        if !session.armed {
            session.armed = true;
            debug!(pane, "trzsz transfer armed by client answer");
        }

        let ended = if data == [0x03] {
            Some(EndReason::Cancelled)
        } else if contains_end_marker(data) {
            Some(EndReason::Completed)
        } else {
            None
        };
        if let Some(reason) = ended {
            // The child still needs these bytes: `#EXIT:` is what makes it
            // restore the terminal and exit. The caller writes them either way.
            self.finish(&mut inner, reason);
        }
        ClientInput::Passthrough(pane)
    }

    pub(crate) fn check_timeouts(&self) -> Option<TransferEnded> {
        if self.active_pane.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let mut inner = self.lock();
        let session = inner.session.as_ref()?;
        let now = Instant::now();
        let reason = if !session.armed {
            if now.duration_since(session.started) < ARMING_TIMEOUT {
                return None;
            }
            EndReason::FalseTrigger
        } else if now.duration_since(session.last_activity) >= IDLE_TIMEOUT {
            EndReason::Idle
        } else {
            return None;
        };
        self.finish(&mut inner, reason)
    }

    pub(crate) fn end_for_client(&self, client_id: u64) -> Option<TransferEnded> {
        let mut inner = self.lock();
        if inner.session.as_ref()?.client_id != client_id {
            return None;
        }
        self.finish(&mut inner, EndReason::Abandoned)
    }

    pub(crate) fn end_for_pane(&self, pane: u32) -> Option<TransferEnded> {
        let mut inner = self.lock();
        if inner.session.as_ref()?.pane != pane {
            return None;
        }
        self.finish(&mut inner, EndReason::Abandoned)
    }

    fn finish(&self, inner: &mut GateInner, reason: EndReason) -> Option<TransferEnded> {
        let session = inner.session.take()?;
        self.active_pane.store(0, Ordering::Relaxed);
        self.owner_client.store(0, Ordering::Relaxed);
        inner.tail.clear();
        let _ = send_message(
            &session.writer,
            &ServerMessage::TransferPassthrough { active: false },
        );
        // A pane that stopped emulating and then started again is a visible
        // event with a cause the user cannot otherwise see. Log it at info so
        // "the pane froze for a moment" is answerable from the log alone.
        info!(
            pane = session.pane,
            client_id = session.client_id,
            ?reason,
            armed = session.armed,
            chunks = session.seq,
            client_chunks = session.client_seq,
            held_ms = session.started.elapsed().as_millis(),
            idle_ms = session.last_activity.elapsed().as_millis(),
            "trzsz passthrough ended"
        );
        Some(TransferEnded {
            pane: session.pane,
            client_id: session.client_id,
            reason,
        })
    }

    /// Marks a pane as transferring without a client, for tests that only need
    /// the PTY write guard.
    #[cfg(test)]
    pub(crate) fn test_take_pane(&self, pane: u32) {
        self.active_pane.store(pane, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn test_release_pane(&self) {
        self.active_pane.store(0, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn test_session_active(&self) -> bool {
        self.lock().session.is_some()
    }

    #[cfg(test)]
    pub(crate) fn test_set_started(&self, started: Instant) {
        if let Some(session) = self.lock().session.as_mut() {
            session.started = started;
        }
    }

    #[cfg(test)]
    pub(crate) fn test_set_last_activity(&self, at: Instant) {
        if let Some(session) = self.lock().session.as_mut() {
            session.last_activity = at;
        }
    }
}

fn send_passthrough(session: &mut Session, bytes: &[u8]) -> Result<(), ()> {
    session.seq += 1;
    send_message(
        &session.writer,
        &ServerMessage::Terminal(TerminalFrame {
            seq: session.seq,
            width: 0,
            height: 0,
            full: false,
            bytes: bytes.to_vec(),
        }),
    )
}

fn send_message(writer: &ClientControlWriter, message: &ServerMessage) -> Result<(), ()> {
    let mut framed = Vec::new();
    if crate::protocol::write_message(&mut framed, message).is_err() {
        return Err(());
    }
    writer.send(framed).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VERSION: &str = "1.2.0";

    fn trigger_bytes(mode: char, unique_id: u64) -> Vec<u8> {
        format!("\x1b[s::TRZSZ:TRANSFER:{mode}:{VERSION}:{unique_id:013}:0\r\n").into_bytes()
    }

    #[test]
    fn detects_every_transfer_mode() {
        for mode in ['S', 'R', 'D', 'F'] {
            let found = find_trigger(&trigger_bytes(mode, 1_700_000_000_000));
            assert!(found.is_some(), "mode {mode} should be detected");
            assert_eq!(found.unwrap().unique_id, "1700000000000");
        }
    }

    #[test]
    fn detects_trigger_without_unique_id_or_port() {
        let found = find_trigger(b"padding-padding-padding::TRZSZ:TRANSFER:S:1.2.0\r\n");
        assert!(found.is_some());
        assert_eq!(found.unwrap().unique_id, "");
    }

    #[test]
    fn rejects_trigger_that_does_not_close_the_chunk() {
        let mut buf = trigger_bytes('S', 1_700_000_000_000);
        buf.extend_from_slice(b"and then more log output\r\n");
        assert!(find_trigger(&buf).is_none());
    }

    #[test]
    fn rejects_malformed_mode_and_version() {
        assert!(find_trigger(b"xxxxxxxxxxxxxxxxxxxxxxxx::TRZSZ:TRANSFER:X:1.2.0\r\n").is_none());
        assert!(find_trigger(b"xxxxxxxxxxxxxxxxxxxxxxxx::TRZSZ:TRANSFER:S:1.2\r\n").is_none());
        assert!(find_trigger(b"xxxxxxxxxxxxxxxxxxxxxxxx::TRZSZ:TRANSFER:S:\r\n").is_none());
    }

    #[test]
    fn rejects_echoed_trigger_carrying_a_result_word() {
        let mut buf = trigger_bytes('S', 1_700_000_000_000);
        buf.extend_from_slice(b"                                        Cancelled\r\n");
        assert!(find_trigger(&buf).is_none());
    }

    #[test]
    fn rejects_short_chunks() {
        assert!(find_trigger(b"::TRZSZ:TRANSFER:S").is_none());
    }

    #[test]
    fn finds_the_last_trigger_in_a_chunk() {
        let mut buf = trigger_bytes('R', 1_700_000_000_000);
        buf.extend_from_slice(&trigger_bytes('S', 1_700_000_000_099));
        assert_eq!(find_trigger(&buf).unwrap().unique_id, "1700000000099");
    }

    #[test]
    fn end_markers_match_both_directions() {
        assert!(contains_end_marker(b"junk#EXIT:c2F2ZWQ=\n"));
        assert!(contains_end_marker(b"#FAIL:x\n"));
        assert!(contains_end_marker(b"#fail:x\n"));
        assert!(!contains_end_marker(b"#DATA:0123456789\n"));
    }

    #[test]
    fn gate_ignores_panes_that_are_not_eligible() {
        let gate = TransferGate::new();
        gate.set_enabled(true);
        assert_eq!(
            gate.on_pane_output(7, &trigger_bytes('S', 1_700_000_000_000)),
            PaneOutput::Emulate
        );
        assert!(!gate.test_session_active());
    }

    #[test]
    fn gate_ignores_trigger_with_no_eligible_client() {
        let gate = TransferGate::new();
        gate.set_enabled(true);
        gate.set_candidate(Some(7), None);
        assert_eq!(
            gate.on_pane_output(7, &trigger_bytes('S', 1_700_000_000_000)),
            PaneOutput::Emulate
        );
        assert!(!gate.test_session_active());
    }

    #[test]
    fn gate_stays_inert_when_disabled() {
        let gate = TransferGate::new();
        gate.set_candidate(Some(7), None);
        assert_eq!(gate.eligible_pane.load(Ordering::Relaxed), 0);
        assert!(!gate.is_enabled());
    }

    #[test]
    fn write_blocked_only_for_the_active_pane() {
        let gate = TransferGate::new();
        assert!(!gate.write_blocked(0));
        gate.active_pane.store(7, Ordering::Relaxed);
        assert!(gate.write_blocked(7));
        assert!(!gate.write_blocked(8));
    }

    #[test]
    fn input_is_normal_when_no_session_runs() {
        let gate = TransferGate::new();
        assert_eq!(gate.on_client_input(1, b"hello"), ClientInput::Normal);
    }

    const OWNER: u64 = 4;
    const PANE: u32 = 7;

    struct Harness {
        gate: TransferGate,
        control: std::sync::mpsc::Receiver<Vec<u8>>,
        _writer: crate::server::client_transport::ClientWriter,
        _render: std::sync::mpsc::Receiver<Vec<u8>>,
    }

    impl Harness {
        fn new() -> Self {
            let (control_tx, control) = std::sync::mpsc::channel();
            let (render_tx, _render) = std::sync::mpsc::sync_channel(8);
            let writer =
                crate::server::client_transport::ClientWriter::test_channel(control_tx, render_tx);
            let gate = TransferGate::new();
            gate.set_enabled(true);
            gate.set_candidate(Some(PANE), Some((OWNER, writer.control.clone())));
            Self {
                gate,
                control,
                _writer: writer,
                _render,
            }
        }

        fn start(&self, unique_id: u64) {
            assert_eq!(
                self.gate
                    .on_pane_output(PANE, &trigger_bytes('S', unique_id)),
                PaneOutput::EmulatePrefix(3),
                "only the child's DECSC belongs to the emulator, never the trigger"
            );
        }

        fn next_message(&self) -> ServerMessage {
            let framed = self
                .control
                .recv_timeout(Duration::from_secs(5))
                .expect("owner should have received a message");
            crate::protocol::read_message(
                &mut std::io::Cursor::new(framed),
                crate::protocol::MAX_FRAME_SIZE,
            )
            .expect("decode server message")
        }

        /// Skips the `TransferPassthrough` control message a start emits first.
        fn next_bytes_after_start(&self) -> Vec<u8> {
            assert_eq!(
                self.next_message(),
                ServerMessage::TransferPassthrough { active: true }
            );
            self.next_bytes()
        }

        fn next_bytes(&self) -> Vec<u8> {
            match self.next_message() {
                ServerMessage::Terminal(frame) => frame.bytes,
                other => panic!("expected raw transfer bytes, got {other:?}"),
            }
        }
    }

    #[test]
    fn starting_tells_the_owner_first_then_forwards_the_trigger() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);

        assert!(harness.gate.test_session_active());
        assert_eq!(harness.gate.active_pane(), Some(PANE));
        assert_eq!(harness.gate.owner_client(), Some(OWNER));
        assert_eq!(
            harness.next_message(),
            ServerMessage::TransferPassthrough { active: true },
            "the client must stop interpreting stdin before it sees the trigger"
        );
        assert_eq!(harness.next_bytes(), trigger_bytes('S', 1_700_000_000_000));
    }

    #[test]
    fn payload_reaches_the_owner_and_never_the_emulator() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);
        harness.next_message();
        harness.next_bytes();

        assert_eq!(
            harness.gate.on_pane_output(PANE, b"#DATA:0123456789"),
            PaneOutput::Consumed
        );
        assert_eq!(harness.next_bytes(), b"#DATA:0123456789");
    }

    #[test]
    fn other_panes_keep_emulating_during_a_transfer() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);

        assert_eq!(
            harness
                .gate
                .on_pane_output(PANE + 1, b"ordinary shell output"),
            PaneOutput::Emulate
        );
        assert!(harness.gate.write_blocked(PANE));
        assert!(!harness.gate.write_blocked(PANE + 1));
    }

    #[test]
    fn a_false_trigger_escapes_on_the_arming_timeout() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);
        harness.next_message();
        harness.next_bytes();

        assert!(harness.gate.check_timeouts().is_none());

        harness
            .gate
            .test_set_started(Instant::now() - ARMING_TIMEOUT);
        let ended = harness.gate.check_timeouts().expect("timeout should fire");
        assert_eq!(ended.reason, EndReason::FalseTrigger);
        assert!(!harness.gate.test_session_active());
        assert_eq!(harness.gate.active_pane(), None);
        assert!(!harness.gate.write_blocked(PANE));
        assert_eq!(
            harness.next_message(),
            ServerMessage::TransferPassthrough { active: false }
        );
    }

    #[test]
    fn an_answered_trigger_survives_the_arming_timeout() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);

        assert_eq!(
            harness.gate.on_client_input(OWNER, b"#ACT:eyJ9\n"),
            ClientInput::Passthrough(PANE)
        );
        harness
            .gate
            .test_set_started(Instant::now() - ARMING_TIMEOUT);

        assert!(harness.gate.check_timeouts().is_none());
        assert!(harness.gate.test_session_active());
    }

    /// Regression: arming used to require matching `#ACT:` in one chunk, so an
    /// answer split across two reads left a working transfer to be killed by
    /// the arming timeout.
    #[test]
    fn any_answer_arms_the_transfer_even_split_across_reads() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);

        assert_eq!(
            harness.gate.on_client_input(OWNER, b"#A"),
            ClientInput::Passthrough(PANE)
        );
        harness
            .gate
            .test_set_started(Instant::now() - ARMING_TIMEOUT);
        assert!(
            harness.gate.check_timeouts().is_none(),
            "a client that answered at all is not a false trigger"
        );
    }

    #[test]
    fn an_answered_transfer_still_escapes_when_the_link_goes_quiet() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);
        harness.gate.on_client_input(OWNER, b"#ACT:eyJ9\n");

        harness
            .gate
            .test_set_last_activity(Instant::now() - IDLE_TIMEOUT);
        let ended = harness.gate.check_timeouts().expect("idle should fire");
        assert_eq!(ended.reason, EndReason::Idle);
    }

    /// Regression: a real transfer was killed at 10s while the user was still
    /// picking a download folder, because trzsz only sends `#ACT:` afterwards.
    #[test]
    fn an_unanswered_trigger_outlives_the_idle_timeout() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);

        harness.gate.test_set_started(Instant::now() - IDLE_TIMEOUT);
        assert!(
            harness.gate.check_timeouts().is_none(),
            "silence before #ACT: is the user thinking, not a dead link"
        );
        assert!(harness.gate.test_session_active());

        harness
            .gate
            .test_set_started(Instant::now() - ARMING_TIMEOUT);
        assert_eq!(
            harness.gate.check_timeouts().map(|ended| ended.reason),
            Some(EndReason::FalseTrigger)
        );
    }

    #[test]
    fn exit_from_the_owner_ends_the_transfer_but_still_reaches_the_child() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);

        assert_eq!(
            harness.gate.on_client_input(OWNER, b"#EXIT:c2F2ZWQ=\n"),
            ClientInput::Passthrough(PANE),
            "`#EXIT:` is what makes the child restore the terminal and exit"
        );
        assert!(!harness.gate.test_session_active());
    }

    #[test]
    fn a_lone_ctrl_c_from_the_owner_cancels() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);

        assert_eq!(
            harness.gate.on_client_input(OWNER, &[0x03]),
            ClientInput::Passthrough(PANE)
        );
        assert!(!harness.gate.test_session_active());
    }

    #[test]
    fn a_ctrl_c_byte_inside_payload_does_not_cancel() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);

        assert_eq!(
            harness.gate.on_client_input(OWNER, &[0x03, 0x04]),
            ClientInput::Passthrough(PANE)
        );
        assert!(harness.gate.test_session_active());
    }

    #[test]
    fn a_failure_in_pane_output_ends_and_resumes_emulation() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);

        assert_eq!(
            harness.gate.on_pane_output(PANE, b"#fail:bm9wZQ==\n"),
            PaneOutput::Emulate,
            "the closing chunk carries the child's cursor restore"
        );
        assert!(!harness.gate.test_session_active());
    }

    #[test]
    fn input_from_another_client_is_left_to_the_normal_path() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);

        assert_eq!(
            harness.gate.on_client_input(OWNER + 1, b"ls\r"),
            ClientInput::Normal
        );
        assert!(
            harness.gate.write_blocked(PANE),
            "the PTY write guard is what keeps it out of the transfer"
        );
        assert!(harness.gate.test_session_active());
    }

    #[test]
    fn the_owner_leaving_ends_the_transfer() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);

        assert!(harness.gate.end_for_client(OWNER + 1).is_none());
        let ended = harness.gate.end_for_client(OWNER).expect("owner left");
        assert_eq!(ended.reason, EndReason::Abandoned);
        assert!(!harness.gate.test_session_active());
    }

    #[test]
    fn the_pane_dying_ends_the_transfer() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);

        assert!(harness.gate.end_for_pane(PANE + 1).is_none());
        assert_eq!(
            harness.gate.end_for_pane(PANE).map(|ended| ended.reason),
            Some(EndReason::Abandoned)
        );
    }

    #[test]
    fn a_replayed_trigger_id_does_not_start_a_second_transfer() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);
        harness.gate.on_client_input(OWNER, b"#EXIT:c2F2ZWQ=\n");
        assert!(!harness.gate.test_session_active());

        assert_eq!(
            harness
                .gate
                .on_pane_output(PANE, &trigger_bytes('S', 1_700_000_000_000)),
            PaneOutput::Emulate,
            "a rejected trigger is ordinary text and still belongs on screen"
        );
        assert!(
            !harness.gate.test_session_active(),
            "an echoed trigger must not re-enter passthrough"
        );

        harness.start(1_700_000_000_099);
        assert!(harness.gate.test_session_active());
    }

    /// Regression: the trigger used to be emulated, so it sat in the pane's
    /// screen buffer. The full repaint that ends a transfer then shipped it
    /// straight back to the trzsz client, which started a second handshake that
    /// nothing would ever answer.
    #[test]
    fn the_emulator_sees_the_decsc_but_never_the_trigger() {
        let harness = Harness::new();
        let mut chunk = b"$ tsz report.pdf\r\n".to_vec();
        let decsc_at = chunk.len();
        chunk.extend_from_slice(&trigger_bytes('S', 1_700_000_000_000));

        let PaneOutput::EmulatePrefix(prefix) = harness.gate.on_pane_output(PANE, &chunk) else {
            panic!("a trigger chunk must not be emulated whole");
        };
        assert_eq!(prefix, decsc_at + 3);
        assert_eq!(&chunk[..prefix], b"$ tsz report.pdf\r\n\x1b[s");
        assert!(
            memchr::memmem::find(&chunk[..prefix], TRIGGER).is_none(),
            "no part of the trigger may reach the screen buffer"
        );
        assert_eq!(harness.next_bytes_after_start(), chunk);
    }

    #[test]
    fn a_split_trigger_keeps_its_carried_half_out_of_the_prefix() {
        let harness = Harness::new();
        let trigger = trigger_bytes('S', 1_700_000_000_000);
        let (head, tail) = trigger.split_at(26);

        harness.gate.on_pane_output(PANE, head);
        assert_eq!(
            harness.gate.on_pane_output(PANE, tail),
            PaneOutput::EmulatePrefix(0),
            "the trigger began in the previous chunk, so none of this one is output"
        );
    }

    #[test]
    fn a_trigger_split_across_two_reads_is_still_one_trigger() {
        let harness = Harness::new();
        let trigger = trigger_bytes('S', 1_700_000_000_000);
        // Split inside the version, so the leading chunk carries the whole
        // needle yet cannot parse on its own.
        let (head, tail) = trigger.split_at(26);
        assert_eq!(&head[head.len() - 6..], b"S:1.2.");

        harness.gate.on_pane_output(PANE, head);
        assert!(!harness.gate.test_session_active());

        harness.gate.on_pane_output(PANE, tail);
        assert!(harness.gate.test_session_active());
    }

    #[test]
    fn a_disabled_gate_never_enters_passthrough() {
        let harness = Harness::new();
        harness.gate.set_enabled(false);

        harness.gate.on_pane_output(PANE, &trigger_bytes('S', 1));
        assert!(!harness.gate.test_session_active());
    }

    #[test]
    fn disabling_mid_transfer_releases_the_pane() {
        let harness = Harness::new();
        harness.start(1_700_000_000_000);
        assert!(harness.gate.write_blocked(PANE));

        let ended = harness
            .gate
            .set_enabled(false)
            .expect("disabling must release the running transfer");
        assert_eq!(ended.reason, EndReason::Abandoned);
        assert!(!harness.gate.test_session_active());
        assert!(
            !harness.gate.write_blocked(PANE),
            "a released pane must accept writes again"
        );
    }
}
