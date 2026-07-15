//! Simulator cluster lifecycle, including dynamic node churn.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, ensure};
use rand::{Rng, RngExt};

use crate::channel_transport::{ChannelTransport, NodeEntry, NodeRegistry};
use crate::chitchat_bus::ChitchatGossipBus;
use crate::churn::{ChurnPlan, ChurnReport, ChurnTracker, ClusterObservation, NodeObservation};
use crate::config::SimConfig;
use crate::metrics::{MetricsCollector, Report};
use crate::report::RunReport;
use crate::stub_renderer::StubRenderer;
use crate::workload::run_workload;
use biei::activity::ProfileActivityTracker;
use biei::gossip::GossipBus;
use biei::node::{Node, NodeSpawn};
use biei::renderer::{BoxRenderer, NoopProfilePreparer};
use biei::style_catalog::StyleCatalog;
use biei::transport::Transport;
use biei::types::{NodeId, TaskOutcome, TaskResult};

const SEED_MIX: u64 = 0x9E37_79B9_7F4A_7C15;

pub struct Simulation {
    pub config: SimConfig,
}

pub struct SimulationOptions {
    pub churn_plan: Option<ChurnPlan>,
    pub sample_every_requests: u64,
}

impl Default for SimulationOptions {
    fn default() -> Self {
        Self {
            churn_plan: None,
            sample_every_requests: 1_000,
        }
    }
}

impl Simulation {
    pub fn new(config: SimConfig) -> Self {
        Self { config }
    }

    /// Compatibility entry point used by the existing sweep suite.
    pub async fn run(self) -> Report {
        self.execute(None, 1_000).await.expect("simulation run").0
    }

    pub async fn run_report(self, options: SimulationOptions) -> Result<RunReport> {
        let config = self.config.clone();
        let (result, churn) = self
            .execute(options.churn_plan, options.sample_every_requests)
            .await?;
        Ok(RunReport::new(&config, &result, churn))
    }

    async fn execute(
        self,
        churn_plan: Option<ChurnPlan>,
        sample_every_requests: u64,
    ) -> Result<(Report, Option<ChurnReport>)> {
        ensure!(
            self.config.node_count > 0,
            "simulation needs at least one node"
        );
        if let Some(plan) = &churn_plan {
            plan.validate()?;
            ensure!(
                sample_every_requests > 0,
                "sample_every_requests must be greater than zero"
            );
        }
        let cpu_render_permits = self.config.cluster.resolved_cpu_render_permits_per_node();
        let metrics = Arc::new(MetricsCollector::with_cpu_render_permits(
            cpu_render_permits * self.config.node_count,
        ));
        let activity = Arc::new(ProfileActivityTracker::new());
        let mut cluster = WorkloadCluster::new(self.config.clone(), activity.clone()).await?;
        let mut churn = churn_plan
            .map(|plan| {
                ChurnTracker::new(plan, sample_every_requests, cluster.observation(&metrics))
            })
            .transpose()?;

        let workload_result = run_workload(
            self.config.workload.clone(),
            &mut cluster,
            metrics.clone(),
            activity,
            self.config.seed,
            churn.as_mut(),
        )
        .await;
        let submitted = match workload_result {
            Ok(submitted) => submitted,
            Err(error) => {
                cluster.shutdown().await;
                return Err(error);
            }
        };
        let churn_result = churn
            .map(|tracker| tracker.finish(&cluster, &metrics, submitted))
            .transpose();
        let report = metrics.report(self.config.costs.sla);
        cluster.shutdown().await;
        Ok((report, churn_result?))
    }
}

pub(crate) struct WorkloadCluster {
    config: SimConfig,
    gossip: Arc<ChitchatGossipBus>,
    activity: Arc<ProfileActivityTracker>,
    style_catalog: Arc<StyleCatalog>,
    registry: Arc<NodeRegistry>,
    transport: Arc<ChannelTransport>,
    nodes: Vec<ActiveNode>,
    retired: Vec<RetiredNode>,
    next_node_index: usize,
}

struct ActiveNode {
    id: NodeId,
    node: Node,
    _registry_entry: Arc<NodeEntry>,
    counters: Arc<NodeCounters>,
}

struct RetiredNode {
    id: NodeId,
    counters: Arc<NodeCounters>,
}

#[derive(Default)]
pub(crate) struct NodeCounters {
    submitted: AtomicU64,
    completed: AtomicU64,
    rejected: AtomicU64,
    failed: AtomicU64,
}

impl NodeCounters {
    pub(crate) fn submit(&self) {
        self.submitted.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record(&self, outcome: &TaskOutcome) {
        match &outcome.result {
            TaskResult::Completed { .. } => &self.completed,
            TaskResult::Rejected { .. } => &self.rejected,
            TaskResult::Failed { .. } => &self.failed,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    fn observation(&self, node_id: String, active: bool) -> NodeObservation {
        NodeObservation {
            node_id,
            active,
            submitted: self.submitted.load(Ordering::Relaxed),
            completed: self.completed.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            ..NodeObservation::default()
        }
    }
}

impl WorkloadCluster {
    async fn new(config: SimConfig, activity: Arc<ProfileActivityTracker>) -> Result<Self> {
        let gossip = Arc::new(
            ChitchatGossipBus::new(
                Vec::new(),
                config.gossip.publish_interval,
                config.costs.hop_latency,
            )
            .await?,
        );
        let catalog = StyleCatalog::new();
        catalog.set_url_template("http://simulator.local/styles/{style_id}/style.json");
        let registry = NodeRegistry::new();
        let transport = Arc::new(ChannelTransport::new(
            config.costs.hop_latency,
            registry.clone(),
        ));
        let initial_nodes = config.node_count;
        let mut cluster = Self {
            config,
            gossip,
            activity,
            style_catalog: Arc::new(catalog),
            registry,
            transport,
            nodes: Vec::with_capacity(initial_nodes),
            retired: Vec::new(),
            next_node_index: 0,
        };
        for _ in 0..initial_nodes {
            if let Err(error) = cluster.add_node().await {
                cluster.shutdown().await;
                return Err(error);
            }
        }
        Ok(cluster)
    }

    pub(crate) async fn add_node(&mut self) -> Result<String> {
        let index = self.next_node_index;
        self.next_node_index += 1;
        let node_id = NodeId::from_index(index);
        self.gossip.add_node(node_id.clone()).await?;

        let renderers: Vec<BoxRenderer> = (0..self.config.cluster.renderer_slots_per_node)
            .map(|worker| {
                let renderer_seed = self.config.seed.wrapping_add(
                    ((index as u64).wrapping_mul(SEED_MIX))
                        .wrapping_add((worker as u64).wrapping_mul(SEED_MIX.wrapping_mul(3))),
                );
                Box::new(StubRenderer::new(
                    self.config.costs.style_setup_cost,
                    self.config.costs.source_load_cost,
                    self.config.costs.render_cost,
                    renderer_seed,
                )) as BoxRenderer
            })
            .collect();
        let queue_limits = self
            .config
            .cluster
            .resolved_queue_limits(&self.config.costs);
        let gossip: Arc<dyn GossipBus> = self.gossip.clone();
        let transport: Arc<dyn Transport> = self.transport.clone();
        let node = Node::spawn(NodeSpawn {
            id: node_id.clone(),
            renderers,
            profile_preparer: Arc::new(NoopProfilePreparer),
            gossip,
            transport,
            style_catalog: self.style_catalog.clone(),
            activity: self.activity.clone(),
            routing: self.config.routing.clone(),
            costs: self.config.costs.clone(),
            gossip_cfg: self.config.gossip.clone(),
            bl_capacity: queue_limits.soft,
            queue_capacity: queue_limits.hard,
            render_permits: self.config.cluster.resolved_render_permits_per_node(),
            cpu_render_permits: self.config.cluster.resolved_cpu_render_permits_per_node(),
            source_cache_capacity: self.config.cluster.source_cache_capacity,
            render_output_cache_capacity_bytes: self
                .config
                .cluster
                .render_output_cache_capacity_bytes,
            dispatcher_seed: self
                .config
                .seed
                .wrapping_add((index as u64 + 1).wrapping_mul(SEED_MIX.wrapping_mul(5))),
        });
        let entry = self.registry.register(node_id.clone(), node.clone());
        self.nodes.push(ActiveNode {
            id: node_id.clone(),
            node,
            _registry_entry: entry,
            counters: Arc::new(NodeCounters::default()),
        });
        Ok(node_id.to_string())
    }

    pub(crate) async fn remove_node(&mut self, node_id: &str) -> Result<()> {
        ensure!(
            self.nodes.len() > 1,
            "cannot remove the final simulator node"
        );
        let Some(index) = self
            .nodes
            .iter()
            .position(|node| node.id.to_string() == node_id)
        else {
            anyhow::bail!("unknown active simulator node {node_id}");
        };
        let node = self.nodes.remove(index);
        self.registry.unregister(&node.id);
        self.gossip.remove_node(&node.id).await?;
        self.retired.push(RetiredNode {
            id: node.id,
            counters: node.counters,
        });
        Ok(())
    }

    pub(crate) fn select(&self, rng: &mut impl Rng) -> (Node, Arc<NodeCounters>) {
        let index = rng.random_range(0..self.nodes.len());
        let selected = &self.nodes[index];
        (selected.node.clone(), selected.counters.clone())
    }

    pub(crate) fn cpu_render_permits_total(&self) -> usize {
        self.nodes.len() * self.config.cluster.resolved_cpu_render_permits_per_node()
    }

    pub(crate) fn observation(&self, metrics: &MetricsCollector) -> ClusterObservation {
        let metrics = metrics.observation();
        let mut total_queue_depth = 0;
        let mut loaded_workers = 0;
        let mut nodes = Vec::with_capacity(self.nodes.len() + self.retired.len());
        for active in &self.nodes {
            let workers = active.node.worker_snapshot();
            let queue_depth = workers.iter().map(|worker| worker.queue_depth).sum();
            let loaded = workers
                .iter()
                .filter(|worker| worker.loaded_profile.is_some())
                .count();
            total_queue_depth += queue_depth;
            loaded_workers += loaded;
            let mut observation = active.counters.observation(active.id.to_string(), true);
            observation.queue_depth = queue_depth;
            observation.loaded_workers = loaded;
            nodes.push(observation);
        }
        nodes.extend(
            self.retired
                .iter()
                .map(|retired| retired.counters.observation(retired.id.to_string(), false)),
        );
        nodes.sort_by(|left, right| left.node_id.cmp(&right.node_id));
        let transport = self.transport.snapshot();
        ClusterObservation {
            submitted: nodes.iter().map(|node| node.submitted).sum(),
            recorded: metrics.total,
            completed: metrics.completed,
            rejected: metrics.rejected,
            failed: metrics.failed,
            active_nodes: self.nodes.len(),
            total_queue_depth,
            loaded_workers,
            cold_starts: metrics.cold_starts,
            style_swaps: metrics.style_swaps,
            source_hits: metrics.source_hits,
            source_loads: metrics.source_loads,
            tier_counts: metrics
                .tier_counts
                .iter()
                .map(|(tier, count)| (format!("{tier:?}"), *count))
                .collect(),
            forward_attempts: transport.attempts,
            forward_successes: transport.successes,
            nodes,
        }
    }

    async fn shutdown(mut self) {
        for node in self.nodes.drain(..) {
            self.registry.unregister(&node.id);
        }
        self.gossip.shutdown_all().await;
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Simulation, SimulationOptions};
    use crate::churn::{ChurnEvent, ChurnPlan};
    use crate::config::SimConfig;

    #[tokio::test]
    async fn applies_add_and_remove_churn_events() {
        tokio::time::pause();
        let mut workload = SimConfig::default().workload;
        workload.duration = Duration::from_millis(100);
        workload.warmup = Duration::ZERO;
        workload.total_rate = 1_000.0;
        let config = SimConfig {
            node_count: 2,
            workload,
            ..SimConfig::default()
        };
        let report = Simulation::new(config)
            .run_report(SimulationOptions {
                churn_plan: Some(ChurnPlan {
                    events: vec![
                        ChurnEvent::Add { at_request: 5 },
                        ChurnEvent::Remove {
                            at_request: 10,
                            node_id: "node-0".to_string(),
                        },
                    ],
                }),
                sample_every_requests: 4,
            })
            .await
            .expect("simulation");
        let churn = report.churn.expect("churn report");
        assert_eq!(churn.events.len(), 2);
        assert_eq!(churn.events[0].active_nodes, 3);
        assert_eq!(churn.events[1].active_nodes, 2);
        assert!(
            churn
                .samples
                .iter()
                .any(|sample| sample.reason == "periodic")
        );
    }
}
