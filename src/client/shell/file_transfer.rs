//! Native file transfer, client half: the overlay that drives it and the bytes
//! it moves.
//!
//! The engine (`crate::file_transfer`) does the bytes and the trust boundary.
//! Everything here is the shell's own state, because the whole UI is
//! client-side; the server only runs its half of the state machine.

use std::path::PathBuf;

use tracing::{debug, warn};

use crate::file_transfer as engine;
use crate::protocol::file_transfer::{
    decode_chunk, encode_chunk, ClientFileTransferControl, ServerFileTransferControl,
};

use super::*;

/// Which way the bytes move, from the perspective of the machine running the
/// herdr server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientFileTransferDirection {
    /// Client-local file into the focused pane's working directory.
    Send,
    /// Server-side file into the client's configured download directory.
    Receive,
}

impl ClientFileTransferDirection {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Send => "send file",
            Self::Receive => "receive file",
        }
    }
}

/// One row in the receive browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ClientFileBrowserEntry {
    pub(super) name: String,
    pub(super) is_dir: bool,
    /// `None` for directories and for anything whose metadata could not be read.
    pub(super) size: Option<u64>,
    /// The `..` row, which sorts first and is never filtered out.
    pub(super) is_parent: bool,
}

impl ClientFileBrowserEntry {
    fn matches(&self, query: &str) -> bool {
        self.is_parent || self.name.to_lowercase().contains(&query.to_lowercase())
    }
}

/// Receive-side file browser. The source lives on the machine running the
/// server, so every listing is a round trip.
#[derive(Debug, Default)]
pub(super) struct ClientFileBrowser {
    pub(super) dir: String,
    /// Parent of `dir` as the server reported it. Never derived here: the
    /// server's path syntax is not necessarily this machine's.
    parent: Option<String>,
    pub(super) entries: Vec<ClientFileBrowserEntry>,
    pub(super) selected: usize,
    pub(super) query: String,
    pub(super) show_hidden: bool,
    /// Set when the directory could not be read; the list is empty but the
    /// browser stays open so the user can go back up.
    pub(super) error: Option<String>,
    /// True when the listing was capped, so the footer can say so rather than
    /// silently showing a partial directory.
    pub(super) truncated: bool,
    /// Where the selected file will land, resolved from this client's config.
    pub(super) destination: String,
    /// First filtered row drawn. Remembered rather than derived from the
    /// selection: a window that re-centres on every selection scrolls the list
    /// out from under the pointer, so clicking a row moves the row you just
    /// clicked.
    pub(super) scroll: usize,
    /// A listing is in flight, so the empty list has a reason.
    pub(super) loading: bool,
    /// Rows the last render drew. Shared with key handling so the keyboard and
    /// the mouse agree on the window.
    pub(super) visible_rows: usize,
}

impl ClientFileBrowser {
    pub(super) fn filtered_indices(&self) -> Vec<usize> {
        let query = self.query.trim();
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(idx, entry)| (query.is_empty() || entry.matches(query)).then_some(idx))
            .collect()
    }

    fn selected_entry(&self) -> Option<&ClientFileBrowserEntry> {
        let indices = self.filtered_indices();
        if indices.contains(&self.selected) {
            return self.entries.get(self.selected);
        }
        indices.first().and_then(|idx| self.entries.get(*idx))
    }

    fn select_next(&mut self) {
        let indices = self.filtered_indices();
        if indices.is_empty() {
            return;
        }
        let pos = indices
            .iter()
            .position(|idx| *idx == self.selected)
            .unwrap_or(0);
        self.selected = indices[(pos + 1).min(indices.len() - 1)];
    }

    fn select_previous(&mut self) {
        let indices = self.filtered_indices();
        if indices.is_empty() {
            return;
        }
        let pos = indices
            .iter()
            .position(|idx| *idx == self.selected)
            .unwrap_or(0);
        self.selected = indices[pos.saturating_sub(1)];
    }

    /// First filtered row shown in a list `visible` rows tall. Shared by the
    /// renderer and mouse hit-testing so a click lands on the row it looks like.
    pub(super) fn window_start(&self, visible: usize) -> usize {
        let indices = self.filtered_indices();
        if visible == 0 || indices.len() <= visible {
            return 0;
        }
        self.scroll.min(indices.len() - visible)
    }

    /// Scrolls the minimum needed to keep the selection on screen. Called after
    /// anything that moves the selection, so a click never shifts the list.
    fn ensure_selection_visible(&mut self) {
        let visible = self.visible_rows;
        if visible == 0 {
            return;
        }
        let indices = self.filtered_indices();
        if indices.len() <= visible {
            self.scroll = 0;
            return;
        }
        let Some(position) = indices.iter().position(|idx| *idx == self.selected) else {
            return;
        };
        if position < self.scroll {
            self.scroll = position;
        } else if position >= self.scroll + visible {
            self.scroll = position + 1 - visible;
        }
        self.scroll = self.scroll.min(indices.len() - visible);
    }

    /// Keeps the cursor on a row that survives the current filter.
    fn clamp_selection(&mut self) {
        let indices = self.filtered_indices();
        if !indices.contains(&self.selected) {
            self.selected = indices.first().copied().unwrap_or(0);
        }
    }
}

/// What the client is doing with the one in-flight transfer.
#[derive(Debug)]
enum RunStage {
    /// The download is requested; the server has not announced the file yet.
    AwaitingStart,
    /// Uploading: waiting for the ack that releases `pending_seq + 1`.
    Sending {
        sender: engine::Sender,
        pending_seq: Option<u32>,
    },
    /// Downloading into the configured directory.
    Receiving { receiver: engine::Receiver },
    /// Nothing left to move; the popup is only waiting to be dismissed.
    Done,
}

/// The one transfer this client will run at a time, and the popup that shows it.
#[derive(Debug)]
pub(super) struct ClientFileTransferRun {
    id: u64,
    pub(super) direction: ClientFileTransferDirection,
    /// File name, or the typed path until the peer announces the real name.
    pub(super) name: String,
    pub(super) size: u64,
    pub(super) done: u64,
    /// Set once the transfer has stopped; `None` while it is still running.
    pub(super) outcome: Option<Result<(), String>>,
    stage: RunStage,
}

impl ClientFileTransferRun {
    pub(super) fn finished(&self) -> bool {
        self.outcome.is_some()
    }

    /// Completion in 0..=1, which must not divide by zero. A size of zero is
    /// also a download the server has not announced yet, so it only reads as
    /// complete once the transfer has actually settled.
    pub(super) fn ratio(&self) -> f64 {
        if self.size == 0 {
            return if self.finished() { 1.0 } else { 0.0 };
        }
        (self.done as f64 / self.size as f64).clamp(0.0, 1.0)
    }

    /// Settles the popup and drops the engine. Dropping a `Receiver` unlinks
    /// the partial file it was writing.
    ///
    /// The first verdict wins. A download writes its own the moment the bytes
    /// land, and the server's trailing end message, or its stall abort if that
    /// message is lost, would otherwise turn a file that is on disk into a
    /// reported failure.
    fn settle(&mut self, outcome: Result<(), String>) {
        if self.finished() {
            return;
        }
        if outcome.is_ok() {
            self.done = self.size;
        }
        self.outcome = Some(outcome);
        self.stage = RunStage::Done;
    }

    /// Settles locally and tells the server, so it stops waiting on an ack that
    /// will never come.
    fn fail(&mut self, reason: String, outcome: &mut ClientShellInput) {
        let transfer_id = self.id;
        self.settle(Err(reason.clone()));
        push(
            outcome,
            ClientFileTransferControl::End {
                transfer_id,
                ok: false,
                error: Some(reason),
            },
        );
    }
}

#[derive(Debug)]
pub(super) enum ClientFileTransferOverlay {
    /// The path of a local file to send, as typed or dropped.
    SendPath(String),
    /// Browsing the server's filesystem to pick a file to receive.
    Browse(Box<ClientFileBrowser>),
    /// Watching a transfer run, or reading why it failed.
    Progress(Box<ClientFileTransferRun>),
}

fn push(outcome: &mut ClientShellInput, control: ClientFileTransferControl) {
    outcome.requests.push(control.message());
}

/// Where received files land. Client-local by definition: the server never sees
/// this path, and "wherever the client happened to be started" is not an answer.
fn download_dir(configured: &str) -> PathBuf {
    let configured = if configured.trim().is_empty() {
        "~/Downloads"
    } else {
        configured
    };
    crate::worktree::expand_tilde_path(configured)
}

/// `expand_tilde_path` returns the input verbatim when HOME/USERPROFILE is unset,
/// which would otherwise make `create_dir_all` build a directory literally named
/// `~` under whatever the client's cwd happens to be.
fn usable_download_dir(configured: &str) -> Result<PathBuf, engine::TransferError> {
    let dir = download_dir(configured);
    if dir.starts_with("~") {
        return Err(engine::TransferError::NoHome);
    }
    Ok(dir)
}

/// Sends the next upload chunk, or stops when the source is drained. The
/// server's own end message carries the verdict, since only it knows whether
/// the write landed.
fn pump_upload(run: &mut ClientFileTransferRun, outcome: &mut ClientShellInput) {
    let transfer_id = run.id;
    let RunStage::Sending {
        sender,
        pending_seq,
    } = &mut run.stage
    else {
        return;
    };
    if pending_seq.is_some() {
        return;
    }
    match sender.next_chunk() {
        Ok(Some((seq, data))) => {
            *pending_seq = Some(seq);
            // Progress is what the peer has acknowledged, not what was handed
            // to the socket: reporting `sent` shows 100% a round trip early.
            run.done = sender.sent().saturating_sub(data.len() as u64);
            push(
                outcome,
                ClientFileTransferControl::Chunk {
                    transfer_id,
                    seq,
                    data: encode_chunk(&data),
                },
            );
        }
        // Every byte is out; wait for the server to confirm the write.
        Ok(None) => {}
        Err(err) => run.fail(err.to_string(), outcome),
    }
}

/// Closes out a download once the announced size is on disk.
fn finish_download_if_complete(run: &mut ClientFileTransferRun, outcome: &mut ClientShellInput) {
    let transfer_id = run.id;
    if !matches!(&run.stage, RunStage::Receiving { receiver } if receiver.is_complete()) {
        return;
    }
    let RunStage::Receiving { receiver } = std::mem::replace(&mut run.stage, RunStage::Done) else {
        return;
    };
    match receiver.finish() {
        Ok(path) => {
            debug!(path = %path.display(), "file transfer received");
            run.settle(Ok(()));
            push(
                outcome,
                ClientFileTransferControl::End {
                    transfer_id,
                    ok: true,
                    error: None,
                },
            );
        }
        Err(err) => run.fail(err.to_string(), outcome),
    }
}

impl ClientShellState {
    fn next_file_transfer_id(&mut self) -> u64 {
        self.next_file_transfer_id = self.next_file_transfer_id.wrapping_add(1);
        self.next_file_transfer_id
    }

    fn file_transfer_overlay(&mut self) -> Option<&mut ClientFileTransferOverlay> {
        match self.overlay.as_mut() {
            Some(ClientShellOverlay::FileTransfer(overlay)) => Some(overlay),
            _ => None,
        }
    }

    fn browser(&mut self) -> Option<&mut ClientFileBrowser> {
        match self.file_transfer_overlay() {
            Some(ClientFileTransferOverlay::Browse(browser)) => Some(browser),
            _ => None,
        }
    }

    fn running_transfer(&mut self) -> Option<&mut ClientFileTransferRun> {
        match self.file_transfer_overlay() {
            Some(ClientFileTransferOverlay::Progress(run)) => Some(run),
            _ => None,
        }
    }

    /// A running transfer owns the popup: replacing it would strip the user of
    /// the only cancel affordance while the bytes keep moving.
    fn transfer_is_running(&self) -> bool {
        matches!(
            self.overlay.as_ref(),
            Some(ClientShellOverlay::FileTransfer(
                ClientFileTransferOverlay::Progress(run)
            )) if !run.finished()
        )
    }

    pub(super) fn open_file_transfer_send(&mut self) {
        if self.transfer_is_running() {
            return;
        }
        self.overlay = Some(ClientShellOverlay::FileTransfer(
            ClientFileTransferOverlay::SendPath(String::new()),
        ));
    }

    pub(super) fn open_file_transfer_receive(&mut self, outcome: &mut ClientShellInput) {
        if self.transfer_is_running() {
            return;
        }
        self.overlay = Some(ClientShellOverlay::FileTransfer(
            ClientFileTransferOverlay::Browse(Box::new(ClientFileBrowser {
                destination: download_dir(&self.config.file_transfer_dir)
                    .to_string_lossy()
                    .into_owned(),
                loading: true,
                ..ClientFileBrowser::default()
            })),
        ));
        // `None` starts the browser at the focused pane's working directory,
        // which only the server can resolve.
        push(
            outcome,
            ClientFileTransferControl::List {
                path: None,
                child: None,
                show_hidden: false,
            },
        );
    }

    fn request_listing(
        &mut self,
        path: Option<String>,
        child: Option<String>,
        outcome: &mut ClientShellInput,
    ) {
        let Some(browser) = self.browser() else {
            return;
        };
        browser.loading = true;
        browser.error = None;
        let show_hidden = browser.show_hidden;
        push(
            outcome,
            ClientFileTransferControl::List {
                path,
                child,
                show_hidden,
            },
        );
    }

    /// Enter on the send prompt: open the local file and start pushing it.
    fn start_send(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientFileTransferOverlay::SendPath(input)) = self.file_transfer_overlay() else {
            return;
        };
        let typed = input.trim().to_owned();
        if typed.is_empty() {
            return;
        }
        outcome.repaint = true;
        let path = crate::worktree::expand_tilde_path(&typed);
        let (file, name, size) = match engine::open_source(&path) {
            Ok(source) => source,
            Err(err) => {
                warn!(err = %err, "cannot open local file for upload");
                self.show_send_failure(typed, err.to_string());
                return;
            }
        };
        let transfer_id = self.next_file_transfer_id();
        push(
            outcome,
            ClientFileTransferControl::Start {
                transfer_id,
                name: name.clone(),
                size,
            },
        );
        let mut run = ClientFileTransferRun {
            id: transfer_id,
            direction: ClientFileTransferDirection::Send,
            name,
            size,
            done: 0,
            outcome: None,
            stage: RunStage::Sending {
                sender: engine::Sender::new(file, size),
                pending_seq: None,
            },
        };
        pump_upload(&mut run, outcome);
        self.overlay = Some(ClientShellOverlay::FileTransfer(
            ClientFileTransferOverlay::Progress(Box::new(run)),
        ));
    }

    /// A refusal that never reached the wire still needs the popup, or the
    /// failure is invisible.
    fn show_send_failure(&mut self, name: String, error: String) {
        self.overlay = Some(ClientShellOverlay::FileTransfer(
            ClientFileTransferOverlay::Progress(Box::new(ClientFileTransferRun {
                id: 0,
                direction: ClientFileTransferDirection::Send,
                name,
                size: 0,
                done: 0,
                outcome: Some(Err(error)),
                stage: RunStage::Done,
            })),
        ));
    }

    /// Enter on a browser row: descend into a directory, or pick a file and
    /// start the transfer.
    fn activate_browser_selection(&mut self, outcome: &mut ClientShellInput) {
        let Some(browser) = self.browser() else {
            return;
        };
        let Some(entry) = browser.selected_entry().cloned() else {
            return;
        };
        outcome.repaint = true;
        let dir = browser.dir.clone();
        if entry.is_dir {
            if entry.is_parent {
                let parent = browser.parent.clone();
                if parent.is_some() {
                    self.request_listing(parent, None, outcome);
                }
            } else {
                self.request_listing(Some(dir), Some(entry.name), outcome);
            }
            return;
        }

        let transfer_id = self.next_file_transfer_id();
        push(
            outcome,
            ClientFileTransferControl::Download {
                transfer_id,
                dir,
                name: entry.name.clone(),
            },
        );
        self.overlay = Some(ClientShellOverlay::FileTransfer(
            ClientFileTransferOverlay::Progress(Box::new(ClientFileTransferRun {
                id: transfer_id,
                direction: ClientFileTransferDirection::Receive,
                name: entry.name,
                size: 0,
                done: 0,
                outcome: None,
                stage: RunStage::AwaitingStart,
            })),
        ));
    }

    /// Esc on the progress popup: ask the server to abort while it runs,
    /// dismiss once it has stopped.
    fn stop_transfer(&mut self, outcome: &mut ClientShellInput) {
        outcome.repaint = true;
        match self.running_transfer() {
            Some(run) if !run.finished() => run.fail("cancelled".to_owned(), outcome),
            _ => self.overlay = None,
        }
    }

    pub(super) fn route_file_transfer_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        match self.file_transfer_overlay() {
            Some(ClientFileTransferOverlay::Progress(_)) => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Enter) {
                    self.stop_transfer(outcome);
                }
            }
            Some(ClientFileTransferOverlay::SendPath(input)) => match key.code {
                KeyCode::Enter => self.start_send(outcome),
                KeyCode::Esc => {
                    self.overlay = None;
                    outcome.repaint = true;
                }
                _ => {
                    outcome.repaint |= edit_path_field(input, key);
                }
            },
            Some(ClientFileTransferOverlay::Browse(_)) => self.route_browser_key(key, outcome),
            None => {}
        }
    }

    fn route_browser_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        use crossterm::event::KeyModifiers;

        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        // `.` is the file-manager convention and always reaches us. Ctrl+H is
        // accepted as an alias, but cannot be relied on: terminals send it as
        // 0x08, which is also Backspace, so depending on the emulator it arrives
        // as `Backspace` (with or without CONTROL) rather than `Ctrl+H`. All
        // three spellings toggle; bare Backspace still edits the filter.
        let toggles_hidden = matches!(key.code, KeyCode::Char('.')) && !control
            || matches!(key.code, KeyCode::Char('h') | KeyCode::Backspace) && control;
        if toggles_hidden {
            let dir = self.browser().and_then(|browser| {
                browser.show_hidden = !browser.show_hidden;
                // Before the first listing lands there is no directory to
                // re-list, so let the server resolve the pane cwd again.
                (!browser.dir.is_empty()).then(|| browser.dir.clone())
            });
            self.request_listing(dir, None, outcome);
            outcome.repaint = true;
            return;
        }
        match key.code {
            KeyCode::Esc => self.overlay = None,
            KeyCode::Enter | KeyCode::Right => {
                self.activate_browser_selection(outcome);
                return;
            }
            KeyCode::Left => {
                // Same as picking `..`; a browser should go up without hunting.
                let parent = self.browser().and_then(|browser| browser.parent.clone());
                if parent.is_some() {
                    self.request_listing(parent, None, outcome);
                }
            }
            KeyCode::Up | KeyCode::Down => {
                if let Some(browser) = self.browser() {
                    if key.code == KeyCode::Up {
                        browser.select_previous();
                    } else {
                        browser.select_next();
                    }
                    browser.ensure_selection_visible();
                }
            }
            KeyCode::Backspace => {
                if let Some(browser) = self.browser() {
                    browser.query.pop();
                    browser.clamp_selection();
                    browser.ensure_selection_visible();
                }
            }
            KeyCode::Char(character) if !control => {
                if let Some(browser) = self.browser() {
                    browser.query.push(character);
                    browser.clamp_selection();
                    browser.ensure_selection_visible();
                }
            }
            _ => return,
        }
        outcome.repaint = true;
    }

    /// Wheel over the browser: move the window, not the selection, so the row
    /// under the pointer stays under the pointer.
    pub(super) fn scroll_file_browser(&mut self, delta: isize, outcome: &mut ClientShellInput) {
        let Some(browser) = self.browser() else {
            return;
        };
        let visible = browser.visible_rows;
        let rows = browser.filtered_indices().len();
        let max_scroll = rows.saturating_sub(visible);
        browser.scroll = browser.scroll.saturating_add_signed(delta).min(max_scroll);
        outcome.repaint = true;
    }

    /// Mouse equivalent of Enter on a browser row.
    pub(super) fn activate_file_browser_row(
        &mut self,
        entry_index: usize,
        outcome: &mut ClientShellInput,
    ) {
        if let Some(browser) = self.browser() {
            browser.selected = entry_index;
        }
        self.activate_browser_selection(outcome);
    }

    /// Mouse equivalent of the modal's primary and cancel buttons.
    pub(super) fn accept_file_transfer_overlay(&mut self, outcome: &mut ClientShellInput) {
        if matches!(
            self.file_transfer_overlay(),
            Some(ClientFileTransferOverlay::SendPath(_))
        ) {
            self.start_send(outcome);
        }
    }

    pub(super) fn cancel_file_transfer_overlay(&mut self, outcome: &mut ClientShellInput) {
        match self.file_transfer_overlay() {
            Some(ClientFileTransferOverlay::Progress(_)) => self.stop_transfer(outcome),
            Some(_) => {
                self.overlay = None;
                outcome.repaint = true;
            }
            None => {}
        }
    }

    /// A drag-and-drop reaches the send prompt as a bracketed paste of a
    /// shell-shaped path: quoted, or with spaces backslash-escaped. Undo that
    /// so dropping a file whose name has a space in it works the same as typing
    /// the path by hand.
    pub(super) fn insert_file_transfer_text(&mut self, text: &str) -> bool {
        let Some(ClientFileTransferOverlay::SendPath(input)) = self.file_transfer_overlay() else {
            return false;
        };
        input.push_str(&engine::normalize_dropped_path(text));
        true
    }

    /// The renderer owns the window height; key paging and the wheel read it
    /// back so all three agree on what is on screen.
    pub(super) fn set_file_browser_visible_rows(&mut self, rows: usize) {
        if let Some(browser) = self.browser() {
            browser.visible_rows = rows;
        }
    }

    pub(crate) fn handle_file_transfer_control(
        &mut self,
        control: ServerFileTransferControl,
    ) -> ClientShellInput {
        let mut outcome = ClientShellInput::default();
        match control {
            ServerFileTransferControl::Listing {
                dir,
                parent,
                entries,
                truncated,
                error,
            } => {
                let Some(browser) = self.browser() else {
                    return outcome;
                };
                browser.loading = false;
                browser.dir = dir;
                browser.parent = parent;
                browser.truncated = truncated;
                browser.error = error;
                let up = browser.parent.is_some().then(|| ClientFileBrowserEntry {
                    name: "..".to_owned(),
                    is_dir: true,
                    size: None,
                    is_parent: true,
                });
                browser.entries = up
                    .into_iter()
                    .chain(entries.into_iter().map(|entry| ClientFileBrowserEntry {
                        name: entry.name,
                        is_dir: entry.is_dir,
                        size: entry.size,
                        is_parent: false,
                    }))
                    .collect();
                browser.query.clear();
                browser.selected = 0;
                browser.scroll = 0;
                browser.clamp_selection();
                outcome.repaint = true;
            }
            ServerFileTransferControl::Start {
                transfer_id,
                name,
                size,
            } => self.begin_download(transfer_id, &name, size, &mut outcome),
            ServerFileTransferControl::Chunk {
                transfer_id,
                seq,
                data,
            } => self.handle_download_chunk(transfer_id, seq, &data, &mut outcome),
            ServerFileTransferControl::Ack { transfer_id, seq } => {
                self.handle_upload_ack(transfer_id, seq, &mut outcome);
            }
            ServerFileTransferControl::End {
                transfer_id,
                ok,
                error,
                saved_name,
            } => self.handle_server_end(transfer_id, ok, error, saved_name, &mut outcome),
            ServerFileTransferControl::Unknown => {
                debug!("ignoring unknown file transfer control");
            }
        }
        outcome
    }

    /// True when this client is running `transfer_id`. Otherwise the message is
    /// answered so the server stops waiting on an ack that will never arrive.
    fn owns_transfer(&mut self, transfer_id: u64, outcome: &mut ClientShellInput) -> bool {
        if self
            .running_transfer()
            .is_some_and(|run| run.id == transfer_id)
        {
            return true;
        }
        debug!(transfer_id, "answering a message for an unknown transfer");
        push(
            outcome,
            ClientFileTransferControl::End {
                transfer_id,
                ok: false,
                error: Some("no such transfer".to_owned()),
            },
        );
        false
    }

    fn begin_download(
        &mut self,
        transfer_id: u64,
        name: &str,
        size: u64,
        outcome: &mut ClientShellInput,
    ) {
        if !self.owns_transfer(transfer_id, outcome) {
            return;
        }
        // A second announcement for the same id would open a second
        // destination and restart the sequence, so only a transfer still
        // waiting for its file accepts one.
        if !matches!(
            self.running_transfer().map(|run| &run.stage),
            Some(RunStage::AwaitingStart)
        ) {
            if let Some(run) = self.running_transfer() {
                run.fail("transfer desynchronized".to_owned(), outcome);
            }
            outcome.repaint = true;
            return;
        }
        // `Receiver::create` runs `checked_name`, so a hostile or buggy server
        // cannot steer this outside the download directory.
        let receiver = usable_download_dir(&self.config.file_transfer_dir)
            .and_then(|dir| engine::Receiver::create(&dir, name, size));
        let Some(run) = self.running_transfer() else {
            return;
        };
        outcome.repaint = true;
        match receiver {
            Ok(receiver) => {
                run.name = receiver.name().to_owned();
                run.size = size;
                run.done = 0;
                run.stage = RunStage::Receiving { receiver };
                // A zero-byte file has no chunks and is complete on
                // announcement.
                finish_download_if_complete(run, outcome);
            }
            Err(err) => {
                warn!(err = %err, "cannot create download destination");
                run.fail(err.to_string(), outcome);
            }
        }
    }

    fn handle_download_chunk(
        &mut self,
        transfer_id: u64,
        seq: u32,
        data: &str,
        outcome: &mut ClientShellInput,
    ) {
        if !self.owns_transfer(transfer_id, outcome) {
            return;
        }
        let decoded = decode_chunk(data);
        let Some(run) = self.running_transfer() else {
            return;
        };
        outcome.repaint = true;
        let RunStage::Receiving { receiver } = &mut run.stage else {
            run.fail("transfer desynchronized".to_owned(), outcome);
            return;
        };
        let written = match decoded {
            Some(data) => receiver
                .write_chunk(seq, &data)
                .map(|()| receiver.written()),
            None => Err(engine::TransferError::Desync),
        };
        match written {
            Ok(written) => {
                run.done = written;
                push(outcome, ClientFileTransferControl::Ack { transfer_id, seq });
                finish_download_if_complete(run, outcome);
            }
            Err(err) => run.fail(err.to_string(), outcome),
        }
    }

    fn handle_upload_ack(&mut self, transfer_id: u64, seq: u32, outcome: &mut ClientShellInput) {
        let Some(run) = self.running_transfer() else {
            return;
        };
        if run.id != transfer_id {
            return;
        }
        let RunStage::Sending { pending_seq, .. } = &mut run.stage else {
            return;
        };
        if *pending_seq != Some(seq) {
            // Releasing on an ack the peer never sent would put more than one
            // chunk in flight, which is the whole thing stop-and-wait prevents.
            debug!(transfer_id, seq, "ignoring unexpected transfer ack");
            return;
        }
        *pending_seq = None;
        pump_upload(run, outcome);
        outcome.repaint = true;
    }

    fn handle_server_end(
        &mut self,
        transfer_id: u64,
        ok: bool,
        error: Option<String>,
        saved_name: Option<String>,
        outcome: &mut ClientShellInput,
    ) {
        let Some(run) = self.running_transfer() else {
            return;
        };
        if run.id != transfer_id {
            return;
        }
        outcome.repaint = true;
        if !ok {
            run.settle(Err(
                error.unwrap_or_else(|| "the transfer stopped".to_owned())
            ));
            return;
        }
        if let Some(saved_name) = saved_name {
            // Peer-supplied; bound it before it reaches the popup.
            run.name = engine::display_name(&saved_name);
        }
        if matches!(run.stage, RunStage::Receiving { .. }) {
            // The server says every byte is out. Either the last chunk
            // completes the file here, or it announced more than it sent and
            // both sides would otherwise wait for the stall deadline.
            finish_download_if_complete(run, outcome);
            if matches!(run.stage, RunStage::Receiving { .. }) {
                run.fail("transfer desynchronized".to_owned(), outcome);
            }
            return;
        }
        run.settle(Ok(()));
    }
}

/// Path-field editing for the send prompt: the same chords as the rename
/// overlay, because a path is what people paste and word-delete most.
fn edit_path_field(input: &mut String, key: &crate::input::TerminalKey) -> bool {
    use crossterm::event::KeyModifiers;

    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    if (matches!(key.code, KeyCode::Char('c') | KeyCode::Char('u')) && control)
        || (key.code == KeyCode::Backspace && key.modifiers.contains(KeyModifiers::SUPER))
    {
        input.clear();
        return true;
    }
    if (key.code == KeyCode::Backspace && (control || key.modifiers.contains(KeyModifiers::ALT)))
        || (matches!(key.code, KeyCode::Char('h' | 'w')) && control)
    {
        super::delete_text_field_word(input, &mut false);
        return true;
    }
    if key.code == KeyCode::Backspace {
        input.pop();
        return true;
    }
    if let KeyCode::Char(character) = key.code {
        if key.modifiers.difference(KeyModifiers::SHIFT).is_empty() {
            match key.generated_text.as_deref() {
                Some(text) => input.push_str(text),
                None => input.push(character),
            }
            return true;
        }
    }
    false
}
