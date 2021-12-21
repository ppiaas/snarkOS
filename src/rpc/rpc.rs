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

//! Logic for instantiating the RPC server.

use crate::{
    helpers::Status,
    rpc::{rpc_impl::RpcImpl, rpc_trait::RpcFunctions},
    Environment,
    LedgerReader,
    Peers,
    ProverRouter,
};
use snarkvm::dpc::{MemoryPool, Network};

use hyper::{
    body::HttpBody,
    server::{conn::AddrStream, Server},
    service::{make_service_fn, service_fn},
    Body,
};
use json_rpc_types as jrt;
use jsonrpc_core::{Metadata, Params};
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, net::SocketAddr, sync::Arc};
use tokio::sync::{oneshot, RwLock};

/// Defines the authentication format for accessing private endpoints on the RPC server.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RpcCredentials {
    /// The username in the credential
    pub username: String,
    /// The password in the credential
    pub password: String,
}

/// RPC metadata for encoding authentication.
#[derive(Default, Clone)]
pub struct Meta {
    /// An optional authentication string for protected RPC functions.
    pub auth: Option<String>,
}

impl Metadata for Meta {}

const METHODS_EXPECTING_PARAMS: [&str; 12] = [
    // public
    "getblock",
    "getblocks",
    "getblockheight",
    "getblockhash",
    "getblockhashes",
    "getblockheader",
    "getblocktransactions",
    "getciphertext",
    "getledgerproof",
    "gettransaction",
    "gettransition",
    "sendtransaction",
    // "validaterawtransaction",
    // // private
    // "createrawtransaction",
    // "createtransaction",
    // "getrawrecord",
    // "decoderecord",
    // "decryptrecord",
    // "disconnect",
    // "connect",
];

/// Starts a local RPC HTTP server at `rpc_port` in a dedicated `tokio` task.
/// RPC failures do not affect the rest of the node.
pub async fn initialize_rpc_server<N: Network, E: Environment>(
    rpc_addr: SocketAddr,
    username: String,
    password: String,
    status: &Status,
    peers: &Arc<Peers<N, E>>,
    ledger: LedgerReader<N>,
    prover_router: ProverRouter<N>,
    memory_pool: Arc<RwLock<MemoryPool<N>>>,
) -> tokio::task::JoinHandle<()> {
    let credentials = RpcCredentials { username, password };
    let rpc_impl = RpcImpl::new(credentials, status.clone(), peers.clone(), ledger, prover_router, memory_pool);

    let service = make_service_fn(move |conn: &AddrStream| {
        let caller = conn.remote_addr();
        let rpc = rpc_impl.clone();
        async move { Ok::<_, Infallible>(service_fn(move |req| handle_rpc::<N, E>(caller, rpc.clone(), req))) }
    });

    let server = Server::bind(&rpc_addr).serve(service);

    let (router, handler) = oneshot::channel();
    let task = tokio::spawn(async move {
        // Notify the outer function that the task is ready.
        let _ = router.send(());
        server.await.expect("Failed to start the RPC server");
    });
    // Wait until the spawned task is ready.
    let _ = handler.await;

    task
}

async fn handle_rpc<N: Network, E: Environment>(
    caller: SocketAddr,
    rpc: RpcImpl<N, E>,
    req: hyper::Request<Body>,
) -> Result<hyper::Response<Body>, Infallible> {
    // Obtain the username and password, if present.
    let auth = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .map(|h| h.to_str().unwrap_or("").to_owned());
    let _meta = Meta { auth };

    // Save the headers.
    let headers = req.headers().clone();

    // Ready the body of the request
    let mut body = req.into_body();
    let data = match body.data().await {
        Some(Ok(data)) => data,
        err_or_none => {
            let mut error = jrt::Error::with_custom_msg(jrt::ErrorCode::ParseError, "Couldn't read the RPC body");
            if let Some(Err(err)) = err_or_none {
                error.data = Some(err.to_string());
            }

            let resp = jrt::Response::<(), String>::error(jrt::Version::V2, error, None);
            let body = serde_json::to_vec(&resp).unwrap_or_default();

            return Ok(hyper::Response::new(body.into()));
        }
    };

    // Deserialize the JSON-RPC request.
    let req: jrt::Request<Params> = match serde_json::from_slice(&data) {
        Ok(req) => req,
        Err(_) => {
            let resp = jrt::Response::<(), ()>::error(
                jrt::Version::V2,
                jrt::Error::with_custom_msg(jrt::ErrorCode::ParseError, "Couldn't parse the RPC body"),
                None,
            );
            let body = serde_json::to_vec(&resp).unwrap_or_default();

            return Ok(hyper::Response::new(body.into()));
        }
    };

    debug!("Received '{}' RPC request from {}: {:?}", &*req.method, caller, headers);

    // Read the request params.
    let mut params = match read_params(&req) {
        Ok(params) => params,
        Err(err) => {
            let resp = jrt::Response::<(), ()>::error(jrt::Version::V2, err, req.id.clone());
            let body = serde_json::to_vec(&resp).unwrap_or_default();

            return Ok(hyper::Response::new(body.into()));
        }
    };

    // Handle the request method.
    let response = match &*req.method {
        // Public
        "latestblock" => {
            let result = rpc.latest_block().await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "latestblockheight" => {
            let result = rpc.latest_block_height().await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "latestcumulativeweight" => {
            let result = rpc.latest_cumulative_weight().await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "latestblockhash" => {
            let result = rpc.latest_block_hash().await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "latestblockheader" => {
            let result = rpc.latest_block_header().await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "latestblocktransactions" => {
            let result = rpc.latest_block_transactions().await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "latestledgerroot" => {
            let result = rpc.latest_ledger_root().await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "getblock" => match serde_json::from_value::<u32>(params.remove(0)) {
            Ok(height) => {
                let result = rpc.get_block(height).await.map_err(convert_crate_err);
                result_to_response(&req, result)
            }
            Err(_) => {
                let err = jrt::Error::with_custom_msg(jrt::ErrorCode::ParseError, "Invalid block height!");
                jrt::Response::error(jrt::Version::V2, err, req.id.clone())
            }
        },
        "getblocks" => {
            match (
                serde_json::from_value::<u32>(params.remove(0)),
                serde_json::from_value::<u32>(params.remove(0)),
            ) {
                (Ok(start_block_height), Ok(end_block_height)) => {
                    let result = rpc
                        .get_blocks(start_block_height, end_block_height)
                        .await
                        .map_err(convert_crate_err);
                    result_to_response(&req, result)
                }
                (Err(_), _) | (_, Err(_)) => {
                    let err = jrt::Error::with_custom_msg(jrt::ErrorCode::ParseError, "Invalid block height!");
                    jrt::Response::error(jrt::Version::V2, err, req.id.clone())
                }
            }
        }
        "getblockheight" => {
            let result = rpc.get_block_height(params.remove(0)).await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "getblockhash" => match serde_json::from_value::<u32>(params.remove(0)) {
            Ok(height) => {
                let result = rpc.get_block_hash(height).await.map_err(convert_crate_err);
                result_to_response(&req, result)
            }
            Err(_) => {
                let err = jrt::Error::with_custom_msg(jrt::ErrorCode::ParseError, "Invalid block height!");
                jrt::Response::error(jrt::Version::V2, err, req.id.clone())
            }
        },
        "getblockhashes" => {
            match (
                serde_json::from_value::<u32>(params.remove(0)),
                serde_json::from_value::<u32>(params.remove(0)),
            ) {
                (Ok(start_block_height), Ok(end_block_height)) => {
                    let result = rpc
                        .get_block_hashes(start_block_height, end_block_height)
                        .await
                        .map_err(convert_crate_err);
                    result_to_response(&req, result)
                }
                (Err(_), _) | (_, Err(_)) => {
                    let err = jrt::Error::with_custom_msg(jrt::ErrorCode::ParseError, "Invalid block height!");
                    jrt::Response::error(jrt::Version::V2, err, req.id.clone())
                }
            }
        }
        "getblockheader" => match serde_json::from_value::<u32>(params.remove(0)) {
            Ok(height) => {
                let result = rpc.get_block_header(height).await.map_err(convert_crate_err);
                result_to_response(&req, result)
            }
            Err(_) => {
                let err = jrt::Error::with_custom_msg(jrt::ErrorCode::ParseError, "Invalid block height!");
                jrt::Response::error(jrt::Version::V2, err, req.id.clone())
            }
        },
        "getblocktemplate" => {
            let result = rpc.get_block_template().await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "getblocktransactions" => match serde_json::from_value::<u32>(params.remove(0)) {
            Ok(height) => {
                let result = rpc.get_block_transactions(height).await.map_err(convert_crate_err);
                result_to_response(&req, result)
            }
            Err(_) => {
                let err = jrt::Error::with_custom_msg(jrt::ErrorCode::ParseError, "Invalid block height!");
                jrt::Response::error(jrt::Version::V2, err, req.id.clone())
            }
        },
        "getciphertext" => {
            let result = rpc.get_ciphertext(params.remove(0)).await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "getledgerproof" => {
            let result = rpc.get_ledger_proof(params.remove(0)).await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "getmemorypool" => {
            let result = rpc.get_memory_pool().await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "gettransaction" => {
            let result = rpc.get_transaction(params.remove(0)).await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "gettransition" => {
            let result = rpc.get_transition(params.remove(0)).await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "getconnectedpeers" => {
            let result = rpc.get_connected_peers().await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "getnodestate" => {
            let result = rpc.get_node_state().await.map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        "sendtransaction" => {
            let result = rpc
                .send_transaction(params[0].as_str().unwrap_or("").into())
                .await
                .map_err(convert_crate_err);
            result_to_response(&req, result)
        }
        // "getblocktemplate" => {
        //     let result = rpc.get_block_template().await.map_err(convert_crate_err);
        //     result_to_response(&req, result)
        // }
        // // private
        // "createaccount" => {
        //     let result = rpc
        //         .create_account_protected(Params::Array(params), meta)
        //         .await
        //         .map_err(convert_core_err);
        //     result_to_response(&req, result)
        // }
        // "createtransaction" => {
        //     let result = rpc
        //         .create_transaction_protected(Params::Array(params), meta)
        //         .await
        //         .map_err(convert_core_err);
        //     result_to_response(&req, result)
        // }
        // "getrecordcommitments" => {
        //     let result = rpc
        //         .get_record_commitments_protected(Params::Array(params), meta)
        //         .await
        //         .map_err(convert_core_err);
        //     result_to_response(&req, result)
        // }
        // "getrawrecord" => {
        //     let result = rpc
        //         .get_raw_record_protected(Params::Array(params), meta)
        //         .await
        //         .map_err(convert_core_err);
        //     result_to_response(&req, result)
        // }
        // "decryptrecord" => {
        //     let result = rpc
        //         .decrypt_record_protected(Params::Array(params), meta)
        //         .await
        //         .map_err(convert_core_err);
        //     result_to_response(&req, result)
        // }
        // "connect" => {
        //     let result = rpc
        //         .connect_protected(Params::Array(params), meta)
        //         .await
        //         .map_err(convert_core_err);
        //     result_to_response(&req, result)
        // }
        _ => {
            let err = jrt::Error::from_code(jrt::ErrorCode::MethodNotFound);
            jrt::Response::error(jrt::Version::V2, err, req.id.clone())
        }
    };

    // Serialize the response object.
    let body = serde_json::to_vec(&response).unwrap_or_default();

    // Send the HTTP response.
    Ok(hyper::Response::new(body.into()))
}

/// Ensures that the params are a non-empty (this assumption is taken advantage of later) array and returns them.
fn read_params(req: &jrt::Request<Params>) -> Result<Vec<serde_json::Value>, jrt::Error<()>> {
    if METHODS_EXPECTING_PARAMS.contains(&&*req.method) {
        match &req.params {
            Some(Params::Array(arr)) if !arr.is_empty() => Ok(arr.clone()),
            Some(_) => Err(jrt::Error::from_code(jrt::ErrorCode::InvalidParams)),
            None => Err(jrt::Error::from_code(jrt::ErrorCode::InvalidParams)),
        }
    } else {
        Ok(vec![]) // unused in methods other than METHODS_EXPECTING_PARAMS
    }
}

/// Converts the crate's RpcError into a jrt::RpcError
fn convert_crate_err(err: crate::rpc::rpc_impl::RpcError) -> jrt::Error<String> {
    let error = jrt::Error::with_custom_msg(jrt::ErrorCode::ServerError(-32000), "internal error");
    error.set_data(err.to_string())
}

/// Converts the jsonrpc-core's Error into a jrt::RpcError
#[allow(unused)]
fn convert_core_err(err: jsonrpc_core::Error) -> jrt::Error<String> {
    let error = jrt::Error::with_custom_msg(jrt::ErrorCode::InternalError, "JSONRPC server error");
    error.set_data(err.to_string())
}

fn result_to_response<T: Serialize>(
    request: &jrt::Request<Params>,
    result: Result<T, jrt::Error<String>>,
) -> jrt::Response<serde_json::Value, String> {
    match result {
        Ok(res) => {
            let result = serde_json::to_value(&res).unwrap_or_default();
            jrt::Response::result(jrt::Version::V2, result, request.id.clone())
        }
        Err(err) => jrt::Response::error(jrt::Version::V2, err, request.id.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{helpers::State, ledger::Ledger, Client, Prover};

    use crate::helpers::Tasks;
    use snarkos_storage::{
        storage::{rocksdb::RocksDB, Storage},
        LedgerState,
    };
    use snarkvm::{
        dpc::{testnet2::Testnet2, AccountScheme, AleoAmount, Transaction, Transactions, Transition},
        prelude::{Account, Block, BlockHeader},
        utilities::ToBytes,
    };

    use hyper::Request;
    use rand::{thread_rng, SeedableRng};
    use rand_chacha::ChaChaRng;
    use snarkvm::dpc::Record;
    use std::{
        path::{Path, PathBuf},
        sync::atomic::AtomicBool,
    };

    fn temp_dir() -> std::path::PathBuf {
        tempfile::tempdir().expect("Failed to open temporary directory").into_path()
    }

    /// Returns a dummy caller IP address.
    fn caller() -> SocketAddr {
        "0.0.0.0:3030".to_string().parse().unwrap()
    }

    /// Initializes a new instance of the ledger state.
    fn new_ledger_state<N: Network, S: Storage, P: AsRef<Path>>(path: Option<P>) -> LedgerState<N> {
        match path {
            Some(path) => LedgerState::<N>::open_writer::<S, _>(path).expect("Failed to initialize ledger"),
            None => LedgerState::<N>::open_writer::<S, _>(temp_dir()).expect("Failed to initialize ledger"),
        }
    }

    /// Initializes a new instance of the rpc.
    async fn new_rpc<N: Network, E: Environment, S: Storage, P: AsRef<Path>>(path: Option<P>) -> RpcImpl<N, E> {
        let credentials = RpcCredentials {
            username: "root".to_string(),
            password: "pass".to_string(),
        };

        // Derive the storage paths.
        let (ledger_path, prover_path) = match &path {
            Some(p) => (p.as_ref().to_path_buf(), temp_dir()),
            None => (temp_dir(), temp_dir()),
        };

        // Initialize the node.
        let local_ip: SocketAddr = "0.0.0.0:8888".parse().expect("Failed to parse ip");

        // Initialize the status indicator.
        let status = Status::new();
        status.update(State::Ready);

        // Initialize the terminator bit.
        let terminator = Arc::new(AtomicBool::new(false));
        // Initialize the tasks handler.
        let mut tasks = Tasks::new();

        // Initialize a new instance for managing peers.
        let peers = Peers::new(tasks.clone(), local_ip, None, &status).await;
        // Initialize a new instance for managing the ledger.
        let ledger = Ledger::<N, E>::open::<S, _>(&mut tasks, &ledger_path, &status, &terminator, peers.router())
            .await
            .expect("Failed to initialize ledger");

        // Initialize a new instance for managing the prover.
        let prover = Prover::open::<S, _>(
            &mut tasks,
            &prover_path,
            None,
            local_ip,
            &status,
            &terminator,
            peers.router(),
            ledger.reader(),
            ledger.router(),
        )
        .await
        .expect("Failed to initialize prover");

        RpcImpl::<N, E>::new(credentials, status, peers, ledger.reader(), prover.router(), prover.memory_pool())
    }

    /// Deserializes a rpc response into the given type.
    async fn process_response<T: serde::de::DeserializeOwned>(response: hyper::Response<Body>) -> T {
        assert!(response.status().is_success());

        let response_bytes = hyper::body::to_bytes(response.into_body()).await.unwrap();
        let response_json: jrt::Response<serde_json::Value, String> = serde_json::from_slice(&response_bytes).unwrap();

        serde_json::from_value(response_json.payload.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn test_handle_rpc() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Initialize a new request with an empty body.
        let request = Request::new(Body::empty());

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request).await;

        // Check the response was received without errors.
        assert!(response.is_ok());
        assert!(response.unwrap().status().is_success());
    }

    #[tokio::test]
    async fn test_latest_block() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Initialize a new request that calls the `latestblock` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc":"2.0",
	"id": "1",
	"method": "latestblock"
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a block.
        let actual: Block<Testnet2> = process_response(response).await;

        // Check the block.
        let expected = Testnet2::genesis_block();
        assert_eq!(*expected, actual);
    }

    #[tokio::test]
    async fn test_latest_block_height() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Initialize a new request that calls the `latestblockheight` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc":"2.0",
	"id": "1",
	"method": "latestblockheight"
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a block height.
        let actual: u32 = process_response(response).await;

        // Check the block height.
        let expected = Testnet2::genesis_block().height();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn test_latest_block_hash() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Initialize a new request that calls the `latestblockhash` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc":"2.0",
	"id": "1",
	"method": "latestblockhash"
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a block hash.
        let actual: <Testnet2 as Network>::BlockHash = process_response(response).await;

        // Check the block hash.
        let expected = Testnet2::genesis_block().hash();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn test_latest_block_header() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Initialize a new request that calls the `latestblockheader` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc":"2.0",
	"id": "1",
	"method": "latestblockheader"
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a block header.
        let actual: BlockHeader<Testnet2> = process_response(response).await;

        // Check the block header.
        let expected = Testnet2::genesis_block().header();
        assert_eq!(*expected, actual);
    }

    #[tokio::test]
    async fn test_latest_block_transactions() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Initialize a new request that calls the `latestblocktransactions` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc":"2.0",
	"id": "1",
	"method": "latestblocktransactions"
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into transactions.
        let actual: Transactions<Testnet2> = process_response(response).await;

        // Check the transactions.
        let expected = Testnet2::genesis_block().transactions();
        assert_eq!(*expected, actual);
    }

    #[tokio::test]
    async fn test_latest_ledger_root() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        let expected = rpc.latest_ledger_root().await.unwrap();

        // Initialize a new request that calls the `latestledgerroot` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc":"2.0",
	"id": "1",
	"method": "latestledgerroot"
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a ledger root.
        let actual: <Testnet2 as Network>::LedgerRoot = process_response(response).await;

        // Check the ledger root.
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn test_get_block() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Initialize a new request that calls the `getblock` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc": "2.0",
	"id": "1",
	"method": "getblock",
	"params": [
        0
    ]
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a block.
        let actual: Block<Testnet2> = process_response(response).await;

        // Check the block.
        let expected = Testnet2::genesis_block();
        assert_eq!(*expected, actual);
    }

    #[tokio::test]
    async fn test_get_blocks() {
        let rng = &mut thread_rng();
        let terminator = AtomicBool::new(false);

        // Initialize a new temporary directory.
        let directory = temp_dir();

        // Initialize a new ledger state at the temporary directory.
        let ledger_state = new_ledger_state::<Testnet2, RocksDB, PathBuf>(Some(directory.clone()));
        assert_eq!(0, ledger_state.latest_block_height());

        // Initialize a new account.
        let account = Account::<Testnet2>::new(&mut thread_rng());
        let address = account.address();

        // Mine the next block.
        let (block_1, _) = ledger_state
            .mine_next_block(address, true, &[], &terminator, rng)
            .expect("Failed to mine");
        ledger_state.add_next_block(&block_1).expect("Failed to add next block to ledger");
        assert_eq!(1, ledger_state.latest_block_height());

        // Drop the handle to ledger_state. Note this does not remove the blocks in the temporary directory.
        drop(ledger_state);

        // Initialize a new RPC with the ledger state containing the genesis block and block_1.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(Some(directory.clone())).await;

        // Initialize a new request that calls the `getblocks` endpoint.
        let request = Request::new(Body::from(
            r#"{
    "jsonrpc": "2.0",
    "id": "1",
    "method": "getblocks",
    "params": [
        0, 1
    ]
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into blocks.
        let actual: Vec<Block<Testnet2>> = process_response(response).await;

        // Check the blocks.
        let expected = vec![Testnet2::genesis_block(), &block_1];
        expected.into_iter().zip(actual.into_iter()).for_each(|(expected, actual)| {
            assert_eq!(*expected, actual);
        });
    }

    #[tokio::test]
    async fn test_get_block_height() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Get the genesis block hash.
        let block_hash = Testnet2::genesis_block().hash().to_string();

        // Initialize a new request that calls the `getblockheight` endpoint.
        let request = Request::new(Body::from(format!(
            "{{
	\"jsonrpc\": \"2.0\",
	\"id\": \"1\",
	\"method\": \"getblockheight\",
	\"params\": [
        \"{}\"
    ]
}}",
            block_hash
        )));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a block height.
        let actual: u32 = process_response(response).await;

        // Check the block height.
        let expected = Testnet2::genesis_block().height();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn test_get_block_hash() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Initialize a new request that calls the `getblockhash` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc": "2.0",
	"id": "1",
	"method": "getblockhash",
	"params": [
        0
    ]
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a block hash.
        let actual: <Testnet2 as Network>::BlockHash = process_response(response).await;

        // Check the block hash.
        let expected = Testnet2::genesis_block().hash();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn test_get_block_hashes() {
        let rng = &mut thread_rng();
        let terminator = AtomicBool::new(false);

        // Initialize a new temporary directory.
        let directory = temp_dir();

        // Initialize a new ledger state at the temporary directory.
        let ledger_state = new_ledger_state::<Testnet2, RocksDB, PathBuf>(Some(directory.clone()));
        assert_eq!(0, ledger_state.latest_block_height());

        // Initialize a new account.
        let account = Account::<Testnet2>::new(&mut thread_rng());
        let address = account.address();

        // Mine the next block.
        let (block_1, _) = ledger_state
            .mine_next_block(address, true, &[], &terminator, rng)
            .expect("Failed to mine");
        ledger_state.add_next_block(&block_1).expect("Failed to add next block to ledger");
        assert_eq!(1, ledger_state.latest_block_height());

        // Drop the handle to ledger_state. Note this does not remove the blocks in the temporary directory.
        drop(ledger_state);

        // Initialize a new RPC with the ledger state containing the genesis block and block_1.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(Some(directory.clone())).await;

        // Initialize a new request that calls the `getblockhashes` endpoint.
        let request = Request::new(Body::from(
            r#"{
    "jsonrpc": "2.0",
    "id": "1",
    "method": "getblockhashes",
    "params": [
        0, 1
    ]
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into block hashes.
        let actual: Vec<<Testnet2 as Network>::BlockHash> = process_response(response).await;

        // Check the block hashes.
        let expected = vec![Testnet2::genesis_block().hash(), block_1.hash()];
        expected.into_iter().zip(actual.into_iter()).for_each(|(expected, actual)| {
            assert_eq!(expected, actual);
        });
    }

    #[tokio::test]
    async fn test_get_block_header() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Initialize a new request that calls the `getblockheader` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc": "2.0",
	"id": "1",
	"method": "getblockheader",
	"params": [
        0
    ]
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a block header.
        let actual: BlockHeader<Testnet2> = process_response(response).await;

        // Check the block header.
        let expected = Testnet2::genesis_block().header();
        assert_eq!(*expected, actual);
    }

    #[tokio::test]
    async fn test_get_block_template() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Initialize the expected block template values.
        let expected_previous_block_hash = Testnet2::genesis_block().hash().to_string();
        let expected_block_height = 1;
        let expected_ledger_root = rpc.latest_ledger_root().await.unwrap().to_string();
        let expected_transactions = Vec::<serde_json::Value>::new();
        let expected_block_reward = Block::<Testnet2>::block_reward(1).0;

        // Initialize a new request that calls the `getblocktemplate` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc":"2.0",
	"id": "1",
	"method": "getblocktemplate"
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a ledger root.
        let actual: serde_json::Value = process_response(response).await;

        // Check the block template state.
        assert_eq!(expected_previous_block_hash, actual["previous_block_hash"]);
        assert_eq!(expected_block_height, actual["block_height"]);
        assert_eq!(expected_ledger_root, actual["ledger_root"].as_str().unwrap());
        assert_eq!(&expected_transactions, actual["transactions"].as_array().unwrap());
        assert_eq!(expected_block_reward, actual["coinbase_reward"].as_i64().unwrap());
    }

    #[tokio::test]
    async fn test_get_block_transactions() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Initialize a new request that calls the `getblocktransactions` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc": "2.0",
	"id": "1",
	"method": "getblocktransactions",
	"params": [
        0
    ]
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into transactions.
        let actual: Transactions<Testnet2> = process_response(response).await;

        // Check the transactions.
        let expected = Testnet2::genesis_block().transactions();
        assert_eq!(*expected, actual);
    }

    #[tokio::test]
    async fn test_get_ciphertext() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Get the commitment from the genesis coinbase transaction.
        let commitment = Testnet2::genesis_block().to_coinbase_transaction().unwrap().transitions()[0]
            .commitments()
            .next()
            .unwrap()
            .to_string();

        // Initialize a new request that calls the `getciphertext` endpoint.
        let request = Request::new(Body::from(format!(
            "{{
	\"jsonrpc\": \"2.0\",
	\"id\": \"1\",
	\"method\": \"getciphertext\",
	\"params\": [
        \"{}\"
    ]
}}",
            commitment
        )));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a ciphertext.
        let actual: <Testnet2 as Network>::RecordCiphertext = process_response(response).await;

        // Check the ciphertext.
        assert!(
            Testnet2::genesis_block()
                .transactions()
                .first()
                .unwrap()
                .ciphertexts()
                .any(|expected| *expected == actual)
        );
    }

    #[tokio::test]
    async fn test_get_ledger_proof() {
        let mut rng = ChaChaRng::seed_from_u64(123456789);
        let terminator = AtomicBool::new(false);

        // Initialize a new temporary directory.
        let directory = temp_dir();

        // Initialize a new ledger state at the temporary directory.
        let ledger_state = new_ledger_state::<Testnet2, RocksDB, PathBuf>(Some(directory.clone()));
        assert_eq!(0, ledger_state.latest_block_height());

        // Initialize a new account.
        let account = Account::<Testnet2>::new(&mut rng);
        let address = account.address();

        // Mine the next block.
        let (block_1, _) = ledger_state
            .mine_next_block(address, true, &[], &terminator, &mut rng)
            .expect("Failed to mine");
        ledger_state.add_next_block(&block_1).expect("Failed to add next block to ledger");
        assert_eq!(1, ledger_state.latest_block_height());

        // Get the record commitment.
        let decrypted_records = block_1.transactions().first().unwrap().to_decrypted_records(account.view_key());
        assert!(!decrypted_records.is_empty());
        let record_commitment = decrypted_records[0].commitment();

        // Get the ledger proof.
        let ledger_proof = ledger_state.get_ledger_inclusion_proof(record_commitment).unwrap();

        // Drop the handle to ledger_state. Note this does not remove the blocks in the temporary directory.
        drop(ledger_state);

        // Initialize a new RPC with the ledger state containing the genesis block and block_1.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(Some(directory.clone())).await;

        // Initialize a new request that calls the `getledgerproof` endpoint.
        let request = Request::new(Body::from(format!(
            "{{
	\"jsonrpc\": \"2.0\",
	\"id\": \"1\",
	\"method\": \"getledgerproof\",
	\"params\": [
        \"{}\"
    ]
}}",
            record_commitment.to_string()
        )));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a ledger proof string.
        let actual: String = process_response(response).await;

        // Check the ledger proof.
        let expected = hex::encode(ledger_proof.to_bytes_le().expect("Failed to serialize ledger proof"));
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn test_get_node_state() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Declare the expected node state.
        let expected = serde_json::json!({
            "candidate_peers": Vec::<SocketAddr>::new(),
            "connected_peers": Vec::<SocketAddr>::new(),
            "latest_block_hash": Testnet2::genesis_block().hash(),
            "latest_block_height": 0,
            "latest_cumulative_weight": 0,
            "number_of_candidate_peers": 0,
            "number_of_connected_peers": 0,
            "number_of_connected_sync_nodes": 0,
            "software": format!("snarkOS {}", env!("CARGO_PKG_VERSION")),
            "status": rpc.status.to_string(),
            "type": Client::<Testnet2>::NODE_TYPE,
            "version": Client::<Testnet2>::MESSAGE_VERSION,
        });

        // Initialize a new request that calls the `getnodestate` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc":"2.0",
	"id": "1",
	"method": "getnodestate"
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a ledger root.
        let actual: serde_json::Value = process_response(response).await;

        println!("get_node_state: {:?}", actual);

        // Check the node state.
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn test_get_transaction() {
        /// Additional metadata included with a transaction response
        #[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
        pub struct GetTransactionResponse {
            pub transaction: Transaction<Testnet2>,
            pub metadata: snarkos_storage::Metadata<Testnet2>,
            pub decrypted_records: Vec<Record<Testnet2>>,
        }

        // Initialize a new ledger.
        let ledger = new_ledger_state::<Testnet2, RocksDB, PathBuf>(None);

        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Get the genesis coinbase transaction ID.
        let transaction_id = Testnet2::genesis_block().to_coinbase_transaction().unwrap().transaction_id();

        // Initialize a new request that calls the `gettransaction` endpoint.
        let request = Request::new(Body::from(format!(
            "{{
	\"jsonrpc\": \"2.0\",
	\"id\": \"1\",
	\"method\": \"gettransaction\",
	\"params\": [
        \"{}\"
    ]
}}",
            transaction_id.to_string()
        )));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a transaction and transaction metadata.
        let actual: GetTransactionResponse = process_response(response).await;

        // Check the transaction.
        let expected_transaction = Testnet2::genesis_block().transactions().first().unwrap();
        assert_eq!(*expected_transaction, actual.transaction);

        // Check the metadata.
        let expected_transaction_metadata = ledger.get_transaction_metadata(&transaction_id).unwrap();
        assert_eq!(expected_transaction_metadata, actual.metadata);

        // Check the records.
        let expected_decrypted_records: Vec<Record<Testnet2>> = expected_transaction.to_records().collect();
        assert_eq!(expected_decrypted_records, actual.decrypted_records)
    }

    #[tokio::test]
    async fn test_get_transition() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Get a transition ID from the genesis coinbase transaction.
        let transition_id = Testnet2::genesis_block().to_coinbase_transaction().unwrap().transitions()[0]
            .transition_id()
            .to_string();

        // Initialize a new request that calls the `gettransaction` endpoint.
        let request = Request::new(Body::from(format!(
            "{{
	\"jsonrpc\": \"2.0\",
	\"id\": \"1\",
	\"method\": \"gettransition\",
	\"params\": [
        \"{}\"
    ]
}}",
            transition_id
        )));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a transition.
        let actual: Transition<Testnet2> = process_response(response).await;

        // Check the transition.
        assert!(
            Testnet2::genesis_block()
                .transactions()
                .first()
                .unwrap()
                .transitions()
                .iter()
                .any(|expected| *expected == actual)
        );
    }

    #[tokio::test]
    async fn test_get_connected_peers() {
        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Initialize a new request that calls the `gettransition` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc": "2.0",
	"id": "1",
	"method": "getconnectedpeers",
	"params": []
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a transition.
        let actual: Vec<String> = process_response(response).await;

        // Check the transition.
        assert_eq!(actual, Vec::<String>::new());
    }

    #[tokio::test]
    async fn test_send_transaction() {
        let mut rng = ChaChaRng::seed_from_u64(123456789);

        // Initialize a new account.
        let account = Account::<Testnet2>::new(&mut rng);
        let address = account.address();

        // Initialize a new transaction.
        let (transaction, _) = Transaction::<Testnet2>::new_coinbase(address, AleoAmount(1234), true, &mut rng)
            .expect("Failed to create a coinbase transaction");

        // Initialize a new rpc.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Initialize a new request that calls the `sendtransaction` endpoint.
        let request = Request::new(Body::from(format!(
            "{{
	\"jsonrpc\": \"2.0\",
	\"id\": \"1\",
	\"method\": \"sendtransaction\",
	\"params\": [
        \"{}\"
    ]
}}",
            hex::encode(transaction.to_bytes_le().unwrap())
        )));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into a ciphertext.
        let actual: <Testnet2 as Network>::TransactionID = process_response(response).await;

        // Check the transaction id.
        let expected = transaction.transaction_id();
        assert_eq!(expected, actual);
    }

    #[tokio::test]
    async fn test_get_memory_pool() {
        let mut rng = ChaChaRng::seed_from_u64(123456789);

        // Initialize a new RPC.
        let rpc = new_rpc::<Testnet2, Client<Testnet2>, RocksDB, PathBuf>(None).await;

        // Send a transaction to the node.

        // Initialize a new account.
        let account = Account::<Testnet2>::new(&mut rng);
        let address = account.address();

        // Initialize a new transaction.
        let (transaction, _) =
            Transaction::<Testnet2>::new_coinbase(address, AleoAmount(0), true, &mut rng).expect("Failed to create a coinbase transaction");

        // Initialize a new request that calls the `sendtransaction` endpoint.
        let request = Request::new(Body::from(format!(
            "{{
	\"jsonrpc\": \"2.0\",
	\"id\": \"1\",
	\"method\": \"sendtransaction\",
	\"params\": [
        \"{}\"
    ]
}}",
            hex::encode(transaction.to_bytes_le().unwrap())
        )));

        // Send the request to the RPC.
        let _response = handle_rpc(caller(), rpc.clone(), request)
            .await
            .expect("Test RPC failed to process request");

        // Give the node some time to process the transaction.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        // Fetch the transaction from the memory_pool.

        // Initialize a new request that calls the `getmemorypool` endpoint.
        let request = Request::new(Body::from(
            r#"{
	"jsonrpc":"2.0",
	"id": "1",
	"method": "getmemorypool"
}"#,
        ));

        // Send the request to the RPC.
        let response = handle_rpc(caller(), rpc, request)
            .await
            .expect("Test RPC failed to process request");

        // Process the response into transactions.
        let actual: Vec<Transaction<Testnet2>> = process_response(response).await;

        // Check the transactions.
        let expected = vec![transaction];
        assert_eq!(*expected, actual);
    }
}
