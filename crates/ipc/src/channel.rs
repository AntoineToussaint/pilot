//! In-process transport: direct tokio mpsc channels between TUI and
//! daemon. No serialization, no sockets. This is the default when both
//! halves live in the same process.

use crate::{Client, Connection};
use tokio::sync::mpsc;

/// Create a connected `Client` / `Connection` pair.
///
/// The daemon holds the `Connection`; the TUI holds the `Client`. Dropping
/// either end signals the other to shut down (channels close).
pub fn pair() -> (Client, Connection) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel();
    (
        Client::from_channels(cmd_tx, evt_rx),
        Connection::from_channels(evt_tx, cmd_rx),
    )
}
