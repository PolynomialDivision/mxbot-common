//! Sending messages: idempotent retries and thread replies.

use std::time::{Duration, SystemTime};

use matrix_sdk::{
    ruma::{
        api::error::{ErrorKind, RetryAfter},
        events::{
            relation::Thread,
            room::message::{OriginalSyncRoomMessageEvent, Relation, RoomMessageEventContent},
            MessageLikeEventContent,
        },
        OwnedEventId, TransactionId,
    },
    Room,
};
use tokio::time::sleep;
use tracing::warn;

use crate::retry::Backoff;

const MAX_ATTEMPTS: u32 = 4;
const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Classify a send failure as retryable, with any server-suggested delay
/// (`M_LIMIT_EXCEEDED`'s `retry_after`).
fn classify_send_error(error: &matrix_sdk::Error) -> (bool, Option<Duration>) {
    let matrix_sdk::Error::Http(http_error) = error else {
        // Serialization, crypto or store errors are not known to be transient.
        return (false, None);
    };
    let Some(api_error) = http_error.as_client_api_error() else {
        // No Matrix error body (connection reset, timeout, …): transient.
        return (true, None);
    };
    let retry_after = api_error.error_kind().and_then(|kind| match kind {
        ErrorKind::LimitExceeded(data) => data.retry_after.as_ref().and_then(|r| match r {
            RetryAfter::Delay(delay) => Some(*delay),
            RetryAfter::DateTime(at) => at.duration_since(SystemTime::now()).ok(),
        }),
        _ => None,
    });
    let status = api_error.status_code.as_u16();
    (status == 429 || status >= 500, retry_after)
}

/// Send `content`, retrying transient failures (5xx, 429, network).
///
/// Every attempt reuses one transaction ID: `PUT /send/{type}/{txnId}` is
/// idempotent, so a retry after a lost *response* returns the original event
/// instead of posting a duplicate.
pub async fn send_with_retry<C>(room: &Room, content: C) -> Result<OwnedEventId, matrix_sdk::Error>
where
    C: MessageLikeEventContent + Clone,
{
    let txn_id = TransactionId::new();
    let mut backoff = Backoff::new(INITIAL_BACKOFF, MAX_BACKOFF);
    let mut attempt = 1;
    loop {
        match room
            .send(content.clone())
            .with_transaction_id(txn_id.clone())
            .await
        {
            Ok(response) => return Ok(response.response.event_id),
            Err(error) => {
                let (retryable, retry_after) = classify_send_error(&error);
                if !retryable || attempt >= MAX_ATTEMPTS {
                    return Err(error);
                }
                let delay = retry_after
                    .unwrap_or_else(|| backoff.next_delay())
                    .min(MAX_BACKOFF);
                warn!(
                    room_id = %room.room_id(),
                    attempt,
                    %error,
                    "Matrix send failed transiently; retrying in {delay:?}"
                );
                sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

/// The thread root to answer `event` in: its own thread root, or itself.
pub fn thread_root(event: &OriginalSyncRoomMessageEvent) -> OwnedEventId {
    match &event.content.relates_to {
        Some(Relation::Thread(thread)) => thread.event_id.clone(),
        _ => event.event_id.clone(),
    }
}

/// Make `content` a thread reply quoting `reply_to` under `root`.
pub fn in_thread(
    mut content: RoomMessageEventContent,
    root: OwnedEventId,
    reply_to: OwnedEventId,
) -> RoomMessageEventContent {
    content.relates_to = Some(Relation::Thread(Thread::reply(root, reply_to)));
    content
}
