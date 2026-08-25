//! Native file transfer engine shared by the server and the remote client.
//!
//! Both peers play both roles, so the trust boundary lives here once: a
//! receiver never joins an attacker-controlled path onto its destination
//! directory. `checked_name` is that boundary and every destination goes
//! through it.
//!
//! No sockets, no async, no PTYs — the wire framing is `crate::protocol` and
//! the transport loops live in `crate::server::headless` and `crate::client`.

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};

use crate::protocol::{FILE_TRANSFER_CHUNK_SIZE, MAX_FILE_TRANSFER_SIZE};

/// Why a transfer was refused. `Display` output travels on the wire in
/// `FileTransferEnd::error` and is shown verbatim in the progress popup, so it
/// has to read as a user-facing sentence.
#[derive(Debug)]
pub(crate) enum TransferError {
    /// The sender's `name` is not a plain filename.
    UnsafeName,
    /// The source path is not a regular file.
    NotAFile,
    /// The source, or the announced size, exceeds `MAX_FILE_TRANSFER_SIZE`.
    TooLarge {
        size: u64,
    },
    /// Every suffixed candidate for that name was taken.
    NoFreeName {
        name: String,
    },
    /// The peer sent a chunk out of order, or more bytes than it announced.
    Desync,
    Io(io::Error),
}

impl std::fmt::Display for TransferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsafeName => write!(f, "rejected: the file name is not a plain file name"),
            Self::NotAFile => write!(f, "not a regular file"),
            Self::TooLarge { size } => write!(
                f,
                "file is {size} bytes; Herdr's transfer limit is {MAX_FILE_TRANSFER_SIZE} bytes"
            ),
            Self::NoFreeName { name } => {
                write!(
                    f,
                    "could not find a free name for {name} at the destination"
                )
            }
            Self::Desync => write!(f, "transfer desynchronized"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl From<io::Error> for TransferError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// The trust boundary. Returns `name` only when it is a single, plain path
/// component that a receiver can safely join onto its own destination
/// directory.
///
/// The real work is `is_unsafe_name`, which is deliberately **free of any
/// `cfg`**: `std::path` is compiled for the host, so on a Unix build there is
/// no `Component::Prefix` and `\` is an ordinary character. A Unix server that
/// leaned on `Path::components()` would accept `C:\Users\me\.ssh\authorized_keys`
/// as one `Normal` component and hand it to a Windows client, which resolves it
/// as drive-relative and escapes the download directory. Topology A is exactly
/// that mixed pair, so every rule is enforced on every target and the component
/// walk below is only a second gate.
pub(crate) fn checked_name(name: &str) -> Result<&str, TransferError> {
    if is_unsafe_name(name) {
        return Err(TransferError::UnsafeName);
    }

    let path = Path::new(name);
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(component)), None) if component == name => Ok(name),
        _ => Err(TransferError::UnsafeName),
    }
}

/// Longest destination file name, in bytes. `NAME_MAX` is 255 on the platforms
/// Herdr ships for.
const MAX_NAME_BYTES: usize = 255;

/// Platform-independent rejection table. No `cfg` here, ever — see
/// `checked_name`.
fn is_unsafe_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return true;
    }
    // `\0` truncates C-string path APIs; `/` and `\` are separators somewhere;
    // `:` is both a Windows drive separator and an NTFS alternate data stream,
    // and `notes.txt:payload` writes into an existing `notes.txt`.
    if name.contains(['\0', '/', '\\', ':']) {
        return true;
    }
    // Win32 strips trailing dots and spaces and leading spaces when resolving a
    // path, so `secrets.txt.` and `secrets.txt` name the same file — which would
    // defeat the atomic no-overwrite guarantee `create_new` is there to give.
    if name.ends_with('.') || name.ends_with(' ') || name.starts_with(' ') {
        return true;
    }
    // `.` and `..` never reach the component walk below on their own merits.
    if name == "." || name == ".." {
        return true;
    }
    is_windows_reserved_name(name)
}

/// `CON`, `NUL`, `COM1`… resolve to devices on Windows regardless of extension:
/// `create_new("NUL")` succeeds, every byte is discarded, and the popup would
/// report a success that produced no file.
fn is_windows_reserved_name(name: &str) -> bool {
    const RESERVED: [&str; 26] = [
        "CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM0", "COM1", "COM2", "COM3", "COM4",
        "COM5", "COM6", "COM7", "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5",
        "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = name.split('.').next().unwrap_or(name);
    if RESERVED
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        return true;
    }
    // Win32 also resolves the superscript digits as COM/LPT device numbers, so
    // `COM¹` is a device even though it is not ASCII.
    let Some(rest) = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("com"))
        .or_else(|| stem.strip_prefix("LPT"))
        .or_else(|| stem.strip_prefix("lpt"))
    else {
        return false;
    };
    matches!(rest, "¹" | "²" | "³")
}

/// Opens a file to send, returning its handle, its bare name for the wire, and
/// its length.
pub(crate) fn open_source(path: &Path) -> Result<(fs::File, String, u64), TransferError> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(TransferError::NotAFile);
    }
    let size = metadata.len();
    if size > MAX_FILE_TRANSFER_SIZE {
        return Err(TransferError::TooLarge { size });
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(TransferError::UnsafeName)?;
    let name = checked_name(name)?.to_owned();
    Ok((fs::File::open(path)?, name, size))
}

/// Number of `name-N` candidates tried before giving up, matching the retry
/// bound `server::clipboard_image::stage` uses.
const MAX_NAME_ATTEMPTS: u32 = 100;

/// Creates the destination file, suffixing the name rather than overwriting.
///
/// Every attempt uses `create_new`, so an existing file is never clobbered and
/// there is no check-then-create race; the loop only picks the next candidate.
/// `create_new` also refuses to follow a pre-existing symlink at the
/// destination.
///
/// Returns the path actually written, whose file name may differ from `name`.
fn create_destination(dir: &Path, name: &str) -> Result<(PathBuf, fs::File), TransferError> {
    let name = checked_name(name)?;
    fs::create_dir_all(dir)?;

    for attempt in 0..MAX_NAME_ATTEMPTS {
        let candidate = if attempt == 0 {
            name.to_owned()
        } else {
            suffixed_name(name, attempt)
        };
        // `name` was validated, but suffixing rebuilds it: the NAME_MAX clamp can
        // cut inside an extension and leave a trailing space, which Win32 strips.
        // Re-run the gate on what will actually be joined onto `dir`.
        if is_unsafe_name(&candidate) {
            continue;
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        restrict_file_options(&mut options);
        let path = dir.join(&candidate);
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(TransferError::Io(err)),
        }
    }

    Err(TransferError::NoFreeName {
        name: name.to_owned(),
    })
}

/// `notes.txt` + 1 -> `notes-1.txt`. A dotfile with no extension keeps its
/// leading dot (`.gitignore` -> `.gitignore-1`) because `Path::file_stem`
/// treats the whole name as the stem.
///
/// The result is clamped to `MAX_NAME_BYTES`; `checked_name` already bounds the
/// input there, so appending a suffix is exactly what can push it over and make
/// the filesystem reject the write with an opaque `ENAMETOOLONG`.
fn suffixed_name(name: &str, attempt: u32) -> String {
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(name);
    let extension = path.extension().and_then(|ext| ext.to_str());

    let tail = match extension {
        Some(extension) => format!("-{attempt}.{extension}"),
        None => format!("-{attempt}"),
    };
    let budget = MAX_NAME_BYTES.saturating_sub(tail.len());
    let candidate = format!("{}{tail}", truncate_on_char_boundary(stem, budget));
    // A long enough extension blows the budget on its own (`a.` + 253 chars is a
    // legal 255-byte name), so clamping only the stem is not sufficient.
    truncate_on_char_boundary(&candidate, MAX_NAME_BYTES).to_owned()
}

fn truncate_on_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(unix)]
fn restrict_file_options(options: &mut fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;

    options.mode(0o600);
}

#[cfg(windows)]
fn restrict_file_options(_options: &mut fs::OpenOptions) {}

/// Sending half. One chunk is read per ack, so at most `FILE_TRANSFER_CHUNK_SIZE`
/// bytes of this transfer are ever resident.
#[derive(Debug)]
pub(crate) struct Sender {
    file: fs::File,
    size: u64,
    sent: u64,
    next_seq: u32,
}

impl Sender {
    pub(crate) fn new(file: fs::File, size: u64) -> Self {
        Self {
            file,
            size,
            sent: 0,
            next_seq: 0,
        }
    }

    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn sent(&self) -> u64 {
        self.sent
    }

    /// Reads the next chunk, or `None` once `size` bytes have been produced.
    ///
    /// The announced `size` bounds the read even if the file grew after
    /// `open_source` measured it: the receiver allocated its accounting from
    /// that number and would treat extra bytes as a desync.
    pub(crate) fn next_chunk(&mut self) -> Result<Option<(u32, Vec<u8>)>, TransferError> {
        let remaining = self.size - self.sent;
        if remaining == 0 {
            return Ok(None);
        }
        let want = remaining.min(FILE_TRANSFER_CHUNK_SIZE as u64) as usize;
        let mut buf = vec![0u8; want];
        // A file truncated mid-transfer short-reads here; report it rather than
        // shipping a zero-padded tail the receiver would accept as complete.
        self.file.read_exact(&mut buf).map_err(|err| {
            if err.kind() == io::ErrorKind::UnexpectedEof {
                TransferError::Desync
            } else {
                TransferError::Io(err)
            }
        })?;
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.sent += want as u64;
        Ok(Some((seq, buf)))
    }
}

/// Receiving half. Owns the partially written destination file and unlinks it
/// unless the transfer completes.
#[derive(Debug)]
pub(crate) struct Receiver {
    path: PathBuf,
    file: Option<fs::File>,
    size: u64,
    written: u64,
    next_seq: u32,
    name: String,
}

impl Receiver {
    /// Validates `name` against the trust boundary and creates the destination
    /// inside `dir`.
    pub(crate) fn create(dir: &Path, name: &str, size: u64) -> Result<Self, TransferError> {
        if size > MAX_FILE_TRANSFER_SIZE {
            return Err(TransferError::TooLarge { size });
        }
        let (path, file) = create_destination(dir, name)?;
        // The written name, not the announced one: a collision was suffixed and
        // the sender's popup has to name the file that actually exists.
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(name)
            .to_owned();
        Ok(Self {
            path,
            file: Some(file),
            size,
            written: 0,
            next_seq: 0,
            name,
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.written == self.size
    }

    /// Writes one chunk. Out-of-order sequences and overruns past the announced
    /// size are refused rather than tolerated: under strict stop-and-wait
    /// either one means the peer is broken or hostile.
    pub(crate) fn write_chunk(&mut self, seq: u32, data: &[u8]) -> Result<(), TransferError> {
        if seq != self.next_seq {
            return Err(TransferError::Desync);
        }
        let Some(file) = self.file.as_mut() else {
            return Err(TransferError::Desync);
        };
        // Exactly the size stop-and-wait implies, never merely "not too big".
        // An empty chunk would advance `next_seq` without advancing `written`,
        // so a peer could emit unlimited chunks and harvest unlimited acks from
        // a transfer that can never complete.
        let remaining = self.size - self.written;
        let expected = remaining.min(FILE_TRANSFER_CHUNK_SIZE as u64);
        if data.len() as u64 != expected {
            return Err(TransferError::Desync);
        }
        file.write_all(data)?;
        self.written += data.len() as u64;
        self.next_seq = self.next_seq.wrapping_add(1);
        Ok(())
    }

    /// Flushes and keeps the destination file. Refuses a short transfer.
    pub(crate) fn finish(mut self) -> Result<PathBuf, TransferError> {
        if !self.is_complete() {
            return Err(TransferError::Desync);
        }
        // Flush through the borrow: taking the handle first would leave `Drop`
        // with nothing to unlink if the flush fails, stranding a corrupt file
        // that a failed transfer just reported as failed.
        if let Some(file) = self.file.as_mut() {
            file.flush()?;
        }
        self.file = None;
        Ok(self.path.clone())
    }
}

impl Drop for Receiver {
    /// A dropped receiver never completed: close the handle before unlinking so
    /// Windows lets the delete through, and do not leave a truncated file that
    /// looks like a successful transfer.
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Resolves a user-typed source path against the pane's working directory.
///
/// Absolute paths are honored: the person typing has a shell on that side
/// already, so this is not a privilege boundary — only *destination* paths are.
pub(crate) fn resolve_source(base: &Path, typed: &str) -> PathBuf {
    let expanded = crate::worktree::expand_tilde_path(typed);
    if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "herdr-file-transfer-test-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn checked_name_accepts_plain_file_names() {
        for name in ["a", "notes.txt", "a b.txt", ".hidden", "안녕.txt", "a..b"] {
            assert!(checked_name(name).is_ok(), "{name} should be accepted");
        }
    }

    #[test]
    fn checked_name_rejects_everything_that_escapes_the_destination() {
        for name in [
            "",
            ".",
            "..",
            "../etc/passwd",
            "..\\windows\\system32",
            "/etc/passwd",
            "\\\\server\\share",
            "C:\\Windows\\notepad.exe",
            "C:relative.txt",
            "sub/dir.txt",
            "sub\\dir.txt",
            "with\0nul.txt",
            "stream.txt:hidden",
            "trailing.",
            "trailing ",
            " leading",
            "CON",
            "nul.txt",
            "CoM1.log",
            "CONIN$",
            "conout$.txt",
            "COM¹",
            "lpt².txt",
        ] {
            assert!(checked_name(name).is_err(), "{name:?} should be rejected");
        }
        assert!(checked_name(&"x".repeat(256)).is_err());
    }

    #[test]
    fn the_rejection_table_is_platform_independent() {
        // `is_unsafe_name` carries no `cfg`, so this table is the same on every
        // target; `just windows-lint` compiles these very assertions for
        // x86_64-pc-windows-msvc. A Unix server must reject a Windows-shaped
        // escape before a Windows client ever resolves it.
        for name in [
            "C:\\Windows\\notepad.exe",
            "C:relative.txt",
            "..\\windows\\system32",
            "\\\\server\\share",
            "stream.txt:hidden",
            "NUL",
            "trailing.",
        ] {
            assert!(
                is_unsafe_name(name),
                "{name:?} must be rejected by the cfg-free gate, not by std::path"
            );
        }
    }

    #[test]
    fn create_destination_suffixes_instead_of_overwriting() {
        let dir = tempdir("overwrite");
        let (first, _a) = create_destination(&dir, "a.txt").expect("first create");
        assert_eq!(first.file_name().unwrap(), "a.txt");
        fs::write(&first, b"original").expect("seed");

        let (second, _b) = create_destination(&dir, "a.txt").expect("second create");
        assert_eq!(second.file_name().unwrap(), "a-1.txt");
        let (third, _c) = create_destination(&dir, "a.txt").expect("third create");
        assert_eq!(third.file_name().unwrap(), "a-2.txt");

        // The point of suffixing: the original is still intact.
        assert_eq!(fs::read(&first).expect("read original"), b"original");
    }

    #[test]
    fn suffixed_name_keeps_extensions_and_dotfiles_intact() {
        assert_eq!(suffixed_name("notes.txt", 1), "notes-1.txt");
        assert_eq!(suffixed_name("notes", 2), "notes-2");
        // `.gitignore` is all stem, so the leading dot must survive.
        assert_eq!(suffixed_name(".gitignore", 1), ".gitignore-1");
        assert_eq!(suffixed_name("archive.tar.gz", 1), "archive.tar-1.gz");
    }

    #[test]
    fn a_suffixed_candidate_is_revalidated_before_use() {
        // `checked_name` passes a 255-byte name whose extension is mostly spaces;
        // suffixing then clamps it and can leave a trailing space, which the
        // rejection table exists to block.
        let hostile = format!("a.{}{}", "x".repeat(MAX_NAME_BYTES - 4), " b");
        if checked_name(&hostile).is_ok() {
            let suffixed = suffixed_name(&hostile, 1);
            if is_unsafe_name(&suffixed) {
                let dir = tempdir("revalidate");
                // The unsafe candidate must be skipped, not written.
                let (path, _f) = create_destination(&dir, &hostile).expect("first");
                assert!(!path.file_name().unwrap().to_str().unwrap().ends_with(' '));
            }
        }
        // The invariant that matters regardless of the crafted case above.
        for attempt in 1..4u32 {
            let candidate = suffixed_name("notes.txt", attempt);
            assert!(!is_unsafe_name(&candidate), "{candidate:?} must stay safe");
        }
    }

    #[test]
    fn suffixed_name_stays_within_the_name_length_limit() {
        // `checked_name` accepts exactly 255 bytes, so the suffix is what would
        // push the write past NAME_MAX and fail with an opaque ENAMETOOLONG.
        let long = format!("{}.txt", "a".repeat(MAX_NAME_BYTES - 4));
        assert_eq!(long.len(), MAX_NAME_BYTES);
        assert!(checked_name(&long).is_ok());
        let suffixed = suffixed_name(&long, 7);
        assert!(suffixed.len() <= MAX_NAME_BYTES, "{}", suffixed.len());
        assert!(suffixed.ends_with("-7.txt"));

        // Multi-byte stems must not be cut mid-character.
        let wide = format!("{}.txt", "가".repeat(80));
        let suffixed = suffixed_name(&wide, 3);
        assert!(suffixed.len() <= MAX_NAME_BYTES);
        assert!(std::str::from_utf8(suffixed.as_bytes()).is_ok());

        // A long *extension* blows the budget even when the stem is tiny.
        let long_ext = format!("a.{}", "x".repeat(MAX_NAME_BYTES - 2));
        assert_eq!(long_ext.len(), MAX_NAME_BYTES);
        assert!(checked_name(&long_ext).is_ok());
        assert!(suffixed_name(&long_ext, 1).len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn receiver_reports_the_name_it_actually_wrote() {
        let dir = tempdir("reported-name");
        let _first = Receiver::create(&dir, "a.bin", 1).expect("first");
        let second = Receiver::create(&dir, "a.bin", 1).expect("second");
        assert_eq!(second.name(), "a-1.bin");
    }

    #[test]
    fn create_destination_refuses_an_escaping_name() {
        let dir = tempdir("escape");
        let err = create_destination(&dir, "../escaped.txt").expect_err("escape");
        assert!(matches!(err, TransferError::UnsafeName), "{err}");
        assert!(!dir.parent().expect("parent").join("escaped.txt").exists());
    }

    #[test]
    fn open_source_rejects_directories_and_oversized_files() {
        let dir = tempdir("source");
        let err = open_source(&dir).expect_err("directory");
        assert!(matches!(err, TransferError::NotAFile), "{err}");

        let big = dir.join("big.bin");
        let file = fs::File::create(&big).expect("create");
        file.set_len(MAX_FILE_TRANSFER_SIZE + 1).expect("set_len");
        drop(file);
        let err = open_source(&big).expect_err("oversized");
        assert!(matches!(err, TransferError::TooLarge { .. }), "{err}");
    }

    #[test]
    fn sender_and_receiver_roundtrip_a_multi_chunk_file() {
        let dir = tempdir("roundtrip");
        let src_dir = dir.join("src");
        let dst_dir = dir.join("dst");
        fs::create_dir_all(&src_dir).expect("src dir");

        let payload: Vec<u8> = (0..FILE_TRANSFER_CHUNK_SIZE * 2 + 7)
            .map(|i| (i % 251) as u8)
            .collect();
        let src = src_dir.join("payload.bin");
        fs::write(&src, &payload).expect("write source");

        let (file, name, size) = open_source(&src).expect("open source");
        assert_eq!(name, "payload.bin");
        let mut sender = Sender::new(file, size);
        let mut receiver = Receiver::create(&dst_dir, &name, size).expect("create receiver");

        let mut chunks = 0;
        while let Some((seq, data)) = sender.next_chunk().expect("next chunk") {
            receiver.write_chunk(seq, &data).expect("write chunk");
            chunks += 1;
        }
        assert_eq!(chunks, 3);
        assert!(receiver.is_complete());
        let path = receiver.finish().expect("finish");
        assert_eq!(fs::read(&path).expect("read back"), payload);
    }

    #[test]
    fn receiver_rejects_out_of_order_and_overrun_chunks() {
        let dir = tempdir("desync");
        let mut receiver = Receiver::create(&dir, "a.bin", 4).expect("create");
        assert!(matches!(
            receiver.write_chunk(1, &[0; 4]).expect_err("out of order"),
            TransferError::Desync
        ));
        assert!(matches!(
            receiver.write_chunk(0, &[0; 5]).expect_err("overrun"),
            TransferError::Desync
        ));
        // An empty chunk is a desync, not a no-op: it would advance the
        // sequence without advancing progress.
        assert!(matches!(
            receiver.write_chunk(0, &[]).expect_err("empty"),
            TransferError::Desync
        ));
        // A short-but-nonempty chunk is equally wrong while bytes remain.
        assert!(matches!(
            receiver.write_chunk(0, &[1, 2]).expect_err("short"),
            TransferError::Desync
        ));
        receiver.write_chunk(0, &[1, 2, 3, 4]).expect("in order");
        assert!(receiver.is_complete());
    }

    #[test]
    fn receiver_rejects_an_oversized_announced_size() {
        let dir = tempdir("announced");
        let err = Receiver::create(&dir, "a.bin", MAX_FILE_TRANSFER_SIZE + 1).expect_err("size");
        assert!(matches!(err, TransferError::TooLarge { .. }), "{err}");
    }

    #[test]
    fn dropping_an_incomplete_receiver_unlinks_the_partial_file() {
        let dir = tempdir("abort");
        let path = {
            let size = FILE_TRANSFER_CHUNK_SIZE as u64 + 10;
            let mut receiver = Receiver::create(&dir, "a.bin", size).expect("create");
            receiver
                .write_chunk(0, &vec![7u8; FILE_TRANSFER_CHUNK_SIZE])
                .expect("first chunk");
            assert!(!receiver.is_complete());
            receiver.path().to_path_buf()
        };
        assert!(!path.exists(), "partial file should be removed on abort");
    }

    #[test]
    fn finishing_a_short_transfer_fails_and_still_unlinks() {
        let dir = tempdir("short");
        let receiver = Receiver::create(&dir, "a.bin", 8).expect("create");
        let path = receiver.path().to_path_buf();
        assert!(matches!(
            receiver.finish().expect_err("short"),
            TransferError::Desync
        ));
        assert!(!path.exists());
    }

    #[test]
    fn sender_reports_desync_when_the_source_is_truncated_mid_transfer() {
        let dir = tempdir("truncated");
        let src = dir.join("shrink.bin");
        fs::write(&src, vec![7u8; FILE_TRANSFER_CHUNK_SIZE * 2]).expect("write");
        let (file, _name, size) = open_source(&src).expect("open");
        let mut sender = Sender::new(file, size);
        sender.next_chunk().expect("first chunk");
        fs::write(&src, b"tiny").expect("truncate");
        // The handle still points at the old inode on unix, so only platforms
        // that truncate in place surface the short read; both outcomes are
        // acceptable, a silent zero-padded tail is not.
        match sender.next_chunk() {
            Ok(Some((_, data))) => assert_eq!(data.len(), FILE_TRANSFER_CHUNK_SIZE),
            Ok(None) => panic!("sender ended early without reporting"),
            Err(err) => assert!(matches!(err, TransferError::Desync), "{err}"),
        }
    }

    #[test]
    fn resolve_source_joins_relative_paths_onto_the_pane_cwd() {
        let base = Path::new("/panes/here");
        assert_eq!(
            resolve_source(base, "logs/out.txt"),
            base.join("logs/out.txt")
        );
        assert_eq!(
            resolve_source(base, "/etc/hosts"),
            PathBuf::from("/etc/hosts")
        );
    }
}
