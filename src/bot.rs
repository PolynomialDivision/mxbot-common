//! One-call startup for the parts every bot shares.

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use matrix_sdk::{
    ruma::{OwnedUserId, UserId},
    Client, Room,
};
use tracing::{info, warn};

use crate::{
    admin::AdminConsole,
    config::{MatrixConfig, SecurityConfig},
    rooms::{self, InvitePolicy},
    session,
    settings::SettingsStore,
    sync,
    verify::VerificationService,
};

/// Options for [`Bot::start`].
pub struct BotBuilder {
    name: String,
    version: String,
    store_path: PathBuf,
    matrix_store_path: Option<PathBuf>,
    settings: Option<SettingsStore>,
    admin_help: Option<String>,
}

impl BotBuilder {
    /// Directory for the bot's own files; defaults to `$STORE_PATH` or `store`.
    pub fn store_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.store_path = path.into();
        self
    }

    /// Where the Matrix SDK keeps its sqlite stores; defaults to the store
    /// directory itself.
    pub fn matrix_store_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.matrix_store_path = Some(path.into());
        self
    }

    /// Runtime settings admins may change with `!admin set`.
    pub fn settings(mut self, settings: SettingsStore) -> Self {
        self.settings = Some(settings);
        self
    }

    /// Bot-specific admin commands, listed by `!admin`.
    pub fn admin_help(mut self, help: impl Into<String>) -> Self {
        self.admin_help = Some(help.into());
        self
    }

    /// Restore the Matrix session and install the shared handlers:
    /// verification, invite handling and the admin console.
    ///
    /// Call before the first sync so no startup events are missed.
    pub async fn start(self, matrix: &MatrixConfig, security: &SecurityConfig) -> Result<Bot> {
        let mut matrix = matrix.clone();
        matrix.apply_env_overrides();

        tokio::fs::create_dir_all(&self.store_path)
            .await
            .with_context(|| format!("Creating store directory {}", self.store_path.display()))?;
        let matrix_store = self
            .matrix_store_path
            .unwrap_or_else(|| self.store_path.clone());
        let (client, user_id) =
            session::build_and_restore(&matrix, &matrix_store, security.encryption_strategy.into())
                .await?;

        let admins = Arc::new(security.admins()?);
        if admins.is_empty() {
            warn!("No admin_users configured — admin commands are disabled");
        } else {
            info!(admins = ?admins, "Admin users configured");
        }
        info!(
            inviters = %security.allowed_inviters.describe(),
            rooms = %security.allowed_rooms.describe(),
            "Invite policy"
        );

        let verification = VerificationService::from_config(
            client.clone(),
            &security.verification,
            &security.allowed_inviters,
            &admins,
        );
        verification.install_handlers();
        if verification.allowed_users().is_empty() {
            warn!("No users may verify with the bot without an administrative grant");
        }

        let invites = InvitePolicy {
            inviters: security.allowed_inviters.clone(),
            rooms: security.allowed_rooms.clone(),
            admins: Arc::clone(&admins),
        };
        rooms::install_invite_handler(&client, invites.clone());

        let admin = AdminConsole::new(
            client.clone(),
            self.name.clone(),
            self.version,
            Arc::clone(&admins),
            security.admin_dm.clone(),
            verification.clone(),
            self.settings,
            self.admin_help,
        );

        Ok(Bot {
            client,
            user_id,
            verification,
            admin,
            invites,
            admin_dms_receive_broadcasts: security.admin_dm.receive_broadcasts,
        })
    }
}

/// A logged-in bot with the shared handlers installed. Cheap to clone.
#[derive(Clone)]
pub struct Bot {
    pub client: Client,
    pub user_id: OwnedUserId,
    pub verification: VerificationService,
    pub admin: AdminConsole,
    invites: InvitePolicy,
    admin_dms_receive_broadcasts: bool,
}

impl Bot {
    /// `name` and `version` are shown by `!admin status`.
    pub fn builder(name: impl Into<String>, version: impl Into<String>) -> BotBuilder {
        BotBuilder {
            name: name.into(),
            version: version.into(),
            store_path: crate::config::store_path_from_env(),
            matrix_store_path: None,
            settings: None,
            admin_help: None,
        }
    }

    pub fn is_admin(&self, user_id: &UserId) -> bool {
        self.admin.is_admin(user_id)
    }

    pub fn admins(&self) -> &HashSet<OwnedUserId> {
        self.admin.admins()
    }

    /// First sync (retried until it succeeds), then pending invites.
    pub async fn initial_sync(&self) {
        sync::initial_sync(&self.client).await;
        rooms::join_pending_invites(&self.client, &self.invites).await;
    }

    /// The long-running sync loop.
    pub async fn sync_forever(&self) -> ! {
        sync::sync_forever(&self.client).await
    }

    /// [`Bot::sync_forever`], calling `on_reconnect` whenever the loop restarts.
    pub async fn sync_forever_with(&self, on_reconnect: impl FnMut()) -> ! {
        sync::sync_forever_with(&self.client, on_reconnect).await
    }

    /// Joined rooms for broadcast posts: allowed rooms, without admin
    /// direct chats (unless `[security.admin_dm] receive_broadcasts`).
    pub fn broadcast_rooms(&self) -> Vec<Room> {
        rooms::broadcast_rooms(
            &self.client,
            &self.invites.rooms,
            self.admins(),
            self.admin_dms_receive_broadcasts,
        )
    }
}
