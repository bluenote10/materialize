// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! A controller that provides an interface to the compute layer, and the storage layer below it.
//!
//! The compute controller manages the creation, maintenance, and removal of compute instances.
//! This involves ensuring the intended service state with the orchestrator, as well as maintaining
//! a dedicated compute instance controller for each active compute instance.
//!
//! For each compute instance, the compute controller curates the creation of indexes and sinks
//! installed on the instance, the progress of readers through these collections, and their
//! eventual dropping and resource reclamation.
//!
//! The state maintained for a compute instance can be viewed as a partial map from `GlobalId` to
//! collection. It is an error to use an identifier before it has been "created" with
//! `create_dataflow()`. Once created, the controller holds a read capability for each output
//! collection of a dataflow, which is manipulated with `set_read_policy()`. Eventually, a
//! collection is dropped with `drop_collections()`.
//!
//! Created dataflows will prevent the compaction of their inputs, including other compute
//! collections but also collections managed by the storage layer. Each dataflow input is prevented
//! from compacting beyond the allowed compaction of each of its outputs, ensuring that we can
//! recover each dataflow to its current state in case of failure or other reconfiguration.

use std::collections::BTreeMap;
use std::num::NonZeroI64;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use differential_dataflow::consolidation::consolidate;
use futures::{future, Future, FutureExt};
use mz_build_info::BuildInfo;
use mz_cluster_client::client::ClusterReplicaLocation;
use mz_cluster_client::ReplicaId;
use mz_compute_types::dataflows::DataflowDescription;
use mz_compute_types::ComputeInstanceId;
use mz_dyncfg::ConfigSet;
use mz_expr::RowSetFinishing;
use mz_ore::metrics::MetricsRegistry;
use mz_ore::tracing::OpenTelemetryContext;
use mz_repr::{Datum, Diff, GlobalId, Row, TimestampManipulation};
use mz_storage_client::controller::{IntrospectionType, StorageController};
use mz_storage_types::read_policy::ReadPolicy;
use serde::{Deserialize, Serialize};
use timely::progress::frontier::{AntichainRef, MutableAntichain};
use timely::progress::Antichain;
use tokio::time::{self, MissedTickBehavior};
use tracing::warn;
use uuid::Uuid;

use crate::controller::error::{
    CollectionLookupError, CollectionMissing, CollectionUpdateError, DataflowCreationError,
    InstanceExists, InstanceMissing, PeekError, ReadPolicyError, ReplicaCreationError,
    ReplicaDropError, SubscribeTargetError,
};
use crate::controller::instance::{ActiveInstance, Instance};
use crate::controller::replica::ReplicaConfig;
use crate::logging::{LogVariant, LoggingConfig};
use crate::metrics::ComputeControllerMetrics;
use crate::protocol::command::{ComputeParameters, PeekTarget};
use crate::protocol::response::{ComputeResponse, PeekResponse, SubscribeBatch};
use crate::service::{ComputeClient, ComputeGrpcClient};

mod instance;
mod replica;
mod sequential_hydration;

pub mod error;

type IntrospectionUpdates = (IntrospectionType, Vec<(Row, Diff)>);

/// A composite trait for types that serve as timestamps in the Compute Controller.
/// `Into<Datum<'a>>` is needed for writing timestamps to introspection collections.
pub trait ComputeControllerTimestamp: TimestampManipulation + for<'a> Into<Datum<'a>> {}

impl ComputeControllerTimestamp for mz_repr::Timestamp {}

/// Responses from the compute controller.
#[derive(Debug)]
pub enum ComputeControllerResponse<T> {
    /// See [`ComputeResponse::PeekResponse`].
    PeekResponse(Uuid, PeekResponse, OpenTelemetryContext),
    /// See [`ComputeResponse::SubscribeResponse`].
    SubscribeResponse(GlobalId, SubscribeBatch<T>),
    /// The response from a dataflow containing an `CopyToS3Oneshot` sink.
    ///
    /// The `GlobalId` identifies the sink. The `Result` is the response from
    /// the sink, where an `Ok(n)` indicates that `n` rows were successfully
    /// copied to S3 and an `Err` indicates that an error was encountered
    /// during the copy operation.
    ///
    /// For a given `CopyToS3Oneshot` sink, there will be at most one `CopyToResponse`
    /// produced. (The sink may produce no responses if its dataflow is dropped
    /// before completion.)
    CopyToResponse(GlobalId, Result<u64, anyhow::Error>),
    /// A response reporting advancement of a collection's upper frontier.
    ///
    /// Once a collection's upper (aka "write frontier") has advanced to beyond a given time, the
    /// contents of the collection as of that time have been sealed and cannot change anymore.
    FrontierUpper {
        /// The ID of a compute collection.
        id: GlobalId,
        /// The new upper frontier of the identified compute collection.
        upper: Antichain<T>,
    },
}

/// Replica configuration
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComputeReplicaConfig {
    /// TODO(#25239): Add documentation.
    pub logging: ComputeReplicaLogging,
}

/// Logging configuration of a replica.
#[derive(Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct ComputeReplicaLogging {
    /// Whether to enable logging for the logging dataflows.
    pub log_logging: bool,
    /// The interval at which to log.
    ///
    /// A `None` value indicates that logging is disabled.
    pub interval: Option<Duration>,
}

impl ComputeReplicaLogging {
    /// Return whether logging is enabled.
    pub fn enabled(&self) -> bool {
        self.interval.is_some()
    }
}

/// A controller for the compute layer.
pub struct ComputeController<T> {
    instances: BTreeMap<ComputeInstanceId, Instance<T>>,
    build_info: &'static BuildInfo,
    /// Set to `true` once `initialization_complete` has been called.
    initialized: bool,
    /// Compute configuration to apply to new instances.
    config: ComputeParameters,
    /// `arrangement_exert_proportionality` value passed to new replicas.
    arrangement_exert_proportionality: u32,
    /// A replica response to be handled by the corresponding `Instance` on a subsequent call to
    /// `ActiveComputeController::process`.
    stashed_replica_response: Option<(ComputeInstanceId, ReplicaId, ComputeResponse<T>)>,
    /// A number that increases on every `environmentd` restart.
    envd_epoch: NonZeroI64,
    /// The compute controller metrics.
    metrics: ComputeControllerMetrics,
    /// Dynamic system configuration.
    ///
    /// Updated through `ComputeController::update_configuration` calls and shared with all
    /// subcompontents of the compute controller.
    dyncfg: Arc<ConfigSet>,

    /// Receiver for responses produced by `Instance`s, to be delivered on subsequent calls to
    /// `ActiveComputeController::process`.
    response_rx: crossbeam_channel::Receiver<ComputeControllerResponse<T>>,
    /// Response sender that's passed to new `Instance`s.
    response_tx: crossbeam_channel::Sender<ComputeControllerResponse<T>>,
    /// Receiver for introspection updates produced by `Instance`s.
    introspection_rx: crossbeam_channel::Receiver<IntrospectionUpdates>,
    /// Introspection updates sender that's passed to new `Instance`s.
    introspection_tx: crossbeam_channel::Sender<IntrospectionUpdates>,

    /// Ticker for scheduling periodic maintenance work.
    maintenance_ticker: tokio::time::Interval,
    /// Whether maintenance work was scheduled.
    maintenance_scheduled: bool,
}

impl<T: ComputeControllerTimestamp> ComputeController<T> {
    /// Construct a new [`ComputeController`].
    pub fn new(
        build_info: &'static BuildInfo,
        envd_epoch: NonZeroI64,
        metrics_registry: MetricsRegistry,
    ) -> Self {
        let (response_tx, response_rx) = crossbeam_channel::unbounded();
        let (introspection_tx, introspection_rx) = crossbeam_channel::unbounded();

        let mut maintenance_ticker = time::interval(Duration::from_secs(1));
        maintenance_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        Self {
            instances: BTreeMap::new(),
            build_info,
            initialized: false,
            config: Default::default(),
            arrangement_exert_proportionality: 16,
            stashed_replica_response: None,
            envd_epoch,
            metrics: ComputeControllerMetrics::new(metrics_registry),
            dyncfg: Arc::new(mz_dyncfgs::all_dyncfgs()),
            response_rx,
            response_tx,
            introspection_rx,
            introspection_tx,
            maintenance_ticker,
            maintenance_scheduled: false,
        }
    }

    /// TODO(#25239): Add documentation.
    pub fn instance_exists(&self, id: ComputeInstanceId) -> bool {
        self.instances.contains_key(&id)
    }

    /// Return a reference to the indicated compute instance.
    fn instance(&self, id: ComputeInstanceId) -> Result<&Instance<T>, InstanceMissing> {
        self.instances.get(&id).ok_or(InstanceMissing(id))
    }

    /// Return a mutable reference to the indicated compute instance.
    fn instance_mut(&mut self, id: ComputeInstanceId) -> Result<&mut Instance<T>, InstanceMissing> {
        self.instances.get_mut(&id).ok_or(InstanceMissing(id))
    }

    /// Return a read-only handle to the indicated compute instance.
    pub fn instance_ref(
        &self,
        id: ComputeInstanceId,
    ) -> Result<ComputeInstanceRef<T>, InstanceMissing> {
        self.instance(id).map(|instance| ComputeInstanceRef {
            instance_id: id,
            instance,
        })
    }

    /// Return a read-only handle to the indicated collection.
    pub fn collection(
        &self,
        instance_id: ComputeInstanceId,
        collection_id: GlobalId,
    ) -> Result<&CollectionState<T>, CollectionLookupError> {
        let collection = self.instance(instance_id)?.collection(collection_id)?;
        Ok(collection)
    }

    /// Return a read-only handle to the indicated collection.
    pub fn find_collection(
        &self,
        collection_id: GlobalId,
    ) -> Result<&CollectionState<T>, CollectionLookupError> {
        self.instances
            .values()
            .flat_map(|i| i.collection(collection_id).ok())
            .next()
            .ok_or(CollectionLookupError::CollectionMissing(collection_id))
    }

    /// Acquire an [`ActiveComputeController`] by supplying a storage connection.
    pub fn activate<'a>(
        &'a mut self,
        storage: &'a mut dyn StorageController<Timestamp = T>,
    ) -> ActiveComputeController<'a, T> {
        ActiveComputeController {
            compute: self,
            storage,
        }
    }

    /// List compute collections that depend on the given collection.
    pub fn collection_reverse_dependencies(
        &self,
        instance_id: ComputeInstanceId,
        id: GlobalId,
    ) -> Result<impl Iterator<Item = &GlobalId>, InstanceMissing> {
        Ok(self
            .instance(instance_id)?
            .collection_reverse_dependencies(id))
    }

    /// Set the `arrangement_exert_proportionality` value to be passed to new replicas.
    pub fn set_arrangement_exert_proportionality(&mut self, value: u32) {
        self.arrangement_exert_proportionality = value;
    }

    /// Returns the read and write frontiers for each collection.
    pub fn collection_frontiers(&self) -> BTreeMap<GlobalId, (Antichain<T>, Antichain<T>)> {
        let collections = self.instances.values().flat_map(|i| i.collections_iter());
        collections
            .map(|(id, collection)| {
                let since = collection.read_frontier().to_owned();
                let upper = collection.write_frontier().to_owned();
                (*id, (since, upper))
            })
            .collect()
    }

    /// Returns the write frontier for each collection installed on each replica.
    pub fn replica_write_frontiers(&self) -> BTreeMap<(GlobalId, ReplicaId), Antichain<T>> {
        let mut result = BTreeMap::new();
        let collections = self.instances.values().flat_map(|i| i.collections_iter());
        for (&collection_id, collection) in collections {
            for (&replica_id, frontier) in &collection.replica_write_frontiers {
                result.insert((collection_id, replica_id), frontier.clone());
            }
        }
        result
    }

    /// Returns the state of the [`ComputeController`] formatted as JSON.
    ///
    /// The returned value is not guaranteed to be stable and may change at any point in time.
    pub fn dump(&self) -> Result<serde_json::Value, anyhow::Error> {
        // Note: We purposefully use the `Debug` formatting for the value of all fields in the
        // returned object as a tradeoff between usability and stability. `serde_json` will fail
        // to serialize an object if the keys aren't strings, so `Debug` formatting the values
        // prevents a future unrelated change from silently breaking this method.

        // Destructure `self` here so we don't forget to consider dumping newly added fields.
        let Self {
            instances,
            build_info: _,
            initialized,
            config: _,
            arrangement_exert_proportionality,
            stashed_replica_response,
            envd_epoch,
            metrics: _,
            dyncfg: _,
            response_rx: _,
            response_tx: _,
            introspection_rx: _,
            introspection_tx: _,
            maintenance_ticker: _,
            maintenance_scheduled,
        } = self;

        let instances: BTreeMap<_, _> = instances
            .iter()
            .map(|(id, instance)| Ok((id.to_string(), instance.dump()?)))
            .collect::<Result<_, anyhow::Error>>()?;

        fn field(
            key: &str,
            value: impl Serialize,
        ) -> Result<(String, serde_json::Value), anyhow::Error> {
            let value = serde_json::to_value(value)?;
            Ok((key.to_string(), value))
        }

        let map = serde_json::Map::from_iter([
            field("instances", instances)?,
            field("initialized", initialized)?,
            field(
                "arrangement_exert_proportionality",
                arrangement_exert_proportionality,
            )?,
            field(
                "stashed_replica_response",
                format!("{stashed_replica_response:?}"),
            )?,
            field("envd_epoch", envd_epoch)?,
            field("maintenance_scheduled", maintenance_scheduled)?,
        ]);
        Ok(serde_json::Value::Object(map))
    }
}

impl<T> ComputeController<T>
where
    T: ComputeControllerTimestamp,
    ComputeGrpcClient: ComputeClient<T>,
{
    /// Create a compute instance.
    pub fn create_instance(
        &mut self,
        id: ComputeInstanceId,
        arranged_logs: BTreeMap<LogVariant, GlobalId>,
    ) -> Result<(), InstanceExists> {
        if self.instances.contains_key(&id) {
            return Err(InstanceExists(id));
        }

        self.instances.insert(
            id,
            Instance::new(
                self.build_info,
                arranged_logs,
                self.envd_epoch,
                self.metrics.for_instance(id),
                Arc::clone(&self.dyncfg),
                self.response_tx.clone(),
                self.introspection_tx.clone(),
            ),
        );

        let instance = self.instances.get_mut(&id).expect("instance just added");
        if self.initialized {
            instance.initialization_complete();
        }

        let config_params = self.config.clone();
        instance.update_configuration(config_params);

        Ok(())
    }

    /// Remove a compute instance.
    ///
    /// # Panics
    ///
    /// Panics if the identified `instance` still has active replicas.
    pub fn drop_instance(&mut self, id: ComputeInstanceId) {
        if let Some(compute_state) = self.instances.remove(&id) {
            compute_state.drop();
        }
    }

    /// Update compute configuration.
    pub fn update_configuration(&mut self, config_params: ComputeParameters) {
        // Apply dyncfg updates.
        config_params.dyncfg_updates.apply(&self.dyncfg);

        // Forward updates to existing clusters.
        for instance in self.instances.values_mut() {
            instance.update_configuration(config_params.clone());
        }

        // Remember updates for future clusters.
        self.config.update(config_params);
    }

    /// Mark the end of any initialization commands.
    ///
    /// The implementor may wait for this method to be called before implementing prior commands,
    /// and so it is important for a user to invoke this method as soon as it is comfortable.
    /// This method can be invoked immediately, at the potential expense of performance.
    pub fn initialization_complete(&mut self) {
        self.initialized = true;
        for instance in self.instances.values_mut() {
            instance.initialization_complete();
        }
    }

    /// Wait until the controller is ready to do some processing.
    ///
    /// This method may block for an arbitrarily long time.
    ///
    /// When the method returns, the caller should call [`ActiveComputeController::process`].
    ///
    /// This method is cancellation safe.
    pub async fn ready(&mut self) {
        if self.stashed_replica_response.is_some() {
            // We still have a response stashed, which we are immediately ready to process.
            return;
        }
        if !self.response_rx.is_empty() {
            // We have responses waiting to be processed.
            return;
        }
        if self.maintenance_scheduled {
            // Maintenance work has been scheduled.
            return;
        }

        let receives: Pin<Box<dyn Future<Output = _>>> = if self.instances.is_empty() {
            // Calling `select_all` with an empty list of futures will panic.
            Box::pin(future::pending())
        } else {
            // `Instance::recv` is cancellation safe, so it is safe to construct this `select_all`.
            let iter = self
                .instances
                .iter_mut()
                .map(|(id, instance)| Box::pin(instance.recv().map(|result| (*id, result))));
            Box::pin(future::select_all(iter))
        };

        tokio::select! {
             ((instance_id, result), _index, _remaining) = receives => {
                match result {
                    Ok((replica_id, resp)) => {
                        self.stashed_replica_response = Some((instance_id, replica_id, resp));
                    }
                    Err(_) => {
                        // There is nothing to do here. `recv` has already added the failed replica to
                        // `instance.failed_replicas`, so it will be rehydrated in the next call to
                        // `ActiveComputeController::process`.
                    }
                }
            },
            _ = self.maintenance_ticker.tick() => {
                self.maintenance_scheduled = true;
            },
        }
    }

    /// Assign a target replica to the identified subscribe.
    ///
    /// If a subscribe has a target replica assigned, only subscribe responses
    /// sent by that replica are considered.
    pub fn set_subscribe_target_replica(
        &mut self,
        instance_id: ComputeInstanceId,
        subscribe_id: GlobalId,
        target_replica: ReplicaId,
    ) -> Result<(), SubscribeTargetError> {
        self.instance_mut(instance_id)?
            .set_subscribe_target_replica(subscribe_id, target_replica)?;
        Ok(())
    }
}

/// A wrapper around a [`ComputeController`] with a live connection to a storage controller.
pub struct ActiveComputeController<'a, T> {
    compute: &'a mut ComputeController<T>,
    storage: &'a mut dyn StorageController<Timestamp = T>,
}

impl<T: ComputeControllerTimestamp> ActiveComputeController<'_, T> {
    /// TODO(#25239): Add documentation.
    pub fn instance_exists(&self, id: ComputeInstanceId) -> bool {
        self.compute.instance_exists(id)
    }

    /// Return a read-only handle to the indicated collection.
    pub fn collection(
        &self,
        instance_id: ComputeInstanceId,
        collection_id: GlobalId,
    ) -> Result<&CollectionState<T>, CollectionLookupError> {
        self.compute.collection(instance_id, collection_id)
    }

    /// Return a handle to the indicated compute instance.
    fn instance(&mut self, id: ComputeInstanceId) -> Result<ActiveInstance<T>, InstanceMissing> {
        self.compute
            .instance_mut(id)
            .map(|c| c.activate(self.storage))
    }
}

impl<T> ActiveComputeController<'_, T>
where
    T: ComputeControllerTimestamp,
    ComputeGrpcClient: ComputeClient<T>,
{
    /// Adds replicas of an instance.
    pub fn add_replica_to_instance(
        &mut self,
        instance_id: ComputeInstanceId,
        replica_id: ReplicaId,
        location: ClusterReplicaLocation,
        config: ComputeReplicaConfig,
    ) -> Result<(), ReplicaCreationError> {
        let (enable_logging, interval) = match config.logging.interval {
            Some(interval) => (true, interval),
            None => (false, Duration::from_secs(1)),
        };

        let replica_config = ReplicaConfig {
            location,
            logging: LoggingConfig {
                interval,
                enable_logging,
                log_logging: config.logging.log_logging,
                index_logs: Default::default(),
            },
            arrangement_exert_proportionality: self.compute.arrangement_exert_proportionality,
            grpc_client: self.compute.config.grpc_client.clone(),
        };

        self.instance(instance_id)?
            .add_replica(replica_id, replica_config)?;
        Ok(())
    }

    /// Removes a replica from an instance, including its service in the orchestrator.
    pub fn drop_replica(
        &mut self,
        instance_id: ComputeInstanceId,
        replica_id: ReplicaId,
    ) -> Result<(), ReplicaDropError> {
        self.instance(instance_id)?.remove_replica(replica_id)?;
        Ok(())
    }

    /// Create and maintain the described dataflows, and initialize state for their output.
    ///
    /// This method creates dataflows whose inputs are still readable at the dataflow `as_of`
    /// frontier, and initializes the outputs as readable from that frontier onward.
    /// It installs read dependencies from the outputs to the inputs, so that the input read
    /// capabilities will be held back to the output read capabilities, ensuring that we are
    /// always able to return to a state that can serve the output read capabilities.
    pub fn create_dataflow(
        &mut self,
        instance_id: ComputeInstanceId,
        dataflow: DataflowDescription<mz_compute_types::plan::Plan<T>, (), T>,
    ) -> Result<(), DataflowCreationError> {
        self.instance(instance_id)?.create_dataflow(dataflow)?;
        Ok(())
    }

    /// Drop the read capability for the given collections and allow their resources to be
    /// reclaimed.
    pub fn drop_collections(
        &mut self,
        instance_id: ComputeInstanceId,
        collection_ids: Vec<GlobalId>,
    ) -> Result<(), CollectionUpdateError> {
        self.instance(instance_id)?
            .drop_collections(collection_ids)?;
        Ok(())
    }

    /// Initiate a peek request for the contents of the given collection at `timestamp`.
    pub fn peek(
        &mut self,
        instance_id: ComputeInstanceId,
        collection_id: GlobalId,
        literal_constraints: Option<Vec<Row>>,
        uuid: Uuid,
        timestamp: T,
        finishing: RowSetFinishing,
        map_filter_project: mz_expr::SafeMfpPlan,
        target_replica: Option<ReplicaId>,
        peek_target: PeekTarget,
    ) -> Result<(), PeekError> {
        self.instance(instance_id)?.peek(
            collection_id,
            literal_constraints,
            uuid,
            timestamp,
            finishing,
            map_filter_project,
            target_replica,
            peek_target,
        )?;
        Ok(())
    }

    /// Cancel an existing peek request.
    ///
    /// Canceling a peek is best effort. The caller may see any of the following
    /// after canceling a peek request:
    ///
    ///   * A `PeekResponse::Rows` indicating that the cancellation request did
    ///     not take effect in time and the query succeeded.
    ///   * A `PeekResponse::Canceled` affirming that the peek was canceled.
    ///   * No `PeekResponse` at all.
    pub fn cancel_peek(
        &mut self,
        instance_id: ComputeInstanceId,
        uuid: Uuid,
    ) -> Result<(), InstanceMissing> {
        self.instance(instance_id)?.cancel_peek(uuid);
        Ok(())
    }

    /// Assign a read policy to specific identifiers.
    ///
    /// The policies are assigned in the order presented, and repeated identifiers should
    /// conclude with the last policy. Changing a policy will immediately downgrade the read
    /// capability if appropriate, but it will not "recover" the read capability if the prior
    /// capability is already ahead of it.
    ///
    /// Identifiers not present in `policies` retain their existing read policies.
    ///
    /// It is an error to attempt to set a read policy for a collection that is not readable in the
    /// context of compute. At this time, only indexes are readable compute collections.
    pub fn set_read_policy(
        &mut self,
        instance_id: ComputeInstanceId,
        policies: Vec<(GlobalId, ReadPolicy<T>)>,
    ) -> Result<(), ReadPolicyError> {
        self.instance(instance_id)?.set_read_policy(policies)?;
        Ok(())
    }

    #[mz_ore::instrument(level = "debug")]
    async fn record_introspection_updates(&mut self) {
        // We could record the contents of `introspection_rx` directly here, but to reduce the
        // pressure on persist we spend some effort consolidating first.
        let mut updates_by_type = BTreeMap::new();

        for (type_, updates) in self.compute.introspection_rx.try_iter() {
            updates_by_type
                .entry(type_)
                .or_insert_with(Vec::new)
                .extend(updates);
        }
        for updates in updates_by_type.values_mut() {
            consolidate(updates);
        }

        for (type_, updates) in updates_by_type {
            if !updates.is_empty() {
                self.storage
                    .record_introspection_updates(type_, updates)
                    .await;
            }
        }
    }

    /// Processes the work queued by [`ComputeController::ready`].
    #[mz_ore::instrument(level = "debug")]
    pub async fn process(&mut self) -> Option<ComputeControllerResponse<T>> {
        // Perform periodic maintenance work.
        if self.compute.maintenance_scheduled {
            self.maintain().await;
            self.compute.maintenance_ticker.reset();
            self.compute.maintenance_scheduled = false;
        }

        // Process pending ready responses.
        match self.compute.response_rx.try_recv() {
            Ok(response) => return Some(response),
            Err(crossbeam_channel::TryRecvError::Empty) => (),
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                // This should never happen, since the `ComputeController` is always holding on to
                // a copy of the `response_tx`.
                panic!("response_tx has disconnected");
            }
        }

        // Process pending responses from replicas.
        if let Some((instance_id, replica_id, response)) =
            self.compute.stashed_replica_response.take()
        {
            if let Ok(mut instance) = self.instance(instance_id) {
                return instance.handle_response(response, replica_id);
            } else {
                warn!(
                    ?instance_id,
                    ?response,
                    "processed response from unknown instance"
                );
            };
        }

        None
    }

    #[mz_ore::instrument(level = "debug")]
    async fn maintain(&mut self) {
        // Perform instance maintenance work.
        for instance in self.compute.instances.values_mut() {
            instance.activate(self.storage).maintain();
        }

        // Record pending introspection updates.
        //
        // It's beneficial to do this as the last maintenance step because previous steps can cause
        // dropping of state, which can can cause introspection retractions, which lower the volume
        // of data we have to record.
        self.record_introspection_updates().await;
    }
}

/// A read-only handle to a compute instance.
#[derive(Debug, Clone, Copy)]
pub struct ComputeInstanceRef<'a, T> {
    instance_id: ComputeInstanceId,
    instance: &'a Instance<T>,
}

impl<T: ComputeControllerTimestamp> ComputeInstanceRef<'_, T> {
    /// Return the ID of this compute instance.
    pub fn instance_id(&self) -> ComputeInstanceId {
        self.instance_id
    }

    /// Return a read-only handle to the indicated collection.
    pub fn collection(&self, id: GlobalId) -> Result<&CollectionState<T>, CollectionMissing> {
        self.instance.collection(id)
    }

    /// Return an iterator over the installed collections.
    pub fn collections(&self) -> impl Iterator<Item = (&GlobalId, &CollectionState<T>)> {
        self.instance.collections_iter()
    }
}

/// State maintained about individual compute collections.
///
/// A compute collection is either an index, or a storage sink, or a subscribe, exported by a
/// compute dataflow.
#[derive(Debug)]
pub struct CollectionState<T> {
    /// Whether this collection is a log collection.
    ///
    /// Log collections are special in that they are only maintained by a subset of all replicas.
    log_collection: bool,
    /// Whether this collection has been dropped by a controller client.
    ///
    /// The controller is allowed to remove the `CollectionState` for a collection only when
    /// `dropped == true`. Otherwise, clients might still expect to be able to query information
    /// about this collection.
    dropped: bool,
    /// Whether this collection has been scheduled, i.e., the controller has sent a `Schedule`
    /// command for it.
    scheduled: bool,

    /// Accumulation of read capabilities for the collection.
    ///
    /// This accumulation will always contain `implied_capability` and `warmup_capability`, but may
    /// also contain capabilities held by others who have read dependencies on this collection.
    read_capabilities: MutableAntichain<T>,
    /// The implicit capability associated with collection creation.
    implied_capability: Antichain<T>,
    /// A capability held to enable dataflow warmup.
    ///
    /// Dataflow warmup is an optimization that allows dataflows to immediately start hydrating
    /// even when their next output time (as implied by the `write_frontier`) is in the future.
    /// By installing a read capability derived from the write frontiers of the collection's
    /// inputs, we ensure that the as-of of new dataflows installed for the collection is at a time
    /// that is immediately available, so hydration can begin immediately too.
    warmup_capability: Antichain<T>,
    /// The policy to use to downgrade `self.implied_capability`.
    ///
    /// If `None`, the collection is a write-only collection (i.e. a sink). For write-only
    /// collections, the `implied_capability` is only required for maintaining read holds on the
    /// inputs, so we can immediately downgrade it to the `write_frontier`.
    read_policy: Option<ReadPolicy<T>>,

    /// Storage identifiers on which this collection depends.
    storage_dependencies: Vec<GlobalId>,
    /// Compute identifiers on which this collection depends.
    compute_dependencies: Vec<GlobalId>,

    /// The write frontier of this collection.
    write_frontier: Antichain<T>,
    /// The write frontiers reported by individual replicas.
    replica_write_frontiers: BTreeMap<ReplicaId, Antichain<T>>,
    /// The input frontiers reported by individual replicas.
    replica_input_frontiers: BTreeMap<ReplicaId, Antichain<T>>,
}

impl<T> CollectionState<T> {
    /// Reports the current read capability.
    pub fn read_capability(&self) -> &Antichain<T> {
        &self.implied_capability
    }

    /// Reports the current read frontier.
    pub fn read_frontier(&self) -> AntichainRef<T> {
        self.read_capabilities.frontier()
    }

    /// Reports the current write frontier.
    pub fn write_frontier(&self) -> AntichainRef<T> {
        self.write_frontier.borrow()
    }

    /// Reports the IDs of the dependencies of this collection.
    fn dependency_ids(&self) -> impl Iterator<Item = GlobalId> + '_ {
        let compute = self.compute_dependencies.iter().copied();
        let storage = self.storage_dependencies.iter().copied();
        compute.chain(storage)
    }
}

impl<T: ComputeControllerTimestamp> CollectionState<T> {
    /// Creates a new collection state, with an initial read policy valid from `since`.
    pub fn new(
        as_of: Antichain<T>,
        storage_dependencies: Vec<GlobalId>,
        compute_dependencies: Vec<GlobalId>,
    ) -> Self {
        // A collection is not readable before the `as_of`.
        let since = as_of.clone();
        // A collection won't produce updates for times before the `as_of`.
        let upper = as_of;

        // Initialize all read capabilities to the `since`.
        let implied_capability = since.clone();
        let warmup_capability = since.clone();

        let mut read_capabilities = MutableAntichain::new();
        read_capabilities.update_iter(implied_capability.iter().map(|time| (time.clone(), 1)));
        read_capabilities.update_iter(warmup_capability.iter().map(|time| (time.clone(), 1)));

        Self {
            log_collection: false,
            dropped: false,
            scheduled: false,
            read_capabilities,
            implied_capability,
            warmup_capability,
            read_policy: Some(ReadPolicy::ValidFrom(since)),
            storage_dependencies,
            compute_dependencies,
            write_frontier: upper,
            replica_write_frontiers: BTreeMap::new(),
            replica_input_frontiers: BTreeMap::new(),
        }
    }

    /// Creates a new collection state for a log collection.
    pub fn new_log_collection() -> Self {
        let since = Antichain::from_elem(timely::progress::Timestamp::minimum());
        let mut state = Self::new(since, Vec::new(), Vec::new());
        state.log_collection = true;
        // Log collections are created and scheduled implicitly as part of replica initialization.
        state.scheduled = true;
        state
    }
}
