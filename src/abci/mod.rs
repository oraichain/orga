use crate::call::Call;
use crate::query::Query;
use crate::state::State;

use crate::Result;
#[cfg(feature = "abci")]
mod node;
#[cfg(feature = "abci")]
pub use node::*;

pub mod prost;

use messages::*;
pub use tendermint_proto::v0_34::abci as messages;

#[cfg(feature = "abci")]
mod server {
    use super::*;
    use crate::merk::MerkStore;
    use crate::store::{BufStore, BufStoreMap, MapStore, Read, Shared, Write, KV};
    use crate::Error;
    use log::info;
    use std::env;
    use std::net::ToSocketAddrs;
    use std::sync::mpsc::{self, Receiver, SyncSender};
    use std::sync::{Arc, RwLock};
    use tendermint_proto::v0_34::abci::request::Value as Req;
    use tendermint_proto::v0_34::abci::response::Value as Res;
    use tendermint_proto::v0_34::types::Header;

    /// Top-level struct for running an ABCI application. Maintains an ABCI server,
    /// mempool, and handles committing data to the store.
    pub struct ABCIStateMachine<A: Application> {
        app: Option<A>,
        store: Option<Shared<MerkStore>>,
        receiver: Receiver<(Request, SyncSender<Response>)>,
        sender: SyncSender<(Request, SyncSender<Response>)>,
        mempool_state: Option<BufStoreMap>,
        consensus_state: Option<BufStoreMap>,
        height: u64,
        skip_init_chain: bool,
        header: Option<Header>,
        shutdown: Arc<RwLock<Option<Error>>>,
        shutdown_notifier: Arc<RwLock<bool>>,
    }

    impl<A: Application> ABCIStateMachine<A> {
        /// Constructs an `ABCIStateMachine` from the given app (a set of handlers
        /// for transactions and blocks), and store (a key/value store to persist
        /// the state data).
        pub fn new(
            app: A,
            store: MerkStore,
            skip_init_chain: bool,
            shutdown: Arc<RwLock<Option<Error>>>,
            shutdown_notifier: Arc<RwLock<bool>>,
        ) -> Self {
            let (sender, receiver) = mpsc::sync_channel(0);
            ABCIStateMachine {
                app: Some(app),
                store: Some(Shared::new(store)),
                sender,
                receiver,
                mempool_state: Some(Default::default()),
                consensus_state: Some(Default::default()),
                height: 0,
                skip_init_chain,
                header: None,
                shutdown,
                shutdown_notifier,
            }
        }

        /// Handles a single incoming ABCI request.
        ///
        /// Some messages, such as `info`, `flush`, and `echo` are automatically
        /// handled by the `ABCIStateMachine`, while others are passed to the
        /// [`Application`](trait.Application.html).
        pub fn run(&mut self, req: Request) -> Result<Res> {
            let value = match req.value {
                None => {
                    return Err(Error::ABCI("Received empty request".into()));
                }
                Some(value) => value,
            };

            match value {
                Req::Info(_) => {
                    let self_store = self.store.take().unwrap().into_inner();

                    let start_height = self_store.height()?;
                    info!("State is at height {}", start_height);

                    let app_hash = if start_height == 0 {
                        vec![]
                    } else {
                        self_store.root_hash()?
                    };

                    let res_info = ResponseInfo {
                        data: "Rust ABCI State Machine".into(),
                        version: "X".into(),
                        app_version: 0,
                        last_block_height: start_height as i64,
                        last_block_app_hash: app_hash.into(),
                    };

                    self.store = Some(Shared::new(self_store));
                    Ok(Res::Info(res_info))
                }
                Req::Flush(_) => Ok(Res::Flush(Default::default())),
                Req::Echo(_) => Ok(Res::Echo(Default::default())),
                Req::SetOption(_) => Ok(Res::SetOption(Default::default())),
                Req::Query(req) => {
                    let store = self.store.take().unwrap();
                    let app = self.app.take().unwrap();

                    let res = app
                        .query(store.clone(), req)
                        .unwrap_or_else(|err| ResponseQuery {
                            code: 1,
                            log: err.to_string(),
                            info: err.to_string(),
                            codespace: "".to_string(),
                            height: self.height as i64,
                            index: 0,
                            key: vec![].into(),
                            proof_ops: None,
                            value: vec![].into(),
                        });

                    self.store.replace(store);
                    self.app.replace(app);

                    Ok(Res::Query(res))
                }
                Req::InitChain(req) => {
                    if self.skip_init_chain {
                        return Ok(Res::InitChain(Default::default()));
                    }
                    let app = self.app.take().unwrap();
                    let self_store = self.store.take().unwrap().into_inner();
                    let self_store_shared = Shared::new(self_store);

                    let mut store = Some(Shared::new(BufStore::wrap_with_map(
                        self_store_shared.clone(),
                        self.consensus_state.take().unwrap(),
                    )));

                    let res_init_chain = {
                        let owned_store = store.take().unwrap();
                        let flush_store = Shared::new(BufStore::wrap(owned_store.clone()));
                        let res = app.init_chain(flush_store.clone(), req)?;
                        let mut unwrapped_fs = flush_store.into_inner();
                        unwrapped_fs.flush()?;
                        store.replace(owned_store);
                        res
                    };

                    store.unwrap().into_inner().flush()?;
                    let self_store = self_store_shared.into_inner();

                    self.app.replace(app);
                    self.consensus_state.replace(Default::default());
                    self.store = Some(Shared::new(self_store));
                    Ok(Res::InitChain(res_init_chain))
                }
                Req::BeginBlock(req) => {
                    if let Some(stop_height_str) = env::var_os("ORGA_STOP_HEIGHT") {
                        let stop_height: i64 = stop_height_str
                            .into_string()
                            .unwrap()
                            .parse()
                            .expect("Invalid ORGA_STOP_HEIGHT value");
                        if req.header.as_ref().unwrap().height > stop_height {
                            return Err(Error::ABCI(format!(
                                "Reached stop height ({})",
                                stop_height
                            )));
                        }
                    }

                    let app = self.app.take().unwrap();
                    let self_store = self.store.take().unwrap().into_inner();
                    let self_store_shared = Shared::new(self_store);
                    self.header = req.header.clone();

                    let mut store = Some(Shared::new(BufStore::wrap_with_map(
                        self_store_shared.clone(),
                        self.consensus_state.take().unwrap(),
                    )));

                    let res_begin_block = {
                        let owned_store = store.take().unwrap();
                        let flush_store = Shared::new(BufStore::wrap(owned_store.clone()));
                        let res = app.begin_block(flush_store.clone(), req)?;
                        let mut unwrapped_fs = flush_store.into_inner();
                        unwrapped_fs.flush()?;
                        store.replace(owned_store);
                        res
                    };

                    self.app.replace(app);
                    self.consensus_state
                        .replace(store.unwrap().into_inner().into_map());

                    let self_store = self_store_shared.into_inner();
                    self.store = Some(Shared::new(self_store));
                    Ok(Res::BeginBlock(res_begin_block))
                }
                Req::DeliverTx(req) => {
                    let app = self.app.take().unwrap();
                    let self_store = self.store.take().unwrap().into_inner();
                    let self_store_shared = Shared::new(self_store);
                    let mut store = Some(Shared::new(BufStore::wrap_with_map(
                        self_store_shared.clone(),
                        self.consensus_state.take().unwrap(),
                    )));

                    let res_deliver_tx = {
                        let owned_store = store.take().unwrap();
                        let flush_store = Shared::new(BufStore::wrap(owned_store.clone()));
                        let res = app.deliver_tx(flush_store.clone(), req)?;
                        {
                            let mut unwrapped_fs = flush_store.into_inner();
                            unwrapped_fs.flush()?;
                        }
                        let mut owned_store_inner = owned_store.into_inner();
                        owned_store_inner.flush()?;
                        let owned_store = Shared::new(owned_store_inner);
                        store.replace(owned_store);
                        res
                    };

                    self.app.replace(app);
                    self.consensus_state
                        .replace(store.unwrap().into_inner().into_map());
                    let self_store = self_store_shared.into_inner();
                    self.store = Some(Shared::new(self_store));
                    Ok(Res::DeliverTx(res_deliver_tx))
                }
                Req::EndBlock(req) => {
                    self.height = req.height as u64;

                    let app = self.app.take().unwrap();
                    let self_store = self.store.take().unwrap().into_inner();
                    let self_store_shared = Shared::new(self_store);
                    let mut store = Some(Shared::new(BufStore::wrap_with_map(
                        self_store_shared.clone(),
                        self.consensus_state.take().unwrap(),
                    )));

                    let res_end_block = {
                        let owned_store = store.take().unwrap();
                        let flush_store = Shared::new(BufStore::wrap(owned_store.clone()));
                        let res = app.end_block(flush_store.clone(), req)?;
                        let mut unwrapped_fs = flush_store.into_inner();
                        unwrapped_fs.flush()?;
                        store.replace(owned_store);
                        res
                    };

                    self.app.replace(app);
                    self.consensus_state
                        .replace(store.unwrap().into_inner().into_map());
                    let self_store = self_store_shared.into_inner();
                    self.store = Some(Shared::new(self_store));
                    Ok(Res::EndBlock(res_end_block))
                }
                Req::Commit(_) => {
                    let self_store = self.store.take().unwrap().into_inner();
                    let mut self_store_shared = Shared::new(self_store);
                    {
                        let mut store = BufStore::wrap_with_map(
                            self_store_shared.clone(),
                            self.consensus_state.take().unwrap(),
                        );
                        store.flush()?;
                    }

                    self_store_shared
                        .borrow_mut()
                        .commit(self.header.clone().unwrap())?;

                    self.mempool_state.replace(Default::default());
                    self.consensus_state.replace(Default::default());

                    let mut res_commit = ResponseCommit::default();
                    let self_store = self_store_shared.into_inner();

                    res_commit.data = self_store.root_hash()?.into();
                    self.store = Some(Shared::new(self_store));
                    Ok(Res::Commit(res_commit))
                }
                Req::CheckTx(req) => {
                    let app = self.app.take().unwrap();
                    let self_store = self.store.take().unwrap().into_inner();
                    let self_store_shared = Shared::new(self_store);
                    let mut store = Some(Shared::new(BufStore::wrap_with_map(
                        self_store_shared.clone(),
                        self.mempool_state.take().unwrap(),
                    )));

                    let res_check_tx = {
                        let owned_store = store.take().unwrap();
                        let flush_store = Shared::new(BufStore::wrap(owned_store.clone()));
                        let res = app.check_tx(flush_store.clone(), req)?;

                        let mut unwrapped_fs = flush_store.into_inner();
                        unwrapped_fs.flush()?;
                        store.replace(owned_store);
                        res
                    };

                    self.app.replace(app);
                    self.mempool_state
                        .replace(store.unwrap().into_inner().into_map());
                    self.store = Some(Shared::new(self_store_shared.into_inner()));
                    Ok(Res::CheckTx(res_check_tx))
                }
                Req::ListSnapshots(_req) => {
                    let self_store = self.store.as_mut().unwrap();
                    let snapshots = self_store.borrow_mut().list_snapshots()?;
                    let res = ResponseListSnapshots { snapshots };

                    Ok(Res::ListSnapshots(res))
                }
                Req::OfferSnapshot(req) => {
                    let self_store = self.store.as_mut().unwrap();
                    let return_val =
                        Res::OfferSnapshot(self_store.borrow_mut().offer_snapshot(req)?);
                    Ok(return_val)
                }
                Req::LoadSnapshotChunk(req) => {
                    let self_store = self.store.as_mut().unwrap();
                    let chunk = self_store.borrow_mut().load_snapshot_chunk(req)?.into();
                    let res = ResponseLoadSnapshotChunk { chunk };

                    Ok(Res::LoadSnapshotChunk(res))
                }
                Req::ApplySnapshotChunk(req) => {
                    let self_store = self.store.as_mut().unwrap();
                    let mut res = ResponseApplySnapshotChunk::default();
                    match self_store.borrow_mut().apply_snapshot_chunk(req.clone()) {
                        Ok(_) => res.result = 1, // ACCEPT
                        Err(_) => {
                            res.result = 3; // RETRY
                            res.refetch_chunks = vec![req.index];
                            res.reject_senders = vec![req.sender];
                        }
                    };
                    let return_val = Res::ApplySnapshotChunk(res);
                    Ok(return_val)
                }
            }
        }

        /// Creates a TCP server for the ABCI protocol and begins handling the
        /// incoming connections.
        pub fn listen<SA: ToSocketAddrs>(mut self, addr: SA) -> Result<Arc<RwLock<bool>>> {
            if let Some(stop_height_str) = env::var_os("ORGA_STOP_HEIGHT") {
                let _stop_height: u64 = stop_height_str
                    .into_string()
                    .unwrap()
                    .parse()
                    .expect("Invalid ORGA_STOP_HEIGHT value");
            }

            let server = abci2::Server::listen(addr)?;

            // TODO: keep workers in struct
            // TODO: more intelligently handle connections, e.g. handle tendermint dying/reconnecting?
            self.create_worker(server.accept()?, self.shutdown.clone())?;
            self.create_worker(server.accept()?, self.shutdown.clone())?;
            self.create_worker(server.accept()?, self.shutdown.clone())?;
            self.create_worker(server.accept()?, self.shutdown.clone())?;

            loop {
                if let Some(e) = self.shutdown.read().unwrap().as_ref() {
                    let mut shutdown = self.shutdown_notifier.write().unwrap();
                    *shutdown = true;
                    return Err(Error::ABCI(e.to_string()));
                }
                let (req, cb) = match self
                    .receiver
                    .recv_timeout(std::time::Duration::from_secs(1))
                {
                    Ok((req, cb)) => (req, cb),
                    Err(e) => {
                        log::debug!("{}", e.to_string());
                        continue;
                    }
                };
                let is_commit = matches!(req.value, Some(Req::Commit(_)));
                let value = match self.run(req) {
                    Ok(val) => val,
                    Err(e) => {
                        let mut shutdown = self.shutdown.write().unwrap();
                        *shutdown = Some(Error::ABCI(e.to_string()));
                        let mut shutdown = self.shutdown_notifier.write().unwrap();
                        *shutdown = true;
                        return Err(e);
                    }
                };
                let res = Response { value: Some(value) };
                cb.send(res).unwrap();

                if is_commit {
                    if let Some(stop_height_str) = env::var_os("ORGA_STOP_HEIGHT") {
                        let stop_height: u64 = stop_height_str
                            .into_string()
                            .unwrap()
                            .parse()
                            .expect("Invalid ORGA_STOP_HEIGHT value");
                        if self.height >= stop_height {
                            let mut shutdown = self.shutdown_notifier.write().unwrap();
                            *shutdown = true;
                            log::info!("Yummy touch stop height");
                            break Err(Error::ABCI(format!(
                                "Reached stop height ({})",
                                stop_height
                            )));
                        }
                    }
                }
            }
        }

        /// Creates a new worker to handle the incoming ABCI requests for `conn`
        /// within its own threads.
        fn create_worker(
            &self,
            conn: abci2::Connection,
            shutdown: Arc<RwLock<Option<Error>>>,
        ) -> Result<Worker> {
            Ok(Worker::new(self.sender.clone(), conn, shutdown))
        }
    }

    struct Worker {
        #[allow(dead_code)]
        thread: std::thread::JoinHandle<()>, // TODO: keep handle to connection or socket so we can close it
    }

    impl Worker {
        fn new(
            req_sender: SyncSender<(Request, SyncSender<Response>)>,
            mut conn: abci2::Connection,
            shutdown: Arc<RwLock<Option<Error>>>,
        ) -> Self {
            let thread = std::thread::spawn(move || {
                let (res_sender, res_receiver) = mpsc::sync_channel(0);
                loop {
                    if shutdown.read().unwrap().is_some() {
                        if let Err(e) = conn.close() {
                            log::debug!("Error closing connection: {}", e);
                        };
                        break;
                    }
                    let req = match conn.read() {
                        Ok(req) => req,
                        Err(e) => {
                            let mut shutdown = shutdown.write().unwrap();
                            *shutdown = Some(Error::ABCI2(e));
                            return;
                        }
                    };
                    if let Err(err) = req_sender.send((req, res_sender.clone())) {
                        log::warn!("Error sending request from worker: {}", err);
                        log::info!("req sender: {:?}, res_sender: {:?}", req_sender, res_sender);
                        break;
                    }
                    let res = res_receiver.recv().unwrap();
                    conn.write(res).unwrap();
                }
            });
            Worker { thread }
        }
    }

    pub type WrappedMerk = Shared<BufStore<Shared<BufStore<Shared<MerkStore>>>>>;
    /// An interface for handling ABCI requests.
    ///
    /// All methods have a default implemenation which returns an empty response.
    ///
    /// Only exposes the core state machine requests since messages like Echo and
    /// Info are automatically handled within
    /// [`ABCIStateMachine`](struct.ABCIStateMachine.html).
    pub trait Application {
        fn init_chain(
            &self,
            _store: WrappedMerk,
            _req: RequestInitChain,
        ) -> Result<ResponseInitChain> {
            Ok(Default::default())
        }

        fn begin_block(
            &self,
            _store: WrappedMerk,
            _req: RequestBeginBlock,
        ) -> Result<ResponseBeginBlock> {
            Ok(Default::default())
        }

        fn deliver_tx(
            &self,
            _store: WrappedMerk,
            _req: RequestDeliverTx,
        ) -> Result<ResponseDeliverTx> {
            Ok(Default::default())
        }

        fn end_block(
            &self,
            _store: WrappedMerk,
            _req: RequestEndBlock,
        ) -> Result<ResponseEndBlock> {
            Ok(Default::default())
        }

        fn check_tx(&self, _store: WrappedMerk, _req: RequestCheckTx) -> Result<ResponseCheckTx> {
            Ok(Default::default())
        }

        fn query(&self, _store: Shared<MerkStore>, _req: RequestQuery) -> Result<ResponseQuery> {
            Ok(Default::default())
        }
    }

    /// Interface for persisting ABCI app state, as a supertrait of [`store::Store`](../store/trait.Store.html).
    pub trait ABCIStore: Read + Write {
        fn height(&self) -> Result<u64>;

        fn root_hash(&self) -> Result<Vec<u8>>;

        fn commit(&mut self, header: Header) -> Result<()>;

        fn list_snapshots(&self) -> Result<Vec<Snapshot>>;

        fn load_snapshot_chunk(&self, req: RequestLoadSnapshotChunk) -> Result<Vec<u8>>;

        fn offer_snapshot(&mut self, req: RequestOfferSnapshot) -> Result<ResponseOfferSnapshot>;

        fn apply_snapshot_chunk(&mut self, req: RequestApplySnapshotChunk) -> Result<()>;
    }

    /// A basic implementation of [`ABCIStore`](trait.ABCIStore.html) which persists
    /// data in memory (mostly for use in testing).
    pub struct MemStore {
        height: u64,
        store: MapStore,
    }

    impl MemStore {
        pub fn new() -> Self {
            MemStore {
                height: 0,
                store: MapStore::new(),
            }
        }
    }

    impl Default for MemStore {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Read for MemStore {
        fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
            self.store.get(key)
        }

        fn get_next(&self, key: &[u8]) -> Result<Option<KV>> {
            self.store.get_next(key)
        }

        fn get_prev(&self, key: Option<&[u8]>) -> Result<Option<KV>> {
            self.store.get_prev(key)
        }
    }

    impl Write for MemStore {
        fn put(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result<()> {
            self.store.put(key, value)
        }

        fn delete(&mut self, key: &[u8]) -> Result<()> {
            self.store.delete(key)
        }
    }

    impl ABCIStore for MemStore {
        fn height(&self) -> Result<u64> {
            Ok(self.height)
        }

        fn root_hash(&self) -> Result<Vec<u8>> {
            // TODO: real hashing based on writes
            Ok(vec![])
        }

        fn commit(&mut self, header: Header) -> Result<()> {
            self.height = header.height as u64;
            Ok(())
        }

        fn list_snapshots(&self) -> Result<Vec<Snapshot>> {
            Ok(Default::default())
        }

        fn load_snapshot_chunk(&self, _req: RequestLoadSnapshotChunk) -> Result<Vec<u8>> {
            unimplemented!()
        }

        fn apply_snapshot_chunk(&mut self, _req: RequestApplySnapshotChunk) -> Result<()> {
            unimplemented!()
        }

        fn offer_snapshot(&mut self, _req: RequestOfferSnapshot) -> Result<ResponseOfferSnapshot> {
            Ok(Default::default())
        }
    }
}

#[cfg(feature = "abci")]
pub use server::*;

use crate::plugins::{BeginBlockCtx, EndBlockCtx, InitChainCtx};
pub trait BeginBlock {
    fn begin_block(&mut self, ctx: &BeginBlockCtx) -> Result<()>;
}

impl<S> BeginBlock for S {
    default fn begin_block(&mut self, _req: &BeginBlockCtx) -> Result<()> {
        Ok(())
    }
}
pub trait EndBlock {
    fn end_block(&mut self, ctx: &EndBlockCtx) -> Result<()>;
}

impl<S> EndBlock for S {
    default fn end_block(&mut self, _ctx: &EndBlockCtx) -> Result<()> {
        Ok(())
    }
}
pub trait InitChain {
    fn init_chain(&mut self, ctx: &InitChainCtx) -> Result<()>;
}

impl<S> InitChain for S {
    default fn init_chain(&mut self, _ctx: &InitChainCtx) -> Result<()> {
        Ok(())
    }
}

pub trait AbciQuery {
    fn abci_query(&self, request: &RequestQuery) -> Result<ResponseQuery>;
}

impl<S> AbciQuery for S {
    default fn abci_query(&self, request: &RequestQuery) -> Result<ResponseQuery> {
        Ok(ResponseQuery {
            code: 1,
            height: request.height,
            log: format!("Query path not handled: {}", request.path),
            ..Default::default()
        })
    }
}

pub trait App:
    BeginBlock + EndBlock + InitChain + State + Call + Query + Default + AbciQuery
{
}
impl<T: Default + BeginBlock + EndBlock + InitChain + State + Call + Query + AbciQuery> App for T {}
