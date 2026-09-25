//! Managed Matrix verification and SAS handling.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use futures_util::{stream, StreamExt};
use matrix_sdk::{
    encryption::verification::{
        SasState, SasVerification, Verification, VerificationRequest, VerificationRequestState,
    },
    ruma::{
        events::{
            key::verification::{request::ToDeviceKeyVerificationRequestEvent, VerificationMethod},
            room::member::MembershipState,
            room::message::{MessageType, OriginalSyncRoomMessageEvent},
        },
        OwnedDeviceId, OwnedRoomId, OwnedUserId, UserId,
    },
    Client,
};
use tokio::{
    sync::{Mutex, Semaphore},
    time::{timeout, Instant},
};
use tracing::{error, info, warn};

use crate::config::{UserAllowList, VerificationConfig};

const CANCELLATION_TIMEOUT: Duration = Duration::from_secs(10);

/// How a verification request is transported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerificationTransport {
    ToDevice,
    InRoom(OwnedRoomId),
}

/// Trust state observed before a verification starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExistingTrust {
    Unverified,
    Verified,
    VerificationViolation,
}

/// SAS data passed to an approval policy. It must not be written to ordinary logs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SasChallenge {
    pub user_id: OwnedUserId,
    pub device_id: OwnedDeviceId,
    pub transport: VerificationTransport,
    pub existing_trust: ExistingTrust,
    pub emojis: Option<Vec<(String, String)>>,
    pub decimals: Option<(u16, u16, u16)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    Reject,
}

pub type ApprovalFuture = Pin<Box<dyn Future<Output = ApprovalDecision> + Send>>;
pub type ApprovalHandler = Arc<dyn Fn(SasChallenge) -> ApprovalFuture + Send + Sync + 'static>;

/// Runtime limits for verification handling.
#[derive(Clone, Debug)]
pub struct VerificationSettings {
    pub flow_timeout: Duration,
    pub grant_ttl: Duration,
    pub max_concurrent: usize,
    pub allow_users_from_joined_rooms: bool,
}

impl Default for VerificationSettings {
    fn default() -> Self {
        Self {
            flow_timeout: Duration::from_secs(300),
            grant_ttl: Duration::from_secs(600),
            max_concurrent: 8,
            allow_users_from_joined_rooms: false,
        }
    }
}

#[derive(Clone, Debug)]
struct VerificationGrant {
    device_id: Option<OwnedDeviceId>,
    expires_at: Instant,
}

impl VerificationGrant {
    fn matches(&self, device_id: Option<&OwnedDeviceId>, now: Instant) -> bool {
        if self.expires_at <= now {
            return false;
        }
        match (&self.device_id, device_id) {
            (None, _) => true,
            (Some(expected), Some(actual)) => expected == actual,
            (Some(_), None) => false,
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct FlowKey {
    user_id: OwnedUserId,
    flow_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reservation {
    Reserved,
    Duplicate,
    Concurrent,
}

struct VerificationInner {
    client: Client,
    allowed_users: HashSet<OwnedUserId>,
    settings: VerificationSettings,
    approval: ApprovalHandler,
    grants: Mutex<HashMap<OwnedUserId, Vec<VerificationGrant>>>,
    active: Mutex<HashSet<FlowKey>>,
    slots: Arc<Semaphore>,
}

/// Shared verification service used by Matrix bots.
#[derive(Clone)]
pub struct VerificationService {
    inner: Arc<VerificationInner>,
}

impl VerificationService {
    pub fn new(
        client: Client,
        allowed_users: HashSet<OwnedUserId>,
        settings: VerificationSettings,
        approval: ApprovalHandler,
    ) -> Self {
        let max_concurrent = settings.max_concurrent.max(1);
        Self {
            inner: Arc::new(VerificationInner {
                client,
                allowed_users,
                settings,
                approval,
                grants: Mutex::new(HashMap::new()),
                active: Mutex::new(HashSet::new()),
                slots: Arc::new(Semaphore::new(max_concurrent)),
            }),
        }
    }

    /// Construct an explicitly allowlisted trust-on-first-use policy.
    pub fn allowlisted_tofu(
        client: Client,
        allowed_users: HashSet<OwnedUserId>,
        settings: VerificationSettings,
    ) -> Self {
        Self::new(
            client,
            allowed_users,
            settings,
            Arc::new(|_| Box::pin(async { ApprovalDecision::Approve })),
        )
    }

    /// Construct the standard allowlisted TOFU policy from shared config.
    ///
    /// Allowed first-time users are `verification.allowed_users`, or — when
    /// that is empty — an explicit `allowed_inviters` list. Admins are always
    /// allowed so that they and the bot can verify each other.
    pub fn from_config(
        client: Client,
        config: &VerificationConfig,
        inviters: &UserAllowList,
        admins: &HashSet<OwnedUserId>,
    ) -> Self {
        let mut allowed_users =
            crate::config::parse_user_ids("verification.allowed_users", &config.allowed_users);
        if config.allowed_users.is_empty() {
            allowed_users.extend(inviters.entries().cloned());
        }
        allowed_users.extend(admins.iter().cloned());
        Self::allowlisted_tofu(
            client,
            allowed_users,
            VerificationSettings {
                flow_timeout: Duration::from_secs(config.flow_timeout_secs.max(1)),
                grant_ttl: Duration::from_secs(config.grant_ttl_secs.max(1)),
                max_concurrent: config.max_concurrent.max(1),
                allow_users_from_joined_rooms: config.allow_users_from_joined_rooms,
            },
        )
    }

    /// Users allowed a first-time (TOFU) verification.
    pub fn allowed_users(&self) -> &HashSet<OwnedUserId> {
        &self.inner.allowed_users
    }

    /// Register both standard verification request transports.
    ///
    /// Call this before the first sync so startup events are not missed.
    pub fn install_handlers(&self) {
        self.inner.client.add_event_handler({
            let service = self.clone();
            move |event: ToDeviceKeyVerificationRequestEvent, client: Client| {
                let service = service.clone();
                async move {
                    let Some(request) = client
                        .encryption()
                        .get_verification_request(&event.sender, &event.content.transaction_id)
                        .await
                    else {
                        warn!(user_id = %event.sender, "To-device verification request was not available in the crypto store");
                        return;
                    };
                    service.spawn(request, None);
                }
            }
        });

        self.inner.client.add_event_handler({
            let service = self.clone();
            move |event: OriginalSyncRoomMessageEvent, client: Client| {
                let service = service.clone();
                async move {
                    if !matches!(event.content.msgtype, MessageType::VerificationRequest(_))
                        || client.user_id() == Some(event.sender.as_ref())
                    {
                        // Requests the bot sent itself are already being
                        // driven by the task that sent them.
                        return;
                    }
                    let Some(request) = client
                        .encryption()
                        .get_verification_request(&event.sender, &event.event_id)
                        .await
                    else {
                        warn!(user_id = %event.sender, "In-room verification request was not available in the crypto store");
                        return;
                    };
                    service.spawn(request, None);
                }
            }
        });
    }

    fn spawn(&self, request: VerificationRequest, device_hint: Option<OwnedDeviceId>) {
        let service = self.clone();
        tokio::spawn(async move { service.handle_request(request, device_hint).await });
    }

    /// Grant one user a single re-verification attempt for a bounded period.
    pub async fn grant_user(&self, user_id: OwnedUserId) {
        self.insert_grant(user_id, None).await;
    }

    /// Grant one specific device a single verification attempt.
    pub async fn grant_device(&self, user_id: OwnedUserId, device_id: OwnedDeviceId) {
        self.insert_grant(user_id, Some(device_id)).await;
    }

    async fn insert_grant(&self, user_id: OwnedUserId, device_id: Option<OwnedDeviceId>) {
        let grant = VerificationGrant {
            device_id,
            expires_at: Instant::now() + self.inner.settings.grant_ttl,
        };
        self.inner
            .grants
            .lock()
            .await
            .entry(user_id)
            .or_default()
            .push(grant);
    }

    /// Start a verification directly with a known device using to-device events.
    pub async fn request_device_verification(
        &self,
        user_id: &UserId,
        device_id: &matrix_sdk::ruma::DeviceId,
    ) -> anyhow::Result<()> {
        if !self.inner.allowed_users.contains(user_id) && !self.has_grant(user_id, device_id).await
        {
            anyhow::bail!("user is not allowed to verify with this bot");
        }
        let Some(device) = self
            .inner
            .client
            .encryption()
            .get_device(user_id, device_id)
            .await?
        else {
            anyhow::bail!("device is not present in the bot's crypto store");
        };
        let request = device
            .request_verification_with_methods(vec![VerificationMethod::SasV1])
            .await?;
        self.spawn(request, Some(device_id.to_owned()));
        Ok(())
    }

    /// Ask `user_id` to verify with the bot (SAS in the direct chat shared
    /// with them). Used by admins to establish mutual verification.
    pub async fn request_user_verification(&self, user_id: &UserId) -> anyhow::Result<()> {
        let Some(identity) = self
            .inner
            .client
            .encryption()
            .get_user_identity(user_id)
            .await?
        else {
            anyhow::bail!("no cross-signing identity known for {user_id}; set up secure backup/cross-signing in your client first");
        };
        let request = identity
            .request_verification_with_methods(vec![VerificationMethod::SasV1])
            .await?;
        self.spawn(request, None);
        Ok(())
    }

    /// Human-readable trust state of `user_id` as seen by the bot.
    pub async fn trust_summary(&self, user_id: &UserId) -> &'static str {
        match self.existing_trust(user_id).await {
            ExistingTrust::Verified => "verified",
            ExistingTrust::Unverified => "not verified",
            ExistingTrust::VerificationViolation => "identity changed since verification",
        }
    }

    async fn handle_request(
        &self,
        request: VerificationRequest,
        device_hint: Option<OwnedDeviceId>,
    ) {
        let user_id = request.other_user_id().to_owned();
        if request.is_done() || request.is_cancelled() {
            info!(user_id = %user_id, "Ignoring verification request that is already terminal");
            return;
        }
        let flow_key = FlowKey {
            user_id: user_id.clone(),
            flow_id: request.flow_id().to_owned(),
        };

        let reservation = {
            let mut active = self.inner.active.lock().await;
            reserve_flow(&mut active, &flow_key)
        };
        match reservation {
            Reservation::Duplicate => {
                info!(user_id = %user_id, "Ignoring duplicate verification request event");
                return;
            }
            Reservation::Concurrent => {
                warn!(user_id = %user_id, "Rejecting concurrent verification request from the same user");
                cancel_request(&request).await;
                return;
            }
            Reservation::Reserved => {}
        }

        let permit = match Arc::clone(&self.inner.slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.inner.active.lock().await.remove(&flow_key);
                warn!(user_id = %user_id, "Rejecting verification because the global flow limit is reached");
                cancel_request(&request).await;
                return;
            }
        };

        let request_for_timeout = request.clone();
        let result = timeout(
            self.inner.settings.flow_timeout,
            self.run_flow(request, device_hint),
        )
        .await;
        if result.is_err() {
            warn!(user_id = %user_id, "Verification flow timed out and was cancelled");
            cancel_request(&request_for_timeout).await;
        }

        self.inner.active.lock().await.remove(&flow_key);
        drop(permit);
    }

    async fn run_flow(&self, request: VerificationRequest, device_hint: Option<OwnedDeviceId>) {
        let user_id = request.other_user_id().to_owned();
        let transport = request
            .room_id()
            .map(|room_id| VerificationTransport::InRoom(room_id.to_owned()))
            .unwrap_or(VerificationTransport::ToDevice);
        let device_id = request_device_id(&request).or(device_hint);
        let existing_trust = self.existing_trust(&user_id).await;

        let first_time_allowed = self.inner.allowed_users.contains(&user_id)
            || (existing_trust == ExistingTrust::Unverified
                && self.inner.settings.allow_users_from_joined_rooms
                && self.shares_joined_room(&user_id).await);
        let needs_grant = authorization_requires_grant(first_time_allowed, existing_trust);
        if needs_grant && !self.consume_grant(&user_id, device_id.as_ref()).await {
            warn!(
                user_id = %user_id,
                transport = ?transport,
                existing_trust = ?existing_trust,
                "Rejecting verification because no matching administrative grant exists"
            );
            cancel_request(&request).await;
            return;
        }

        if !request.we_started() {
            info!(user_id = %user_id, transport = ?transport, "Accepting verification request");
            if let Err(error) = request
                .accept_with_methods(vec![VerificationMethod::SasV1])
                .await
            {
                error!(user_id = %user_id, %error, "Failed to accept verification request");
                cancel_request(&request).await;
                return;
            }
        }

        let mut changes = request.changes();
        while let Some(state) = changes.next().await {
            match state {
                VerificationRequestState::Transitioned { verification } => {
                    match verification {
                        Verification::SasV1(sas) => {
                            self.handle_sas(sas, transport, existing_trust).await;
                        }
                        _ => {
                            warn!(user_id = %user_id, "Cancelling unsupported non-SAS verification method");
                            cancel_request(&request).await;
                        }
                    }
                    return;
                }
                VerificationRequestState::Ready { .. } if request.we_started() => {
                    // We asked for this verification; as the requester we
                    // start SAS once the other side is ready.
                    if let Err(error) = request.start_sas().await {
                        error!(user_id = %user_id, %error, "Failed to start SAS verification");
                        cancel_request(&request).await;
                        return;
                    }
                }
                VerificationRequestState::Done => return,
                VerificationRequestState::Cancelled(info) => {
                    info!(user_id = %user_id, reason = %info.reason(), "Verification request was cancelled");
                    return;
                }
                _ => {}
            }
        }
    }

    async fn handle_sas(
        &self,
        sas: SasVerification,
        transport: VerificationTransport,
        existing_trust: ExistingTrust,
    ) {
        let user_id = sas.other_device().user_id().to_owned();
        let device_id = sas.other_device().device_id().to_owned();

        if let Err(error) = sas.accept().await {
            error!(user_id = %user_id, device_id = %device_id, %error, "Failed to accept SAS verification");
            cancel_sas(&sas).await;
            return;
        }

        let mut confirmed = false;
        let mut changes = sas.changes();
        while let Some(state) = changes.next().await {
            match state {
                SasState::KeysExchanged { .. } if !confirmed => {
                    let emojis = sas.emoji().map(|values| {
                        values
                            .iter()
                            .map(|emoji| (emoji.symbol.to_owned(), emoji.description.to_owned()))
                            .collect()
                    });
                    let challenge = SasChallenge {
                        user_id: user_id.clone(),
                        device_id: device_id.clone(),
                        transport: transport.clone(),
                        existing_trust,
                        emojis,
                        decimals: sas.decimals(),
                    };

                    match (self.inner.approval)(challenge).await {
                        ApprovalDecision::Approve => {
                            if let Err(error) = sas.confirm().await {
                                error!(user_id = %user_id, device_id = %device_id, %error, "SAS confirmation failed");
                                cancel_sas(&sas).await;
                                return;
                            }
                            confirmed = true;
                        }
                        ApprovalDecision::Reject => {
                            warn!(user_id = %user_id, device_id = %device_id, "SAS approval policy rejected verification");
                            cancel_sas(&sas).await;
                            return;
                        }
                    }
                }
                SasState::Done { .. } => {
                    info!(user_id = %user_id, device_id = %device_id, "Verification completed");
                    return;
                }
                SasState::Cancelled(info) => {
                    info!(user_id = %user_id, device_id = %device_id, reason = %info.reason(), "SAS verification was cancelled");
                    return;
                }
                _ => {}
            }
        }
    }

    async fn existing_trust(&self, user_id: &UserId) -> ExistingTrust {
        match self
            .inner
            .client
            .encryption()
            .get_user_identity(user_id)
            .await
        {
            Ok(Some(identity)) if identity.has_verification_violation() => {
                ExistingTrust::VerificationViolation
            }
            Ok(Some(identity)) if identity.is_verified() => ExistingTrust::Verified,
            Ok(_) => ExistingTrust::Unverified,
            Err(error) => {
                warn!(user_id = %user_id, %error, "Could not query existing identity trust; requiring an administrative grant");
                ExistingTrust::VerificationViolation
            }
        }
    }

    async fn shares_joined_room(&self, user_id: &UserId) -> bool {
        let rooms = self.inner.client.joined_rooms();
        let room_count = rooms.len();
        let mut checks = stream::iter(rooms)
            .map(|room| async move {
                room.get_member(user_id).await.map(|member| {
                    member.is_some_and(|member| member.membership() == &MembershipState::Join)
                })
            })
            .buffer_unordered(4);
        let mut failed_lookups = 0usize;

        while let Some(result) = checks.next().await {
            match result {
                Ok(true) => {
                    info!(user_id = %user_id, "Allowing first-time verification for a user in a shared joined room");
                    return true;
                }
                Ok(false) => {}
                Err(_) => failed_lookups += 1,
            }
        }

        if failed_lookups > 0 {
            warn!(
                user_id = %user_id,
                failed_lookups,
                room_count,
                "Could not confirm shared-room membership in all joined rooms"
            );
        }
        false
    }

    async fn consume_grant(&self, user_id: &UserId, device_id: Option<&OwnedDeviceId>) -> bool {
        let now = Instant::now();
        let mut grants = self.inner.grants.lock().await;
        consume_matching_grant(&mut grants, user_id, device_id, now)
    }

    async fn has_grant(&self, user_id: &UserId, device_id: &matrix_sdk::ruma::DeviceId) -> bool {
        let now = Instant::now();
        let mut grants = self.inner.grants.lock().await;
        let Some(user_grants) = grants.get_mut(user_id) else {
            return false;
        };
        user_grants.retain(|grant| grant.expires_at > now);
        let owned_device_id = device_id.to_owned();
        user_grants
            .iter()
            .any(|grant| grant.matches(Some(&owned_device_id), now))
    }
}

fn authorization_requires_grant(allowed: bool, existing_trust: ExistingTrust) -> bool {
    !allowed || existing_trust != ExistingTrust::Unverified
}

fn reserve_flow(active: &mut HashSet<FlowKey>, flow: &FlowKey) -> Reservation {
    if active.contains(flow) {
        Reservation::Duplicate
    } else if active
        .iter()
        .any(|active_flow| active_flow.user_id == flow.user_id)
    {
        Reservation::Concurrent
    } else {
        active.insert(flow.clone());
        Reservation::Reserved
    }
}

fn consume_matching_grant(
    grants: &mut HashMap<OwnedUserId, Vec<VerificationGrant>>,
    user_id: &UserId,
    device_id: Option<&OwnedDeviceId>,
    now: Instant,
) -> bool {
    let Some(user_grants) = grants.get_mut(user_id) else {
        return false;
    };
    user_grants.retain(|grant| grant.expires_at > now);
    let Some(index) = user_grants
        .iter()
        .position(|grant| grant.matches(device_id, now))
    else {
        return false;
    };
    user_grants.remove(index);
    if user_grants.is_empty() {
        grants.remove(user_id);
    }
    true
}

fn request_device_id(request: &VerificationRequest) -> Option<OwnedDeviceId> {
    match request.state() {
        VerificationRequestState::Requested {
            other_device_data, ..
        }
        | VerificationRequestState::Ready {
            other_device_data, ..
        } => Some(other_device_data.device_id().to_owned()),
        VerificationRequestState::Transitioned {
            verification: Verification::SasV1(sas),
        } => Some(sas.other_device().device_id().to_owned()),
        VerificationRequestState::Transitioned { .. } => None,
        _ => None,
    }
}

async fn cancel_request(request: &VerificationRequest) {
    match timeout(CANCELLATION_TIMEOUT, request.cancel()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            warn!(user_id = %request.other_user_id(), %error, "Failed to send verification cancellation");
        }
        Err(_) => {
            warn!(user_id = %request.other_user_id(), "Timed out while sending verification cancellation");
        }
    }
}

async fn cancel_sas(sas: &SasVerification) {
    match timeout(CANCELLATION_TIMEOUT, sas.cancel()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            warn!(user_id = %sas.other_device().user_id(), device_id = %sas.other_device().device_id(), %error, "Failed to send SAS cancellation");
        }
        Err(_) => {
            warn!(user_id = %sas.other_device().user_id(), device_id = %sas.other_device().device_id(), "Timed out while sending SAS cancellation");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::{device_id, user_id};

    #[test]
    fn user_grant_is_single_use() {
        let user_id = user_id!("@alice:example.org");
        let now = Instant::now();
        let mut grants = HashMap::from([(
            user_id.to_owned(),
            vec![VerificationGrant {
                device_id: None,
                expires_at: now + Duration::from_secs(60),
            }],
        )]);
        assert!(consume_matching_grant(
            &mut grants,
            user_id,
            Some(&device_id!("DEVICE").to_owned()),
            now,
        ));
        assert!(!consume_matching_grant(
            &mut grants,
            user_id,
            Some(&device_id!("DEVICE").to_owned()),
            now,
        ));
    }

    #[tokio::test]
    async fn device_grant_only_matches_the_expected_device() {
        let grant = VerificationGrant {
            device_id: Some(device_id!("EXPECTED").to_owned()),
            expires_at: Instant::now() + Duration::from_secs(60),
        };
        assert!(grant.matches(Some(&device_id!("EXPECTED").to_owned()), Instant::now()));
        assert!(!grant.matches(Some(&device_id!("OTHER").to_owned()), Instant::now()));
        assert!(!grant.matches(None, Instant::now()));
    }

    #[tokio::test]
    async fn expired_grant_never_matches() {
        let grant = VerificationGrant {
            device_id: None,
            expires_at: Instant::now() - Duration::from_secs(1),
        };
        assert!(!grant.matches(None, Instant::now()));
    }

    #[test]
    fn flow_reservation_suppresses_duplicates_and_bounds_each_user() {
        let first = FlowKey {
            user_id: user_id!("@alice:example.org").to_owned(),
            flow_id: "first".to_owned(),
        };
        let same_user = FlowKey {
            user_id: first.user_id.clone(),
            flow_id: "second".to_owned(),
        };
        let other_user = FlowKey {
            user_id: user_id!("@bob:example.org").to_owned(),
            flow_id: "first".to_owned(),
        };
        let mut active = HashSet::new();

        assert_eq!(reserve_flow(&mut active, &first), Reservation::Reserved);
        assert_eq!(reserve_flow(&mut active, &first), Reservation::Duplicate);
        assert_eq!(
            reserve_flow(&mut active, &same_user),
            Reservation::Concurrent
        );
        assert_eq!(
            reserve_flow(&mut active, &other_user),
            Reservation::Reserved
        );
        assert_eq!(active.len(), 2);
    }

    #[test]
    fn only_allowlisted_first_contact_skips_a_grant() {
        assert!(!authorization_requires_grant(
            true,
            ExistingTrust::Unverified
        ));
        assert!(authorization_requires_grant(
            false,
            ExistingTrust::Unverified
        ));
        assert!(authorization_requires_grant(true, ExistingTrust::Verified));
        assert!(authorization_requires_grant(
            true,
            ExistingTrust::VerificationViolation
        ));
    }
}
