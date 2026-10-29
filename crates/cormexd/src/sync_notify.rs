//! The fan-out bus for realtime sync.
//!
//! The event source is [`cormex_store::Store::listen_sync`]
//! (`LISTEN`/`NOTIFY` on `sync_log`; the idempotent installation of the trigger
//! DDL, reconnection and backoff all live inside the storage layer, and so does
//! the reasoning for "why a database trigger"). This module does one thing only:
//! turn the signals the storage layer emits into [`SyncEvent`]s and broadcast
//! them to every `/ws` connection.
//!
//! # Why this layer needs to exist at all
//!
//! The storage layer's subscription handle is **single-consumer** (one `LISTEN`
//! connection, one `recv`), while `/ws` has many connections. The broadcast in
//! between turns "one event source" into "N subscribers", and along the way it
//! absorbs the "subscriber is too slow" degradation: a connection that falls
//! behind receives `Lagged`, which it handles exactly the way it handles a
//! reconnect — backfill from its own cursor.

use cormex_store::{Store, SyncSignal};
use tokio::sync::broadcast;

use crate::dto::SyncEvent;

/// Broadcast buffer.
///
/// An event is only the signal "there is new data"; a client that falls behind
/// simply backfills, so this does not need to be large. When it fills up, the
/// slowest subscriber receives `Lagged`, which it handles exactly the way it
/// handles a reconnect.
const BUS_CAPACITY: usize = 256;

/// The publish bus for realtime sync signals.
#[derive(Clone)]
pub struct SyncBus {
    tx: broadcast::Sender<SyncEvent>,
}

impl SyncBus {
    /// Subscribe to the storage layer's advance notifications and spawn the
    /// forwarding task.
    ///
    /// Cannot fail: the storage layer's subscription heals itself (reconnect,
    /// backoff), and failing to install the trigger only means "push goes
    /// silently dead" rather than a failed startup — the `GET /sync` polling
    /// path, which is the floor everything falls back to, does not depend on it.
    pub fn spawn(store: Store) -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);

        let bus_tx = tx.clone();
        tokio::spawn(async move {
            let mut notifications = store.listen_sync().await;
            // recv returns None for exactly one reason: the storage layer's
            // connection pool has closed, i.e. the process is shutting down
            while let Some(signal) = notifications.recv().await {
                // With no subscribers, send returns Err; that is not an error
                // here
                let _ = bus_tx.send(to_event(signal));
            }
            tracing::info!("the storage layer shut down; the sync-signal forwarder is done");
        });

        Self { tx }
    }

    /// The inert bus used when no database is attached: it accepts subscribers
    /// but never emits an event.
    ///
    /// This keeps `/ws` a **behaviourally correct** endpoint under the mock
    /// backend (the handshake succeeds, hello arrives, it closes gracefully), so
    /// a client's connection and reconnection logic stays testable.
    #[must_use]
    pub fn inert() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self { tx }
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<SyncEvent> {
        self.tx.subscribe()
    }

    /// Push one signal by hand. The mock backend uses this to demonstrate push
    /// timing.
    pub fn publish(&self, event: SyncEvent) {
        let _ = self.tx.send(event);
    }
}

/// Storage-layer signal → wire event. The two sides correspond one-to-one on
/// purpose, and the conversion **makes no judgement of its own** — whether a
/// gap occurred is something only the event source knows, and an intermediate
/// layer that merges variants on its own initiative flattens away an operational
/// signal.
///
/// # Why a free function rather than a `From` impl
///
/// `SyncSignal` belongs to `cormex-store` and `SyncEvent` belongs to
/// `cormex-proto`; cormexd owns neither, so the orphan rule forbids writing the
/// impl here. And putting the impl into either of those crates would require
/// adding a dependency edge in the **wrong direction**: the protocol crate
/// depending on a database driver, or the storage layer depending on the HTTP
/// contract. Neither is acceptable.
///
/// The answer to "who owns this conversion" was cormexd all along — it is the
/// only place that can see both sides at once.
#[must_use]
pub fn to_event(signal: SyncSignal) -> SyncEvent {
    match signal {
        SyncSignal::Bump { cursor } => SyncEvent::Bump { cursor },
        SyncSignal::Resync { cursor } => SyncEvent::Resync { cursor },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two variants must map to their own kinds; neither may collapse into
    /// bump.
    ///
    /// After such a collapse the client still works (both mean "go fetch"), but
    /// the operational signal "the notification path broke at some point"
    /// becomes permanently unobservable — and seeing it appear frequently is
    /// the only clue that the database connection is unstable.
    #[test]
    fn each_signal_keeps_its_own_kind() {
        assert!(
            matches!(
                to_event(SyncSignal::Bump { cursor: 7 }),
                SyncEvent::Bump { cursor: 7 }
            ),
            "a bump must map to a bump and keep its cursor"
        );
        assert!(
            matches!(
                to_event(SyncSignal::Resync { cursor: 9 }),
                SyncEvent::Resync { cursor: 9 }
            ),
            "a resync must not be merged into a bump, or the \"there may be a gap\" signal is lost"
        );
    }

    /// The inert bus must accept subscribers and publishes without panicking —
    /// the mock backend rests entirely on it.
    #[tokio::test]
    async fn an_inert_bus_still_delivers_manual_pushes() {
        let bus = SyncBus::inert();
        let mut rx = bus.subscribe();
        bus.publish(SyncEvent::Bump { cursor: 42 });

        let got = rx
            .recv()
            .await
            .expect("an event published a moment ago must be received");
        assert!(
            matches!(got, SyncEvent::Bump { cursor: 42 }),
            "a manual push on the inert bus was rewritten: {got:?}"
        );

        // publish must not panic when there are no subscribers
        drop(rx);
        bus.publish(SyncEvent::Resync { cursor: 43 });
    }
}
