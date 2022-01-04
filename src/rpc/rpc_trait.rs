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

//! Definition of the public and private RPC endpoints.

use crate::rpc::rpc_impl::RpcError;
use snarkvm::dpc::{Block, BlockHeader, Network, Transaction, Transactions, Transition};

use std::net::SocketAddr;

/// Definition of public RPC endpoints.
#[async_trait::async_trait]
pub trait RpcFunctions<N: Network> {
    #[doc = include_str!("./documentation/public_endpoints/latestblock.md")]
    async fn latest_block(&self) -> Result<Block<N>, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/latestblockheight.md")]
    async fn latest_block_height(&self) -> Result<u32, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/latestcumulativeweight.md")]
    async fn latest_cumulative_weight(&self) -> Result<u128, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/latestblockhash.md")]
    async fn latest_block_hash(&self) -> Result<N::BlockHash, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/latestblockheader.md")]
    async fn latest_block_header(&self) -> Result<BlockHeader<N>, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/latestblocktransactions.md")]
    async fn latest_block_transactions(&self) -> Result<Transactions<N>, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/latestledgerroot.md")]
    async fn latest_ledger_root(&self) -> Result<N::LedgerRoot, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/getblock.md")]
    async fn get_block(&self, block_height: u32) -> Result<Block<N>, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/getblocks.md")]
    async fn get_blocks(&self, start_block_height: u32, end_block_height: u32) -> Result<Vec<Block<N>>, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/getblockheight.md")]
    async fn get_block_height(&self, block_hash: serde_json::Value) -> Result<u32, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/getblockhash.md")]
    async fn get_block_hash(&self, block_height: u32) -> Result<N::BlockHash, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/getblockhashes.md")]
    async fn get_block_hashes(&self, start_block_height: u32, end_block_height: u32) -> Result<Vec<N::BlockHash>, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/getblockheader.md")]
    async fn get_block_header(&self, block_height: u32) -> Result<BlockHeader<N>, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/getblocktemplate.md")]
    async fn get_block_template(&self) -> Result<serde_json::Value, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/getblocktransactions.md")]
    async fn get_block_transactions(&self, block_height: u32) -> Result<Transactions<N>, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/getciphertext.md")]
    async fn get_ciphertext(&self, commitment: serde_json::Value) -> Result<N::RecordCiphertext, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/getledgerproof.md")]
    async fn get_ledger_proof(&self, record_commitment: serde_json::Value) -> Result<String, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/getmemorypool.md")]
    async fn get_memory_pool(&self) -> Result<Vec<Transaction<N>>, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/gettransaction.md")]
    async fn get_transaction(&self, transaction_id: serde_json::Value) -> Result<serde_json::Value, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/gettransition.md")]
    async fn get_transition(&self, transition_id: serde_json::Value) -> Result<Transition<N>, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/getconnectedpeers.md")]
    async fn get_connected_peers(&self) -> Result<Vec<SocketAddr>, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/getnodestate.md")]
    async fn get_node_state(&self) -> Result<serde_json::Value, RpcError>;

    #[doc = include_str!("./documentation/public_endpoints/sendtransaction.md")]
    async fn send_transaction(&self, transaction_bytes: String) -> Result<N::TransactionID, RpcError>;
}

// /// Definition of private RPC endpoints that require authentication.
// #[async_trait::async_trait]
// pub trait ProtectedRpcFunctions {
//     #[doc = include_str!("../documentation/private_endpoints/createtransaction.md")]
//     async fn create_transaction(
//         &self,
//         private_keys: [String; 2], // TODO (howardwu): Genericize this.
//         transaction_kernel: String,
//     ) -> Result<CreateRawTransactionOuput, RpcError>;
//
//     #[doc = include_str!("../documentation/private_endpoints/getrecordcommitments.md")]
//     async fn get_record_commitments(&self) -> Result<Vec<String>, RpcError>;
//
//     #[doc = include_str!("../documentation/private_endpoints/getrawrecord.md")]
//     async fn get_raw_record(&self, record_commitment: String) -> Result<String, RpcError>;
//
//     #[doc = include_str!("../documentation/private_endpoints/decryptrecord.md")]
//     async fn decrypt_record(&self, decryption_input: DecryptRecordInput) -> Result<String, RpcError>;
//
//     #[doc = include_str!("../documentation/private_endpoints/connect.md")]
//     async fn connect(&self, addresses: Vec<SocketAddr>);
// }
