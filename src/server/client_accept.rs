use std::io;
use std::sync::{atomic::AtomicBool, atomic::Ordering, Arc};

use interprocess::local_socket::traits::{Listener as _, Stream as _};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::ipc::LocalListener;
use crate::server::client_transport::{self, ServerEvent};

/// Accepts pending thin-client connections and starts their handshake readers.
pub(crate) fn accept_pending_client_connections(
    listener: &LocalListener,
    next_client_id: &mut u64,
    should_quit: &Arc<AtomicBool>,
    server_event_tx: &mpsc::Sender<ServerEvent>,
) -> io::Result<()> {
    loop {
        if should_quit.load(Ordering::Acquire) {
            break;
        }
        match listener.accept() {
            Ok(stream) => {
                let client_id = *next_client_id;
                *next_client_id = next_client_id.saturating_add(1);

                if let Err(err) = stream.set_nonblocking(true) {
                    warn!(err = %err, "failed to set client stream nonblocking");
                    continue;
                }

                let should_quit = should_quit.clone();
                let server_event_tx = server_event_tx.clone();
                if let Err(err) = spawn_handshake_thread(move || {
                    if let Err(err) = client_transport::handle_client_handshake(
                        stream,
                        client_id,
                        &server_event_tx,
                        &should_quit,
                    ) {
                        debug!(client_id, err = %err, "client handshake failed");
                    }
                }) {
                    warn!(err = %err, client_id, "failed to spawn client handshake thread; dropping connection");
                }
            }
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => {
                error!(err = %err, "client listener accept failed");
                break;
            }
        }
    }

    Ok(())
}

/// Test hook: forces [`spawn_handshake_thread`] to fail.
#[cfg(test)]
static FAIL_HANDSHAKE_SPAWN: AtomicBool = AtomicBool::new(false);

/// This loop runs on the server's main event loop, so a failed spawn must stay
/// a per-connection error: unwinding here would take the whole server down
/// along with every pane it owns.
fn spawn_handshake_thread<F>(handshake: F) -> io::Result<()>
where
    F: FnOnce() + Send + 'static,
{
    #[cfg(test)]
    if FAIL_HANDSHAKE_SPAWN.load(Ordering::Relaxed) {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "forced client handshake spawn failure",
        ));
    }

    std::thread::Builder::new()
        .name("herdr-client-handshake".into())
        .spawn(handshake)
        .map(|_| ())
}

/// Drains pending thin-client connections without starting handshakes.
///
/// During live handoff the old server must not let clients sit in the Unix
/// listener backlog waiting for a welcome frame that will never be sent.
pub(crate) fn reject_pending_client_connections(listener: &LocalListener) -> io::Result<()> {
    loop {
        match listener.accept() {
            Ok(_stream) => {}
            Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => {
                error!(err = %err, "client listener reject failed");
                break;
            }
        }
    }

    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use interprocess::local_socket::ListenerNonblockingMode;

    #[test]
    fn accept_loop_survives_handshake_thread_spawn_failure() {
        // Keep the name short: the full path has to fit sockaddr_un's sun_path.
        let path = std::env::temp_dir().join(format!(
            "herdr-accept-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos()
        ));
        let listener = crate::ipc::bind_local_listener(&path).expect("bind listener");
        listener
            .set_nonblocking(ListenerNonblockingMode::Accept)
            .expect("nonblocking listener");

        let mut first = crate::ipc::connect_local_stream(&path).expect("first client");
        let _second = crate::ipc::connect_local_stream(&path).expect("second client");

        let mut next_client_id = 1;
        let should_quit = Arc::new(AtomicBool::new(false));
        let (server_event_tx, _server_event_rx) = mpsc::channel(1);

        FAIL_HANDSHAKE_SPAWN.store(true, Ordering::Relaxed);
        let result = accept_pending_client_connections(
            &listener,
            &mut next_client_id,
            &should_quit,
            &server_event_tx,
        );
        FAIL_HANDSHAKE_SPAWN.store(false, Ordering::Relaxed);

        result.expect("accept loop keeps running after a failed spawn");
        assert_eq!(
            next_client_id, 3,
            "both pending connections are drained even when no handshake thread starts"
        );
        assert!(
            crate::ipc::local_stream_peer_closed(&mut first).expect("probe first client"),
            "the accepted connection is dropped instead of being held by a handshake thread"
        );

        let _ = std::fs::remove_file(&path);
    }
}
