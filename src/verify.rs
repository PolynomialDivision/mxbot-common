//! Matrix verification and SAS handling.
//!
//! This is the most valuable module to share: 70+ lines of stateful
//! verification logic that is identical across all bots and never needs
//! customisation.
//!
//! ## What is shared
//!
//! - `handle_verification_request` — the outer state machine. Takes
//!   `reset_allowed` directly instead of the full BotState, because that is
//!   the only field it uses. The type is the same (`Arc<Mutex<HashSet<OwnedUserId>>>`)
//!   in every bot.
//! - `is_join_terminal` — four-line helper used in every bot's invite handler.
//!
//! ## What is NOT shared
//!
//! The event handler registration (`client.add_event_handler(...)`) is not
//! here. It requires BotState, which is bot-specific. Bots register their own
//! handlers and call `handle_verification_request` from them:
//!
//! ```rust,no_run
//! client.add_event_handler({
//!     let state = bot_state.clone();
//!     move |ev: ToDeviceKeyVerificationRequestEvent, client: Client| {
//!         let state = state.clone();
//!         async move {
//!             let Some(request) = client.encryption()
//!                 .get_verification_request(&ev.sender, &ev.content.transaction_id)
//!                 .await
//!             else { return };
//!             tokio::spawn(bot_core_lite::verify::handle_verification_request(
//!                 client,
//!                 Arc::clone(&state.reset_allowed),
//!                 request,
//!             ));
//!         }
//!     }
//! });
//! ```

use std::collections::HashSet;
use std::sync::Arc;

use futures_util::StreamExt;
use matrix_sdk::{
    Client,
    encryption::verification::{
        SasState, Verification, VerificationRequest, VerificationRequestState,
    },
    ruma::OwnedUserId,
};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// Handle an incoming verification request.
///
/// Accepts the request unless the requesting user already has a verified
/// device and has not been explicitly unlocked via `reset_allowed`.
///
/// This function is designed to be spawned:
/// ```rust,no_run
/// tokio::spawn(bot_core_lite::verify::handle_verification_request(
///     client, Arc::clone(&state.reset_allowed), request,
/// ));
/// ```
///
/// `reset_allowed` is populated by the !reset-trust admin command in each bot.
pub async fn handle_verification_request(
    client: Client,
    reset_allowed: Arc<Mutex<HashSet<OwnedUserId>>>,
    request: VerificationRequest,
) {
    let user_id = request.other_user_id();

    // Reject re-verification unless an admin explicitly reset this user's trust.
    // This prevents a compromised device from silently replacing verified keys.
    let already_verified = client
        .encryption()
        .get_user_devices(user_id)
        .await
        .map(|devices| devices.devices().any(|d| d.is_verified()))
        .unwrap_or(false);

    if already_verified {
        let allowed = reset_allowed.lock().await.remove(user_id);
        if !allowed {
            warn!(
                "Rejecting verification from {user_id} — already has a verified device"
            );
            request.cancel().await.ok();
            return;
        }
        info!("Allowing re-verification for {user_id} (trust was reset by admin)");
    }

    info!("Accepting verification from {user_id}");
    if let Err(e) = request.accept().await {
        error!("Failed to accept verification request: {e}");
        return;
    }

    let mut stream = request.changes();
    while let Some(state) = stream.next().await {
        match state {
            VerificationRequestState::Transitioned { verification } => {
                if let Verification::SasV1(sas) = verification {
                    tokio::spawn(handle_sas(sas));
                    break;
                }
            }
            VerificationRequestState::Done | VerificationRequestState::Cancelled(_) => break,
            _ => {}
        }
    }
}

async fn handle_sas(sas: matrix_sdk::encryption::verification::SasVerification) {
    info!(
        "SAS with {} {}",
        sas.other_device().user_id(),
        sas.other_device().device_id()
    );

    if let Err(e) = sas.accept().await {
        error!("Failed to accept SAS: {e}");
        return;
    }

    let mut stream = sas.changes();
    while let Some(state) = stream.next().await {
        match state {
            SasState::KeysExchanged { .. } => {
                info!("Auto-confirming SAS emojis");
                if let Err(e) = sas.confirm().await {
                    error!("SAS confirm failed: {e}");
                    break;
                }
            }
            SasState::Done { .. } => {
                info!(
                    "Verification done: {} {}",
                    sas.other_device().user_id(),
                    sas.other_device().device_id()
                );
                break;
            }
            SasState::Cancelled(info) => {
                warn!("SAS cancelled: {}", info.reason());
                break;
            }
            _ => {}
        }
    }
}

/// Returns true for Matrix join errors that will not resolve with a retry.
///
/// Used in every bot's invite handler to decide whether to give up immediately
/// or retry with backoff. Identical across all bots.
pub fn is_join_terminal(e: &matrix_sdk::Error) -> bool {
    let s = e.to_string();
    s.contains("No known servers")
        || s.contains("M_FORBIDDEN")
        || s.contains("M_UNKNOWN_TOKEN")
        || s.contains("M_GUEST_ACCESS_FORBIDDEN")
}
