// Copyright (c) SimpleStaking and Tezedge Contributors
// SPDX-License-Identifier: MIT

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

use failure::Error;
use riker::actors::*;
use slog::{debug, error, info, Logger, warn};

use storage::{BlockMetaStorage, BlockStorage, BlockStorageReader, OperationsMetaStorage, OperationsStorage, OperationsStorageReader};
use tezos_api::client::TezosStorageInitInfo;
use tezos_encoding::hash::{BlockHash, ChainId, HashEncoding, HashType};
use tezos_wrapper::service::{ProtocolService, ProtocolWrapperIpc, ProtocolServiceConfiguration, ProtocolServiceError};

use crate::shell_channel::{BlockApplied, ShellChannelRef, ShellChannelTopic};

/// This command triggers feeding of completed blocks to the tezos protocol
#[derive(Clone, Debug)]
pub struct FeedChainToProtocol;

type SharedJoinHandle = Arc<Mutex<Option<JoinHandle<Result<(), Error>>>>>;

/// Feeds blocks and operations to the tezos protocol (ocaml code).
#[actor(FeedChainToProtocol)]
pub struct ChainFeeder {
    /// Thread where blocks are applied will run until this is set to `false`
    block_applier_run: Arc<AtomicBool>,
    /// Block applier thread
    block_applier_thread: SharedJoinHandle,
}

pub type ChainFeederRef = ActorRef<ChainFeederMsg>;

impl ChainFeeder {
    pub fn actor(sys: &impl ActorRefFactory, shell_channel: ShellChannelRef, rocks_db: Arc<rocksdb::DB>, tezos_init: &TezosStorageInitInfo, protocol_service: ProtocolService, log: Logger) -> Result<ChainFeederRef, CreateError> {
        let apply_block_run = Arc::new(AtomicBool::new(true));
        let block_applier_thread = {
            let apply_block_run = apply_block_run.clone();
            let current_head_hash = tezos_init.current_block_header_hash.clone();
            let chain_id = tezos_init.chain_id.clone();

            thread::spawn(move || {

                let block_storage = BlockStorage::new(rocks_db.clone());
                let mut block_meta_storage = BlockMetaStorage::new(rocks_db.clone());
                let operations_storage = OperationsStorage::new(rocks_db.clone());
                let operations_meta_storage = OperationsMetaStorage::new(rocks_db);
                let mut protocol_service = protocol_service;

                while apply_block_run.load(Ordering::Acquire) {
                    match feed_chain_to_protocol(
                        &chain_id,
                        &apply_block_run,
                        &current_head_hash,
                        &shell_channel,
                        &block_storage,
                        &mut block_meta_storage,
                        &operations_storage,
                        &operations_meta_storage,
                        &mut protocol_service,
                        &log,
                    ) {
                        Ok(()) => debug!(log, "Feed chain to protocol finished"),
                        Err(err) => error!(log, "Error while feeding chain to protocol"; "reason" => format!("{:?}", err)),
                    }
                }

                Ok(())
            })
        };

        let myself = sys.actor_of(
            Props::new_args(ChainFeeder::new, (apply_block_run, Arc::new(Mutex::new(Some(block_applier_thread))))),
            ChainFeeder::name())?;

        Ok(myself)
    }

    /// The `ChainFeeder` is intended to serve as a singleton actor so that's why
    /// we won't support multiple names per instance.
    fn name() -> &'static str {
        "chain-feeder"
    }

    fn new((block_applier_run, block_applier_thread): (Arc<AtomicBool>, SharedJoinHandle)) -> Self {
        ChainFeeder {
            block_applier_run,
            block_applier_thread,
        }
    }
}

impl Actor for ChainFeeder {
    type Msg = ChainFeederMsg;

    fn pre_start(&mut self, ctx: &Context<Self::Msg>) {
        ctx.schedule::<Self::Msg, _>(
            Duration::from_secs(15),
            Duration::from_secs(60),
            ctx.myself(),
            None,
            FeedChainToProtocol.into());
    }

    fn post_stop(&mut self) {
        // Set the flag, and let the thread wake up. There is no race condition here, if `unpark`
        // happens first, `park` will return immediately. Hence there is no risk of a deadlock.
        self.block_applier_run.store(false, Ordering::Release);

        let join_handle = self.block_applier_thread.lock().unwrap()
            .take().expect("Thread join handle is missing");
        join_handle.thread().unpark();
        let _ = join_handle.join().expect("Failed to join block applier thread");
    }

    fn recv(&mut self, ctx: &Context<Self::Msg>, msg: Self::Msg, sender: Sender) {
        self.receive(ctx, msg, sender);
    }
}

impl Receive<FeedChainToProtocol> for ChainFeeder {
    type Msg = ChainFeederMsg;

    fn receive(&mut self, _ctx: &Context<Self::Msg>, _msg: FeedChainToProtocol, _sender: Sender) {
        if let Some(join_handle) = self.block_applier_thread.lock().unwrap().as_ref() {
            join_handle.thread().unpark();
        }
    }
}


fn feed_chain_to_protocol(
    chain_id: &ChainId,
    apply_block_run: &AtomicBool,
    current_head_hash: &BlockHash,
    shell_channel: &ShellChannelRef,
    block_storage: &BlockStorage,
    block_meta_storage: &mut BlockMetaStorage,
    operations_storage: &OperationsStorage,
    operations_meta_storage: &OperationsMetaStorage,
    protocol_service: &mut ProtocolService,
    log: &Logger,
) -> Result<(), Error> {
    let block_hash_encoding = HashEncoding::new(HashType::BlockHash);
    let mut current_head_hash = current_head_hash.clone();

    let mut protocol_wrapper_ipc = protocol_service.spawn_protocol_wrapper()?;
    init_protocol_env(&mut protocol_wrapper_ipc, protocol_service.configuration())?;

    while apply_block_run.load(Ordering::Acquire) {
        match block_meta_storage.get(&current_head_hash)? {
            Some(mut current_head_meta) => {
                if current_head_meta.is_applied {
                    // Current head is already applied, so we should move to successor
                    // or in case no successor is available do nothing.
                    match current_head_meta.successor {
                        Some(successor_hash) => {
                            current_head_hash = successor_hash;
                            continue;
                        }
                        None => ( /* successor is not yet available, we do nothing for now */ )
                    }
                } else {
                    // Current head is not applied, so we should apply it now.
                    // But first let's fetch current head data from block storage..
                    match block_storage.get(&current_head_hash)? {
                        Some(current_head) => {
                            // Good, we have block data available, let's' look is we have all operations
                            // available. If yes we will apply them. If not, we will do nothing.
                            if operations_meta_storage.is_complete(&current_head.hash)? {
                                info!(log, "Applying block"; "block_header_hash" => block_hash_encoding.bytes_to_string(&current_head.hash));
                                let operations = operations_storage.get_operations(&current_head_hash)?
                                    .drain(..)
                                    .map(Some)
                                    .collect();
                                // apply block and it's operations
                                let apply_block_result = protocol_wrapper_ipc.apply_block(&chain_id, &current_head.hash, &current_head.header, &operations)?;
                                info!(log, "Block was applied";"block_header_hash" => block_hash_encoding.bytes_to_string(&current_head.hash), "validation_result_message" => apply_block_result.validation_result_message);
                                // mark current head as applied
                                current_head_meta.is_applied = true;
                                block_meta_storage.put(&current_head.hash, &current_head_meta)?;
                                // notify others that the block successfully applied
                                shell_channel.tell(
                                    Publish {
                                        msg: BlockApplied {
                                            hash: current_head.hash.clone(),
                                            level: current_head.header.level(),
                                            header: current_head.header.clone(),
                                        }.into(),
                                        topic: ShellChannelTopic::ShellEvents.into(),
                                    }, None);

                                // Current head is already applied, so we should move to successor
                                // or in case no successor is available do nothing.
                                match current_head_meta.successor {
                                    Some(successor_hash) => {
                                        current_head_hash = successor_hash;
                                        continue;
                                    }
                                    None => ( /* successor is not yet available, we do nothing for now */ )
                                }
                            } else {
                                // we don't have all operations available, do nothing
                            }
                        }
                        None => ( /* it's possible that data was not yet written do the storage, so don't panic! */ )
                    }
                }
            }
            None => warn!(log, "No meta info record was found in database for the current head"; "block_header_hash" => block_hash_encoding.bytes_to_string(&current_head_hash))
        }

        // This should be hit only in case that the current branch is applied
        // and no successor was available to continue the apply cycle. In that case
        // this thread will be stopped and will wait until it's waked again.
        thread::park();
    }

    Ok(())
}

fn init_protocol_env(protocol_wrapper: &mut ProtocolWrapperIpc, configuration: &ProtocolServiceConfiguration) -> Result<(), ProtocolServiceError>{
    protocol_wrapper.change_runtime_configuration(configuration.runtime_configuration().clone())?;
    protocol_wrapper.init_storage(configuration.data_dir().to_str().unwrap().to_string(), configuration.environment())?;
    Ok(())
}