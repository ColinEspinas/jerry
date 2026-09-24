//! Notification fan-out: every connected client, in-process or over a socket, gets every
//! `event/*` message. A socket sink is bounded; one that falls behind or has gone away is
//! dropped, never retried. The in-process sink is the app's own, trusted to keep up.

use futures::channel::mpsc;
use jerry_core::Message;
use std::sync::{mpsc as std_mpsc, Arc, Mutex, PoisonError};

/// How many undelivered notifications a socket client may fall behind before it is dropped.
pub(crate) const SOCKET_BACKLOG: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SinkId(u64);

enum Sink {
    Local(mpsc::UnboundedSender<Message>),
    Socket(std_mpsc::SyncSender<Message>),
}

/// A cheap, `Clone`-able handle: `Inner` and `crate::session::SessionManager` each hold one and
/// broadcast on the same underlying sink list - a session's own exit-observing relay thread
/// (`SessionManager::spawn`) publishes `event/session-exited` this way, with no back-reference to
/// `Inner` at all.
#[derive(Default, Clone)]
pub(crate) struct Fanout {
    sinks: Arc<Mutex<Vec<(SinkId, Sink)>>>,
    next_id: Arc<Mutex<u64>>,
}

impl Fanout {
    pub(crate) fn subscribe_local(&self) -> mpsc::UnboundedReceiver<Message> {
        let (sender, receiver) = mpsc::unbounded();
        let id = self.next_id();
        self.lock().push((id, Sink::Local(sender)));
        receiver
    }

    pub(crate) fn subscribe_socket(&self, outbox: std_mpsc::SyncSender<Message>) -> SinkId {
        let id = self.next_id();
        self.lock().push((id, Sink::Socket(outbox)));
        id
    }

    pub(crate) fn unsubscribe(&self, id: SinkId) {
        self.lock().retain(|(sink_id, _)| *sink_id != id);
    }

    /// Drops every sink; each socket writer then ends once its reader has let go too.
    pub(crate) fn clear(&self) {
        self.lock().clear();
    }

    /// How many sinks are registered right now - `Inner::is_idle`'s own "a connected `event/
    /// subscribe` client counts as connected" half (`docs/architecture/decisions.md` §24).
    pub(crate) fn sink_count(&self) -> usize {
        self.lock().len()
    }

    pub(crate) fn broadcast(&self, message: Message) {
        self.lock().retain(|(_, sink)| match sink {
            Sink::Local(sender) => sender.unbounded_send(message.clone()).is_ok(),
            Sink::Socket(sender) => sender.try_send(message.clone()).is_ok(),
        });
    }

    fn next_id(&self) -> SinkId {
        let mut next = self.next_id.lock().unwrap_or_else(PoisonError::into_inner);
        let id = SinkId(*next);
        *next += 1;
        id
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(SinkId, Sink)>> {
        self.sinks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod fanout_tests {
    use super::{Fanout, SOCKET_BACKLOG};
    use jerry_core::Message;
    use std::sync::mpsc;

    fn event(n: usize) -> Message {
        Message::Notification {
            method: "event/test".into(),
            params: serde_json::json!({ "n": n }),
        }
    }

    #[test]
    fn a_socket_client_that_stops_reading_is_dropped_once_its_backlog_is_full() {
        let fanout = Fanout::default();
        let (slow_tx, _slow_rx) = mpsc::sync_channel(SOCKET_BACKLOG);
        let (fast_tx, fast_rx) = mpsc::sync_channel(SOCKET_BACKLOG);
        fanout.subscribe_socket(slow_tx);
        fanout.subscribe_socket(fast_tx);

        for n in 0..=SOCKET_BACKLOG {
            fanout.broadcast(event(n));
            // The fast client keeps draining.
            let _ = fast_rx.try_recv();
        }
        assert_eq!(
            fanout.lock().len(),
            1,
            "the slow sink is gone, the fast one stays"
        );
    }

    #[test]
    fn unsubscribing_removes_exactly_that_sink() {
        let fanout = Fanout::default();
        let (a_tx, a_rx) = mpsc::sync_channel(SOCKET_BACKLOG);
        let (b_tx, b_rx) = mpsc::sync_channel(SOCKET_BACKLOG);
        let a = fanout.subscribe_socket(a_tx);
        let _b = fanout.subscribe_socket(b_tx);
        fanout.unsubscribe(a);
        fanout.broadcast(event(1));
        assert!(a_rx.try_recv().is_err());
        assert!(b_rx.try_recv().is_ok());
    }
}
