//! One-call startup for the parts every bot shares.

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use matrix_sdk::{
    ruma::{events::MessageLikeEventContent, OwnedRoomId, OwnedUserId, UserId},
    Client, Room,
};
use tracing::{info, warn};

use crate::{
    admin::AdminConsole,
    config::{MatrixConfig, SecurityConfig},
    rooms::{self, DynamicRooms, InvitePolicy},
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
    dynamic_rooms: DynamicRooms,
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

    /// Rooms the bot may be invited to besides `[security] allowed_rooms`,
    /// changeable while it runs (e.g. rooms admins authorize by command).
    pub fn dynamic_rooms(mut self, rooms: DynamicRooms) -> Self {
        self.dynamic_rooms = rooms;
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
        // Fail closed: a forgotten allow-list must not open the bot up.
        security.validate()?;
        sync::listen_for_stop();
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
        crate::trust::set(security.trust);
        if crate::trust::follows_new_identities() {
            crate::trust::watch(client.clone()).await;
        }

        let admins = Arc::new(security.admins()?);
        if admins.is_empty() {
            warn!("No admin_users configured. Admin commands are disabled");
        } else {
            info!(admins = ?admins, "Admin users configured");
        }
        info!(
            inviters = %security.allowed_inviters.describe(),
            rooms = %security.allowed_rooms.describe(),
            "Invite policy"
        );
        let dm_rooms = Arc::new(security.dm_rooms()?);
        if !dm_rooms.is_empty() {
            info!(
                rooms = dm_rooms.len(),
                "Members of dm_rooms may open private chats"
            );
        }

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
            dynamic_rooms: self.dynamic_rooms,
            admins: Arc::clone(&admins),
            dm_rooms: Arc::clone(&dm_rooms),
        };
        rooms::install_invite_handler(&client, invites.clone());

        let admin = AdminConsole::new(
            client.clone(),
            self.name.clone(),
            self.version,
            Arc::clone(&admins),
            security.admin_dm.clone(),
            security.trust,
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
            dynamic_rooms: DynamicRooms::default(),
        }
    }

    pub fn is_admin(&self, user_id: &UserId) -> bool {
        self.admin.is_admin(user_id)
    }

    pub fn admins(&self) -> &HashSet<OwnedUserId> {
        self.admin.admins()
    }

    /// First sync (retried until it succeeds), then pending invites. With
    /// `trust = "cross_signed"`, identities that changed meanwhile are
    /// accepted before anything is sent.
    pub async fn initial_sync(&self) {
        sync::initial_sync(&self.client).await;
        if crate::trust::follows_new_identities() {
            crate::trust::accept_changed_in_rooms(&self.client).await;
        }
        rooms::join_pending_invites(&self.client, &self.invites).await;
    }

    /// Sync until SIGTERM/SIGINT, then return so `main` can exit cleanly
    /// (instead of being killed by `docker stop` after its timeout).
    pub async fn run(&self) -> anyhow::Result<()> {
        self.run_with(|| {}).await
    }

    /// [`Bot::run`], calling `on_reconnect` whenever the sync loop restarts.
    ///
    /// On SIGTERM the batch of events being handled is finished (up to
    /// [`sync::STOP_GRACE`]), so no tap or message is half done and lost:
    /// the SDK has already moved its sync position past that batch. Idle,
    /// waiting for the server, there is nothing to finish and the wait
    /// simply runs out.
    pub async fn run_with(&self, on_reconnect: impl FnMut()) -> anyhow::Result<()> {
        let sync = sync::sync_until_stopped(&self.client, on_reconnect);
        tokio::pin!(sync);
        tokio::select! {
            () = &mut sync => return Ok(()),
            () = sync::stop_requested() => {}
        }
        if tokio::time::timeout(sync::STOP_GRACE, &mut sync)
            .await
            .is_ok()
        {
            info!("Stopped after the last sync batch");
        } else {
            info!("Stopped: no sync batch finished in time (idle, or a handler still busy)");
        }
        Ok(())
    }

    /// Whether `room` is `user`'s private chat with the bot and `user` is a
    /// member of a `[security] dm_rooms` room right now: both asked of the
    /// server, any error means no. For handlers that serve such chats.
    pub async fn is_member_dm(&self, room: &Room, user: &UserId) -> bool {
        for trusted in self.invites.dm_rooms.iter() {
            if crate::private::is_private_chat_of_member(
                &self.client,
                room.room_id(),
                trusted,
                user,
            )
            .await
            {
                return true;
            }
        }
        false
    }

    /// `content` into every room of [`Bot::broadcast_rooms`], each with retries of transient
    /// failures (429, 5xx, network: [`crate::send::send_with_retry`]), so a busy homeserver
    /// loses no post. Returns the rooms it still failed in; each is logged as a warning.
    pub async fn broadcast<C>(&self, content: C) -> Vec<(OwnedRoomId, matrix_sdk::Error)>
    where
        C: MessageLikeEventContent + Clone,
    {
        let mut failed = Vec::new();
        for room in self.broadcast_rooms() {
            if let Err(e) = crate::send::send_with_retry(&room, content.clone()).await {
                warn!(room = %room.room_id(), "Broadcast failed: {e}");
                failed.push((room.room_id().to_owned(), e));
            }
        }
        failed
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
