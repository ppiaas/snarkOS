// Copyright (C) 2019-2021 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use crate::{
    helpers::{State, Status, Tasks},
    Environment,
    LedgerReader,
    LedgerRequest,
    LedgerRouter,
    Message,
    NodeType,
    PeersRequest,
    PeersRouter,
};
use snarkos_storage::{storage::Storage, ProverState};
use snarkvm::dpc::prelude::*;

use anyhow::Result;
use rand::thread_rng;
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::{
    net::SocketAddr,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant
};
use tokio::{
    sync::{mpsc, oneshot, RwLock},
    task,
    task::JoinHandle,
};

/// Shorthand for the parent half of the `Prover` message channel.
pub(crate) type ProverRouter<N> = mpsc::Sender<ProverRequest<N>>;
#[allow(unused)]
/// Shorthand for the child half of the `Prover` message channel.
type ProverHandler<N> = mpsc::Receiver<ProverRequest<N>>;

///
/// An enum of requests that the `Prover` struct processes.
///
#[derive(Debug)]
pub enum ProverRequest<N: Network> {
    /// MemoryPoolClear := (block)
    MemoryPoolClear(Option<Block<N>>),
    /// UnconfirmedTransaction := (peer_ip, transaction)
    UnconfirmedTransaction(SocketAddr, Transaction<N>),
}

///
/// A prover for a specific network on the node server.
///
#[derive(Debug)]
pub struct Prover<N: Network, E: Environment> {
    /// The state storage of the prover.
    state: Arc<ProverState<N>>,
    /// The thread pool for the miner.
    miner: Arc<ThreadPool>,
    /// The prover router of the node.
    prover_router: ProverRouter<N>,
    /// The pool of unconfirmed transactions.
    memory_pool: Arc<RwLock<MemoryPool<N>>>,
    /// The status of the node.
    status: Status,
    /// A terminator bit for the prover.
    terminator: Arc<AtomicBool>,
    /// The peers router of the node.
    peers_router: PeersRouter<N, E>,
    /// The ledger state of the node.
    ledger_reader: LedgerReader<N>,
    /// The ledger router of the node.
    ledger_router: LedgerRouter<N>,
    /// The current coinbase transaction template that is being mined on by the operator.
    coinbase_template: RwLock<Option<CoinbaseTemplate<N>>>,
}

#[derive(Clone, Debug)]
struct CoinbaseTemplate<N: Network> {
    pub transaction: Transaction<N>,
    pub record: Record<N>,
}

impl<N: Network, E: Environment> Prover<N, E> {
    /// Initializes a new instance of the prover.
    pub async fn open<S: Storage, P: AsRef<Path> + Copy>(
        tasks: &mut Tasks<JoinHandle<()>>,
        path: P,
        miner: Option<Address<N>>,
        local_ip: SocketAddr,
        status: &Status,
        terminator: &Arc<AtomicBool>,
        peers_router: PeersRouter<N, E>,
        ledger_reader: LedgerReader<N>,
        ledger_router: LedgerRouter<N>,
    ) -> Result<Arc<Self>> {
        // Initialize an mpsc channel for sending requests to the `Prover` struct.
        let (prover_router, mut prover_handler) = mpsc::channel(1024);
        // Initialize the prover pool.
        let pool = ThreadPoolBuilder::new()
            .stack_size(8 * 1024 * 1024)
            .num_threads((num_cpus::get() / 8 * 7).max(1))
            .build()?;

        // Initialize the prover.
        let prover = Arc::new(Self {
            state: Arc::new(ProverState::open_writer::<S, P>(path)?),
            miner: Arc::new(pool),
            prover_router,
            memory_pool: Arc::new(RwLock::new(MemoryPool::new())),
            status: status.clone(),
            terminator: terminator.clone(),
            peers_router,
            ledger_reader,
            ledger_router,
            coinbase_template: RwLock::new(None),
        });

        // Initialize the handler for the prover.
        {
            let prover = prover.clone();
            let (router, handler) = oneshot::channel();
            tasks.append(task::spawn(async move {
                // Notify the outer function that the task is ready.
                let _ = router.send(());
                // Asynchronously wait for a prover request.
                while let Some(request) = prover_handler.recv().await {
                    // Hold the prover write lock briefly, to update the state of the prover.
                    prover.update(request).await;
                }
            }));
            // Wait until the prover handler is ready.
            let _ = handler.await;
        }

        // Initialize a new instance of the miner.
        if E::NODE_TYPE == NodeType::Miner {
            if let Some(recipient) = miner {
                // Initialize the prover process.
                let prover = prover.clone();
                let tasks_clone = tasks.clone();
                let (router, handler) = oneshot::channel();
                tasks.append(task::spawn(async move {
                    // Notify the outer function that the task is ready.
                    let _ = router.send(());
                    loop {
                        // If `terminator` is `false` and the status is not `Peering` or `Mining` already, mine the next block.
                        if !prover.terminator.load(Ordering::SeqCst) && !prover.status.is_peering() && !prover.status.is_mining() {
                            // Set the status to `Mining`.
                            prover.status.update(State::Mining);

                            // Prepare the unconfirmed transactions, terminator, and status.
                            let state = prover.state.clone();
                            let miner = prover.miner.clone();
                            let canon = prover.ledger_reader.clone(); // This is *safe* as the ledger only reads.
                            let _unconfirmed_transactions = prover.memory_pool.read().await.transactions();
                            let terminator = prover.terminator.clone();
                            let status = prover.status.clone();
                            let ledger_router = prover.ledger_router.clone();
                            let prover_router = prover.prover_router.clone();
                            let prover = prover.clone();

                            tasks_clone.append(task::spawn(async move {
                                let coinbase_template = prover.coinbase_template.read().await.clone(); 

                                // Mine the next block.
                                let result = task::spawn_blocking(move || {
                                    miner.install(move || {
                                        // Prepare the new block.
                                        let previous_block_hash = canon.latest_block_hash();
                                        let block_height = canon.latest_block_height() + 1;
                                
                                        // Compute the block difficulty target.
                                        let previous_timestamp = canon.latest_block_timestamp();
                                        let previous_difficulty_target = canon.latest_block_difficulty_target();
                                
                                        // Ensure that the new timestamp is ahead of the previous timestamp.
                                        let block_timestamp = std::cmp::max(chrono::Utc::now().timestamp(), previous_timestamp.saturating_add(1));
                                
                                        let difficulty_target = Blocks::<N>::compute_difficulty_target(previous_timestamp, previous_difficulty_target, block_timestamp);
                                        let cumulative_weight = canon
                                            .latest_cumulative_weight()
                                            .saturating_add((u64::MAX / difficulty_target) as u128);
                                
                                        // Construct the ledger root.
                                        let ledger_root = canon.latest_ledger_root();

                                        // Craft a coinbase transaction.
                                        let new_coinbase_start = Instant::now();
                                        let amount = Block::<N>::block_reward(block_height);

                                        let (coinbase_transaction, coinbase_record) = if let Some(coinbase_template) = coinbase_template {
                                            trace!("Reuse coinbase transaction {}, height {}", coinbase_template.transaction.transaction_id(), block_height);
                                            (coinbase_template.transaction, coinbase_template.record)
                                        } else {
                                            Transaction::<N>::new_coinbase(recipient, amount, E::COINBASE_IS_PUBLIC, &mut thread_rng()).expect("coinbase transaction")
                                        };
                                        
                                        let coinbase_transaction_inner_circuit_id = coinbase_transaction.inner_circuit_id();
                                        let coinbase_transaction_ledger_root = coinbase_transaction.ledger_root();
                                        let coinbase_transaction_transitions = coinbase_transaction.transitions().clone();

                                        trace!("Execute coinbase transaction time: {:?}, height: {}, timestamp: {}, difficulty: {}, weight: {}", new_coinbase_start.elapsed(), block_height, block_timestamp, difficulty_target, cumulative_weight);
                                        
                                        // Construct the new block transactions.
                                        let transactions = Transactions::from(&[coinbase_transaction]).expect("Failed to");
                                        
                                        // Mine the next block.
                                        match Block::mine(
                                            previous_block_hash,
                                            block_height,
                                            block_timestamp,
                                            difficulty_target,
                                            cumulative_weight,
                                            ledger_root,
                                            transactions,
                                            &terminator,
                                            &mut thread_rng(),
                                        ) {
                                            Ok(block) => {
                                                Ok((block, coinbase_record))
                                            },
                                            Err(error) => {
                                                trace!("Unable to mine the next block: {}", error);
                                                Err((Transaction::from(coinbase_transaction_inner_circuit_id, coinbase_transaction_ledger_root, coinbase_transaction_transitions).expect("transaction"), coinbase_record, block_height))
                                            },
                                        }
                                    })
                                })
                                .await;

                                // Set the status to `Ready`.
                                status.update(State::Ready);

                                match result {
                                    Ok(Ok((block, coinbase_record))) => {
                                        *prover.coinbase_template.write().await = None;
                                        debug!("Miner has found unconfirmed block {} ({})", block.height(), block.hash());
                                        // Store the coinbase record.
                                        if let Err(error) = state.add_coinbase_record(block.height(), coinbase_record) {
                                            warn!("[Miner] Failed to store coinbase record - {}", error);
                                        }

                                        // Broadcast the next block.
                                        let request = LedgerRequest::UnconfirmedBlock(local_ip, block, prover_router.clone());
                                        if let Err(error) = ledger_router.send(request).await {
                                            warn!("Failed to broadcast mined block - {}", error);
                                        }
                                    },
                                    Ok(Err((coinbase_transaction, coinbase_record, block_height))) => {
                                        trace!("Save coinbase transaction {}, height {}", coinbase_transaction.transaction_id(), block_height);
                                        *prover.coinbase_template.write().await = Some(CoinbaseTemplate{
                                            transaction: coinbase_transaction,
                                            record: coinbase_record,
                                        });
                                    },
                                    Err(error) => trace!("{}", error),
                                }
                            }));
                        }
                        // Sleep for 2 seconds.
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }));
                // Wait until the miner task is ready.
                let _ = handler.await;
            } else {
                error!("Missing miner address. Please specify an Aleo address in order to mine");
            }
        }

        Ok(prover)
    }

    /// Returns an instance of the prover router.
    pub fn router(&self) -> ProverRouter<N> {
        self.prover_router.clone()
    }

    /// Returns an instance of the memory pool.
    pub(crate) fn memory_pool(&self) -> Arc<RwLock<MemoryPool<N>>> {
        self.memory_pool.clone()
    }

    /// Returns all coinbase records in storage.
    pub fn to_coinbase_records(&self) -> Vec<(u32, Record<N>)> {
        self.state.to_coinbase_records()
    }

    ///
    /// Performs the given `request` to the prover.
    /// All requests must go through this `update`, so that a unified view is preserved.
    ///
    pub(super) async fn update(&self, request: ProverRequest<N>) {
        match request {
            ProverRequest::MemoryPoolClear(block) => match block {
                Some(block) => self.memory_pool.write().await.remove_transactions(block.transactions()),
                None => *self.memory_pool.write().await = MemoryPool::new(),
            },
            ProverRequest::UnconfirmedTransaction(peer_ip, transaction) => {
                // Ensure the node is not peering.
                if !self.status.is_peering() {
                    // Process the unconfirmed transaction.
                    self.add_unconfirmed_transaction(peer_ip, transaction).await
                }
            }
        }
    }

    ///
    /// Adds the given unconfirmed transaction to the memory pool.
    ///
    async fn add_unconfirmed_transaction(&self, peer_ip: SocketAddr, transaction: Transaction<N>) {
        // Process the unconfirmed transaction.
        trace!("Received unconfirmed transaction {} from {}", transaction.transaction_id(), peer_ip);
        // Ensure the unconfirmed transaction is new.
        if let Ok(false) = self.ledger_reader.contains_transaction(&transaction.transaction_id()) {
            debug!("Adding unconfirmed transaction {} to memory pool", transaction.transaction_id());
            // Attempt to add the unconfirmed transaction to the memory pool.
            match self.memory_pool.write().await.add_transaction(&transaction) {
                Ok(()) => {
                    // Upon success, propagate the unconfirmed transaction to the connected peers.
                    let request = PeersRequest::MessagePropagate(peer_ip, Message::UnconfirmedTransaction(transaction));
                    if let Err(error) = self.peers_router.send(request).await {
                        warn!("[UnconfirmedTransaction] {}", error);
                    }
                }
                Err(error) => error!("{}", error),
            }
        }
    }
}
