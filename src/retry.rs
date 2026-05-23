//! Generic exponential-backoff retry.
//!
//! The join-with-retry loop appears verbatim in every bot's invite handler
//! (MAX_ATTEMPTS=8, initial delay 2s, doubling to 300s cap). This module
//! provides the underlying mechanism so bots can express that loop more
//! concisely and reuse it for other retryable operations.
//!
//! Note: the join loop also checks `is_join_terminal` to bail out early on
//! permanent errors. That check is NOT baked into this function — pass it via
//! the closure return value (map terminal errors to a distinct type, or check
//! inside the closure and return a non-retryable variant).

use std::{future::Future, time::Duration};

use tokio::time::sleep;
use tracing::warn;

/// Retry `f` up to `max_attempts` times with exponential backoff.
///
/// - Starts at `initial_delay_secs`, doubles each attempt, caps at 300s.
/// - On success returns `Ok(T)` immediately.
/// - After all attempts returns the last `Err(E)`.
///
/// The closure receives no arguments. Clone any needed state before calling.
///
/// # Example — join with retry (replacing the inline loop in each bot)
///
/// ```rust,no_run
/// use bot_core_lite::retry::retry_with_backoff;
/// use bot_core_lite::verify::is_join_terminal;
///
/// let result = retry_with_backoff(8, 2, &format!("join {room_id}"), || {
///     let client = client.clone();
///     let alias  = room_or_alias.clone();
///     let via    = via.clone();
///     async move {
///         let r = client.join_room_by_id_or_alias(&alias, &via).await;
///         // Turn terminal errors into an immediately-returned Err by mapping them
///         // to a type that signals "do not retry" — or just check inside and abort.
///         r
///     }
/// }).await;
/// ```
pub async fn retry_with_backoff<F, Fut, T, E>(
    max_attempts: u32,
    initial_delay_secs: u64,
    label: &str,
    mut f: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut delay = initial_delay_secs;
    for attempt in 1..=max_attempts {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt == max_attempts => {
                warn!("{label}: failed after {max_attempts} attempts: {e}");
                return Err(e);
            }
            Err(e) => {
                warn!("{label}: attempt {attempt}/{max_attempts} failed: {e}; retry in {delay}s");
                sleep(Duration::from_secs(delay)).await;
                delay = (delay * 2).min(300);
            }
        }
    }
    unreachable!()
}
