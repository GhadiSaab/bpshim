//! The boundary between libbp and a Bundle Protocol Agent (BPA).

use std::time::Duration;

use crate::eid::Eid;
use crate::error::Result;

/// One adapter instance backs one BP socket.
///
/// Everything is synchronous: `recv` blocks the calling thread. No background
/// threads or async runtime may be started, because libbp is loaded into
/// arbitrary programs that may `fork`.
pub trait BpaAdapter {
    /// Claims `eid` as this socket's local endpoint so bundles for it are queued.
    fn open(&mut self, eid: Eid) -> Result<()>;

    /// Releases the local endpoint (if any). Safe to call more than once.
    fn close(&mut self) -> Result<()>;

    /// Hands one payload to the BPA addressed to `dst`. If the socket is open,
    /// its local endpoint is the bundle's source.
    fn send(&mut self, dst: Eid, payload: &[u8]) -> Result<()>;

    /// Waits for the next bundle addressed to the local endpoint and returns
    /// its source and payload. `None` waits forever; `Some(Duration::ZERO)`
    /// checks once. Fails with `EAGAIN` on timeout and `EINTR` if a signal
    /// interrupts the wait.
    fn recv(&mut self, timeout: Option<Duration>) -> Result<(Eid, Vec<u8>)>;
}
