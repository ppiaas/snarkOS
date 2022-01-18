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
    helpers::{NodeType, State},
    Data, Environment, Message, Node, OutboundRouter, Peer,
};
use rayon::ThreadPoolBuilder;
use snarkvm::dpc::prelude::*;
use snarkvm::dpc::testnet2::V12_UPGRADE_BLOCK_HEIGHT;

use anyhow::{anyhow, Result};
use metrics::{gauge, increment_counter};
use rand::{thread_rng, CryptoRng, Rng};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, RwLock},
    task,
    time::timeout,
};
use tokio_stream::StreamExt;

/// Shorthand for the parent half of the `Worker` message channel.
pub(crate) type WorkerRouter<N, E> = mpsc::Sender<WorkerRequest<N, E>>;
#[allow(unused)]
/// Shorthand for the child half of the `Worker` message channel.
type WorkerHandler<N, E> = mpsc::Receiver<WorkerRequest<N, E>>;

/// Shorthand for the parent half of the connection result channel.
type ConnectionResult = oneshot::Sender<Result<()>>;

///
/// An enum of requests that the `Worker` struct processes.
///
#[derive(Debug)]
pub enum WorkerRequest<N: Network, E: Environment> {
    /// Connect := (peer_ip, connection_result)
    Connect(SocketAddr, ConnectionResult),
    /// Connecting := (stream, peer_ip)
    Connecting(TcpStream, SocketAddr),
    /// Connected := (peer_ip, node_type, outbound_router)
    Connected(SocketAddr, NodeType, OutboundRouter<N, E>),
    /// Disconnected := (peer_ip)
    Disconnected(SocketAddr),
    /// MessageSend := (peer_ip, message)
    MessageSend(SocketAddr, Message<N, E>),
    /// Failure := (peer_ip, failure)
    Failure(SocketAddr, String),
    /// ConfirmedBlock := (peer_ip, node_type, ledger_root, block)
    ConfirmedBlock(SocketAddr, NodeType, N::LedgerRoot, Block<N>),
    /// UnconfirmedBlock := (peer_ip, node_type, block)
    UnconfirmedBlock(SocketAddr, NodeType, Block<N>),
}

///
/// A worker for a specific network on the node server.
///
pub struct Worker<N: Network, E: Environment> {
    /// The worker router of the node.
    worker_router: WorkerRouter<N, E>,
    /// The local address of this node.
    local_ip: SocketAddr,
    /// The map connected peer IPs to their nonce and outbound message router.
    connected_peers: RwLock<HashMap<SocketAddr, (NodeType, OutboundRouter<N, E>)>>,
    // The state of worker.
    state: Arc<WorkerState<N>>,
}

pub struct WorkerState<N: Network> {
    /// The latest block of the worker.
    latest_block: parking_lot::RwLock<Block<N>>,
    /// The latest ledger root of the worker.
    latest_ledger_root: parking_lot::RwLock<N::LedgerRoot>,
    /// The current coinbase transaction template that is being mined on by the operator.
    coinbase_template: parking_lot::RwLock<Option<CoinbaseTemplate<N>>>,
}

impl<N: Network> WorkerState<N> {
    /// Returns the latest block.
    pub fn latest_block(&self) -> Block<N> {
        self.latest_block.read().clone()
    }

    /// Returns the latest ledger root.
    pub fn latest_ledger_root(&self) -> N::LedgerRoot {
        self.latest_ledger_root.read().clone()
    }

    /// Returns a coinbase template based on the latest state of the ledger.
    pub fn get_coinbase_template(&self) -> Option<CoinbaseTemplate<N>> {
        self.coinbase_template.read().clone()
    }

    /// Set the latest coinbase template.
    pub fn set_coinbase_template(&self, coinbase: Option<CoinbaseTemplate<N>>) {
        *self.coinbase_template.write() = coinbase.clone();
    }

    /// Returns a block template based on the latest state of the ledger.
    pub fn get_block_template<R: Rng + CryptoRng>(
        &self,
        recipient: Address<N>,
        is_public: bool,
        _transactions: &[Transaction<N>],
        rng: &mut R,
    ) -> Result<BlockTemplate<N>> {
        // Fetch the latest state of the ledger.
        let latest_block = self.latest_block();
        let previous_ledger_root = self.latest_ledger_root();

        // Prepare the new block.
        let previous_block_hash = latest_block.hash();
        let block_height = latest_block.height().saturating_add(1);
        // Ensure that the new timestamp is ahead of the previous timestamp.
        let block_timestamp = latest_block.timestamp().saturating_add(1);

        // Compute the block difficulty target.
        let difficulty_target = if N::NETWORK_ID == 2 && block_height <= V12_UPGRADE_BLOCK_HEIGHT {
            Blocks::<N>::compute_difficulty_target(latest_block.header(), block_timestamp, block_height)
        } else if N::NETWORK_ID == 2 {
            Blocks::<N>::asert_retarget(
                1640764673,
                20926491045728,
                V12_UPGRADE_BLOCK_HEIGHT,
                block_timestamp,
                block_height,
                N::ALEO_BLOCK_TIME_IN_SECS,
            )
        } else {
            Blocks::<N>::compute_difficulty_target(N::genesis_block().header(), block_timestamp, block_height)
        };

        // Compute the cumulative weight.
        let cumulative_weight = latest_block
            .cumulative_weight()
            .saturating_add((u64::MAX / difficulty_target) as u128);

        // Compute the coinbase reward (not including the transaction fees).
        let mut coinbase_reward = Block::<N>::block_reward(block_height);
        let transaction_fees = AleoAmount::ZERO;

        // Filter the transactions to ensure they are new, and append the coinbase transaction.
        let mut transactions: Vec<Transaction<N>> = vec![];

        // Calculate the final coinbase reward (including the transaction fees).
        coinbase_reward = coinbase_reward.add(transaction_fees);

        // Craft a coinbase transaction, and append it to the list of transactions.
        if self.get_coinbase_template().is_none() {
            let new_coinbase_start = Instant::now();
            self.set_coinbase_template(Some(CoinbaseTemplate::from(Transaction::<N>::new_coinbase(
                recipient,
                coinbase_reward,
                is_public,
                rng,
            )?)));
            trace!(
                "Execute coinbase transaction time: {:?}, height: {}, timestamp: {}, difficulty: {}, weight: {}",
                new_coinbase_start.elapsed(),
                block_height,
                block_timestamp,
                difficulty_target,
                cumulative_weight
            );
        }
        let coinbase_template = self.get_coinbase_template().unwrap();
        let (coinbase_transaction, coinbase_record) = (coinbase_template.transaction(), coinbase_template.record());
        transactions.push(coinbase_transaction);

        // Construct the new block transactions.
        let transactions = Transactions::from(&transactions)?;

        // Construct the block template.
        Ok(BlockTemplate::new(
            previous_block_hash,
            block_height,
            block_timestamp,
            difficulty_target,
            cumulative_weight,
            previous_ledger_root,
            transactions,
            coinbase_record,
        ))
    }

    pub fn add_block(&self, ledger_root: N::LedgerRoot, block: &Block<N>) -> bool {
        if block.cumulative_weight() <= self.latest_block.read().cumulative_weight() {
            return false;
        }

        let mut latest_block = self.latest_block.write();
        if block.cumulative_weight() <= latest_block.cumulative_weight() {
            return false;
        }

        *latest_block = block.clone();
        *self.latest_ledger_root.write() = ledger_root;

        true
    }

    /// Mines a new block using the latest state of the given ledger.
    pub fn mine_next_block<R: Rng + CryptoRng>(
        &self,
        recipient: Address<N>,
        is_public: bool,
        transactions: &[Transaction<N>],
        terminator: &AtomicBool,
        rng: &mut R,
    ) -> Result<(Block<N>, Record<N>)> {
        let template = self.get_block_template(recipient, is_public, transactions, rng)?;
        let coinbase_record = template.coinbase_record().clone();

        gauge!("snarkos_blocks_mining", template.block_height() as f64);

        // Mine the next block.
        match Block::mine(&template, terminator, rng) {
            Ok(block) => {
                self.set_coinbase_template(None);
                Ok((block, coinbase_record))
            }
            Err(error) => Err(anyhow!("Unable to mine the next block: {}", error)),
        }
    }
}

impl<N: Network, E: Environment> Worker<N, E> {
    ///
    /// Initializes a new instance of `Worker`.
    ///
    pub(crate) async fn new(local_ip: SocketAddr, miner: Address<N>) -> Result<Arc<Self>> {
        // Initialize an mpsc channel for sending requests to the `Worker` struct.
        let (worker_router, mut worker_handler) = mpsc::channel(1024);

        let state = Arc::new(WorkerState {
            latest_block: parking_lot::RwLock::new(N::genesis_block().clone()),
            latest_ledger_root: Default::default(),
            coinbase_template: Default::default(),
        });

        // Initialize the worker.
        let worker = Arc::new(Self {
            worker_router,
            local_ip,
            connected_peers: Default::default(),
            state,
        });

        let thread_pool = ThreadPoolBuilder::new()
            .stack_size(8 * 1024 * 1024)
            .num_threads(num_cpus::get().max(1))
            .build()?;

        let thread_pool = Arc::new(thread_pool);

        // Initialize the worker router process.
        {
            let worker_clone = worker.clone();
            let (router, handler) = oneshot::channel();
            E::tasks().append(task::spawn(async move {
                // Notify the outer function that the task is ready.
                let _ = router.send(());
                // Asynchronously wait for a worker request.
                while let Some(request) = worker_handler.recv().await {
                    let worker = worker_clone.clone();
                    E::tasks().append(task::spawn(async move {
                        // Hold the worker write lock briefly, to update the state of the worker.
                        worker.update(request).await;
                    }));
                }
            }));
            // Wait until the worker router task is ready.
            let _ = handler.await;

            let (router, handler) = oneshot::channel();
            let worker_clone = worker.clone();
            E::tasks().append(task::spawn(async move {
                // Notify the outer function that the task is ready.
                let _ = router.send(());
                loop {
                    let state = worker_clone.state.clone();
                    let thread_pool = thread_pool.clone();
                    // Set the status to `Mining`.
                    E::status().update(State::Mining);

                    let result = task::spawn_blocking(move || {
                        thread_pool.install(move || {
                            state.mine_next_block(
                                miner,
                                E::COINBASE_IS_PUBLIC,
                                &vec![],
                                E::terminator(),
                                &mut thread_rng(),
                            )
                        })
                    })
                    .await
                    .map_err(|e| e.into());

                    match result {
                        Ok(Ok((block, _))) => {
                            increment_counter!("snarkos_blocks_mined_total");
                            gauge!("snarkos_blocks_mined", block.height() as f64);
                            debug!(
                                "Miner has found unconfirmed block {} ({}) (timestamp = {}, cumulative_weight = {}, difficulty_target = {})",
                                block.height(),
                                block.hash(),
                                block.timestamp(),
                                block.cumulative_weight(),
                                block.difficulty_target()
                            );

                            // Broadcast the next block.
                            let request = WorkerRequest::UnconfirmedBlock(local_ip, E::NODE_TYPE, block);
                            // worker.worker_router.send(request).await.context("broadcast mined block")?;
                            if let Err(error) = worker_clone.worker_router.send(request).await {
                                warn!("Failed to broadcast mined block - {}", error);
                            }
                            E::status().update(State::Ready);
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                        Ok(Err(error)) | Err(error) => {
                            error!("{}", error);
                            E::status().update(State::Ready);
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        },
                    };
                    E::terminator().store(false, Ordering::SeqCst);
                }
            }));
            // Wait until the worker router task is ready.
            let _ = handler.await;
        }

        Ok(worker)
    }

    /// Returns an instance of the worker router.
    pub fn router(&self) -> WorkerRouter<N, E> {
        self.worker_router.clone()
    }

    pub(super) async fn shut_down(&self) {
        debug!("Worker is shutting down...");

        // Disconnect all connected peers.
        let connected_peers = self.connected_peers.read().await.keys().copied().collect::<Vec<_>>();
        for peer_ip in connected_peers {
            self.disconnect(peer_ip, "shutting down").await;
        }
        trace!("[ShuttingDown] Disconnect message has been sent to all connected peers");
    }

    ///
    /// Returns `true` if the node is connected to the given IP.
    ///
    pub async fn is_connected_to(&self, ip: SocketAddr) -> bool {
        self.connected_peers.read().await.contains_key(&ip)
    }

    ///
    /// Returns the list of connected peers.
    ///
    pub async fn connected_peers(&self) -> Vec<SocketAddr> {
        self.connected_peers.read().await.keys().copied().collect()
    }

    ///
    /// Returns the number of connected peers.
    ///
    pub async fn number_of_connected_peers(&self) -> usize {
        self.connected_peers.read().await.len()
    }

    ///
    /// Performs the given `request` to the worker.
    /// All requests must go through this `update`, so that a unified view is preserved.
    ///
    pub(super) async fn update(&self, request: WorkerRequest<N, E>) {
        match request {
            WorkerRequest::Connect(peer_ip, connection_result) => {
                // Ensure the peer IP is not this node.
                if peer_ip == self.local_ip
                    || (peer_ip.ip().is_unspecified() || peer_ip.ip().is_loopback()) && peer_ip.port() == self.local_ip.port()
                {
                    debug!("Skipping connection request to {} (attempted to self-connect)", peer_ip);
                    return;
                }
                // Ensure the node does not surpass the maximum number of peer connections.
                else if self.number_of_connected_peers().await >= E::MAXIMUM_NUMBER_OF_PEERS {
                    debug!("Skipping connection request to {} (maximum peers reached)", peer_ip);
                    return;
                }
                // Ensure the peer is a new connection.
                if self.is_connected_to(peer_ip).await {
                    debug!("Skipping connection request to {} (already connected)", peer_ip);
                    return;
                }
                // Initialize the peer handler.
                match timeout(Duration::from_millis(E::CONNECTION_TIMEOUT_IN_MILLIS), TcpStream::connect(peer_ip)).await {
                    Ok(Ok(stream)) => CoordinatedPeer::handler(stream, self.local_ip, &self.worker_router, Some(connection_result)).await,
                    Ok(Err(error)) => {
                        error!("Failed to connect to '{}': '{:?}'", peer_ip, error);
                    }
                    Err(error) => {
                        error!("Unable to reach '{}': '{:?}'", peer_ip, error);
                    }
                };
            }
            WorkerRequest::Connecting(stream, peer_ip) => {
                // Ensure the peer IP is not this node.
                if peer_ip == self.local_ip
                    || (peer_ip.ip().is_unspecified() || peer_ip.ip().is_loopback()) && peer_ip.port() == self.local_ip.port()
                {
                    debug!("Skipping connection request to {} (attempted to self-connect)", peer_ip);
                    return;
                }
                // Ensure the node does not surpass the maximum number of peer connections.
                if self.number_of_connected_peers().await >= E::MAXIMUM_NUMBER_OF_PEERS {
                    debug!("Dropping connection request from {} (maximum peers reached)", peer_ip);
                    return;
                }
                // Ensure the node is not already connected to this peer.
                if self.is_connected_to(peer_ip).await {
                    debug!("Dropping connection request from {} (already connected)", peer_ip);
                    return;
                }
                // Initialize the peer handler.
                CoordinatedPeer::handler(stream, self.local_ip, &self.worker_router, None).await;
            }
            WorkerRequest::Connected(peer_ip, node_type, outbound) => {
                // Add an entry for this `Peer` in the connected peers.
                self.connected_peers.write().await.insert(peer_ip, (node_type, outbound));
            }
            WorkerRequest::Disconnected(peer_ip) => {
                // Remove an entry for this `Peer` in the connected peers, if it exists.
                self.connected_peers.write().await.remove(&peer_ip);
            }
            WorkerRequest::MessageSend(sender, message) => {
                self.send(sender, message).await;
            }
            WorkerRequest::Failure(peer_ip, failure) => {
                error!("Failure {} from Peer {}", failure, peer_ip);
            }
            WorkerRequest::ConfirmedBlock(peer_ip, _, ledger_root, block) => {
                if !block.is_valid() {
                    error!("ConfirmedBlock {} ({}) is invalid", block.height(), block.hash());
                    return;
                }

                if self.state.add_block(ledger_root, &block) {
                    E::terminator().store(true, Ordering::SeqCst);
                    gauge!("snarkos_state_latest_block", block.height() as f64);
                    info!(
                        "Canonical block {} ({}) (cumulative_weight = {}, connected_peers = {}) from Peer {}",
                        block.height(),
                        block.hash(),
                        block.cumulative_weight(),
                        self.number_of_connected_peers().await,
                        peer_ip
                    );
                } else {
                    trace!(
                        "ConfirmedBlock {} ({}) (cumulative_weight = {}) from Peer {}",
                        block.height(),
                        block.hash(),
                        block.cumulative_weight(),
                        peer_ip
                    );
                }
            }
            WorkerRequest::UnconfirmedBlock(peer_ip, _, block) => {
                if peer_ip == self.local_ip {
                    debug!(
                        "Propagating mined block {} ({}) (timestamp = {}, cumulative_weight = {}, difficulty_target = {})",
                        block.height(),
                        block.hash(),
                        block.timestamp(),
                        block.cumulative_weight(),
                        block.difficulty_target()
                    );
                }
                let message = Message::UnconfirmedBlock(block.height(), block.hash(), Data::Object(block));
                self.propagate(peer_ip, message, |_, node_type| *node_type != NodeType::Worker)
                    .await;
            }
        }
    }

    ///
    /// Disconnects the given peer from the worker.
    ///
    async fn disconnect(&self, peer_ip: SocketAddr, message: &str) {
        info!("Disconnecting from {} ({})", peer_ip, message);
        // Send a `Disconnect` message to the peer.
        let request = WorkerRequest::MessageSend(peer_ip, Message::Disconnect);
        if let Err(error) = self.worker_router.send(request).await {
            error!("[Disconnect] {}", error);
        }
        // Route a `PeerDisconnected` to the worker.
        if let Err(error) = self.worker_router.send(WorkerRequest::Disconnected(peer_ip)).await {
            error!("[Disconnected] {}", error);
        }
    }

    ///
    /// Sends the given message to specified peer.
    ///
    async fn send(&self, peer: SocketAddr, message: Message<N, E>) {
        let target_peer = self.connected_peers.read().await.get(&peer).cloned();
        match target_peer {
            Some((_, outbound)) => {
                if let Err(error) = outbound.send(message).await {
                    error!("Outbound channel failed: {}", error);
                    self.connected_peers.write().await.remove(&peer);
                }
            }
            None => error!("Attempted to send to a non-connected peer {}", peer),
        }
    }

    ///
    /// Sends the given message to connected peers with filter, excluding the sender.
    ///
    async fn propagate<F>(&self, sender: SocketAddr, mut message: Message<N, E>, filter: F)
    where
        F: Fn(&SocketAddr, &NodeType) -> bool,
    {
        // Perform ahead-of-time, non-blocking serialization just once for applicable objects.
        if let Message::UnconfirmedBlock(_, _, ref mut data) = message {
            let serialized_block = Data::serialize(data.clone()).await.expect("Block serialization is bugged");
            let _ = std::mem::replace(data, Data::Buffer(serialized_block));
        }

        for (peer_ip, (_node_type, _outbound)) in self
            .connected_peers
            .read()
            .await
            .iter()
            .filter(|(peer_ip, (node_type, _outbound))| *peer_ip != &sender && filter(peer_ip, node_type))
        {
            self.send(peer_ip.clone(), message.clone()).await;
        }
    }
}

///
/// The state for each connected Peer.
///
struct CoordinatedPeer {}

impl CoordinatedPeer {
    /// A handler to process an individual peer.
    #[allow(clippy::too_many_arguments)]
    async fn handler<N: Network, E: Environment>(
        stream: TcpStream,
        local_ip: SocketAddr,
        worker_router: &WorkerRouter<N, E>,
        connection_result: Option<ConnectionResult>,
    ) {
        let local_nonce = thread_rng().gen();
        let worker_router = worker_router.clone();
        let latest_block = N::genesis_block();

        E::tasks().append(task::spawn(async move {
            // Create a channel for this peer.
            let (outbound_router, outbound_handler) = mpsc::channel(1024);

            // Register our peer with state which internally sets up some channels.
            let mut peer = match Peer::open(
                stream,
                local_ip,
                local_nonce,
                latest_block.cumulative_weight(),
                latest_block.hash(),
                latest_block.header().clone(),
                outbound_handler,
                &vec![],
            )
            .await
            {
                Ok(peer) => {
                    // If the optional connection result router is given, report a successful connection result.
                    if let Some(router) = connection_result {
                        if router.send(Ok(())).is_err() {
                            error!("Failed to report a successful connection");
                        }
                    }
                    // Add an entry for this `Peer` in the connected peers.
                    let request = WorkerRequest::Connected(peer.peer_ip(), peer.node_type, outbound_router);
                    if let Err(error) = worker_router.send(request).await {
                        error!("{}", error);
                        return;
                    };
                    peer
                }
                Err(error) => {
                    trace!("{}", error);
                    // If the optional connection result router is given, report a failed connection result.
                    if let Some(router) = connection_result {
                        if router.send(Err(error)).is_err() {
                            error!("Failed to report a failed connection");
                        }
                    }
                    return;
                }
            };

            // Retrieve the peer IP.
            let peer_ip = peer.peer_ip();
            info!("Connected to {} ({})", peer_ip, peer.node_type);

            // Process incoming messages until this stream is disconnected.
            loop {
                tokio::select! {
                    Some(mut message) = peer.outbound_handler.recv() => {
                        // Ensure sufficient time has passed before needing to send the message.
                        let is_ready_to_send = match message {
                            Message::Ping(_, _, _, _, _, ref mut data) => {
                                // Perform non-blocking serialisation of the block header.
                                let serialized_header = Data::serialize(data.clone()).await.expect("Block header serialization is bugged");
                                let _ = std::mem::replace(data, Data::Buffer(serialized_header));

                                true
                            }
                            Message::UnconfirmedBlock(block_height, _block_hash, ref mut data) => {
                                trace!("Preparing to send 'UnconfirmedBlock {}' to {}", block_height, peer_ip);

                                // Perform non-blocking serialization of the block (if it hasn't been serialized yet).
                                let serialized_block = Data::serialize(data.clone()).await.expect("Block serialization is bugged");
                                let _ = std::mem::replace(data, Data::Buffer(serialized_block));

                                true
                            }
                            _ => false,
                        };
                        // Send the message if it is ready.
                        if is_ready_to_send {
                            // Route a message to the peer.
                            if let Err(error) = peer.send(message).await {
                                error!("[OutboundRouter] {}", error);
                            }
                        }
                    }

                    result = peer.outbound_socket.next() => match result {
                        Some(Ok(message)) => {
                            let message_name = message.name().to_string();
                            // Process the message.
                            trace!("Received '{}' from {}", message_name, peer_ip);
                            match Self::handle_message(&mut peer, peer_ip, message, &worker_router).await {
                                Ok(()) => { }
                                Err(error) => {
                                    error!("[{}] {}", message_name.to_string(), error);
                                    break;
                                }
                            };
                        }
                        // An error occurred.
                        Some(Err(error)) => error!("Failed to read message from {}: {}", peer_ip, error),
                        // The stream has been disconnected.
                        None => break,
                    }
                }
            }

            // When this is reached, it means the peer has disconnected.
            // Route a `Disconnect` to the worker.
            if let Err(error) = worker_router.send(WorkerRequest::Disconnected(peer_ip)).await {
                error!("[Peer::Disconnect] {}", error);
            }
        }));
    }

    /// A handler to process an individual peer.
    #[allow(clippy::too_many_arguments)]
    async fn handle_message<N: Network, E: Environment>(
        peer: &mut Peer<N, E>,
        peer_ip: SocketAddr,
        message: Message<N, E>,
        worker_router: &WorkerRouter<N, E>,
    ) -> Result<()> {
        let worker_router = worker_router.clone();
        match message {
            Message::Ping(version, fork_depth, node_type, status, block_hash, block_header) => {
                // Ensure the message protocol version is not outdated.
                if version < E::MESSAGE_VERSION {
                    return Err(anyhow!("Dropping {} on version {} (outdated)", peer_ip, version));
                }
                // Ensure the maximum fork depth is correct.
                if fork_depth != N::ALEO_MAXIMUM_FORK_DEPTH {
                    return Err(anyhow!(
                        "Dropping {} for an incorrect maximum fork depth of {}",
                        peer_ip,
                        fork_depth
                    ));
                }
                // Perform the deferred non-blocking deserialization of the block header.
                match block_header.deserialize().await {
                    Ok(block_header) => {
                        // TODO (howardwu): TEMPORARY - Remove this after testnet2.
                        // Sanity check for a V12 ledger.
                        if N::NETWORK_ID == 2
                            && block_header.height() > snarkvm::dpc::testnet2::V12_UPGRADE_BLOCK_HEIGHT
                            && block_header.proof().is_hiding()
                        {
                            return Err(anyhow!("Peer {} is not V12-compliant, proceeding to disconnect", peer_ip));
                        }

                        // Update the block header of the peer.
                        peer.block_header = block_header;
                    }
                    Err(error) => error!("[Ping] {}", error),
                }

                // Update the version of the peer.
                peer.version = version;
                // Update the node type of the peer.
                peer.node_type = node_type;
                // Update the status of the peer.
                peer.status().update(status);

                debug!(
                    "Peer {} is at block {} ({}) (type = {}, status = {}, fork_depth = {}, cumulative_weight = {})",
                    peer_ip,
                    peer.height(),
                    block_hash,
                    node_type,
                    status,
                    fork_depth,
                    peer.cumulative_weight(),
                );
            }
            Message::Pong(is_fork, block_locators) => {
                // Perform the deferred non-blocking deserialization of block locators.
                match block_locators.deserialize().await {
                    // Route the `Pong` to the worker.
                    Ok(block_locators) => {
                        let latest_block = (&*block_locators).iter().last();
                        if let Some((_height, (hash, Some(header)))) = latest_block {
                            peer.block_header = header.clone();
                            debug!(
                                "Peer {} is at block {} ({}) (type = {}, status = {}, is_fork = {}, cumulative_weight = {})",
                                peer_ip,
                                peer.height(),
                                hash,
                                peer.node_type,
                                peer.status(),
                                is_fork.is_some(),
                                peer.cumulative_weight(),
                            );
                        }
                    }
                    // Route the `Failure` to the worker.
                    Err(error) => {
                        // Route the request to the worker.
                        if let Err(error) = worker_router.send(WorkerRequest::Failure(peer_ip, format!("{}", error))).await {
                            error!("[Pong] {}", error);
                        }
                    }
                };

                E::tasks().append(task::spawn(async move {
                    // Sleep for the preset time before sending a `Ping` request.
                    tokio::time::sleep(Duration::from_secs(E::PING_SLEEP_IN_SECS)).await;

                    let latest_block_hash = N::genesis_block().hash();
                    let latest_block_header = N::genesis_block().header().clone();

                    // Send a `Ping` request to the peer.
                    let message = Message::Ping(
                        E::MESSAGE_VERSION,
                        N::ALEO_MAXIMUM_FORK_DEPTH,
                        E::NODE_TYPE,
                        E::status().get(),
                        latest_block_hash,
                        Data::Object(latest_block_header),
                    );
                    if let Err(error) = worker_router.send(WorkerRequest::MessageSend(peer_ip, message)).await {
                        error!("[Ping] {}", error);
                    }
                }));
            }
            Message::ConfirmedBlock(ledger_root, block) => {
                // Perform the deferred non-blocking deserialization of the block.
                match block.deserialize().await {
                    Ok(block) => {
                        if N::NETWORK_ID == 2 && block.height() > V12_UPGRADE_BLOCK_HEIGHT && block.header().proof().is_hiding() {
                            return Err(anyhow!("Peer {} is not V12-compliant, proceeding to disconnect", peer_ip));
                        }

                        // Route the `ConfirmedBlock` to the worker.
                        let request = WorkerRequest::ConfirmedBlock(peer_ip, peer.node_type, ledger_root, block);
                        if let Err(error) = worker_router.send(request).await {
                            error!("[ConfirmedBlock] {}", error);
                        }
                    }
                    // Route the `Failure` to the worker.
                    Err(error) => {
                        if let Err(error) = worker_router.send(WorkerRequest::Failure(peer_ip, format!("{}", error))).await {
                            error!("[Failure] {}", error);
                        }
                    }
                }
            }
            Message::ChallengeRequest(..) | Message::ChallengeResponse(..) => {
                return Err(anyhow!("Peer {} is not following the protocol", peer_ip));
            }
            Message::Disconnect => {
                return Err(anyhow!("Peer {} disconnected", peer_ip));
            }
            Message::Unused(_) => {
                return Err(anyhow!("Peer {} is not following the protocol", peer_ip));
            }
            _ => {}
        }
        Ok(())
    }
}

///
/// A set of operations to initialize the worker server for a specific network.
///
#[derive(Clone)]
pub struct WorkerServer<N: Network, E: Environment> {
    /// The local address of the node.
    local_ip: SocketAddr,
    /// The worker for the node.
    worker: Arc<Worker<N, E>>,
}

impl<N: Network, E: Environment> WorkerServer<N, E> {
    ///
    /// Starts the connection listener for worker.
    ///
    #[inline]
    #[allow(unused_variables)]
    pub async fn initialize(node: &Node, miner: Address<N>, peer_ips: &Vec<String>, prometheus_addr: &Option<String>) -> Result<Self> {
        // Initialize a new TCP listener at the given IP.
        let (local_ip, listener) = match TcpListener::bind(node.node).await {
            Ok(listener) => (listener.local_addr().expect("Failed to fetch the local IP"), listener),
            Err(error) => panic!("Failed to bind listener: {:?}. Check if another Aleo node is running", error),
        };

        E::status().update(State::Ready);

        // Initialize a new instance for managing worker.
        let worker = Worker::new(local_ip, miner).await?;

        // Initialize the connection listener for new peers.
        Self::initialize_listener(local_ip, listener, worker.router(), worker.clone()).await;

        // Initialise the metrics exporter.
        #[cfg(feature = "prometheus")]
        Self::initialize_metrics(prometheus_addr);

        for peer_ip in peer_ips.iter() {
            // Initialize the connection process.
            let (router, handler) = oneshot::channel();

            let worker = worker.clone();
            let peer_ip = peer_ip.parse().unwrap();

            E::tasks().append(task::spawn(async move {
                // Notify the outer function that the task is ready.
                let _ = router.send(());
                loop {
                    // Initialize the connection process.
                    let (router, handler) = oneshot::channel();

                    // Route a `Connect` request to the pool.
                    let request = WorkerRequest::Connect(peer_ip, router);
                    if let Err(error) = worker.router().send(request).await {
                        trace!("[Connect] {}", error);
                    }

                    // Wait until the connection task is initialized.
                    let _ = handler.await;

                    // Sleep for `10` seconds.
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
            }));

            // Wait until the connection task is initialized.
            let _ = handler.await;
        }

        Ok(Self { local_ip, worker })
    }

    /// Returns the IP address of this node.
    pub fn local_ip(&self) -> SocketAddr {
        self.local_ip
    }

    /// Returns the peer manager of this node.
    pub fn worker(&self) -> Arc<Worker<N, E>> {
        self.worker.clone()
    }

    ///
    /// Disconnects from peers and proceeds to shut down the node.
    ///
    #[inline]
    pub async fn shut_down(&self) {
        info!("Shutting down...");
        // Update the node status.
        E::status().update(State::ShuttingDown);

        // Shut down the worker.
        trace!("Proceeding to shut down the worker...");
        self.worker.shut_down().await;

        // Flush the tasks.
        E::tasks().flush();
        trace!("Node has shut down.");
    }

    ///
    /// Initialize the connection listener for new peers.
    ///
    #[inline]
    async fn initialize_listener(
        local_ip: SocketAddr,
        listener: TcpListener,
        worker_router: WorkerRouter<N, E>,
        worker: Arc<Worker<N, E>>,
    ) {
        // Initialize the listener process.
        let (router, handler) = oneshot::channel();
        E::tasks().append(task::spawn(async move {
            // Notify the outer function that the task is ready.
            let _ = router.send(());
            info!("Listening for peers at {}", local_ip);
            loop {
                // Don't accept connections if the node is breaching the configured peer limit.
                if worker.number_of_connected_peers().await >= E::MAXIMUM_NUMBER_OF_PEERS {
                    // Add a sleep delay as the node has reached peer capacity.
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                // Asynchronously wait for an inbound TcpStream.
                match listener.accept().await {
                    // Process the inbound connection request.
                    Ok((stream, peer_ip)) => {
                        if let Err(error) = worker_router.send(WorkerRequest::Connecting(stream, peer_ip)).await {
                            error!("Failed to send request to worker: {}", error)
                        }
                    }
                    Err(error) => error!("Failed to accept a connection: {}", error),
                }
                // Add a small delay to prevent overloading the network from handshakes.
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }));
        // Wait until the listener task is ready.
        let _ = handler.await;
    }

    #[cfg(feature = "prometheus")]
    fn initialize_metrics(prometheus_addr: &Option<String>) {
        if let Some(prometheus_addr) = prometheus_addr {
            E::tasks().append(snarkos_metrics::initialize_push_gateway(prometheus_addr).expect("couldn't initialise the metrics"));
        }
    }
}
