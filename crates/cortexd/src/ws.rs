//! `/ws` — the realtime push channel for multi-device sync.
//!
//! The contract is in [`crate::dto::SyncEvent`] (including "why we push only a
//! signal and never the data"). The event source is [`crate::sync_notify`]
//! (including "why it goes through a database trigger").
//!
//! This module does exactly one thing: fan the signals on the bus out to every
//! connection, and **leave no dangling task behind when either end goes wrong**.

use std::time::Duration;

use axum::extract::{
    State, WebSocketUpgrade,
    ws::{Message, Utf8Bytes, WebSocket},
};
use axum::response::Response;
use futures::stream::SplitSink;
use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::broadcast::error::RecvError;

use crate::dto::SyncEvent;
use crate::state::AppState;

/// Application-level heartbeat interval.
///
/// Intermediate proxies (nginx, cloud load balancers) commonly cut a connection
/// after 60s of idleness, while sync push can stay silent for hours when the
/// user is not doing anything. TCP keepalive defaults to two hours before it
/// stirs, so it is no help here.
const PING_INTERVAL: Duration = Duration::from_secs(20);

pub async fn handler(ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| serve(socket, st))
}

async fn serve(socket: WebSocket, st: AppState) {
    // Subscribe first, read the cursor second; the order cannot be swapped. The
    // other way round, a transaction committing between the two steps is neither
    // covered by hello's cursor nor delivered as a bump on this connection —
    // a permanently missing stretch. In this order the worst case is receiving
    // one already-known bump twice, and the client is idempotent
    let mut rx = st.subscribe_sync();
    let cursor = st.latest_cursor().await;

    let (mut sink, mut stream) = socket.split();

    if send(
        &mut sink,
        &SyncEvent::Hello {
            cursor,
            version: cortex_core::VERSION,
        },
    )
    .await
    .is_err()
    {
        return;
    }

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // interval fires immediately the first time; drop that one

    loop {
        tokio::select! {
            event = rx.recv() => {
                let event = match event {
                    Ok(e) => e,
                    // This connection consumed too slowly and fell behind.
                    // Semantically identical to a disconnect: the client just
                    // backfills from its own cursor — exactly the payoff of
                    // pushing only a signal
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(missed = n, "the WS subscriber fell behind; degrading to resync");
                        SyncEvent::Resync { cursor: st.latest_cursor().await }
                    }
                    // A closed bus means the server is shutting down
                    Err(RecvError::Closed) => break,
                };
                if send(&mut sink, &event).await.is_err() {
                    break;
                }
            }

            incoming = stream.next() => {
                match incoming {
                    // Everything upstream goes over HTTP, so whatever arrives
                    // here is not interpreted. Keeping the read itself is
                    // mandatory: without it we never see the Close frame and
                    // the connection hangs around until the heartbeat fails
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(e)) => {
                        tracing::debug!(error = %e, "WS read error; closing the connection");
                        break;
                    }
                    Some(Ok(_)) => {}
                }
            }

            _ = ping.tick() => {
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }
        }
    }

    // Close explicitly, so the peer finds out immediately instead of waiting for
    // its own heartbeat to time out
    let _ = sink.close().await;
}

async fn send(sink: &mut SplitSink<WebSocket, Message>, event: &SyncEvent) -> Result<(), ()> {
    // Serialising a SyncEvent cannot fail (it is all numbers and static
    // strings), but if it somehow does, dropping the connection beats sending a
    // malformed frame: the client will at least reconnect
    let Ok(json) = serde_json::to_string(event) else {
        tracing::error!("could not serialize the SyncEvent");
        return Err(());
    };
    sink.send(Message::Text(Utf8Bytes::from(json)))
        .await
        .map_err(|_| ())
}
