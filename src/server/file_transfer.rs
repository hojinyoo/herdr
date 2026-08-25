//! Server side of native file transfer: one transfer at a time, strict
//! stop-and-wait, bound to the client that started it.
//!
//! The engine (`crate::file_transfer`) does the bytes and the trust boundary.
//! This module is only the state machine and its wire plumbing.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::app::state::FileTransferDirection;
use crate::file_transfer::{self as engine, TransferError};
use crate::protocol::ServerMessage;

use super::headless::HeadlessServer;

/// What the server is doing with the one in-flight transfer.
#[derive(Debug)]
enum Stage {
    /// `FileTransferRequest` is out; the client has not announced its file yet.
    /// The destination is captured here, not when the client answers: focus can
    /// move during the round trip and the file must land in the pane the user
    /// picked.
    AwaitingUpload { dir: PathBuf },
    /// Writing bytes the client is sending up.
    Receiving(engine::Receiver),
    /// Reading bytes down to the client, waiting on the ack for `pending_seq`.
    Sending {
        sender: engine::Sender,
        pending_seq: Option<u32>,
    },
}

/// How long a transfer may make no progress before the server reclaims the slot.
///
/// Without this a peer that stops answering without closing its socket holds the
/// single transfer slot for the life of the session, and every later transfer is
/// refused. Stop-and-wait means an honest peer refreshes this every round trip.
const FILE_TRANSFER_STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The one transfer this server will run at a time.
#[derive(Debug)]
pub(super) struct ServerTransfer {
    id: u64,
    /// Captured at start, never re-read. `foreground_client_id` moves whenever
    /// another app client becomes active, and a transfer must not follow it
    /// onto a different process mid-stream.
    client_id: u64,
    direction: FileTransferDirection,
    stage: Stage,
    /// Bumped on every message from the peer; drives the stall timeout.
    last_progress: Instant,
}

impl HeadlessServer {
    /// Drains the app's transfer requests. Called from the same place as the
    /// other `request_*` drains.
    pub(super) fn drain_file_transfer_requests(&mut self) -> bool {
        let mut needs_render = false;

        if self
            .file_transfer
            .as_ref()
            .is_some_and(|transfer| transfer.last_progress.elapsed() >= FILE_TRANSFER_STALL_TIMEOUT)
        {
            self.fail_file_transfer("the transfer stalled and was abandoned".to_owned());
            needs_render = true;
        }

        if std::mem::take(&mut self.app.state.request_file_transfer_cancel) {
            // Input arrives in batches, so Enter and Esc can both land before
            // this runs. Drop the start the cancel raced instead of beginning a
            // transfer the user already abandoned.
            self.app.state.request_file_transfer = None;
            self.abort_file_transfer(Some("cancelled".to_owned()));
            needs_render = true;
        }

        if let Some((direction, path)) = self.app.state.request_file_transfer.take() {
            self.begin_file_transfer(direction, path);
            needs_render = true;
        }

        needs_render
    }

    fn begin_file_transfer(&mut self, direction: FileTransferDirection, path: String) {
        if self.file_transfer.is_some() {
            self.finish_file_transfer_ui(Err("a transfer is already in progress".to_owned()));
            return;
        }
        let Some(client_id) = self.foreground_client_id else {
            self.finish_file_transfer_ui(Err("no client is attached".to_owned()));
            return;
        };
        let id = self.next_file_transfer_id();

        match direction {
            FileTransferDirection::Upload => {
                // The file is on the client, so ask for it and wait. The client
                // resolves and validates the path on its own side.
                let Some(dir) = self.focused_pane_cwd() else {
                    self.finish_file_transfer_ui(Err(
                        "the focused pane has no working directory".to_owned()
                    ));
                    return;
                };
                self.file_transfer = Some(ServerTransfer {
                    id,
                    client_id,
                    direction,
                    stage: Stage::AwaitingUpload { dir },
                    last_progress: Instant::now(),
                });
                self.send_to_client(
                    client_id,
                    ServerMessage::FileTransferRequest {
                        transfer_id: id,
                        path,
                    },
                );
            }
            FileTransferDirection::Download => match self.open_download_source(&path) {
                Ok((sender, name)) => {
                    let size = sender.size();
                    self.file_transfer = Some(ServerTransfer {
                        id,
                        client_id,
                        direction,
                        stage: Stage::Sending {
                            sender,
                            pending_seq: None,
                        },
                        last_progress: Instant::now(),
                    });
                    if let Some(transfer) = self.app.state.file_transfer.as_mut() {
                        transfer.name = name.clone();
                        transfer.size = size;
                        transfer.done = 0;
                    }
                    self.send_to_client(
                        client_id,
                        ServerMessage::FileTransferStart {
                            transfer_id: id,
                            name,
                            size,
                        },
                    );
                    self.pump_download();
                }
                Err(err) => self.finish_file_transfer_ui(Err(err.to_string())),
            },
        }
    }

    /// Resolves a typed download path against the focused pane's working
    /// directory and opens it.
    ///
    /// `foreground_cwd` walks a process tree, so this runs exactly once per
    /// transfer and never from a render or per-pane loop.
    fn open_download_source(&self, typed: &str) -> Result<(engine::Sender, String), TransferError> {
        let base = self
            .focused_pane_cwd()
            .unwrap_or_else(|| PathBuf::from("."));
        let path = engine::resolve_source(&base, typed);
        let (file, name, size) = engine::open_source(&path)?;
        Ok((engine::Sender::new(file, size), name))
    }

    pub(super) fn focused_pane_cwd(&self) -> Option<PathBuf> {
        let ws_idx = self.app.state.active?;
        let ws = self.app.state.workspaces.get(ws_idx)?;
        let pane_id = ws.focused_pane_id()?;
        ws.tabs
            .get(ws.active_tab)?
            .foreground_cwd_for_pane(pane_id, &self.app.terminal_runtimes)
    }

    /// Sends the next download chunk, or finishes when the source is drained.
    fn pump_download(&mut self) {
        let Some(transfer) = self.file_transfer.as_mut() else {
            return;
        };
        let (id, client_id) = (transfer.id, transfer.client_id);
        let Stage::Sending {
            sender,
            pending_seq,
        } = &mut transfer.stage
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
                let acked = sender.sent().saturating_sub(data.len() as u64);
                self.send_to_client(
                    client_id,
                    ServerMessage::FileTransferChunk {
                        transfer_id: id,
                        seq,
                        data,
                    },
                );
                if let Some(state) = self.app.state.file_transfer.as_mut() {
                    state.done = acked;
                }
            }
            Ok(None) => {
                // All bytes are out. The client's own `FileTransferEnd` carries
                // the verdict, since only it knows whether the write landed.
                self.send_to_client(
                    client_id,
                    ServerMessage::FileTransferEnd {
                        transfer_id: id,
                        ok: true,
                        error: None,
                    },
                );
            }
            Err(err) => self.fail_file_transfer(err.to_string()),
        }
    }

    pub(super) fn handle_client_file_transfer_start(
        &mut self,
        client_id: u64,
        transfer_id: u64,
        name: String,
        size: u64,
    ) -> bool {
        // Only a transfer this server asked for is accepted. Without this a
        // client could push an unprompted file into the focused pane's cwd.
        let dir = match self.file_transfer.as_ref() {
            Some(transfer) if transfer.client_id == client_id && transfer.id == transfer_id => {
                match &transfer.stage {
                    Stage::AwaitingUpload { dir } => dir.clone(),
                    _ => {
                        self.fail_file_transfer("transfer desynchronized".to_owned());
                        return true;
                    }
                }
            }
            _ => {
                self.reject_stray_transfer(client_id, transfer_id, "no such transfer");
                return false;
            }
        };

        match engine::Receiver::create(&dir, &name, size) {
            Ok(receiver) => {
                if let Some(state) = self.app.state.file_transfer.as_mut() {
                    state.name = receiver.name().to_owned();
                    state.size = size;
                    state.done = 0;
                }
                if let Some(transfer) = self.file_transfer.as_mut() {
                    transfer.stage = Stage::Receiving(receiver);
                }
                // A zero-byte file has no chunks; it is already complete.
                self.complete_upload_if_done();
            }
            Err(err) => self.fail_file_transfer(err.to_string()),
        }
        true
    }

    /// Records peer liveness for the stall timeout.
    fn note_transfer_progress(&mut self, client_id: u64, transfer_id: u64) {
        if let Some(transfer) = self.file_transfer.as_mut() {
            if transfer.client_id == client_id && transfer.id == transfer_id {
                transfer.last_progress = Instant::now();
            }
        }
    }

    pub(super) fn handle_client_file_transfer_chunk(
        &mut self,
        client_id: u64,
        transfer_id: u64,
        seq: u32,
        data: Vec<u8>,
    ) -> bool {
        self.note_transfer_progress(client_id, transfer_id);
        match self.file_transfer.as_ref() {
            Some(transfer) if transfer.client_id == client_id && transfer.id == transfer_id => {
                if !matches!(transfer.stage, Stage::Receiving(_)) {
                    // Right id, wrong stage. Answering the peer without clearing
                    // our own slot would leave it free and us wedged.
                    self.fail_file_transfer("transfer desynchronized".to_owned());
                    return true;
                }
            }
            _ => {
                self.reject_stray_transfer(client_id, transfer_id, "no such transfer");
                return false;
            }
        }

        let written = {
            let Some(transfer) = self.file_transfer.as_mut() else {
                return false;
            };
            let Stage::Receiving(receiver) = &mut transfer.stage else {
                return false;
            };
            match receiver.write_chunk(seq, &data) {
                Ok(()) => receiver.written(),
                Err(err) => {
                    let message = err.to_string();
                    self.fail_file_transfer(message);
                    return true;
                }
            }
        };

        if let Some(state) = self.app.state.file_transfer.as_mut() {
            state.done = written;
        }
        self.send_to_client(
            client_id,
            ServerMessage::FileTransferAck { transfer_id, seq },
        );
        self.complete_upload_if_done();
        true
    }

    pub(super) fn handle_client_file_transfer_ack(
        &mut self,
        client_id: u64,
        transfer_id: u64,
        seq: u32,
    ) -> bool {
        {
            let Some(transfer) = self.file_transfer.as_mut() else {
                return false;
            };
            if transfer.client_id != client_id || transfer.id != transfer_id {
                return false;
            }
            let Stage::Sending { pending_seq, .. } = &mut transfer.stage else {
                return false;
            };
            if *pending_seq != Some(seq) {
                // A stale or invented ack would release a chunk the peer never
                // acknowledged, which is exactly the unbounded-queue case
                // stop-and-wait exists to prevent.
                debug!(
                    client_id,
                    transfer_id, seq, "ignoring unexpected transfer ack"
                );
                return false;
            }
            *pending_seq = None;
        }
        self.pump_download();
        true
    }

    pub(super) fn handle_client_file_transfer_end(
        &mut self,
        client_id: u64,
        transfer_id: u64,
        ok: bool,
        error: Option<String>,
        saved_name: Option<String>,
    ) -> bool {
        let Some(transfer) = self.file_transfer.as_ref() else {
            return false;
        };
        if transfer.client_id != client_id || transfer.id != transfer_id {
            return false;
        }

        if !ok {
            let reason = error.unwrap_or_else(|| "the transfer was cancelled".to_owned());
            self.fail_file_transfer(reason);
            return true;
        }

        match transfer.direction {
            // The client wrote the file; its `ok` is the verdict, and its
            // `saved_name` is the only place the suffixed name exists. It is
            // still only allowed to claim success for bytes this side actually
            // sent — otherwise a peer answering the announcement makes the
            // popup report "done" for a file that got nothing.
            FileTransferDirection::Download => {
                let drained = matches!(
                    &transfer.stage,
                    Stage::Sending { sender, .. } if sender.sent() == sender.size()
                );
                if !drained {
                    self.fail_file_transfer("transfer desynchronized".to_owned());
                    return true;
                }
                self.file_transfer = None;
                if let (Some(state), Some(saved_name)) =
                    (self.app.state.file_transfer.as_mut(), saved_name)
                {
                    // Peer-supplied; bound it before it reaches the popup.
                    state.name = crate::file_transfer::display_name(&saved_name);
                }
                self.finish_file_transfer_ui(Ok(()));
            }
            // An upload is only complete when the bytes are on disk here. A
            // peer claiming success early would otherwise leave the slot held
            // and the popup spinning with no terminal state, and there is no
            // timeout to rescue it.
            FileTransferDirection::Upload => {
                let complete = matches!(
                    &transfer.stage,
                    Stage::Receiving(receiver) if receiver.is_complete()
                );
                if complete {
                    self.complete_upload_if_done();
                } else {
                    self.fail_file_transfer("transfer desynchronized".to_owned());
                }
            }
        }
        true
    }

    /// Drops a transfer owned by a client that just went away.
    pub(super) fn abort_file_transfer_for_client(&mut self, client_id: u64) {
        if self
            .file_transfer
            .as_ref()
            .is_some_and(|transfer| transfer.client_id == client_id)
        {
            self.abort_file_transfer(Some("the client disconnected".to_owned()));
        }
    }

    /// Tears the transfer down locally and tells the peer, if it is still there.
    pub(super) fn abort_file_transfer(&mut self, reason: Option<String>) {
        let Some(transfer) = self.file_transfer.take() else {
            // Nothing in flight, but a popup may still be waiting on a request
            // that was cancelled before it ever started. Leaving it without an
            // outcome would spin forever.
            if self
                .app
                .state
                .file_transfer
                .as_ref()
                .is_some_and(|state| state.outcome.is_none())
            {
                self.finish_file_transfer_ui(Err(
                    reason.unwrap_or_else(|| "the transfer stopped".to_owned())
                ));
            }
            return;
        };
        // Dropping a `Receiver` unlinks its partial file.
        let (id, client_id) = (transfer.id, transfer.client_id);
        drop(transfer);
        self.send_to_client(
            client_id,
            ServerMessage::FileTransferEnd {
                transfer_id: id,
                ok: false,
                error: reason.clone(),
            },
        );
        self.finish_file_transfer_ui(Err(
            reason.unwrap_or_else(|| "the transfer stopped".to_owned())
        ));
    }

    fn fail_file_transfer(&mut self, reason: String) {
        warn!(reason = %reason, "file transfer failed");
        self.abort_file_transfer(Some(reason));
    }

    fn complete_upload_if_done(&mut self) {
        let done = matches!(
            self.file_transfer.as_ref().map(|transfer| &transfer.stage),
            Some(Stage::Receiving(receiver)) if receiver.is_complete()
        );
        if !done {
            return;
        }
        let Some(transfer) = self.file_transfer.take() else {
            return;
        };
        let (id, client_id) = (transfer.id, transfer.client_id);
        let Stage::Receiving(receiver) = transfer.stage else {
            return;
        };
        let outcome = receiver.finish().map(|_| ()).map_err(|err| err.to_string());
        self.send_to_client(
            client_id,
            ServerMessage::FileTransferEnd {
                transfer_id: id,
                ok: outcome.is_ok(),
                error: outcome.as_ref().err().cloned(),
            },
        );
        self.finish_file_transfer_ui(outcome);
    }

    /// Writes the terminal state into the popup mirror. `error` is always
    /// populated on failure so the popup is never blank.
    fn finish_file_transfer_ui(&mut self, outcome: Result<(), String>) {
        if let Some(state) = self.app.state.file_transfer.as_mut() {
            if outcome.is_ok() {
                state.done = state.size;
            }
            state.outcome = Some(outcome);
        }
    }

    /// Answers a message for a transfer this server does not know about, so the
    /// peer stops waiting instead of hanging on an ack that will never come.
    fn reject_stray_transfer(&mut self, client_id: u64, transfer_id: u64, reason: &str) {
        debug!(
            client_id,
            transfer_id, reason, "rejecting stray file transfer message"
        );
        self.send_to_client(
            client_id,
            ServerMessage::FileTransferEnd {
                transfer_id,
                ok: false,
                error: Some(reason.to_owned()),
            },
        );
    }

    /// Seeds an upload already past its request round trip, so tests can drive
    /// the terminal paths without a live client to answer `FileTransferRequest`.
    #[cfg(test)]
    pub(super) fn begin_upload_for_test(&mut self, client_id: u64, dir: PathBuf) -> u64 {
        let id = self.next_file_transfer_id();
        self.app.state.file_transfer = Some(crate::app::state::FileTransferState {
            direction: FileTransferDirection::Upload,
            name: String::new(),
            size: 0,
            done: 0,
            outcome: None,
        });
        self.file_transfer = Some(ServerTransfer {
            id,
            client_id,
            direction: FileTransferDirection::Upload,
            stage: Stage::AwaitingUpload { dir },
            last_progress: Instant::now(),
        });
        id
    }

    /// Seeds a download already announced to the client, so tests can drive the
    /// terminal paths without a live client to ack chunks.
    #[cfg(test)]
    pub(super) fn begin_download_for_test(
        &mut self,
        client_id: u64,
        source: &std::path::Path,
    ) -> u64 {
        let id = self.next_file_transfer_id();
        let (file, name, size) = engine::open_source(source).expect("test download source");
        self.app.state.file_transfer = Some(crate::app::state::FileTransferState {
            direction: FileTransferDirection::Download,
            name,
            size,
            done: 0,
            outcome: None,
        });
        self.file_transfer = Some(ServerTransfer {
            id,
            client_id,
            direction: FileTransferDirection::Download,
            last_progress: Instant::now(),
            stage: Stage::Sending {
                sender: engine::Sender::new(file, size),
                pending_seq: None,
            },
        });
        id
    }

    /// Ages the in-flight transfer past the stall deadline so a test can assert
    /// reclamation without sleeping.
    #[cfg(test)]
    pub(super) fn expire_file_transfer_for_test(&mut self) {
        if let Some(transfer) = self.file_transfer.as_mut() {
            transfer.last_progress = Instant::now() - FILE_TRANSFER_STALL_TIMEOUT;
        }
    }

    fn next_file_transfer_id(&mut self) -> u64 {
        self.next_file_transfer_id = self.next_file_transfer_id.wrapping_add(1);
        self.next_file_transfer_id
    }
}
