pub mod host;
pub mod peer;

use host::HostSession;
use peer::PeerSession;

// ── Session Event ───────────────────────────────────────────────────────────

/// Events emitted by the session layer to the application / UI.
#[derive(Debug)]
pub enum SessionEvent {
    /// A peer successfully joined the session.
    PeerJoined {
        peer_id: u32,
        display_name: String,
    },
    /// A peer left the session (voluntarily or by disconnect).
    PeerLeft {
        peer_id: u32,
    },
    /// A message was received that the session layer doesn't handle internally.
    MessageReceived {
        peer_id: u32,
        message: crate::network::messages::Message,
    },
    /// The host shut down the session (peer side only).
    HostDisconnected,
    /// The join request was rejected by the host.
    JoinRejected {
        reason: String,
    },
}

// ── Session Coordinator ─────────────────────────────────────────────────────

/// Top-level session state. The app holds exactly one of these.
pub enum Session {
    Host(HostSession),
    Peer(PeerSession),
    None,
}

impl Session {
    /// Returns `true` if there is an active session (host or peer).
    pub fn is_active(&self) -> bool {
        !matches!(self, Session::None)
    }

    /// Shut down whatever session is active and transition to `None`.
    pub async fn shutdown(&mut self) {
        let old = std::mem::replace(self, Session::None);
        match old {
            Session::Host(h) => h.shutdown().await,
            Session::Peer(mut p) => p.leave().await,
            Session::None => {}
        }
    }
}

impl Default for Session {
    fn default() -> Self {
        Session::None
    }
}
