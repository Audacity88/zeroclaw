//! Process-wide broadcast channel for the canonical log stream.

use std::sync::OnceLock;

use parking_lot::RwLock;
use serde_json::Value;
use tokio::sync::broadcast;

const BROADCAST_CAPACITY: usize = 65_536;

/// Type alias for the canonical log broadcast sender.
pub type LogBroadcastSender = broadcast::Sender<Value>;

static BROADCAST: OnceLock<RwLock<Option<LogBroadcastSender>>> = OnceLock::new();

fn slot() -> &'static RwLock<Option<LogBroadcastSender>> {
    BROADCAST.get_or_init(|| RwLock::new(None))
}

/// Install a process-wide broadcast sender. Calling again replaces the
/// previous one (the old sender will be dropped — its `Receiver`s will
/// see `RecvError::Closed` on their next read).
pub fn set_broadcast_hook(sender: LogBroadcastSender) {
    *slot().write() = Some(sender);
}

/// Remove the broadcast sender (tests, orderly shutdown).
pub fn clear_broadcast_hook() {
    *slot().write() = None;
}

/// Read the current broadcast sender, if any.
#[must_use]
pub fn current_broadcast_hook() -> Option<LogBroadcastSender> {
    slot().read().clone()
}

/// Subscribe to the broadcast stream. Returns `None` when no sender has
/// been installed yet (e.g. when running tests that never wired the
/// gateway). The receiver yields every event emitted via
/// [`crate::record_event`] after the subscribe call.
#[must_use]
pub fn subscribe() -> Option<broadcast::Receiver<Value>> {
    slot().read().as_ref().map(|s| s.subscribe())
}

#[doc(hidden)]
#[must_use]
pub fn subscribe_or_install() -> broadcast::Receiver<Value> {
    {
        let read = slot().read();
        if let Some(sender) = read.as_ref() {
            return sender.subscribe();
        }
    }
    subscribe_or_install_after_miss(slot())
}

fn subscribe_or_install_after_miss(
    slot: &RwLock<Option<LogBroadcastSender>>,
) -> broadcast::Receiver<Value> {
    let mut write = slot.write();
    if let Some(sender) = write.as_ref() {
        return sender.subscribe();
    }

    let (tx, rx) = broadcast::channel(BROADCAST_CAPACITY);
    *write = Some(tx);
    rx
}

pub(crate) static HOOK_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    #[tokio::test]
    async fn set_and_subscribe_round_trip() {
        // Install + emit happen inside this scope so the lock is released
        // before the await; otherwise clippy flags a sync Mutex held
        // across an await point.
        let mut rx = {
            let _guard = HOOK_TEST_LOCK.lock();
            clear_broadcast_hook();
            assert!(current_broadcast_hook().is_none());

            let (tx, _rx_keepalive) = broadcast::channel(8);
            set_broadcast_hook(tx);
            let rx = subscribe().expect("subscribe after set");

            let hook = current_broadcast_hook().unwrap();
            let _ = hook.send(serde_json::json!({ "ping": true }));
            rx
        };

        let value = rx.recv().await.unwrap();
        assert_eq!(value["ping"], true);

        let _guard = HOOK_TEST_LOCK.lock();
        clear_broadcast_hook();
        assert!(current_broadcast_hook().is_none());
    }

    #[test]
    fn concurrent_subscribe_or_install_shares_installed_sender() {
        let slot = Arc::new(RwLock::new(None));
        let start = Arc::new(Barrier::new(2));

        let handles = (0..2)
            .map(|_| {
                let slot = Arc::clone(&slot);
                let start = Arc::clone(&start);
                std::thread::spawn(move || {
                    start.wait();
                    subscribe_or_install_after_miss(&slot)
                })
            })
            .collect::<Vec<_>>();

        let mut receivers = handles
            .into_iter()
            .map(|handle| handle.join().expect("subscriber thread must complete"))
            .collect::<Vec<_>>();
        let sender = slot
            .read()
            .clone()
            .expect("one broadcast sender must be installed");
        assert_eq!(sender.receiver_count(), receivers.len());

        let expected = serde_json::json!({ "shared": true });
        sender.send(expected.clone()).expect("receivers are alive");
        for receiver in &mut receivers {
            assert_eq!(receiver.try_recv().unwrap(), expected);
        }
    }
}
