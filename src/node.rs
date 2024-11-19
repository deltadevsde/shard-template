use anyhow::{Context, Result};
use async_channel::{bounded, Receiver, Sender};
use async_lock::Mutex;
use celestia_rpc::{BlobClient, HeaderClient};
use celestia_types::{nmt::Namespace, Blob, TxConfig};
use futures_lite::future;
use smol::Timer;
use std::sync::Arc;
use std::time::Duration;

use crate::tx::Batch;
use crate::{state::State, tx::Transaction};

const DEFAULT_BATCH_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub struct Config {
    /// The namespace used by this rollup.
    namespace: Namespace,

    /// The height from which to start syncing.
    // TODO: Backwards sync, accepting trusted state (celestia blocks get
    // pruned)
    start_height: u64,

    /// The URL of the Celestia node to connect to.
    // TODO: Move fully to Lumina, only use a url for posting transactions
    // until p2p tx transmission is implemented
    celestia_url: String,
    /// The auth token to use when connecting to Celestia.
    auth_token: Option<&'static str>,

    /// The interval at which to post batches of transactions.
    batch_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            namespace: Namespace::new_v0(&[42, 42, 42, 42]).unwrap(),
            start_height: 0,
            celestia_url: "http://localhost:8080".to_string(),
            auth_token: None,
            batch_interval: DEFAULT_BATCH_INTERVAL,
        }
    }
}

pub struct Node {
    da_client: celestia_rpc::Client,
    cfg: Config,

    /// The state of the rollup that is mutated by incoming transactions
    state: Arc<Mutex<State>>,

    /// Transactions that have been queued for batch posting to Celestia
    pending_transactions: Arc<Mutex<Vec<Transaction>>>,

    /// Used to notify the syncer that genesis sync has completed, and queued
    /// stored blocks from incoming sync can be processed
    genesis_sync_completed: (Sender<()>, Receiver<()>),
}

impl Node {
    pub async fn new(cfg: Config) -> Result<Self> {
        let da_client = celestia_rpc::Client::new(&cfg.celestia_url, cfg.auth_token)
            .await
            .context("Couldn't start Celestia client")?;

        Ok(Node {
            cfg,
            da_client,
            genesis_sync_completed: bounded(1),
            pending_transactions: Arc::new(Mutex::new(Vec::new())),
            state: Arc::new(Mutex::new(State::new())),
        })
    }

    pub async fn queue_transaction(&self, tx: Transaction) -> Result<()> {
        self.pending_transactions.lock().await.push(tx);
        Ok(())
    }

    async fn post_pending_batch(&self) -> Result<Batch> {
        let mut pending_txs = self.pending_transactions.lock().await;
        if pending_txs.is_empty() {
            return Ok(Batch::new(Vec::new()));
        }

        let batch = Batch::new(pending_txs.drain(..).collect());
        let encoded_batch = bincode::serialize(&batch)?;
        let blob = Blob::new(self.cfg.namespace, encoded_batch)?;
        BlobClient::blob_submit(&self.da_client, &[blob], TxConfig::default()).await?;

        Ok(batch)
    }

    async fn process_l1_block(&self, blobs: Vec<Blob>) {
        let txs: Vec<Transaction> = blobs
            .into_iter()
            .flat_map(|blob| {
                Batch::try_from(&blob)
                    .map(|b| b.get_transactions())
                    .unwrap_or_default()
            })
            .collect();

        let mut state = self.state.lock().await;
        for tx in txs {
            if let Err(e) = state.process_tx(tx) {
                error!("processing tx: {}", e);
            }
        }
    }

    async fn sync_historical(&self) -> Result<()> {
        let network_head = HeaderClient::header_network_head(&self.da_client).await?;
        let network_height = network_head.height();
        info!(
            "syncing historical blocks from {}-{}",
            self.cfg.start_height,
            network_height.value()
        );

        for height in self.cfg.start_height..network_height.value() {
            if let Some(blobs) =
                BlobClient::blob_get_all(&self.da_client, height, &[self.cfg.namespace]).await?
            {
                self.process_l1_block(blobs).await;
            }
        }

        let _ = self.genesis_sync_completed.0.send(()).await;
        info!("historical sync completed");

        Ok(())
    }

    async fn start_batch_posting(&self) {
        loop {
            Timer::after(self.cfg.batch_interval).await;
            match self.post_pending_batch().await {
                Ok(batch) => {
                    let tx_count = batch.get_transactions().len();
                    if tx_count > 0 {
                        info!("batch posted with {} transactions", tx_count);
                    }
                }
                Err(e) => error!("posting batch: {}", e),
            }
        }
    }

    async fn sync_incoming_blocks(&self) -> Result<()> {
        let mut blobsub = BlobClient::blob_subscribe(&self.da_client, self.cfg.namespace)
            .await
            .context("Failed to subscribe to app namespace")?;

        self.genesis_sync_completed.1.recv().await?;

        while let Some(result) = blobsub.next().await {
            match result {
                Ok(blob_response) => {
                    if let Some(blobs) = blob_response.blobs {
                        self.process_l1_block(blobs).await;
                    }
                }
                Err(e) => error!("retrieving blobs from DA layer: {}", e),
            }
        }
        Ok(())
    }

    pub async fn start(self: Arc<Self>) -> Result<()> {
        let genesis_sync = {
            let node = self.clone();
            smol::spawn(async move { node.sync_historical().await })
        };

        let incoming_sync = {
            let node = self.clone();
            smol::spawn(async move { node.sync_incoming_blocks().await })
        };

        let batch_posting = {
            let node = self.clone();
            smol::spawn(async move {
                node.start_batch_posting().await;
                Ok(()) as Result<()>
            })
        };

        future::race(future::race(genesis_sync, incoming_sync), batch_posting).await?;

        Ok(())
    }
}
