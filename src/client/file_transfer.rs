//! Client side of native file transfer.
//!
//! The client holds no UI state — the prompt and the progress popup live in the
//! server's `AppState` and reach this process as ordinary frames. All this
//! module does is move bytes for the one transfer the server asked for, under
//! the same trust boundary the server uses (`crate::file_transfer`).

use std::path::PathBuf;

use tracing::{debug, warn};

use crate::file_transfer::{self as engine, TransferError};
use crate::protocol::ClientMessage;

/// The one transfer this client will run at a time.
#[derive(Debug)]
pub(super) enum ClientTransfer {
    /// Uploading: waiting for the ack that releases `pending_seq + 1`.
    Sending {
        id: u64,
        sender: engine::Sender,
        pending_seq: Option<u32>,
    },
    /// Downloading into the configured directory.
    Receiving { id: u64, receiver: engine::Receiver },
}

impl ClientTransfer {
    fn id(&self) -> u64 {
        match self {
            Self::Sending { id, .. } | Self::Receiving { id, .. } => *id,
        }
    }
}

/// What to write back to the server after handling one message.
pub(super) type Replies = Vec<ClientMessage>;

fn failure(transfer_id: u64, err: &TransferError) -> ClientMessage {
    ClientMessage::FileTransferEnd {
        transfer_id,
        ok: false,
        error: Some(err.to_string()),
        saved_name: None,
    }
}

fn busy(transfer_id: u64) -> ClientMessage {
    ClientMessage::FileTransferEnd {
        transfer_id,
        ok: false,
        error: Some("a transfer is already in progress".to_owned()),
        saved_name: None,
    }
}

/// Where received files land. Client-local by definition: the server never sees
/// this path, and "wherever the client happened to be started" is not an answer.
fn download_dir() -> PathBuf {
    let configured = crate::config::load_live_config()
        .map(|loaded| loaded.config.remote.file_transfer_dir)
        .unwrap_or_default();
    let configured = if configured.trim().is_empty() {
        "~/Downloads".to_owned()
    } else {
        configured
    };
    crate::worktree::expand_tilde_path(&configured)
}

/// Server asked for a client-local file (upload). Relative paths resolve against
/// this process's working directory, the way a shell would read what the user
/// typed on their own machine.
pub(super) fn begin_upload(
    slot: &mut Option<ClientTransfer>,
    transfer_id: u64,
    path: &str,
) -> Replies {
    if slot.is_some() {
        return vec![busy(transfer_id)];
    }
    let path = crate::worktree::expand_tilde_path(path);
    let (file, name, size) = match engine::open_source(&path) {
        Ok(source) => source,
        Err(err) => {
            warn!(err = %err, "cannot open local file for upload");
            return vec![failure(transfer_id, &err)];
        }
    };

    let mut sender = engine::Sender::new(file, size);
    let mut replies = vec![ClientMessage::FileTransferStart {
        transfer_id,
        name,
        size,
    }];
    match sender.next_chunk() {
        // A zero-byte file has no chunks and is complete on announcement.
        Ok(None) => {
            replies.push(ClientMessage::FileTransferEnd {
                transfer_id,
                ok: true,
                error: None,
                saved_name: None,
            });
            return replies;
        }
        Ok(Some((seq, data))) => {
            replies.push(ClientMessage::FileTransferChunk {
                transfer_id,
                seq,
                data,
            });
            *slot = Some(ClientTransfer::Sending {
                id: transfer_id,
                sender,
                pending_seq: Some(seq),
            });
        }
        Err(err) => return vec![failure(transfer_id, &err)],
    }
    replies
}

/// Server announced a file it is about to send (download).
pub(super) fn begin_download(
    slot: &mut Option<ClientTransfer>,
    transfer_id: u64,
    name: &str,
    size: u64,
) -> Replies {
    if slot.is_some() {
        return vec![busy(transfer_id)];
    }
    // `Receiver::create` runs `checked_name`, so a hostile or buggy server
    // cannot steer this outside `download_dir`.
    match engine::Receiver::create(&download_dir(), name, size) {
        Ok(receiver) => {
            if receiver.is_complete() {
                return finish_download(receiver, transfer_id);
            }
            *slot = Some(ClientTransfer::Receiving {
                id: transfer_id,
                receiver,
            });
            Vec::new()
        }
        Err(err) => {
            warn!(err = %err, "cannot create download destination");
            vec![failure(transfer_id, &err)]
        }
    }
}

pub(super) fn handle_chunk(
    slot: &mut Option<ClientTransfer>,
    transfer_id: u64,
    seq: u32,
    data: &[u8],
) -> Replies {
    let Some(ClientTransfer::Receiving { id, receiver }) = slot.as_mut() else {
        return stray(slot, transfer_id);
    };
    if *id != transfer_id {
        return stray(slot, transfer_id);
    }
    if let Err(err) = receiver.write_chunk(seq, data) {
        let reply = failure(transfer_id, &err);
        *slot = None;
        return vec![reply];
    }
    let complete = receiver.is_complete();
    let mut replies = vec![ClientMessage::FileTransferAck { transfer_id, seq }];
    if complete {
        let Some(ClientTransfer::Receiving { receiver, .. }) = slot.take() else {
            return replies;
        };
        replies.extend(finish_download(receiver, transfer_id));
    }
    replies
}

fn finish_download(receiver: engine::Receiver, transfer_id: u64) -> Replies {
    let saved_name = receiver.name().to_owned();
    match receiver.finish() {
        Ok(path) => {
            debug!(path = %path.display(), "file transfer received");
            vec![ClientMessage::FileTransferEnd {
                transfer_id,
                ok: true,
                error: None,
                // A collision was suffixed here, so the server's popup would
                // otherwise name a file that is not on this disk.
                saved_name: Some(saved_name),
            }]
        }
        Err(err) => vec![failure(transfer_id, &err)],
    }
}

pub(super) fn handle_ack(slot: &mut Option<ClientTransfer>, transfer_id: u64, seq: u32) -> Replies {
    let Some(ClientTransfer::Sending {
        id,
        sender,
        pending_seq,
    }) = slot.as_mut()
    else {
        return Vec::new();
    };
    if *id != transfer_id || *pending_seq != Some(seq) {
        // Releasing on an ack the peer never sent would put more than one chunk
        // in flight, which is the whole thing stop-and-wait is preventing.
        debug!(transfer_id, seq, "ignoring unexpected transfer ack");
        return Vec::new();
    }
    *pending_seq = None;

    match sender.next_chunk() {
        Ok(Some((seq, data))) => {
            *pending_seq = Some(seq);
            vec![ClientMessage::FileTransferChunk {
                transfer_id,
                seq,
                data,
            }]
        }
        Ok(None) => {
            *slot = None;
            vec![ClientMessage::FileTransferEnd {
                transfer_id,
                ok: true,
                error: None,
                saved_name: None,
            }]
        }
        Err(err) => {
            let reply = failure(transfer_id, &err);
            *slot = None;
            vec![reply]
        }
    }
}

/// The server ended the transfer: success, failure, or the user's cancel.
/// Dropping a `Receiver` unlinks the partial file it was writing.
pub(super) fn handle_end(slot: &mut Option<ClientTransfer>, transfer_id: u64) {
    if slot
        .as_ref()
        .is_some_and(|transfer| transfer.id() == transfer_id)
    {
        *slot = None;
    }
}

/// A message for a transfer this client is not running. Answer it so the server
/// stops waiting on an ack that will never arrive.
fn stray(slot: &Option<ClientTransfer>, transfer_id: u64) -> Replies {
    debug!(
        transfer_id,
        active = slot.as_ref().map(ClientTransfer::id),
        "ignoring file transfer message for an unknown transfer"
    );
    vec![ClientMessage::FileTransferEnd {
        transfer_id,
        ok: false,
        error: Some("no such transfer".to_owned()),
        saved_name: None,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FILE_TRANSFER_CHUNK_SIZE;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-client-transfer-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn upload_announces_then_streams_one_chunk_at_a_time() {
        let dir = tempdir("upload");
        let src = dir.join("payload.bin");
        let payload = vec![3u8; FILE_TRANSFER_CHUNK_SIZE + 10];
        std::fs::write(&src, &payload).expect("write source");

        let mut slot = None;
        let replies = begin_upload(&mut slot, 1, &src.to_string_lossy());
        assert!(matches!(
            replies.as_slice(),
            [
                ClientMessage::FileTransferStart { size, .. },
                ClientMessage::FileTransferChunk { seq: 0, .. }
            ] if *size == payload.len() as u64
        ));
        assert!(
            slot.is_some(),
            "an upload should stay resident for its acks"
        );

        // Only the awaited ack releases the next chunk.
        assert!(handle_ack(&mut slot, 1, 7).is_empty());
        assert!(matches!(
            handle_ack(&mut slot, 1, 0).as_slice(),
            [ClientMessage::FileTransferChunk { seq: 1, .. }]
        ));
        assert!(matches!(
            handle_ack(&mut slot, 1, 1).as_slice(),
            [ClientMessage::FileTransferEnd { ok: true, .. }]
        ));
        assert!(slot.is_none());
    }

    #[test]
    fn a_second_transfer_is_refused_rather_than_interleaved() {
        let dir = tempdir("busy");
        let src = dir.join("a.bin");
        std::fs::write(&src, vec![0u8; FILE_TRANSFER_CHUNK_SIZE + 1]).expect("write");
        let mut slot = None;
        begin_upload(&mut slot, 1, &src.to_string_lossy());

        assert!(matches!(
            begin_upload(&mut slot, 2, &src.to_string_lossy()).as_slice(),
            [ClientMessage::FileTransferEnd { ok: false, .. }]
        ));
        assert!(matches!(
            begin_download(&mut slot, 3, "b.bin", 4).as_slice(),
            [ClientMessage::FileTransferEnd { ok: false, .. }]
        ));
    }

    #[test]
    fn upload_of_a_missing_file_reports_instead_of_hanging() {
        let mut slot = None;
        let replies = begin_upload(&mut slot, 1, "/definitely/not/here.bin");
        assert!(matches!(
            replies.as_slice(),
            [ClientMessage::FileTransferEnd {
                ok: false,
                error: Some(_),
                ..
            }]
        ));
        assert!(slot.is_none());
    }

    #[test]
    fn a_chunk_for_an_unknown_transfer_is_answered_not_dropped() {
        let mut slot = None;
        assert!(matches!(
            handle_chunk(&mut slot, 99, 0, b"x").as_slice(),
            [ClientMessage::FileTransferEnd { ok: false, .. }]
        ));
    }
}
