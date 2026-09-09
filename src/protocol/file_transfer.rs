//! Named control payloads for native file transfer.
//!
//! These ride on `EndpointControl` rather than on new `ClientMessage` /
//! `ServerMessage` variants: the variant order of both enums is frozen for
//! endpoint generation 1, and an unrecognized control kind is ignored by a peer
//! that does not know it instead of failing to decode the frame.
//!
//! Chunk payloads are base64 because the envelope carries a `String`. That
//! costs a third more bytes per chunk, which is affordable at one chunk per
//! round trip and is the price of not touching the frozen enums.

use serde::{Deserialize, Serialize};

use super::{ClientMessage, ServerMessage};

/// Every client-to-server transfer control, in one kind.
pub const CLIENT_FILE_TRANSFER_KIND: &str = "herdr.filetransfer.client.v1";
/// Every server-to-client transfer control, in one kind.
pub const SERVER_FILE_TRANSFER_KIND: &str = "herdr.filetransfer.server.v1";

/// Maximum total size of one native file transfer, in either direction.
///
/// An honest peer costs `FILE_TRANSFER_CHUNK_SIZE` of resident memory
/// regardless of file size, because one chunk is in flight at a time.
///
/// A hostile peer can do worse: chunk sequence numbers are predictable, so a
/// client that acknowledges without reading its socket releases the next chunk
/// into the writer queue, which is unbounded. Worst case is this constant's
/// worth of server memory. That is a same-user self-DoS over a mode-restricted
/// local socket, not a privilege boundary, so it is accepted rather than fixed,
/// but it is the reason this number cannot grow freely.
// ponytail: 256 KiB per round trip is also the throughput ceiling, so a full
// 256 MiB transfer costs roughly 21s at 20ms RTT and 105s at 100ms. There is no
// resume, so a drop restarts it. Raise the window before raising this again.
pub const MAX_FILE_TRANSFER_SIZE: u64 = 256 * 1024 * 1024;

/// Payload bytes carried by one chunk control.
// ponytail: strict stop-and-wait, one chunk in flight, because the client
// writer queue is an unbounded VecDeque and an unacked sender would buffer the
// whole file in RAM. On a 20 ms RTT link this ceilings throughput near 12 MB/s;
// upgrade to a sliding window of N acks only if that is measured to matter.
pub const FILE_TRANSFER_CHUNK_SIZE: usize = 256 * 1024;

/// Directories with more entries than this are listed partially, so an enormous
/// one cannot stall the server loop.
pub const FILE_BROWSER_MAX_ENTRIES: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientFileTransferControl {
    /// List a directory on the server for the receive browser. `path` is
    /// `None` for the focused pane's working directory.
    ///
    /// `child` descends into an entry of `path`. The server joins it, because
    /// the client may not share the server's path syntax.
    List {
        path: Option<String>,
        child: Option<String>,
        show_hidden: bool,
    },
    /// Ask the server to send one listed file down. `dir` and `name` are echoed
    /// back from the listing rather than joined on the client, for the same
    /// reason.
    Download {
        transfer_id: u64,
        dir: String,
        name: String,
    },
    /// Announce a client-side file being sent up into the focused pane's
    /// working directory.
    Start {
        transfer_id: u64,
        name: String,
        size: u64,
    },
    /// One upload payload chunk, base64. The sender waits for the matching
    /// server ack before sending `seq + 1`.
    Chunk {
        transfer_id: u64,
        seq: u32,
        data: String,
    },
    /// Acknowledge one received download chunk, releasing the next one.
    Ack { transfer_id: u64, seq: u32 },
    /// Terminal status for a transfer in either direction, and the cancel path.
    End {
        transfer_id: u64,
        ok: bool,
        error: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTransferEntry {
    pub name: String,
    pub is_dir: bool,
    /// `None` for directories and for anything whose metadata could not be read.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ServerFileTransferControl {
    /// Answer to `List`. `error` is set instead of `entries` when the
    /// directory could not be read.
    Listing {
        dir: String,
        /// Parent of `dir`, or `None` at a filesystem root. Server-computed:
        /// a Unix client cannot take the parent of `C:\Users\me`, and a
        /// Windows one would rebuild `/home/me` with backslashes.
        parent: Option<String>,
        entries: Vec<FileTransferEntry>,
        truncated: bool,
        error: Option<String>,
    },
    /// Announce a server-side file being sent down.
    Start {
        transfer_id: u64,
        name: String,
        size: u64,
    },
    /// One download payload chunk, base64.
    Chunk {
        transfer_id: u64,
        seq: u32,
        data: String,
    },
    /// Acknowledge one received upload chunk, releasing the next one.
    Ack { transfer_id: u64, seq: u32 },
    /// Terminal status for a transfer in either direction.
    End {
        transfer_id: u64,
        ok: bool,
        error: Option<String>,
        /// Set only when the server was the receiver: the file name actually
        /// written, which differs from the announced one when a collision was
        /// suffixed. The sender's popup would otherwise name a file that is not
        /// on disk.
        saved_name: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

impl ClientFileTransferControl {
    pub fn message(&self) -> ClientMessage {
        ClientMessage::EndpointControl {
            kind: CLIENT_FILE_TRANSFER_KIND.into(),
            data: serde_json::to_string(self).unwrap_or_default(),
        }
    }
}

impl ServerFileTransferControl {
    pub fn message(&self) -> ServerMessage {
        ServerMessage::EndpointControl {
            kind: SERVER_FILE_TRANSFER_KIND.into(),
            data: serde_json::to_string(self).unwrap_or_default(),
        }
    }
}

pub fn encode_chunk(data: &[u8]) -> String {
    use base64::Engine as _;

    base64::engine::general_purpose::STANDARD.encode(data)
}

pub fn decode_chunk(data: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;

    base64::engine::general_purpose::STANDARD.decode(data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_round_trip_through_the_envelope() {
        let control = ClientFileTransferControl::Chunk {
            transfer_id: 7,
            seq: 3,
            data: encode_chunk(&[1, 2, 3, 250]),
        };
        let ClientMessage::EndpointControl { kind, data } = control.message() else {
            panic!("controls travel as endpoint controls");
        };
        assert_eq!(kind, CLIENT_FILE_TRANSFER_KIND);
        assert_eq!(
            serde_json::from_str::<ClientFileTransferControl>(&data).unwrap(),
            control
        );

        let listing = ServerFileTransferControl::Listing {
            dir: "/tmp".into(),
            parent: Some("/".into()),
            entries: vec![FileTransferEntry {
                name: "a.txt".into(),
                is_dir: false,
                size: Some(4),
            }],
            truncated: false,
            error: None,
        };
        let ServerMessage::EndpointControl { data, .. } = listing.message() else {
            panic!("controls travel as endpoint controls");
        };
        assert_eq!(
            serde_json::from_str::<ServerFileTransferControl>(&data).unwrap(),
            listing
        );
    }

    #[test]
    fn an_unknown_op_decodes_instead_of_failing() {
        // A newer peer may add ops. Decoding must not error, or one unknown
        // control would tear down a session that is otherwise compatible.
        assert_eq!(
            serde_json::from_str::<ClientFileTransferControl>(r#"{"op":"resume","at":5}"#).unwrap(),
            ClientFileTransferControl::Unknown
        );
        assert_eq!(
            serde_json::from_str::<ServerFileTransferControl>(r#"{"op":"resume"}"#).unwrap(),
            ServerFileTransferControl::Unknown
        );
    }

    #[test]
    fn a_full_chunk_still_fits_one_frame() {
        let control = ClientFileTransferControl::Chunk {
            transfer_id: u64::MAX,
            seq: u32::MAX,
            data: encode_chunk(&vec![0xAB; FILE_TRANSFER_CHUNK_SIZE]),
        };
        let mut buf = Vec::new();
        super::super::write_message(&mut buf, &control.message()).unwrap();
        assert!(
            buf.len() - 4 <= super::super::MAX_FRAME_SIZE,
            "chunk frame {} exceeds MAX_FRAME_SIZE",
            buf.len() - 4
        );
    }
}
