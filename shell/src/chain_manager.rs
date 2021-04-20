// Copyright (c) SimpleStaking and Tezedge Contributors
// SPDX-License-Identifier: MIT

//! Manages chain synchronisation process.
//! - tries to download most recent header from the other peers
//! - also supplies downloaded data to other peers
//!
//! Also responsible for:
//! -- managing attribute current head
//! -- start test chain (if needed)
//! -- validate blocks with protocol
//! -- ...

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use failure::{format_err, Error};
use riker::actors::*;
use slog::{debug, info, trace, warn, Logger};

use crypto::hash::{BlockHash, ChainId, CryptoboxPublicKeyHash, OperationHash};
use crypto::seeded_step::Seed;
use networking::p2p::network_channel::{NetworkChannelMsg, NetworkChannelRef, NetworkChannelTopic};
use storage::mempool_storage::MempoolOperationType;
use storage::PersistentStorage;
use storage::{
    BlockHeaderWithHash, BlockMetaStorage, BlockMetaStorageReader, BlockStorage,
    BlockStorageReader, MempoolStorage, OperationsStorage, OperationsStorageReader, StorageError,
    StorageInitInfo,
};
use tezos_identity::Identity;
use tezos_messages::p2p::binary_message::MessageHash;
use tezos_messages::p2p::encoding::block_header::Level;
use tezos_messages::p2p::encoding::prelude::*;
use tezos_messages::Head;
use tezos_wrapper::TezosApiConnectionPool;

use crate::chain_feeder::ChainFeederRef;
use crate::mempool::mempool_channel::{
    MempoolChannelRef, MempoolChannelTopic, MempoolOperationReceived,
};
use crate::mempool::mempool_state::MempoolState;
use crate::mempool::CurrentMempoolStateStorageRef;
use crate::shell_channel::{
    AllBlockOperationsReceived, BlockReceived, InjectBlock, ShellChannelMsg, ShellChannelRef,
    ShellChannelTopic,
};
use crate::state::chain_state::{BlockAcceptanceResult, BlockchainState};
use crate::state::head_state::CurrentHeadRef;
use crate::state::peer_state::{tell_peer, PeerState};
use crate::state::synchronization_state::{
    PeerBranchSynchronizationDone, SynchronizationBootstrapStateRef,
};
use crate::state::StateError;
use crate::stats::apply_block_stats::ApplyBlockStatsRef;
use crate::subscription::*;
use crate::utils::{dispatch_condvar_result, CondvarResult};
use crate::validation;

/// How often to ask all connected peers for current head
const ASK_CURRENT_HEAD_INTERVAL: Duration = Duration::from_secs(90);
/// Initial delay to ask the peers for current head
const ASK_CURRENT_HEAD_INITIAL_DELAY: Duration = Duration::from_secs(15);
/// How often to print stats in logs
const LOG_INTERVAL: Duration = Duration::from_secs(60);

/// After this time we will disconnect peer if his current head level stays the same
const CURRENT_HEAD_LEVEL_UPDATE_TIMEOUT: Duration = Duration::from_secs(60 * 2);
/// After this time peer will be disconnected if it fails to respond to our request
const SILENT_PEER_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum timeout duration in sandbox mode (do not disconnect peers in sandbox mode)
const SILENT_PEER_TIMEOUT_SANDBOX: Duration = Duration::from_secs(31_536_000);

/// Message commands [`ChainManager`] to disconnect stalled peers.
#[derive(Clone, Debug)]
pub struct DisconnectStalledPeers {
    silent_peer_timeout: Duration,
}

/// Message commands [`ChainManager`] to check if all mempool operations were fetched from peer.
#[derive(Clone, Debug)]
pub struct CheckMempoolCompleteness;

/// Message commands [`ChainManager`] to ask all connected peers for their current head.
#[derive(Clone, Debug)]
pub struct AskPeersAboutCurrentHead;

/// Message commands [`ChainManager`] to log its internal stats.
#[derive(Clone, Debug)]
pub struct LogStats;

/// This struct holds info about local and remote "current" head
#[derive(Clone, Debug)]
struct CurrentHead {
    /// Represents local current head. Value here is the same as the
    /// hash of the last applied block.
    local: CurrentHeadRef,
    /// Remote current head. This represents info about
    /// the current branch with the highest level received from network.
    remote: CurrentHeadRef,
}

impl CurrentHead {
    fn need_update_remote_level(&self, new_remote_level: i32) -> Result<bool, StateError> {
        match &self.remote.read()?.as_ref() {
            None => Ok(true),
            Some(current_remote_head) => Ok(new_remote_level > *current_remote_head.level()),
        }
    }

    fn update_remote_head(&mut self, block_header: &BlockHeaderWithHash) -> Result<(), StateError> {
        // TODO: maybe fitness check?
        if self.need_update_remote_level(block_header.header.level())? {
            let mut remote = self.remote.write()?;
            *remote = Some(Head::new(
                block_header.hash.clone(),
                block_header.header.level(),
                block_header.header.fitness().to_vec(),
            ));
        }
        Ok(())
    }

    fn local_debug_info(&self) -> Result<(String, i32, String), StateError> {
        match &self.local.read()?.as_ref() {
            None => Ok(("-none-".to_string(), 0_i32, "-none-".to_string())),
            Some(head) => Ok(head.to_debug_info()),
        }
    }

    fn remote_debug_info(&self) -> Result<(String, i32, String), StateError> {
        match &self.remote.read()?.as_ref() {
            None => Ok(("-none-".to_string(), 0_i32, "-none-".to_string())),
            Some(head) => Ok(head.to_debug_info()),
        }
    }

    fn has_any_higher_than(&self, level_to_check: Level) -> Result<bool, StateError> {
        // check remote head
        // TODO: maybe fitness check?
        if let Some(remote_head) = self.remote.read()?.as_ref() {
            if remote_head.level() > &level_to_check {
                return Ok(true);
            }
        }

        // check local head
        // TODO: maybe fitness check?
        if let Some(local_head) = self.local.read()?.as_ref() {
            if local_head.level() > &level_to_check {
                return Ok(true);
            }
        }

        Ok(false)
    }
}

/// Holds various stats with info about internal synchronization.
struct Stats {
    /// Count of received blocks
    unseen_block_count: usize,
    /// Last time when previously not seen block was received
    unseen_block_last: Instant,
    /// Last time when previously unseen operations were received
    unseen_block_operations_last: Instant,

    /// Shared statistics for applying blocks
    apply_block_stats: ApplyBlockStatsRef,
}

/// Purpose of this actor is to perform chain synchronization.
#[actor(
    DisconnectStalledPeers,
    CheckMempoolCompleteness,
    AskPeersAboutCurrentHead,
    LogStats,
    NetworkChannelMsg,
    ShellChannelMsg,
    SystemEvent
)]
pub struct ChainManager {
    /// All events generated by the network layer will end up in this channel
    network_channel: NetworkChannelRef,
    /// All events from shell will be published to this channel
    shell_channel: ShellChannelRef,
    /// Mempool channel
    mempool_channel: MempoolChannelRef,

    /// Block storage
    block_storage: Box<dyn BlockStorageReader>,
    /// Block meta storage
    block_meta_storage: Box<dyn BlockMetaStorageReader>,
    /// Operations storage
    operations_storage: Box<dyn OperationsStorageReader>,
    /// Mempool operation storage
    mempool_storage: MempoolStorage,
    /// Holds state of the blockchain
    chain_state: BlockchainState,

    /// Node's identity public key - e.g. used for history computation
    identity_peer_id: CryptoboxPublicKeyHash,

    /// Holds the state of all peers
    peers: HashMap<ActorUri, PeerState>,
    /// Current head information
    current_head: CurrentHead,
    /// Internal stats
    stats: Stats,

    /// Holds ref to global current shared mempool state
    current_mempool_state: CurrentMempoolStateStorageRef,
    /// Holds bootstrapped state
    current_bootstrap_state: SynchronizationBootstrapStateRef,

    /// Indicates if mempool is disabled to propagate to p2p
    p2p_disable_mempool: bool,
    /// Indicates that system is shutting down
    shutting_down: bool,
    /// Indicates node mode
    is_sandbox: bool,

    /// Protocol runner pool dedicated to prevalidation
    tezos_readonly_prevalidation_api: Arc<TezosApiConnectionPool>,
}

/// Reference to [chain manager](ChainManager) actor.
pub type ChainManagerRef = ActorRef<ChainManagerMsg>;

impl ChainManager {
    /// Create new actor instance.
    pub fn actor(
        sys: &ActorSystem,
        block_applier: ChainFeederRef,
        network_channel: NetworkChannelRef,
        shell_channel: ShellChannelRef,
        mempool_channel: MempoolChannelRef,
        persistent_storage: PersistentStorage,
        tezos_readonly_prevalidation_api: Arc<TezosApiConnectionPool>,
        init_storage_data: StorageInitInfo,
        is_sandbox: bool,
        local_current_head_state: CurrentHeadRef,
        remote_current_head_state: CurrentHeadRef,
        current_mempool_state: CurrentMempoolStateStorageRef,
        current_bootstrap_state: SynchronizationBootstrapStateRef,
        apply_block_stats: ApplyBlockStatsRef,
        p2p_disable_mempool: bool,
        identity: Arc<Identity>,
    ) -> Result<ChainManagerRef, CreateError> {
        sys.actor_of_props::<ChainManager>(
            ChainManager::name(),
            Props::new_args((
                block_applier,
                network_channel,
                shell_channel,
                mempool_channel,
                persistent_storage,
                tezos_readonly_prevalidation_api,
                init_storage_data,
                is_sandbox,
                local_current_head_state,
                remote_current_head_state,
                current_mempool_state,
                current_bootstrap_state,
                apply_block_stats,
                p2p_disable_mempool,
                identity.peer_id(),
            )),
        )
    }

    /// The `ChainManager` is intended to serve as a singleton actor so that's why
    /// we won't support multiple names per instance.
    fn name() -> &'static str {
        "chain-manager"
    }

    fn check_mempool_completeness(&mut self, _ctx: &Context<ChainManagerMsg>) {
        let ChainManager { peers, .. } = self;

        // check for missing mempool operations
        PeerState::schedule_missing_operations_for_mempool(peers);
    }

    fn process_network_channel_message(
        &mut self,
        ctx: &Context<ChainManagerMsg>,
        msg: NetworkChannelMsg,
    ) -> Result<(), Error> {
        let ChainManager {
            peers,
            chain_state,
            shell_channel,
            mempool_channel,
            network_channel,
            block_storage,
            block_meta_storage,
            operations_storage,
            stats,
            mempool_storage,
            current_head,
            identity_peer_id,
            ..
        } = self;

        match msg {
            NetworkChannelMsg::PeerBootstrapped(peer_id, peer_metadata, _) => {
                let peer =
                    PeerState::new(peer_id, &peer_metadata, chain_state.data_queues_limits());
                // store peer
                let actor_uri = peer.peer_id.peer_ref.uri().clone();
                self.peers.insert(actor_uri.clone(), peer);
                // retrieve mutable reference and use it as `tell_peer()` parameter
                if let Some(peer) = self.peers.get_mut(&actor_uri) {
                    tell_peer(
                        GetCurrentBranchMessage::new(chain_state.get_chain_id().as_ref().clone())
                            .into(),
                        peer,
                    );
                }
            }
            NetworkChannelMsg::PeerStalled(actor_uri) => {
                if let Some(peer_state) = self.peers.remove(&actor_uri) {
                    if let Some(peer_branch_bootstrapper) = peer_state.peer_branch_bootstrapper {
                        ctx.system.stop(peer_branch_bootstrapper);
                    }
                }
            }
            NetworkChannelMsg::PeerMessageReceived(received) => {
                match peers.get_mut(received.peer.uri()) {
                    Some(peer) => {
                        let log = ctx.system.log().new(
                            slog::o!("peer_id" => peer.peer_id.as_ref().peer_id_marker.clone(), "peer_ip" => peer.peer_id.as_ref().peer_address.to_string(), "peer" => peer.peer_id.as_ref().peer_ref.name().to_string(), "peer_uri" => peer.peer_id.as_ref().peer_ref.uri().to_string()),
                        );

                        match received.message.message() {
                            PeerMessage::CurrentBranch(message) => {
                                peer.update_current_head_level(
                                    message.current_branch().current_head().level(),
                                );

                                // at first, check if we can accept branch or just ignore it
                                if !chain_state.can_accept_branch(&message, &current_head.local)? {
                                    let head = message.current_branch().current_head();
                                    debug!(log, "Ignoring received (low) current branch";
                                                    "branch" => head.message_typed_hash::<BlockHash>()?.to_base58_check(),
                                                    "level" => head.level());
                                } else {
                                    let message_current_head = BlockHeaderWithHash::new(
                                        message.current_branch().current_head().clone(),
                                    )?;

                                    // update remote heads
                                    peer.update_current_head(&message_current_head);
                                    if let Err(e) =
                                        current_head.update_remote_head(&message_current_head)
                                    {
                                        warn!(log, "Failed to update remote head (by current branch)"; "reason" => e);
                                    }

                                    // schedule to download missing branch blocks
                                    chain_state.schedule_history_bootstrap(
                                        &ctx.system,
                                        peer,
                                        &message_current_head,
                                        message.current_branch().history().to_vec(),
                                    )?;
                                }
                            }
                            PeerMessage::GetCurrentBranch(message) => {
                                if chain_state.get_chain_id().as_ref() == &message.chain_id {
                                    if let Some(current_head_local) = current_head
                                        .local
                                        .read()
                                        .map_err(StateError::from)?
                                        .as_ref()
                                    {
                                        if let Some(current_head) =
                                            block_storage.get(current_head_local.block_hash())?
                                        {
                                            // calculate history
                                            let history = chain_state.get_history(
                                                &current_head.hash,
                                                &Seed::new(
                                                    &identity_peer_id,
                                                    &peer.peer_id.peer_public_key_hash,
                                                ),
                                            )?;
                                            // send message
                                            let msg = CurrentBranchMessage::new(
                                                chain_state.get_chain_id().as_ref().clone(),
                                                CurrentBranch::new(
                                                    (*current_head.header).clone(),
                                                    history,
                                                ),
                                            );
                                            tell_peer(msg.into(), peer);
                                        }
                                    }
                                } else {
                                    warn!(log, "Peer is requesting current branch from unsupported chain_id"; "chain_id" => chain_state.get_chain_id().to_base58_check());
                                }
                            }
                            PeerMessage::BlockHeader(message) => {
                                let block_header_with_hash =
                                    BlockHeaderWithHash::new(message.block_header().clone())?;

                                // check, if we requested data from this peer
                                if let Some(requested_data) =
                                    chain_state.requester().block_header_received(
                                        &block_header_with_hash.hash,
                                        peer,
                                        &log,
                                    )?
                                {
                                    // now handle received header
                                    Self::process_downloaded_header(
                                        block_header_with_hash,
                                        peer,
                                        stats,
                                        chain_state,
                                        shell_channel,
                                        &log,
                                    )?;

                                    // explicit drop (not needed)
                                    drop(requested_data);
                                }
                            }
                            PeerMessage::GetBlockHeaders(message) => {
                                for block_hash in message.get_block_headers() {
                                    if let Some(block) = block_storage.get(block_hash)? {
                                        let msg: BlockHeaderMessage =
                                            (*block.header).clone().into();
                                        tell_peer(msg.into(), peer);
                                    }
                                }
                            }
                            PeerMessage::GetCurrentHead(message) => {
                                if chain_state.get_chain_id().as_ref() == message.chain_id() {
                                    if let Some(current_head_local) = current_head
                                        .local
                                        .read()
                                        .map_err(StateError::from)?
                                        .as_ref()
                                    {
                                        if let Some(current_head) =
                                            block_storage.get(current_head_local.block_hash())?
                                        {
                                            let msg = CurrentHeadMessage::new(
                                                chain_state.get_chain_id().as_ref().clone(),
                                                current_head.header.as_ref().clone(),
                                                Self::resolve_mempool_to_send_to_peer(
                                                    &peer,
                                                    self.p2p_disable_mempool,
                                                    self.current_mempool_state.clone(),
                                                    &current_head_local,
                                                )?,
                                            );
                                            tell_peer(msg.into(), peer);
                                        }
                                    }
                                }
                            }
                            PeerMessage::OperationsForBlocks(operations) => {
                                if let Some(requested_data) =
                                    chain_state.requester().block_operations_received(
                                        operations.operations_for_block(),
                                        peer,
                                        &log,
                                    )?
                                {
                                    // update stats
                                    stats.unseen_block_operations_last = Instant::now();

                                    // update operations state
                                    let block_hash = operations.operations_for_block().hash();
                                    if chain_state.process_block_operations_from_peer(
                                        peer,
                                        &block_hash,
                                        &operations,
                                    )? {
                                        // TODO: TE-369 - is this necessery?
                                        // notify others that new all operations for block were received
                                        let block_meta = block_meta_storage
                                            .get(&block_hash)?
                                            .ok_or(StorageError::MissingKey)?;

                                        // notify others that new all operations for block were received
                                        shell_channel.tell(
                                            Publish {
                                                msg: AllBlockOperationsReceived {
                                                    hash: block_hash.clone(),
                                                    level: block_meta.level(),
                                                }
                                                .into(),
                                                topic: ShellChannelTopic::ShellEvents.into(),
                                            },
                                            None,
                                        );
                                    }

                                    // explicit drop (not needed)
                                    drop(requested_data)
                                }
                            }
                            PeerMessage::GetOperationsForBlocks(message) => {
                                for get_op in message.get_operations_for_blocks() {
                                    if get_op.validation_pass() < 0 {
                                        continue;
                                    }

                                    let key = get_op.into();
                                    if let Some(op) = operations_storage.get(&key)? {
                                        tell_peer(op.into(), peer);
                                    }
                                }
                            }
                            PeerMessage::CurrentHead(message) => {
                                peer.current_head_response_last = Instant::now();
                                peer.update_current_head_level(
                                    message.current_block_header().level(),
                                );

                                // process current head only if we are bootstrapped
                                if self
                                    .current_bootstrap_state
                                    .read()
                                    .map_err(StateError::from)?
                                    .is_bootstrapped()
                                {
                                    // check if we can accept head
                                    match chain_state.can_accept_head(
                                        &message,
                                        &current_head.local,
                                        &self.tezos_readonly_prevalidation_api.pool.get()?.api,
                                    )? {
                                        BlockAcceptanceResult::AcceptBlock => {
                                            let message_current_head = BlockHeaderWithHash::new(
                                                message.current_block_header().clone(),
                                            )?;

                                            // update remote heads
                                            peer.update_current_head(&message_current_head);
                                            if let Err(e) = current_head
                                                .update_remote_head(&message_current_head)
                                            {
                                                warn!(log, "Failed to update remote head (by current head)"; "reason" => e);
                                            }

                                            // process downloaded block directly
                                            Self::process_downloaded_header(
                                                message_current_head.clone(),
                                                peer,
                                                stats,
                                                chain_state,
                                                shell_channel,
                                                &log,
                                            )?;

                                            // here we accept head, which also means that we know predecessor
                                            // so we can schedule to download diff (last_applied_block .. current_head)
                                            let mut history = Vec::with_capacity(1);
                                            if let Some(cur) = &current_head
                                                .local
                                                .read()
                                                .map_err(StateError::from)?
                                                .as_ref()
                                            {
                                                history.push(cur.block_hash().clone());
                                            }

                                            // this schedule, ensure to download all operations from this peer (if not already)
                                            chain_state.schedule_history_bootstrap(
                                                &ctx.system,
                                                peer,
                                                &message_current_head,
                                                history,
                                            )?;

                                            // schedule mempool download
                                            let peer_current_mempool = message.current_mempool();

                                            // all operations (known_valid + pending) should be added to pending and validated afterwards
                                            // enqueue mempool operations for retrieval
                                            peer_current_mempool
                                                .known_valid()
                                                .iter()
                                                .cloned()
                                                .for_each(|operation_hash| {
                                                    peer.missing_mempool_operations.push((
                                                        operation_hash,
                                                        MempoolOperationType::Pending,
                                                    ));
                                                });
                                            peer_current_mempool
                                                .pending()
                                                .iter()
                                                .cloned()
                                                .for_each(|operation_hash| {
                                                    peer.missing_mempool_operations.push((
                                                        operation_hash,
                                                        MempoolOperationType::Pending,
                                                    ));
                                                });

                                            // trigger CheckMempoolCompleteness
                                            ctx.myself().tell(CheckMempoolCompleteness, None);
                                        }
                                        BlockAcceptanceResult::IgnoreBlock => {
                                            // doing nothing
                                        }
                                        BlockAcceptanceResult::UnknownBranch => {
                                            // ask current_branch from peer
                                            tell_peer(
                                                GetCurrentBranchMessage::new(
                                                    message.chain_id().clone(),
                                                )
                                                .into(),
                                                peer,
                                            );
                                        }
                                        BlockAcceptanceResult::MutlipassValidationError(error) => {
                                            warn!(log, "Mutlipass validation error detected - blacklisting peer";
                                                       "message_head_level" => message.current_block_header().level(),
                                                       "message_head_proto" => message.current_block_header().proto(),
                                                       "reason" => &error);

                                            // clear peer stuff immediatelly
                                            peer.clear();

                                            // blacklist peer
                                            network_channel.tell(
                                                Publish {
                                                    msg: NetworkChannelMsg::BlacklistPeer(
                                                        peer.peer_id.clone(),
                                                        format!("{:?}", error),
                                                    ),
                                                    topic: NetworkChannelTopic::NetworkCommands
                                                        .into(),
                                                },
                                                None,
                                            );
                                        }
                                    };
                                }
                            }
                            PeerMessage::GetOperations(message) => {
                                let requested_operations: &Vec<OperationHash> =
                                    message.get_operations();
                                for operation_hash in requested_operations {
                                    // TODO: where to look for operations for advertised mempool?
                                    // TODO: if not found here, check regular operation storage?
                                    if let Some(found) = mempool_storage.find(&operation_hash)? {
                                        tell_peer(found.into(), peer);
                                    }
                                }
                            }
                            PeerMessage::Operation(message) => {
                                // handling new mempool operations here
                                // parse operation data
                                let operation = message.operation();
                                let operation_hash = operation.message_typed_hash()?;

                                match peer.queued_mempool_operations.remove(&operation_hash) {
                                    Some((operation_type, op_ttl)) => {
                                        // do prevalidation before add the operation to mempool
                                        let result = match validation::prevalidate_operation(
                                            chain_state.get_chain_id(),
                                            &operation_hash,
                                            &operation,
                                            self.current_mempool_state.clone(),
                                            &self.tezos_readonly_prevalidation_api.pool.get()?.api,
                                            block_storage,
                                            block_meta_storage,
                                        ) {
                                            Ok(result) => result,
                                            Err(e) => match e {
                                                validation::PrevalidateOperationError::UnknownBranch { .. }
                                                | validation::PrevalidateOperationError::BranchNotAppliedYet { .. } => {
                                                    // here we just ignore UnknownBranch
                                                    return Ok(());
                                                }
                                                poe => {
                                                    // other error just propagate
                                                    return Err(format_err!("Operation from p2p ({}) was not added to mempool. Reason: {:?}", operation_hash.to_base58_check(), poe));
                                                }
                                            }
                                        };

                                        // can accpect operation ?
                                        if !validation::can_accept_operation_from_p2p(
                                            &operation_hash,
                                            &result,
                                        ) {
                                            return Err(format_err!("Operation from p2p ({}) was not added to mempool. Reason: {:?}", operation_hash.to_base58_check(), result));
                                        }

                                        // store mempool operation
                                        peer.mempool_operations_response_last = Instant::now();
                                        mempool_storage.put(
                                            operation_type.clone(),
                                            message.clone(),
                                            op_ttl,
                                        )?;

                                        // trigger CheckMempoolCompleteness
                                        ctx.myself().tell(CheckMempoolCompleteness, None);

                                        // notify others that new operation was received
                                        mempool_channel.tell(
                                            Publish {
                                                msg: MempoolOperationReceived {
                                                    operation_hash,
                                                    operation_type,
                                                    result_callback: None,
                                                }
                                                .into(),
                                                topic: MempoolChannelTopic.into(),
                                            },
                                            None,
                                        );
                                    }
                                    None => {
                                        debug!(log, "Unexpected mempool operation received")
                                    }
                                }
                            }
                            PeerMessage::Advertise(msg) => {
                                // re-send command to network layer
                                network_channel.tell(
                                    Publish {
                                        msg: NetworkChannelMsg::ProcessAdvertisedPeers(
                                            peer.peer_id.clone(),
                                            msg.clone(),
                                        ),
                                        topic: NetworkChannelTopic::NetworkCommands.into(),
                                    },
                                    None,
                                );
                            }
                            PeerMessage::Bootstrap => {
                                // re-send command to network layer
                                network_channel.tell(
                                    Publish {
                                        msg: NetworkChannelMsg::SendBootstrapPeers(
                                            peer.peer_id.clone(),
                                        ),
                                        topic: NetworkChannelTopic::NetworkCommands.into(),
                                    },
                                    None,
                                );
                            }
                            ignored_message => {
                                trace!(log, "Ignored message"; "message" => format!("{:?}", ignored_message))
                            }
                        }
                    }
                    None => {
                        warn!(ctx.system.log(), "Received message from non-existing peer actor";
                                                "peer" => received.peer.name().to_string(),
                                                "peer_uri" => received.peer.uri().to_string());
                    }
                }
            }
            _ => (),
        }

        Ok(())
    }

    fn process_shell_channel_message(
        &mut self,
        ctx: &Context<ChainManagerMsg>,
        msg: ShellChannelMsg,
    ) -> Result<(), Error> {
        match msg {
            ShellChannelMsg::AdvertiseToP2pNewMempool(chain_id, block_hash, new_mempool) => {
                // get header and send it to p2p
                if let Some(header) = self.block_storage.get(&block_hash)? {
                    self.advertise_current_head_to_p2p(
                        &chain_id,
                        header.header,
                        new_mempool.as_ref().clone(),
                        true,
                    );
                } else {
                    return Err(format_err!(
                        "BlockHeader ({}) was not found!",
                        block_hash.to_base58_check()
                    ));
                }
            }
            ShellChannelMsg::AdvertiseToP2pNewCurrentHead(chain_id, block_hash) => {
                // get header and send it to p2p
                if let Some(header) = self.block_storage.get(&block_hash)? {
                    self.advertise_current_head_to_p2p(
                        &chain_id,
                        header.header,
                        Mempool::default(),
                        false,
                    );
                } else {
                    return Err(format_err!(
                        "BlockHeader ({}) was not found!",
                        block_hash.to_base58_check()
                    ));
                }
            }
            ShellChannelMsg::AdvertiseToP2pNewCurrentBranch(chain_id, block_hash) => {
                // get header and send it to p2p
                if let Some(header) = self.block_storage.get(&block_hash)? {
                    self.advertise_current_branch_to_p2p(&chain_id, &header)?;
                } else {
                    return Err(format_err!(
                        "BlockHeader ({}) was not found!",
                        block_hash.to_base58_check()
                    ));
                }
            }
            ShellChannelMsg::InjectBlock(inject_block, result_callback) => {
                self.process_injected_block(inject_block, result_callback, ctx)?;
            }
            ShellChannelMsg::RequestCurrentHead(_) => {
                let ChainManager {
                    peers, chain_state, ..
                } = self;
                let msg: Arc<PeerMessageResponse> =
                    GetCurrentHeadMessage::new(chain_state.get_chain_id().as_ref().clone()).into();
                peers.iter_mut().for_each(|(_, peer)| {
                    peer.current_head_request_last = Instant::now();
                    tell_peer(msg.clone(), peer)
                });
            }
            ShellChannelMsg::PeerBranchSynchronizationDone(msg) => {
                if let Err(e) = self.resolve_is_bootstrapped(&msg, &ctx.system.log()) {
                    warn!(ctx.system.log(), "Failed to resolve is_bootstrapped for chain manager"; "msg" => format!("{:?}", msg), "reason" => format!("{:?}", e))
                }
            }
            ShellChannelMsg::ShuttingDown(_) => {
                self.shutting_down = true;
            }
            _ => (),
        }

        Ok(())
    }

    fn process_downloaded_header(
        received_block: BlockHeaderWithHash,
        peer: &mut PeerState,
        stats: &mut Stats,
        chain_state: &mut BlockchainState,
        shell_channel: &ShellChannelRef,
        log: &Logger,
    ) -> Result<(), Error> {
        // store header
        if chain_state.process_block_header_from_peer(peer, &received_block, log)? {
            // update stats for new header
            stats.unseen_block_last = Instant::now();
            stats.unseen_block_count += 1;

            // notify others that new block was received
            shell_channel.tell(
                Publish {
                    msg: BlockReceived {
                        hash: received_block.hash,
                        level: received_block.header.level(),
                    }
                    .into(),
                    topic: ShellChannelTopic::ShellEvents.into(),
                },
                None,
            );
        }

        Ok(())
    }

    fn process_injected_block(
        &mut self,
        injected_block: InjectBlock,
        result_callback: Option<CondvarResult<(), failure::Error>>,
        ctx: &Context<ChainManagerMsg>,
    ) -> Result<(), Error> {
        let InjectBlock {
            chain_id,
            block_header: block_header_with_hash,
            operations,
            operation_paths,
        } = injected_block;
        let log = ctx
            .system
            .log()
            .new(slog::o!("block" => block_header_with_hash.hash.to_base58_check(), "chain_id" => chain_id.to_base58_check()));

        // this should  allways return [is_new_block==true], as we are injecting a forged new block
        let (block_metadata, is_new_block, are_operations_complete) = match self
            .chain_state
            .process_injected_block_header(&chain_id, &block_header_with_hash, &log)
        {
            Ok(data) => data,
            Err(e) => {
                if let Err(e) = dispatch_condvar_result(
                    result_callback,
                    || {
                        Err(format_err!(
                            "Failed to store injected block, block_hash: {}, reason: {}",
                            block_header_with_hash.hash.to_base58_check(),
                            e
                        ))
                    },
                    true,
                ) {
                    warn!(log, "Failed to dispatch result to condvar"; "reason" => format!("{}", e));
                }
                return Err(e.into());
            }
        };
        info!(log, "New block injection";
                   "is_new_block" => is_new_block,
                   "level" => block_header_with_hash.header.level());

        if is_new_block {
            // update stats
            self.stats.unseen_block_last = Instant::now();
            self.stats.unseen_block_count += 1;

            // notify others that new block (header) was received
            self.shell_channel.tell(
                Publish {
                    msg: BlockReceived {
                        hash: block_header_with_hash.hash.clone(),
                        level: block_header_with_hash.header.level(),
                    }
                    .into(),
                    topic: ShellChannelTopic::ShellEvents.into(),
                },
                None,
            );

            // handle operations (if expecting any)
            if !are_operations_complete {
                let operations = match operations {
                    Some(operations) => operations,
                    None => {
                        if let Err(e) = dispatch_condvar_result(
                            result_callback,
                            || {
                                Err(format_err!(
                                    "Missing operations in request, block_hash: {}",
                                    block_header_with_hash.hash.to_base58_check()
                                ))
                            },
                            true,
                        ) {
                            warn!(log, "Failed to dispatch result to condvar"; "reason" => format!("{}", e));
                        }
                        return Err(format_err!(
                            "Missing operations in request, block_hash: {}",
                            block_header_with_hash.hash.to_base58_check()
                        ));
                    }
                };
                let op_paths = match operation_paths {
                    Some(op_paths) => op_paths,
                    None => {
                        if let Err(e) = dispatch_condvar_result(
                            result_callback,
                            || {
                                Err(format_err!(
                                    "Missing operation paths in request, block_hash: {}",
                                    block_header_with_hash.hash.to_base58_check()
                                ))
                            },
                            true,
                        ) {
                            warn!(log, "Failed to dispatch result to condvar"; "reason" => format!("{}", e));
                        }
                        return Err(format_err!(
                            "Missing operation paths in request, block_hash: {}",
                            block_header_with_hash.hash.to_base58_check()
                        ));
                    }
                };

                // iterate through all validation passes
                for (idx, ops) in operations.into_iter().enumerate() {
                    let opb =
                        OperationsForBlock::new(block_header_with_hash.hash.clone(), idx as i8);

                    // create OperationsForBlocksMessage - the operations are stored in DB as a OperationsForBlocksMessage per validation pass per block
                    // e.g one block -> 4 validation passes -> 4 OperationsForBlocksMessage to store for the block
                    let operation_hashes_path = match op_paths.get(idx) {
                        Some(path) => path.to_owned(),
                        None => {
                            if let Err(e) = dispatch_condvar_result(
                                result_callback,
                                || {
                                    Err(format_err!("Missing operation paths in request for index: {}, block_hash: {}", idx, block_header_with_hash.hash.to_base58_check()))
                                },
                                true,
                            ) {
                                warn!(log, "Failed to dispatch result to condvar"; "reason" => format!("{}", e));
                            }
                            return Err(format_err!(
                                "Missing operation paths in request for index: {}, block_hash: {}",
                                idx,
                                block_header_with_hash.hash.to_base58_check()
                            ));
                        }
                    };

                    let msg: OperationsForBlocksMessage =
                        OperationsForBlocksMessage::new(opb, operation_hashes_path, ops);

                    match self.chain_state.process_block_operations(&msg) {
                        Ok((all_operations_received, _)) => {
                            if all_operations_received {
                                info!(log, "New block injection - operations are complete";
                                           "is_new_block" => is_new_block,
                                           "level" => block_header_with_hash.header.level());

                                // update stats
                                self.stats.unseen_block_operations_last = Instant::now();

                                // notify others that new all operations for block were received
                                self.shell_channel.tell(
                                    Publish {
                                        msg: AllBlockOperationsReceived {
                                            hash: block_header_with_hash.hash.clone(),
                                            level: block_metadata.level(),
                                        }
                                        .into(),
                                        topic: ShellChannelTopic::ShellEvents.into(),
                                    },
                                    None,
                                );
                            }
                        }
                        Err(e) => {
                            if let Err(e) = dispatch_condvar_result(
                                result_callback,
                                || {
                                    Err(format_err!("Failed to store injected block operations, block_hash: {}, reason: {}", block_header_with_hash.hash.to_base58_check(), e))
                                },
                                true,
                            ) {
                                warn!(log, "Failed to dispatch result to condvar"; "reason" => format!("{}", e));
                            }
                            return Err(e.into());
                        }
                    };
                }
            }

            // try apply block
            if let Err(e) = self.chain_state.requester().try_apply_block(
                chain_id,
                block_header_with_hash.hash.clone(),
                result_callback.clone(),
            ) {
                if let Err(e) = dispatch_condvar_result(
                    result_callback,
                    || {
                        Err(format_err!("Failed to detect if injected block can be applied, block_hash: {}, reason: {}", block_header_with_hash.hash.to_base58_check(), e))
                    },
                    true,
                ) {
                    warn!(log, "Failed to dispatch result to condvar"; "reason" => format!("{}", e));
                }
                return Err(e.into());
            };
        } else {
            warn!(log, "Injected duplicated block - will be ignored!");
            if let Err(e) = dispatch_condvar_result(
                result_callback,
                || {
                    Err(format_err!(
                        "Injected duplicated block - will be ignored!, block_hash: {}",
                        block_header_with_hash.hash.to_base58_check()
                    ))
                },
                true,
            ) {
                warn!(log, "Failed to dispatch result to condvar"; "reason" => format!("{}", e));
            }
        }

        Ok(())
    }

    /// Resolves if chain_manager is bootstrapped,
    /// means that we have at_least <> boostrapped peers
    ///
    /// "bootstrapped peer" means, that peer.current_level <= chain_manager.current_level
    fn resolve_is_bootstrapped(
        &mut self,
        msg: &PeerBranchSynchronizationDone,
        log: &Logger,
    ) -> Result<(), StateError> {
        if self.current_bootstrap_state.read()?.is_bootstrapped() {
            // TODO: TE-386 - global queue for requested operations
            if let Some(peer_state) = self.peers.get_mut(msg.peer().peer_ref.uri()) {
                peer_state.missing_operations_for_blocks.clear();
            }

            return Ok(());
        }

        let chain_manager_current_level = self
            .current_head
            .local
            .read()?
            .as_ref()
            .map(|head| *head.level())
            .unwrap_or(0);

        let remote_best_known_level = self
            .current_head
            .remote
            .read()?
            .as_ref()
            .map(|head| *head.level())
            .unwrap_or(0);

        if let Some(peer_state) = self.peers.get_mut(msg.peer().peer_ref.uri()) {
            self.current_bootstrap_state.write()?.update_by_peer_state(
                msg,
                peer_state,
                remote_best_known_level,
                chain_manager_current_level,
            );

            // TODO: TE-386 - global queue for requested operations
            peer_state.missing_operations_for_blocks.clear();
        }

        {
            // lock and log
            let current_bootstrap_state = self.current_bootstrap_state.read()?;
            if current_bootstrap_state.is_bootstrapped() {
                info!(log, "Bootstrapped (chain_manager)";
                       "num_of_peers_for_bootstrap_threshold" => current_bootstrap_state.num_of_peers_for_bootstrap_threshold(),
                       "remote_best_known_level" => remote_best_known_level,
                       "reached_on_level" => chain_manager_current_level);
            }
        }

        Ok(())
    }

    /// Send CurrentBranch message to the p2p
    fn advertise_current_branch_to_p2p(
        &self,
        chain_id: &ChainId,
        block_header: &BlockHeaderWithHash,
    ) -> Result<(), StorageError> {
        let ChainManager {
            peers,
            chain_state,
            identity_peer_id,
            ..
        } = self;

        for peer in peers.values() {
            tell_peer(
                CurrentBranchMessage::new(
                    chain_id.clone(),
                    CurrentBranch::new(
                        block_header.header.as_ref().clone(),
                        // calculate history for each peer
                        chain_state.get_history(
                            &block_header.hash,
                            &Seed::new(&identity_peer_id, &peer.peer_id.peer_public_key_hash),
                        )?,
                    ),
                )
                .into(),
                peer,
            )
        }

        Ok(())
    }

    /// Send CurrentHead message to the p2p
    ///
    /// `ignore_msg_with_empty_mempool` - if true means: send CurrentHead, only if we have anything in mempool (just to peers with enabled mempool)
    fn advertise_current_head_to_p2p(
        &self,
        chain_id: &ChainId,
        block_header: Arc<BlockHeader>,
        mempool: Mempool,
        ignore_msg_with_empty_mempool: bool,
    ) {
        // prepare messages to prevent unnecessesery cloning of messages
        // message to peers with enabled mempool
        let (msg_for_mempool_enabled_is_mempool_empty, msg_for_mempool_enabled): (
            bool,
            Arc<PeerMessageResponse>,
        ) = {
            let current_head_msg =
                CurrentHeadMessage::new(chain_id.clone(), block_header.as_ref().clone(), {
                    // we must check, if we have allowed mempool
                    if self.p2p_disable_mempool {
                        Mempool::default()
                    } else {
                        mempool
                    }
                });
            (
                current_head_msg.current_mempool().is_empty(),
                current_head_msg.into(),
            )
        };
        // message to peers with disabled mempool
        let (msg_for_mempool_disabled_is_mempool_empty, msg_for_mempool_disabled): (
            bool,
            Arc<PeerMessageResponse>,
        ) = (
            true,
            CurrentHeadMessage::new(
                chain_id.clone(),
                block_header.as_ref().clone(),
                Mempool::default(),
            )
            .into(),
        );

        // send messsages
        self.peers.iter().for_each(|(_, peer)| {
            let (msg, msg_is_mempool_empty) = if peer.mempool_enabled {
                (
                    msg_for_mempool_enabled.clone(),
                    msg_for_mempool_enabled_is_mempool_empty,
                )
            } else {
                (
                    msg_for_mempool_disabled.clone(),
                    msg_for_mempool_disabled_is_mempool_empty,
                )
            };

            let can_send_msg = !(ignore_msg_with_empty_mempool && msg_is_mempool_empty);
            if can_send_msg {
                tell_peer(msg, peer)
            }
        });
    }

    fn resolve_mempool_to_send_to_peer(
        peer: &PeerState,
        p2p_disable_mempool: bool,
        current_mempool_state: CurrentMempoolStateStorageRef,
        current_head: &Head,
    ) -> Result<Mempool, failure::Error> {
        if p2p_disable_mempool {
            return Ok(Mempool::default());
        }
        if !peer.mempool_enabled {
            return Ok(Mempool::default());
        }

        let mempool_state = current_mempool_state
            .read()
            .map_err(|e| format_err!("Failed to lock for read, reason: {}", e))?;
        if let Some(mempool_head_hash) = mempool_state.head() {
            if mempool_head_hash == current_head.block_hash() {
                let mempool_state: &MempoolState = &mempool_state;
                Ok(mempool_state.into())
            } else {
                Ok(Mempool::default())
            }
        } else {
            Ok(Mempool::default())
        }
    }
}

impl
    ActorFactoryArgs<(
        ChainFeederRef,
        NetworkChannelRef,
        ShellChannelRef,
        MempoolChannelRef,
        PersistentStorage,
        Arc<TezosApiConnectionPool>,
        StorageInitInfo,
        bool,
        CurrentHeadRef,
        CurrentHeadRef,
        CurrentMempoolStateStorageRef,
        SynchronizationBootstrapStateRef,
        ApplyBlockStatsRef,
        bool,
        CryptoboxPublicKeyHash,
    )> for ChainManager
{
    fn create_args(
        (
            block_applier,
            network_channel,
            shell_channel,
            mempool_channel,
            persistent_storage,
            tezos_readonly_prevalidation_api,
            init_storage_data,
            is_sandbox,
            local_current_head_state,
            remote_current_head_state,
            current_mempool_state,
            current_bootstrap_state,
            apply_block_stats,
            p2p_disable_mempool,
            identity_peer_id,
        ): (
            ChainFeederRef,
            NetworkChannelRef,
            ShellChannelRef,
            MempoolChannelRef,
            PersistentStorage,
            Arc<TezosApiConnectionPool>,
            StorageInitInfo,
            bool,
            CurrentHeadRef,
            CurrentHeadRef,
            CurrentMempoolStateStorageRef,
            SynchronizationBootstrapStateRef,
            ApplyBlockStatsRef,
            bool,
            CryptoboxPublicKeyHash,
        ),
    ) -> Self {
        ChainManager {
            network_channel,
            shell_channel: shell_channel.clone(),
            mempool_channel,
            block_storage: Box::new(BlockStorage::new(&persistent_storage)),
            block_meta_storage: Box::new(BlockMetaStorage::new(&persistent_storage)),
            operations_storage: Box::new(OperationsStorage::new(&persistent_storage)),
            mempool_storage: MempoolStorage::new(&persistent_storage),
            chain_state: BlockchainState::new(
                block_applier,
                &persistent_storage,
                shell_channel,
                Arc::new(init_storage_data.chain_id),
                Arc::new(init_storage_data.genesis_block_header_hash),
            ),
            peers: HashMap::new(),
            current_head: CurrentHead {
                local: local_current_head_state,
                remote: remote_current_head_state,
            },
            shutting_down: false,
            stats: Stats {
                unseen_block_count: 0,
                unseen_block_last: Instant::now(),
                unseen_block_operations_last: Instant::now(),
                apply_block_stats,
            },
            is_sandbox,
            identity_peer_id,
            current_mempool_state,
            current_bootstrap_state,
            p2p_disable_mempool,
            tezos_readonly_prevalidation_api,
        }
    }
}

impl Actor for ChainManager {
    type Msg = ChainManagerMsg;

    fn pre_start(&mut self, ctx: &Context<Self::Msg>) {
        subscribe_to_actor_terminated(ctx.system.sys_events(), ctx.myself());
        subscribe_to_network_events(&self.network_channel, ctx.myself());
        subscribe_to_shell_shutdown(&self.shell_channel, ctx.myself());
        subscribe_to_shell_commands(&self.shell_channel, ctx.myself());

        ctx.schedule::<Self::Msg, _>(
            ASK_CURRENT_HEAD_INITIAL_DELAY,
            ASK_CURRENT_HEAD_INTERVAL,
            ctx.myself(),
            None,
            AskPeersAboutCurrentHead.into(),
        );
        ctx.schedule::<Self::Msg, _>(
            LOG_INTERVAL / 2,
            LOG_INTERVAL,
            ctx.myself(),
            None,
            LogStats.into(),
        );

        let silent_peer_timeout = if self.is_sandbox {
            SILENT_PEER_TIMEOUT_SANDBOX
        } else {
            SILENT_PEER_TIMEOUT
        };
        ctx.schedule::<Self::Msg, _>(
            silent_peer_timeout,
            silent_peer_timeout,
            ctx.myself(),
            None,
            DisconnectStalledPeers {
                silent_peer_timeout,
            }
            .into(),
        );
    }

    fn post_start(&mut self, ctx: &Context<Self::Msg>) {
        match self.current_bootstrap_state.read() {
            Ok(current_bootstrap_state) => {
                if current_bootstrap_state.is_bootstrapped() {
                    info!(ctx.system.log(), "Bootstrapped on startup (chain_manager)";
                       "num_of_peers_for_bootstrap_threshold" => current_bootstrap_state.num_of_peers_for_bootstrap_threshold());
                }
            }
            Err(e) => {
                warn!(ctx.system.log(), "Failed to read current_bootstrap_state on startup"; "reason" => format!("{}", e))
            }
        }
    }

    fn sys_recv(
        &mut self,
        ctx: &Context<Self::Msg>,
        msg: SystemMsg,
        sender: Option<BasicActorRef>,
    ) {
        if let SystemMsg::Event(evt) = msg {
            self.receive(ctx, evt, sender);
        }
    }

    fn recv(&mut self, ctx: &Context<Self::Msg>, msg: Self::Msg, sender: Sender) {
        self.receive(ctx, msg, sender);
    }
}

impl Receive<SystemEvent> for ChainManager {
    type Msg = ChainManagerMsg;

    fn receive(
        &mut self,
        _: &Context<Self::Msg>,
        msg: SystemEvent,
        _sender: Option<BasicActorRef>,
    ) {
        if let SystemEvent::ActorTerminated(evt) = msg {
            self.peers.remove(evt.actor.uri());
        }
    }
}

impl Receive<LogStats> for ChainManager {
    type Msg = ChainManagerMsg;

    fn receive(&mut self, ctx: &Context<Self::Msg>, _msg: LogStats, _sender: Sender) {
        let log = ctx.system.log();
        let (local, local_level, local_fitness) = match self.current_head.local_debug_info() {
            Ok(result) => result,
            Err(e) => {
                warn!(ctx.system.log(), "Failed to collect local head debug info"; "reason" => e);
                (
                    "-failed-to-collect-".to_string(),
                    0,
                    "-failed-to-collect-".to_string(),
                )
            }
        };

        let (remote, remote_level, remote_fitness) = match self.current_head.remote_debug_info() {
            Ok(result) => result,
            Err(e) => {
                warn!(ctx.system.log(), "Failed to collect local head debug info"; "reason" => e);
                (
                    "-failed-to-collect-".to_string(),
                    0,
                    "-failed-to-collect-".to_string(),
                )
            }
        };

        // calculate applied stats
        let (last_applied, applied_block_level, applied_block_last) = {
            let Stats {
                apply_block_stats, ..
            } = &self.stats;

            match apply_block_stats.write() {
                Ok(mut apply_block_stats) => {
                    let applied_block_lasts_count = apply_block_stats.applied_block_lasts_count();

                    if *applied_block_lasts_count > 0 {
                        let validation = apply_block_stats
                            .applied_block_lasts_sum_validation_timer()
                            .print_formatted_average_for_count(*applied_block_lasts_count);

                        // collect stats before clearing
                        let stats = format!(
                            "({} blocks - average times [{}]",
                            applied_block_lasts_count, validation,
                        );
                        let applied_block_level = *apply_block_stats.applied_block_level();
                        let applied_block_last = apply_block_stats
                            .applied_block_last()
                            .map(|i| i.elapsed().as_secs());

                        // clear stats for next run
                        apply_block_stats.clear_applied_block_lasts();

                        (stats, applied_block_level, applied_block_last)
                    } else {
                        (
                            format!("({} blocks)", applied_block_lasts_count),
                            None,
                            None,
                        )
                    }
                }
                Err(e) => {
                    warn!(log, "Failed to get apply block stats"; "reason" => format!("{}", e));
                    ("(failed to get stats)".to_string(), None, None)
                }
            }
        };

        info!(log, "Head info";
            "local" => local,
            "local_level" => local_level,
            "local_fitness" => local_fitness,
            "remote" => remote,
            "remote_level" => remote_level,
            "remote_fitness" => remote_fitness);
        info!(log, "Blocks and operations info";
            "block_count" => self.stats.unseen_block_count,
            "last_block_secs" => self.stats.unseen_block_last.elapsed().as_secs(),
            "last_block_operations_secs" => self.stats.unseen_block_operations_last.elapsed().as_secs(),
            "applied_block_level" => applied_block_level,
            "applied_block_secs" => applied_block_last);
        // TODO: TE-369 - peers stats
        for peer in self.peers.values() {
            debug!(log, "Peer state info";
                "actor_ref" => format!("{}", peer.peer_id.peer_ref),
                // "missing_blocks" => peer.missing_blocks.missing_data_count(),
                // "missing_block_operations" => peer.missing_operations_for_blocks.missing_data_count(),
                // "queued_block_headers" => peer.queued_block_headers.len(),
                // "queued_block_operations" => peer.queued_block_operations.len(),
                "current_head_request_secs" => peer.current_head_request_last.elapsed().as_secs(),
                "current_head_response_secs" => peer.current_head_response_last.elapsed().as_secs(),
                "block_request_secs" => {
                    match peer.queues.block_request_last.try_read() {
                        Ok(request_last) => format!("{}", request_last.elapsed().as_secs()),
                        _ =>  "-failed-to-collect-".to_string(),
                    }
                },
                "block_response_secs" => {
                    match peer.queues.block_response_last.try_read() {
                        Ok(response_last) => format!("{}", response_last.elapsed().as_secs()),
                        _ =>  "-failed-to-collect-".to_string(),
                    }
                },
                "block_operations_request_secs" => {
                    match peer.queues.block_operations_request_last.try_read() {
                        Ok(request_last) => format!("{}", request_last.elapsed().as_secs()),
                        _ =>  "-failed-to-collect-".to_string(),
                    }
                },
                "block_operations_response_secs" => {
                    match peer.queues.block_operations_response_last.try_read() {
                        Ok(response_last) => format!("{}", response_last.elapsed().as_secs()),
                        _ =>  "-failed-to-collect-".to_string(),
                    }
                },
                "mempool_operations_request_secs" => peer.mempool_operations_request_last.elapsed().as_secs(),
                "mempool_operations_response_secs" => peer.mempool_operations_response_last.elapsed().as_secs(),
                "current_head_level" => peer.current_head_level,
                "current_head_update_secs" => peer.current_head_update_last.elapsed().as_secs());
        }
        info!(log, "Various info";
                   "peer_count" => self.peers.len(),
                   "local_level" => local_level,
                   "last_applied" => last_applied,
        );
    }
}

impl Receive<DisconnectStalledPeers> for ChainManager {
    type Msg = ChainManagerMsg;

    fn receive(&mut self, ctx: &Context<Self::Msg>, msg: DisconnectStalledPeers, _sender: Sender) {
        self.peers.iter()
            .for_each(|(uri, state)| {
                let current_head_response_pending = state.current_head_request_last > state.current_head_response_last;
                let mempool_operations_response_pending = state.mempool_operations_request_last > state.mempool_operations_response_last;
                let known_higher_head = match state.current_head_level {
                    Some(peer_level) => match self.current_head.has_any_higher_than(peer_level) {
                        Ok(result) => result,
                        Err(_) => {
                            warn!(ctx.system.log(), "Failed to collect current local head";
                                            "peer_id" => state.peer_id.peer_id_marker.clone(), "peer_ip" => state.peer_id.peer_address.to_string(), "peer" => state.peer_id.peer_ref.name(), "peer_uri" => uri.to_string());
                            false
                        }
                    },
                    None => true,
                };

                // chcek penalty peer for not responding to our requests on time
                let block_response_pending = match state.is_block_response_pending(msg.silent_peer_timeout) {
                    Ok(response_pending) => {
                        if response_pending {
                            warn!(ctx.system.log(), "Peer did not respond to our request for block on time";
                                                "silent_peer_timeout_exceeded" => format!("{:?}", msg.silent_peer_timeout),
                                                "peer_id" => state.peer_id.peer_id_marker.clone(), "peer_ip" => state.peer_id.peer_address.to_string(), "peer" => state.peer_id.peer_ref.name(), "peer_uri" => uri.to_string());
                        }
                        response_pending
                    }
                    Err(e) => {
                        warn!(ctx.system.log(), "Failed to resolve, if block response pending, for peer (so behave as ok)";
                                                "reason" => format!("{}", e),
                                                "peer_id" => state.peer_id.peer_id_marker.clone(), "peer_ip" => state.peer_id.peer_address.to_string(), "peer" => state.peer_id.peer_ref.name(), "peer_uri" => uri.to_string());
                        false
                    }
                };
                let block_operations_response_pending = match state.is_block_operations_response_pending(msg.silent_peer_timeout) {
                    Ok(response_pending) => {
                        if response_pending {
                            warn!(ctx.system.log(), "Peer did not respond to our request for block operations on time";
                                                "silent_peer_timeout_exceeded" => format!("{:?}", msg.silent_peer_timeout),
                                                "peer_id" => state.peer_id.peer_id_marker.clone(), "peer_ip" => state.peer_id.peer_address.to_string(), "peer" => state.peer_id.peer_ref.name(), "peer_uri" => uri.to_string());
                        }
                        response_pending
                    }
                    Err(e) => {
                        warn!(ctx.system.log(), "Failed to resolve, if block operations response pending, for peer (so behave as ok)";
                                                "reason" => format!("{}", e),
                                                "peer_id" => state.peer_id.peer_id_marker.clone(), "peer_ip" => state.peer_id.peer_address.to_string(), "peer" => state.peer_id.peer_ref.name(), "peer_uri" => uri.to_string());
                        false
                    }
                };

                let should_disconnect = if block_response_pending || block_operations_response_pending {
                    true
                } else if current_head_response_pending && (state.current_head_request_last - state.current_head_response_last > msg.silent_peer_timeout) {
                    warn!(ctx.system.log(), "Peer did not respond to our request for current_head on time"; "request_secs" => state.current_head_request_last.elapsed().as_secs(), "response_secs" => state.current_head_response_last.elapsed().as_secs(),
                                            "peer_id" => state.peer_id.peer_id_marker.clone(), "peer_ip" => state.peer_id.peer_address.to_string(), "peer" => state.peer_id.peer_ref.name(), "peer_uri" => uri.to_string());
                    true
                } else if known_higher_head && (state.current_head_update_last.elapsed() > CURRENT_HEAD_LEVEL_UPDATE_TIMEOUT) {
                    warn!(ctx.system.log(), "Peer failed to update its current head";
                                            "request_secs" => state.current_head_request_last.elapsed().as_secs(),
                                            "response_secs" => state.current_head_response_last.elapsed().as_secs(),
                                            "current_head_update_last" => state.current_head_update_last.elapsed().as_secs(),
                                            "peer_current_level" => {
                                                if let Some(level) = state.current_head_level {
                                                    level.to_string()
                                                } else {
                                                    "-".to_string()
                                                }
                                            },
                                            "node_current_level_remote" => {
                                                if let Ok((_, remote_level, _)) = self.current_head.remote_debug_info() {
                                                    remote_level.to_string()
                                                } else {
                                                    "-failed-to-collect".to_string()
                                                }
                                            },
                                            "node_current_level_local" => {
                                                if let Ok((_, local_level, _)) = self.current_head.local_debug_info() {
                                                    local_level.to_string()
                                                } else {
                                                    "-failed-to-collect".to_string()
                                                }
                                            },
                                            "peer_id" => state.peer_id.peer_id_marker.clone(), "peer_ip" => state.peer_id.peer_address.to_string(), "peer" => state.peer_id.peer_ref.name(), "peer_uri" => uri.to_string());
                    true
                } else if mempool_operations_response_pending && !state.queued_mempool_operations.is_empty() && (state.mempool_operations_response_last.elapsed() > msg.silent_peer_timeout) {
                    warn!(ctx.system.log(), "Peer is not providing requested mempool operations"; "queued_count" => state.queued_mempool_operations.len(), "response_secs" => state.mempool_operations_response_last.elapsed().as_secs(),
                                            "peer_id" => state.peer_id.peer_id_marker.clone(), "peer_ip" => state.peer_id.peer_address.to_string(), "peer" => state.peer_id.peer_ref.name(), "peer_uri" => uri.to_string());
                    true
                } else {
                    false
                };

                if should_disconnect {
                    // stop peer
                    ctx.system.stop(state.peer_id.peer_ref.clone());

                    // stop peer's bootstrap
                    if let Some(boot) = state.peer_branch_bootstrapper.as_ref() {
                        ctx.system.stop(boot);
                    }
                }
            });
    }
}

impl Receive<CheckMempoolCompleteness> for ChainManager {
    type Msg = ChainManagerMsg;

    fn receive(
        &mut self,
        ctx: &Context<Self::Msg>,
        _msg: CheckMempoolCompleteness,
        _sender: Sender,
    ) {
        if !self.shutting_down {
            self.check_mempool_completeness(ctx)
        }
    }
}

impl Receive<NetworkChannelMsg> for ChainManager {
    type Msg = ChainManagerMsg;

    fn receive(&mut self, ctx: &Context<Self::Msg>, msg: NetworkChannelMsg, _sender: Sender) {
        match self.process_network_channel_message(ctx, msg) {
            Ok(_) => (),
            Err(e) => {
                warn!(ctx.system.log(), "Failed to process network channel message"; "reason" => format!("{:?}", e))
            }
        }
    }
}

impl Receive<ShellChannelMsg> for ChainManager {
    type Msg = ChainManagerMsg;

    fn receive(&mut self, ctx: &Context<Self::Msg>, msg: ShellChannelMsg, _sender: Sender) {
        match self.process_shell_channel_message(ctx, msg) {
            Ok(_) => (),
            Err(e) => {
                warn!(ctx.system.log(), "Failed to process shell channel message"; "reason" => format!("{:?}", e))
            }
        }
    }
}

impl Receive<AskPeersAboutCurrentHead> for ChainManager {
    type Msg = ChainManagerMsg;

    fn receive(
        &mut self,
        _ctx: &Context<Self::Msg>,
        _msg: AskPeersAboutCurrentHead,
        _sender: Sender,
    ) {
        let ChainManager {
            peers, chain_state, ..
        } = self;
        peers.iter_mut().for_each(|(_, peer)| {
            peer.current_head_request_last = Instant::now();
            tell_peer(
                GetCurrentHeadMessage::new(chain_state.get_chain_id().as_ref().clone()).into(),
                peer,
            )
        })
    }
}
