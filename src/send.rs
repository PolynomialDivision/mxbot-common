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

/// Classify a failed request as retryable (5xx, 429 / `M_LIMIT_EXCEEDED`,
/// network), with any server-suggested delay (`retry_after`). Also for
/// requests other than sends: creating a room, inviting.
pub fn classify_error(error: &matrix_sdk::Error) -> (bool, Option<Duration>) {
    classify_send_error(error)
}

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
    send_with_retry_txn(room, content, &TransactionId::new()).await
}

/// [`send_with_retry`] with a transaction ID chosen (and stored) by the
/// caller: a bot that records the ID before sending can retry the same
/// logical message after a restart without posting it twice, as long as
/// the homeserver still remembers the ID. Never reuse an ID for a
/// different message.
pub async fn send_with_retry_txn<C>(
    room: &Room,
    content: C,
    txn_id: &TransactionId,
) -> Result<OwnedEventId, matrix_sdk::Error>
where
    C: MessageLikeEventContent + Clone,
{
    retry_send(room, || async {
        room.send(content.clone())
            .with_transaction_id(txn_id.to_owned())
            .await
            .map(|response| response.response.event_id)
    })
    .await
}

/// [`send_with_retry_txn`] for raw JSON content, e.g. a message that
/// carries an extra field of the bot's own.
pub async fn send_raw_with_retry_txn(
    room: &Room,
    event_type: &str,
    content: serde_json::Value,
    txn_id: &TransactionId,
) -> Result<OwnedEventId, matrix_sdk::Error> {
    retry_send(room, || async {
        room.send_raw(event_type, content.clone())
            .with_transaction_id(txn_id)
            .await
            .map(|response| response.response.event_id)
    })
    .await
}

/// Run one send attempt after another until it succeeds, fails
/// permanently, or [`MAX_ATTEMPTS`] are used up.
async fn retry_send<F, Fut>(
    room: &Room,
    mut attempt_send: F,
) -> Result<OwnedEventId, matrix_sdk::Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<OwnedEventId, matrix_sdk::Error>>,
{
    let mut backoff = Backoff::new(INITIAL_BACKOFF, MAX_BACKOFF);
    let mut attempt = 1;
    let mut accepted = false;
    loop {
        match attempt_send().await {
            Ok(event_id) => return Ok(event_id),
            Err(error) => {
                // `trust = "cross_signed"`: someone's identity changed and the
                // watcher hasn't caught up yet. Accept it, then once more.
                if let Some(users) = crate::trust::changed_identities(&error) {
                    if !accepted
                        && crate::trust::follows_new_identities()
                        && crate::trust::accept_new_identities(&room.client(), users).await
                    {
                        accepted = true;
                        continue;
                    }
                }
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
