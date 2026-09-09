//! Server side of native file transfer: one transfer at a time, strict
//! stop-and-wait, bound to the client that started it.
//!
//! The engine (`crate::file_transfer`) does the bytes and the trust boundary.
//! This module is only the state machine and its wire plumbing. The UI is
//! entirely client-side, so nothing here touches `AppState`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::file_transfer as engine;
use crate::protocol::file_transfer::{
    decode_chunk, encode_chunk, ClientFileTransferControl, FileTransferEntry,
    ServerFileTransferControl, FILE_BROWSER_MAX_ENTRIES,
};

use super::headless::HeadlessServer;

/// What the server is doing with the one in-flight transfer.
#[derive(Debug)]
enum Stage {
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
    /// Captured at start, never re-read. A transfer must not follow the
    /// foreground shell onto a different process mid-stream.
    client_id: u64,
    stage: Stage,
    /// Bumped only when the peer moves the transfer forward, never on a
    /// message this side ignored; drives the stall timeout.
    last_progress: Instant,
}

impl HeadlessServer {
    pub(super) fn handle_client_file_transfer_control(
        &mut self,
        client_id: u64,
        control: ClientFileTransferControl,
    ) {
        match control {
            ClientFileTransferControl::List {
                path,
                child,
                show_hidden,
            } => self.send_file_listing(client_id, path, child.as_deref(), show_hidden),
            ClientFileTransferControl::Download {
                transfer_id,
                dir,
                name,
            } => self.begin_download(client_id, transfer_id, &dir, &name),
            ClientFileTransferControl::Start {
                transfer_id,
                name,
                size,
            } => self.begin_upload(client_id, transfer_id, &name, size),
            ClientFileTransferControl::Chunk {
                transfer_id,
                seq,
                data,
            } => self.handle_upload_chunk(client_id, transfer_id, seq, &data),
            ClientFileTransferControl::Ack { transfer_id, seq } => {
                self.handle_download_ack(client_id, transfer_id, seq);
            }
            ClientFileTransferControl::End {
                transfer_id,
                ok,
                error,
            } => self.handle_peer_end(client_id, transfer_id, ok, error),
            ClientFileTransferControl::Unknown => {
                debug!(client_id, "ignoring unknown file transfer control");
            }
        }
    }

    fn send_control(&mut self, client_id: u64, control: ServerFileTransferControl) {
        self.send_to_client(client_id, control.message());
    }

    /// Lists one server-side directory for the receive browser. `path` is
    /// `None` for the focused pane's working directory, and `child` descends
    /// into an entry of it.
    fn send_file_listing(
        &mut self,
        client_id: u64,
        path: Option<String>,
        child: Option<&str>,
        show_hidden: bool,
    ) {
        let mut dir = match path {
            Some(path) => PathBuf::from(path),
            None => self
                .focused_pane_cwd()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from(std::path::MAIN_SEPARATOR_STR)),
        };
        if let Some(child) = child {
            // The same gate the receiver uses: an entry name is one plain
            // component, never a way to walk out of the directory it came from.
            match engine::checked_name(child) {
                Ok(child) => dir.push(child),
                Err(err) => {
                    self.send_listing_error(client_id, &dir, err.to_string());
                    return;
                }
            }
        }
        let (listed, truncated) =
            match engine::list_directory(&dir, show_hidden, FILE_BROWSER_MAX_ENTRIES) {
                Ok(listed) => listed,
                Err(err) => {
                    self.send_listing_error(client_id, &dir, err.to_string());
                    return;
                }
            };
        let control = ServerFileTransferControl::Listing {
            dir: dir.to_string_lossy().into_owned(),
            parent: dir
                .parent()
                .map(|parent| parent.to_string_lossy().into_owned()),
            entries: listed
                .into_iter()
                .map(|entry| FileTransferEntry {
                    name: entry.name,
                    is_dir: entry.is_dir,
                    size: entry.size,
                })
                .collect(),
            truncated,
            error: None,
        };
        self.send_control(client_id, control);
    }

    fn send_listing_error(&mut self, client_id: u64, dir: &std::path::Path, error: String) {
        self.send_control(
            client_id,
            ServerFileTransferControl::Listing {
                dir: dir.to_string_lossy().into_owned(),
                parent: dir
                    .parent()
                    .map(|parent| parent.to_string_lossy().into_owned()),
                entries: Vec::new(),
                truncated: false,
                error: Some(error),
            },
        );
    }

    /// `foreground_cwd_for_pane` walks a process tree, so this runs once per
    /// transfer or navigation and never from a render or per-pane loop.
    pub(super) fn focused_pane_cwd(&self) -> Option<PathBuf> {
        let ws_idx = self.app.state.active?;
        let ws = self.app.state.workspaces.get(ws_idx)?;
        let pane_id = ws.focused_pane_id()?;
        ws.tabs
            .get(ws.active_tab)?
            .foreground_cwd_for_pane(pane_id, &self.app.terminal_runtimes)
    }

    /// True when the one slot is free. Otherwise the peer is told, so it does
    /// not wait on a transfer that will never start.
    fn slot_is_free(&mut self, client_id: u64, transfer_id: u64) -> bool {
        if self.file_transfer.is_some() {
            self.send_control(
                client_id,
                ServerFileTransferControl::End {
                    transfer_id,
                    ok: false,
                    error: Some("a transfer is already in progress".to_owned()),
                    saved_name: None,
                },
            );
            return false;
        }
        true
    }

    fn begin_download(&mut self, client_id: u64, transfer_id: u64, dir: &str, name: &str) {
        if !self.slot_is_free(client_id, transfer_id) {
            return;
        }
        let source = match engine::checked_name(name) {
            Ok(name) => std::path::Path::new(dir).join(name),
            Err(err) => {
                self.send_control(
                    client_id,
                    ServerFileTransferControl::End {
                        transfer_id,
                        ok: false,
                        error: Some(err.to_string()),
                        saved_name: None,
                    },
                );
                return;
            }
        };
        let (file, name, size) = match engine::open_source(&source) {
            Ok(source) => source,
            Err(err) => {
                self.send_control(
                    client_id,
                    ServerFileTransferControl::End {
                        transfer_id,
                        ok: false,
                        error: Some(err.to_string()),
                        saved_name: None,
                    },
                );
                return;
            }
        };
        self.file_transfer = Some(ServerTransfer {
            id: transfer_id,
            client_id,
            stage: Stage::Sending {
                sender: engine::Sender::new(file, size),
                pending_seq: None,
            },
            last_progress: Instant::now(),
        });
        self.send_control(
            client_id,
            ServerFileTransferControl::Start {
                transfer_id,
                name,
                size,
            },
        );
        self.pump_download();
    }

    fn begin_upload(&mut self, client_id: u64, transfer_id: u64, name: &str, size: u64) {
        if !self.slot_is_free(client_id, transfer_id) {
            return;
        }
        // Destination is resolved now, not when the first chunk lands: focus can
        // move mid-transfer and the file must land in the pane the user picked.
        let Some(dir) = self.focused_pane_cwd() else {
            self.send_control(
                client_id,
                ServerFileTransferControl::End {
                    transfer_id,
                    ok: false,
                    error: Some("the focused pane has no working directory".to_owned()),
                    saved_name: None,
                },
            );
            return;
        };
        match engine::Receiver::create(&dir, name, size) {
            Ok(receiver) => {
                self.file_transfer = Some(ServerTransfer {
                    id: transfer_id,
                    client_id,
                    stage: Stage::Receiving(receiver),
                    last_progress: Instant::now(),
                });
                // A zero-byte file has no chunks; it is already complete.
                self.complete_upload_if_done();
            }
            Err(err) => self.send_control(
                client_id,
                ServerFileTransferControl::End {
                    transfer_id,
                    ok: false,
                    error: Some(err.to_string()),
                    saved_name: None,
                },
            ),
        }
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
                self.send_control(
                    client_id,
                    ServerFileTransferControl::Chunk {
                        transfer_id: id,
                        seq,
                        data: encode_chunk(&data),
                    },
                );
            }
            Ok(None) => {
                // All bytes are out. The client's own end carries the verdict,
                // since only it knows whether the write landed.
                self.send_control(
                    client_id,
                    ServerFileTransferControl::End {
                        transfer_id: id,
                        ok: true,
                        error: None,
                        saved_name: None,
                    },
                );
            }
            Err(err) => self.fail_file_transfer(err.to_string()),
        }
    }

    /// Records peer liveness for the stall timeout.
    fn note_transfer_progress(&mut self, client_id: u64, transfer_id: u64) {
        if let Some(transfer) = self.file_transfer.as_mut() {
            if transfer.client_id == client_id && transfer.id == transfer_id {
                transfer.last_progress = Instant::now();
            }
        }
    }

    fn owns_transfer(&self, client_id: u64, transfer_id: u64) -> bool {
        self.file_transfer
            .as_ref()
            .is_some_and(|transfer| transfer.client_id == client_id && transfer.id == transfer_id)
    }

    fn handle_upload_chunk(&mut self, client_id: u64, transfer_id: u64, seq: u32, data: &str) {
        if !self.owns_transfer(client_id, transfer_id) {
            self.reject_stray_transfer(client_id, transfer_id, "no such transfer");
            return;
        }
        let Some(data) = decode_chunk(data) else {
            self.fail_file_transfer("transfer desynchronized".to_owned());
            return;
        };

        let outcome = match self
            .file_transfer
            .as_mut()
            .map(|transfer| &mut transfer.stage)
        {
            Some(Stage::Receiving(receiver)) => Some(receiver.write_chunk(seq, &data)),
            // Right id, wrong stage. Only answering the peer, without also
            // clearing this slot, would free the peer and wedge the server.
            _ => None,
        };
        match outcome {
            None => self.fail_file_transfer("transfer desynchronized".to_owned()),
            Some(Err(err)) => self.fail_file_transfer(err.to_string()),
            Some(Ok(())) => {
                self.note_transfer_progress(client_id, transfer_id);
                self.send_control(
                    client_id,
                    ServerFileTransferControl::Ack { transfer_id, seq },
                );
                self.complete_upload_if_done();
            }
        }
    }

    fn handle_download_ack(&mut self, client_id: u64, transfer_id: u64, seq: u32) {
        {
            let Some(transfer) = self.file_transfer.as_mut() else {
                return;
            };
            if transfer.client_id != client_id || transfer.id != transfer_id {
                return;
            }
            let Stage::Sending { pending_seq, .. } = &mut transfer.stage else {
                return;
            };
            if *pending_seq != Some(seq) {
                // A stale or invented ack would release a chunk the peer never
                // acknowledged, which is exactly the unbounded-queue case
                // stop-and-wait exists to prevent.
                debug!(
                    client_id,
                    transfer_id, seq, "ignoring unexpected transfer ack"
                );
                return;
            }
            *pending_seq = None;
        }
        // Only an ack that actually released a chunk counts as progress. A
        // download's only inbound traffic is acks, so refreshing on every one
        // would let a peer hold the slot forever by repeating a stale seq;
        // refreshing on none would abandon a healthy transfer after
        // `FILE_TRANSFER_STALL_TIMEOUT`, and 256 MiB takes ~105s at 100ms RTT.
        self.note_transfer_progress(client_id, transfer_id);
        self.pump_download();
    }

    fn handle_peer_end(
        &mut self,
        client_id: u64,
        transfer_id: u64,
        ok: bool,
        error: Option<String>,
    ) {
        if !self.owns_transfer(client_id, transfer_id) {
            return;
        }
        if !ok {
            let reason = error.unwrap_or_else(|| "the transfer was cancelled".to_owned());
            self.fail_file_transfer(reason);
            return;
        }

        let Some(transfer) = self.file_transfer.as_ref() else {
            return;
        };
        match &transfer.stage {
            // The client wrote the file, so its `ok` is the verdict. It is
            // still only allowed to claim success for bytes this side actually
            // sent, or a peer answering the announcement reports success for a
            // file that got nothing.
            Stage::Sending { sender, .. } => {
                if sender.sent() == sender.size() {
                    self.file_transfer = None;
                } else {
                    self.fail_file_transfer("transfer desynchronized".to_owned());
                }
            }
            // An upload is only complete when the bytes are on disk here. A
            // peer claiming success early would otherwise leave the slot held
            // with nothing left to release it but the stall deadline.
            Stage::Receiving(receiver) => {
                if receiver.is_complete() {
                    self.complete_upload_if_done();
                } else {
                    self.fail_file_transfer("transfer desynchronized".to_owned());
                }
            }
        }
    }

    /// Reclaims the slot from a peer that stopped answering.
    pub(super) fn expire_stalled_file_transfer(&mut self, now: Instant) {
        if self.file_transfer.as_ref().is_some_and(|transfer| {
            now.saturating_duration_since(transfer.last_progress) >= FILE_TRANSFER_STALL_TIMEOUT
        }) {
            self.fail_file_transfer("the transfer stalled and was abandoned".to_owned());
        }
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
    fn abort_file_transfer(&mut self, reason: Option<String>) {
        let Some(transfer) = self.file_transfer.take() else {
            return;
        };
        // Dropping a `Receiver` unlinks its partial file.
        let (id, client_id) = (transfer.id, transfer.client_id);
        drop(transfer);
        self.send_control(
            client_id,
            ServerFileTransferControl::End {
                transfer_id: id,
                ok: false,
                error: Some(reason.unwrap_or_else(|| "the transfer stopped".to_owned())),
                saved_name: None,
            },
        );
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
        let saved_name = receiver.name().to_owned();
        let outcome = receiver.finish().map(|_| ()).map_err(|err| err.to_string());
        self.send_control(
            client_id,
            ServerFileTransferControl::End {
                transfer_id: id,
                ok: outcome.is_ok(),
                error: outcome.as_ref().err().cloned(),
                // A collision was suffixed here, so the sender would
                // otherwise name a file that is not on this disk.
                saved_name: outcome.is_ok().then_some(saved_name),
            },
        );
    }

    /// Answers a message for a transfer this server does not know about, so the
    /// peer stops waiting instead of hanging on an ack that will never come.
    fn reject_stray_transfer(&mut self, client_id: u64, transfer_id: u64, reason: &str) {
        debug!(
            client_id,
            transfer_id, reason, "rejecting stray file transfer message"
        );
        self.send_control(
            client_id,
            ServerFileTransferControl::End {
                transfer_id,
                ok: false,
                error: Some(reason.to_owned()),
                saved_name: None,
            },
        );
    }

    /// Seeds an upload already past its announcement, so tests can drive the
    /// terminal paths without a live client.
    #[cfg(test)]
    pub(super) fn begin_upload_for_test(
        &mut self,
        client_id: u64,
        transfer_id: u64,
        dir: &std::path::Path,
        name: &str,
        size: u64,
    ) {
        let receiver = engine::Receiver::create(dir, name, size).expect("test destination");
        self.file_transfer = Some(ServerTransfer {
            id: transfer_id,
            client_id,
            stage: Stage::Receiving(receiver),
            last_progress: Instant::now(),
        });
    }

    /// Seeds a download already announced to the client, so tests can drive the
    /// terminal paths without a live client to ack chunks.
    #[cfg(test)]
    pub(super) fn begin_download_for_test(
        &mut self,
        client_id: u64,
        transfer_id: u64,
        source: &std::path::Path,
    ) {
        let (file, _, size) = engine::open_source(source).expect("test download source");
        self.file_transfer = Some(ServerTransfer {
            id: transfer_id,
            client_id,
            stage: Stage::Sending {
                sender: engine::Sender::new(file, size),
                pending_seq: None,
            },
            last_progress: Instant::now(),
        });
        self.pump_download();
    }

    #[cfg(test)]
    pub(super) fn file_transfer_slot_is_free(&self) -> bool {
        self.file_transfer.is_none()
    }

    /// Ages the in-flight transfer past the stall deadline so a test can assert
    /// reclamation without sleeping.
    #[cfg(test)]
    pub(super) fn expire_file_transfer_for_test(&mut self) {
        if let Some(transfer) = self.file_transfer.as_mut() {
            transfer.last_progress = Instant::now() - FILE_TRANSFER_STALL_TIMEOUT;
        }
    }
}
