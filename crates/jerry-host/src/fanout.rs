//! Notification fan-out: every connected client, in-process or over a socket, gets every
//! `event/*` message. A sink that cannot accept is dropped, never retried.

use futures::channel::mpsc;
use jerry_core::Message;
use std::sync::{mpsc as std_mpsc, Mutex, PoisonError};

pub(crate) enum Sink {
    Local(mpsc::UnboundedSender<Message>),
    Socket(std_mpsc::Sender<Message>),
}

#[derive(Default)]
pub(crate) struct Fanout {
    sinks: Mutex<Vec<Sink>>,
}

impl Fanout {
    pub(crate) fn subscribe_local(&self) -> mpsc::UnboundedReceiver<Message> {
        let (sender, receiver) = mpsc::unbounded();
        self.lock().push(Sink::Local(sender));
        receiver
    }

    pub(crate) fn subscribe_socket(&self, outbox: std_mpsc::Sender<Message>) {
        self.lock().push(Sink::Socket(outbox));
    }

    pub(crate) fn broadcast(&self, message: Message) {
        self.lock().retain(|sink| match sink {
            Sink::Local(sender) => sender.unbounded_send(message.clone()).is_ok(),
            Sink::Socket(sender) => sender.send(message.clone()).is_ok(),
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Sink>> {
        self.sinks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
