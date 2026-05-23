//! Matrix client construction and session restore.
//!
//! Replaces the ~30-line startup block that is copy-pasted verbatim into every
//! bot. The only variation across bots is where the sqlite store lives, and
//! that path is passed directly as `store_path`.
//!
//! Most bots pass their `store/` directory directly; radar-bot passes
//! `store/matrix_store` because it also keeps a SQLite items DB alongside it.

use std::path::Path;

use anyhow::{Context, Result};
use matrix_sdk::{
    Client, SessionMeta, SessionTokens,
    authentication::matrix::MatrixSession,
    ruma::{OwnedDeviceId, OwnedUserId},
};
use matrix_sdk_crypto::CollectStrategy;
use tracing::{info, warn};

use crate::config::MatrixConfig;

/// Build a Matrix client, restore the token session, optionally recover
/// cross-signing keys from secure backup, and bootstrap cross-signing.
///
/// `store_path` is passed directly to the sqlite store — pass exactly what you
/// previously passed to `.sqlite_store(...)`. The directory is created if it
/// does not exist. Returns the ready-to-use client and the resolved OwnedUserId.
///
/// # Usage
///
/// ```rust,no_run
/// // Most bots: store_path = PathBuf::from(env::var("STORE_PATH").unwrap_or("store"))
/// let (client, user_id) = mxbot_common::session::build_and_restore(
///     &config.matrix,
///     &store_path,
///     config.security.encryption_strategy.into(),
/// ).await?;
/// ```
pub async fn build_and_restore(
    cfg: &MatrixConfig,
    store_path: &Path,
    encryption: CollectStrategy,
) -> Result<(Client, OwnedUserId)> {
    tokio::fs::create_dir_all(store_path).await?;

    let client = Client::builder()
        .homeserver_url(&cfg.homeserver)
        .sqlite_store(store_path, None)
        .with_room_key_recipient_strategy(encryption)
        .build()
        .await
        .context("Failed to build Matrix client")?;

    let user_id: OwnedUserId   = cfg.user_id.parse().context("Invalid user_id in config")?;
    let device_id: OwnedDeviceId = cfg.device_id.as_str().into();

    client
        .restore_session(MatrixSession {
            meta:   SessionMeta { user_id: user_id.clone(), device_id },
            tokens: SessionTokens {
                access_token: cfg.access_token.clone(),
                refresh_token: None,
            },
        })
        .await
        .context("Failed to restore Matrix session")?;

    info!("Session restored as {user_id}");

    if let Some(ref key) = cfg.recovery_key {
        match client.encryption().recovery().recover(key).await {
            Ok(())  => info!("Cross-signing keys recovered from secure backup"),
            Err(e)  => warn!("Recovery failed (non-fatal — continuing without): {e}"),
        }
    }

    bootstrap_cross_signing(&client, &user_id).await;

    Ok((client, user_id))
}

/// Bootstrap cross-signing keys, skipping if keys are already present.
///
/// Skipping is important: on matrix.org the upload requires UIA (m.oauth)
/// which a headless bot cannot complete. Calling bootstrap when keys are
/// already present triggers a 401 on every startup for no benefit.
///
/// Called internally by `build_and_restore`. Exported so bots that need to
/// call it at a different point in startup can do so.
pub async fn bootstrap_cross_signing(client: &Client, user_id: &OwnedUserId) {
    if let Some(status) = client.encryption().cross_signing_status().await {
        if status.has_master && status.has_self_signing && status.has_user_signing {
            info!(
                user_id = %user_id,
                "Cross-signing already complete — skipping bootstrap"
            );
            return;
        }
        info!(
            user_id = %user_id,
            has_master         = status.has_master,
            has_self_signing   = status.has_self_signing,
            has_user_signing   = status.has_user_signing,
            "Cross-signing incomplete — attempting bootstrap"
        );
    }
    match client.encryption().bootstrap_cross_signing(None).await {
        Ok(())  => info!(user_id = %user_id, "Cross-signing bootstrapped"),
        Err(e)  => warn!(
            user_id = %user_id,
            error   = %e,
            "Cross-signing bootstrap failed (non-fatal)"
        ),
    }
}
