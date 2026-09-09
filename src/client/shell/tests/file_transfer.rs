use super::*;

use crate::protocol::file_transfer::{
    encode_chunk, ClientFileTransferControl as Control, FileTransferEntry,
    ServerFileTransferControl as Reply,
};

fn shell() -> ClientShellState {
    shell_receiving_into(&std::path::PathBuf::from("/dev/null/unused"))
}

/// Received files land in `remote.file_transfer_dir`, so every test that lets a
/// download reach the disk points it at its own temp directory rather than the
/// developer's real one.
fn shell_receiving_into(dir: &std::path::Path) -> ClientShellState {
    let mut config = Config::default();
    config.remote.file_transfer_dir = dir.to_string_lossy().into_owned();
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&config));
    state.set_snapshot(Box::new(snapshot()));
    state.set_pane_surface(surface());
    state
}

fn key(state: &mut ClientShellState, code: KeyCode) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        code,
        KeyModifiers::NONE,
    ))])
}

fn controls(outcome: &ClientShellInput) -> Vec<Control> {
    outcome
        .requests
        .iter()
        .filter_map(|request| match request {
            ClientMessage::EndpointControl { kind, data }
                if kind == crate::protocol::file_transfer::CLIENT_FILE_TRANSFER_KIND =>
            {
                Some(serde_json::from_str(data).expect("decode transfer control"))
            }
            _ => None,
        })
        .collect()
}

fn listing(dir: &str, parent: Option<&str>, entries: Vec<FileTransferEntry>) -> Reply {
    Reply::Listing {
        dir: dir.to_owned(),
        parent: parent.map(str::to_owned),
        entries,
        truncated: false,
        error: None,
    }
}

fn entry(name: &str, is_dir: bool) -> FileTransferEntry {
    FileTransferEntry {
        name: name.to_owned(),
        is_dir,
        size: (!is_dir).then_some(4),
    }
}

fn open_browser(state: &mut ClientShellState) {
    let mut outcome = ClientShellInput::default();
    state.open_file_transfer_receive(&mut outcome);
    assert!(matches!(
        controls(&outcome).as_slice(),
        [Control::List { path: None, .. }]
    ));
    state.handle_file_transfer_control(listing(
        "/srv/work",
        Some("/srv"),
        vec![entry("logs", true), entry("notes.txt", false)],
    ));
    state.compose(120, 40).expect("browser frame");
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("herdr-shell-ft-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn a_listing_keeps_the_parent_row_and_starts_on_it() {
    let mut state = shell();
    open_browser(&mut state);

    let Some(ClientShellOverlay::FileTransfer(ClientFileTransferOverlay::Browse(browser))) =
        state.overlay.as_ref()
    else {
        panic!("the receive browser should be open");
    };
    assert_eq!(browser.dir, "/srv/work");
    let names: Vec<_> = browser
        .entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    assert_eq!(names, ["..", "logs", "notes.txt"]);
    assert_eq!(browser.selected, 0);
    // The renderer owns the window height; without it a held arrow key cannot
    // page and a click lands on the wrong row.
    assert!(browser.visible_rows > 0);
}

#[test]
fn opening_a_directory_asks_for_its_listing_instead_of_transferring() {
    let mut state = shell();
    open_browser(&mut state);

    key(&mut state, KeyCode::Down);
    let outcome = key(&mut state, KeyCode::Enter);

    // The client never joins a server path: it echoes the directory back and
    // names the entry, because the server may not share its path syntax.
    assert!(matches!(
        controls(&outcome).as_slice(),
        [Control::List {
            path: Some(path),
            child: Some(child),
            ..
        }] if path == "/srv/work" && child == "logs"
    ));
}

#[test]
fn picking_a_file_requests_the_download_and_shows_progress() {
    let mut state = shell();
    open_browser(&mut state);

    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Down);
    let outcome = key(&mut state, KeyCode::Enter);

    assert!(matches!(
        controls(&outcome).as_slice(),
        [Control::Download { dir, name, .. }] if dir == "/srv/work" && name == "notes.txt"
    ));
    assert!(matches!(
        state.overlay.as_ref(),
        Some(ClientShellOverlay::FileTransfer(
            ClientFileTransferOverlay::Progress(_)
        ))
    ));
}

#[test]
fn typing_filters_the_list_and_keeps_the_selection_on_a_visible_row() {
    let mut state = shell();
    open_browser(&mut state);

    state.handle_raw_events(vec![RawInputEvent::Key(crate::input::TerminalKey::new(
        KeyCode::Char('n'),
        KeyModifiers::NONE,
    ))]);

    let Some(ClientShellOverlay::FileTransfer(ClientFileTransferOverlay::Browse(browser))) =
        state.overlay.as_ref()
    else {
        panic!("the receive browser should be open");
    };
    assert_eq!(browser.query, "n");
    // `..` always survives the filter, so the selection can stay on it.
    let filtered: Vec<_> = browser
        .filtered_indices()
        .into_iter()
        .map(|index| browser.entries[index].name.as_str())
        .collect();
    assert_eq!(filtered, ["..", "notes.txt"]);
    assert!(browser.filtered_indices().contains(&browser.selected));
}

#[test]
fn an_upload_streams_one_chunk_per_ack_and_reports_the_saved_name() {
    let dir = tempdir("upload");
    let source = dir.join("payload.bin");
    let payload = vec![3u8; crate::protocol::file_transfer::FILE_TRANSFER_CHUNK_SIZE + 10];
    std::fs::write(&source, &payload).expect("write source");

    let mut state = shell();
    state.open_file_transfer_send();
    let outcome = state.insert_overlay_text(&source.to_string_lossy());
    assert!(outcome);
    let outcome = key(&mut state, KeyCode::Enter);
    assert!(matches!(
        controls(&outcome).as_slice(),
        [
            Control::Start { size, .. },
            Control::Chunk { seq: 0, .. }
        ] if *size == payload.len() as u64
    ));

    // Only the awaited ack releases the next chunk.
    let ignored = state.handle_file_transfer_control(Reply::Ack {
        transfer_id: 1,
        seq: 7,
    });
    assert!(controls(&ignored).is_empty());
    let next = state.handle_file_transfer_control(Reply::Ack {
        transfer_id: 1,
        seq: 0,
    });
    assert!(matches!(
        controls(&next).as_slice(),
        [Control::Chunk { seq: 1, .. }]
    ));
    // Every byte is out; the server's verdict names the file it actually wrote.
    let drained = state.handle_file_transfer_control(Reply::Ack {
        transfer_id: 1,
        seq: 1,
    });
    assert!(controls(&drained).is_empty());
    state.handle_file_transfer_control(Reply::End {
        transfer_id: 1,
        ok: true,
        error: None,
        saved_name: Some("payload-1.bin".into()),
    });

    let Some(ClientShellOverlay::FileTransfer(ClientFileTransferOverlay::Progress(run))) =
        state.overlay.as_ref()
    else {
        panic!("the progress popup should be up");
    };
    assert_eq!(run.name, "payload-1.bin");
    assert!(matches!(run.outcome, Some(Ok(()))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unopenable_source_reports_instead_of_hanging() {
    let mut state = shell();
    state.open_file_transfer_send();
    assert!(state.insert_overlay_text("/definitely/not/here.bin"));
    let outcome = key(&mut state, KeyCode::Enter);

    assert!(controls(&outcome).is_empty(), "nothing reached the wire");
    let Some(ClientShellOverlay::FileTransfer(ClientFileTransferOverlay::Progress(run))) =
        state.overlay.as_ref()
    else {
        panic!("the failure needs a popup of its own");
    };
    assert!(matches!(run.outcome, Some(Err(_))));
}

#[test]
fn escape_cancels_a_running_transfer_before_it_dismisses_the_popup() {
    let dir = tempdir("cancel");
    let source = dir.join("payload.bin");
    std::fs::write(
        &source,
        vec![1u8; crate::protocol::file_transfer::FILE_TRANSFER_CHUNK_SIZE + 1],
    )
    .expect("write source");

    let mut state = shell();
    state.open_file_transfer_send();
    assert!(state.insert_overlay_text(&source.to_string_lossy()));
    key(&mut state, KeyCode::Enter);

    let cancel = key(&mut state, KeyCode::Esc);
    assert!(matches!(
        controls(&cancel).as_slice(),
        [Control::End { ok: false, .. }]
    ));
    assert!(
        state.overlay.is_some(),
        "the reason has to stay readable after the cancel"
    );
    key(&mut state, KeyCode::Esc);
    assert!(state.overlay.is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_chunk_for_an_unknown_transfer_is_answered_not_dropped() {
    let mut state = shell();
    let outcome = state.handle_file_transfer_control(Reply::Chunk {
        transfer_id: 99,
        seq: 0,
        data: encode_chunk(b"x"),
    });

    assert!(matches!(
        controls(&outcome).as_slice(),
        [Control::End {
            transfer_id: 99,
            ok: false,
            ..
        }]
    ));
}

#[test]
fn a_dropped_path_reaches_the_prompt_unquoted() {
    // Only the quoting, which is the same on every platform. The
    // backslash-escaping rules are per-target and are covered by the engine's
    // own tests.
    let mut state = shell();
    state.open_file_transfer_send();
    assert!(state.insert_overlay_text("'/srv/my notes.txt'"));

    let Some(ClientShellOverlay::FileTransfer(ClientFileTransferOverlay::SendPath(input))) =
        state.overlay.as_ref()
    else {
        panic!("the send prompt should be open");
    };
    assert_eq!(input, "/srv/my notes.txt");

    // Typing still edits the same field, and Backspace still deletes.
    key(&mut state, KeyCode::Char('x'));
    key(&mut state, KeyCode::Backspace);
    key(&mut state, KeyCode::Backspace);
    let Some(ClientShellOverlay::FileTransfer(ClientFileTransferOverlay::SendPath(input))) =
        state.overlay.as_ref()
    else {
        panic!("the send prompt should be open");
    };
    assert_eq!(input, "/srv/my notes.tx");
}

#[test]
fn a_second_download_announcement_is_refused_rather_than_restarted() {
    let dir = tempdir("duplicate-start");
    let mut state = shell_receiving_into(&dir);
    open_browser(&mut state);
    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Enter);

    // A second announcement would open a second destination and reset the
    // sequence, so the transfer stops instead.
    let announce = |state: &mut ClientShellState| {
        state.handle_file_transfer_control(Reply::Start {
            transfer_id: 1,
            name: "notes.txt".into(),
            size: 4,
        })
    };
    announce(&mut state);
    let second = announce(&mut state);

    assert!(matches!(
        controls(&second).as_slice(),
        [Control::End { ok: false, .. }]
    ));
    let Some(ClientShellOverlay::FileTransfer(ClientFileTransferOverlay::Progress(run))) =
        state.overlay.as_ref()
    else {
        panic!("the progress popup should be up");
    };
    assert!(matches!(run.outcome, Some(Err(_))));
    assert!(
        !dir.join("notes.txt").exists(),
        "the partial destination should be unlinked"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_download_lands_in_the_configured_directory_and_acks_each_chunk() {
    let dir = tempdir("download");
    let mut state = shell_receiving_into(&dir);
    open_browser(&mut state);
    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Enter);

    state.handle_file_transfer_control(Reply::Start {
        transfer_id: 1,
        name: "notes.txt".into(),
        size: 4,
    });
    let chunk = state.handle_file_transfer_control(Reply::Chunk {
        transfer_id: 1,
        seq: 0,
        data: encode_chunk(b"abcd"),
    });

    assert!(matches!(
        controls(&chunk).as_slice(),
        [Control::Ack { seq: 0, .. }, Control::End { ok: true, .. }]
    ));
    assert_eq!(
        std::fs::read(dir.join("notes.txt")).expect("written"),
        b"abcd"
    );
    let Some(ClientShellOverlay::FileTransfer(ClientFileTransferOverlay::Progress(run))) =
        state.overlay.as_ref()
    else {
        panic!("the progress popup should be up");
    };
    assert!(matches!(run.outcome, Some(Ok(()))));

    // The server's own trailing verdict must not turn a file that is on disk
    // into a reported failure.
    state.handle_file_transfer_control(Reply::End {
        transfer_id: 1,
        ok: false,
        error: Some("the transfer stalled and was abandoned".into()),
        saved_name: None,
    });
    let Some(ClientShellOverlay::FileTransfer(ClientFileTransferOverlay::Progress(run))) =
        state.overlay.as_ref()
    else {
        panic!("the progress popup should be up");
    };
    assert!(matches!(run.outcome, Some(Ok(()))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_zero_byte_download_completes_on_its_announcement() {
    let dir = tempdir("zero-byte");
    let mut state = shell_receiving_into(&dir);
    open_browser(&mut state);
    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Enter);

    // No chunks will follow, so the announcement is the whole transfer.
    let announced = state.handle_file_transfer_control(Reply::Start {
        transfer_id: 1,
        name: "empty.txt".into(),
        size: 0,
    });

    assert!(matches!(
        controls(&announced).as_slice(),
        [Control::End { ok: true, .. }]
    ));
    assert_eq!(std::fs::read(dir.join("empty.txt")).expect("written"), b"");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_success_end_with_bytes_still_missing_stops_instead_of_waiting() {
    let dir = tempdir("early-end");
    let mut state = shell_receiving_into(&dir);
    open_browser(&mut state);
    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Enter);
    state.handle_file_transfer_control(Reply::Start {
        transfer_id: 1,
        name: "notes.txt".into(),
        size: 4,
    });

    // The server announces four bytes, sends none, then reports success. Both
    // sides would otherwise wait out the stall deadline.
    let ended = state.handle_file_transfer_control(Reply::End {
        transfer_id: 1,
        ok: true,
        error: None,
        saved_name: None,
    });

    assert!(matches!(
        controls(&ended).as_slice(),
        [Control::End { ok: false, .. }]
    ));
    assert!(!dir.join("notes.txt").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
