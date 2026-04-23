//! In-process transport: direct tokio mpsc channels between TUI and
//! daemon. No serialization, no sockets. This is the default when both
//! halves live in the same process.

use crate::{Client, Server};
use tokio::sync::mpsc;

/// Create a connected `Client` / `Server` pair.
///
/// The daemon holds the `Server`; the TUI holds the `Client`. Dropping
/// either end signals the other to shut down (channels close).
pub fn pair() -> (Client, Server) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::unbounded_channel();
    (
        Client::from_channels(cmd_tx, evt_rx),
        Server::from_channels(evt_tx, cmd_rx),
    )
}
