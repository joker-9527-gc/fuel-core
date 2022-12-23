//! This module handles bridge communications between the fuel node and the data availability layer.

use crate::{
    log::EthEventLog,
    ports::RelayerDb,
    service::state::EthLocal,
    Config,
};
use async_trait::async_trait;
use core::time::Duration;
use ethers_core::types::{
    Filter,
    Log,
    SyncingStatus,
    ValueOrArray,
    H160,
};
use ethers_providers::{
    Http,
    Middleware,
    Provider,
    ProviderError,
};
use fuel_core_services::{
    RunnableService,
    ServiceRunner,
};
use fuel_core_storage::Result as StorageResult;
use fuel_core_types::{
    blockchain::primitives::DaBlockHeight,
    entities::message::Message,
};
use std::{
    convert::TryInto,
    ops::Deref,
    sync::Arc,
};
use synced::update_synced;
use tokio::sync::watch;

use self::{
    get_logs::*,
    run::RelayerData,
};

mod get_logs;
mod run;
mod state;
mod synced;
mod syncing;

#[cfg(test)]
mod test;

type Synced = watch::Receiver<bool>;
type NotifySynced = watch::Sender<bool>;

/// The alias of runnable relayer service.
pub type Service<D> = CustomizableService<Provider<Http>, D>;
type CustomizableService<P, D> = ServiceRunner<Task<P, D>>;

/// Receives signals when the relayer reaches consistency with the DA layer.
#[derive(Clone)]
pub struct RelayerSynced {
    synced: Synced,
}

/// The actual relayer that runs on a background task
/// to sync with the DA layer.
pub struct Task<P, D> {
    /// Sends signals when the relayer reaches consistency with the DA layer.
    synced: NotifySynced,
    /// The node that communicates with Ethereum.
    eth_node: Arc<P>,
    /// The fuel database.
    database: D,
    /// Configuration settings.
    config: Config,
}

impl<P, D> Task<P, D> {
    /// Create a new relayer task.
    fn new(synced: NotifySynced, eth_node: P, database: D, config: Config) -> Self {
        Self {
            synced,
            eth_node: Arc::new(eth_node),
            database,
            config,
        }
    }
}

impl<P, D> Task<P, D>
where
    P: Middleware<Error = ProviderError> + 'static,
    D: RelayerDb + 'static,
{
    async fn set_deploy_height(&mut self) {
        if self.finalized().await.unwrap_or_default() < *self.config.da_deploy_height {
            self.database
                .set_finalized_da_height(self.config.da_deploy_height)
                .await
                .expect("Should be able to set the finalized da height");
        }
    }
}

#[async_trait]
impl<P, D> RelayerData for Task<P, D>
where
    P: Middleware<Error = ProviderError> + 'static,
    D: RelayerDb + 'static,
{
    async fn wait_if_eth_syncing(&self) -> anyhow::Result<()> {
        syncing::wait_if_eth_syncing(
            &self.eth_node,
            self.config.syncing_call_frequency,
            self.config.syncing_log_frequency,
        )
        .await
    }

    async fn download_logs(
        &mut self,
        eth_sync_gap: &state::EthSyncGap,
    ) -> anyhow::Result<()> {
        let logs = download_logs(
            eth_sync_gap,
            self.config.eth_v2_listening_contracts.clone(),
            self.eth_node.clone(),
            self.config.log_page_size,
        );
        write_logs(&mut self.database, logs).await
    }

    async fn set_finalized_da_height(
        &mut self,
        height: DaBlockHeight,
    ) -> StorageResult<()> {
        self.database.set_finalized_da_height(height).await
    }

    fn update_synced(&self, state: &state::EthState) {
        update_synced(&self.synced, state)
    }
}

#[async_trait]
impl<P, D> RunnableService for Task<P, D>
where
    P: Middleware<Error = ProviderError> + 'static,
    D: RelayerDb + 'static,
{
    const NAME: &'static str = "Relayer";

    type SharedData = RelayerSynced;

    fn shared_data(&self) -> Self::SharedData {
        let synced = self.synced.subscribe();

        RelayerSynced { synced }
    }

    async fn initialize(&mut self) -> anyhow::Result<()> {
        self.set_deploy_height().await;
        Ok(())
    }

    async fn run(&mut self) -> anyhow::Result<bool> {
        let now = tokio::time::Instant::now();
        let result = run::run(self).await;

        // Sleep the loop so the da node is not spammed.
        tokio::time::sleep(
            self.config
                .sync_minimum_duration
                .saturating_sub(now.elapsed()),
        )
        .await;

        result.map(|_| true)
    }
}

impl RelayerSynced {
    /// Wait for the [`Task`] to be in sync with
    /// the data availability layer.
    ///
    /// Yields until the relayer reaches a point where it
    /// considered up to date. Note that there's no guarantee
    /// the relayer will ever catch up to the da layer and
    /// may fall behind immediately after this future completes.
    ///
    /// The only guarantee is that if this future completes then
    /// the relayer did reach consistency with the da layer for
    /// some period of time.
    pub async fn await_synced(&self) -> anyhow::Result<()> {
        let mut rx = self.synced.clone();
        if !rx.borrow_and_update().deref() {
            rx.changed().await?;
        }
        Ok(())
    }
}

#[async_trait]
impl<P, D> state::EthRemote for Task<P, D>
where
    P: Middleware<Error = ProviderError>,
    D: RelayerDb + 'static,
{
    async fn current(&self) -> anyhow::Result<u64> {
        Ok(self.eth_node.get_block_number().await?.as_u64())
    }

    fn finalization_period(&self) -> u64 {
        *self.config.da_finalization
    }
}

#[async_trait]
impl<P, D> EthLocal for Task<P, D>
where
    P: Middleware<Error = ProviderError>,
    D: RelayerDb + 'static,
{
    async fn finalized(&self) -> Option<u64> {
        self.database
            .get_finalized_da_height()
            .await
            .map(|h| *h)
            .ok()
    }
}

/// Creates an instance of runnable relayer service.
pub fn new_service<D>(database: D, config: Config) -> anyhow::Result<Service<D>>
where
    D: RelayerDb + 'static,
{
    let url = config.eth_client.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "Tried to start Relayer without setting an eth_client in the config"
        )
    })?;
    // TODO: Does this handle https?
    let http = Http::new(url);
    let eth_node = Provider::new(http);
    Ok(new_service_internal(eth_node, database, config))
}

#[cfg(any(test, feature = "test-helpers"))]
/// Start a test relayer.
pub fn new_service_test<P, D>(
    eth_node: P,
    database: D,
    config: Config,
) -> CustomizableService<P, D>
where
    P: Middleware<Error = ProviderError> + 'static,
    D: RelayerDb + 'static,
{
    new_service_internal(eth_node, database, config)
}

fn new_service_internal<P, D>(
    eth_node: P,
    database: D,
    config: Config,
) -> CustomizableService<P, D>
where
    P: Middleware<Error = ProviderError> + 'static,
    D: RelayerDb + 'static,
{
    let (tx, _) = watch::channel(false);
    let task = Task::new(tx, eth_node, database, config);

    CustomizableService::new(task)
}