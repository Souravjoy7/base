//! ZK proof benchmarking for completed load-test runs.

use std::{
    cmp::Reverse,
    time::{Duration, Instant},
};

use alloy_eips::BlockNumberOrTag;
use alloy_primitives::B256;
use base_load_tests::{BlockLoadMetrics, BlockRange, MetricsSummary, QueryProvider, RpcProviders};
use base_optimism_rpc::OptimismRollupProviderExt;
use base_zk_client::{
    ExecutionStats, GetProofRequest, ProofJobStatus, ProofType, ProveBlockRequest, ReceiptType,
    ZkProofClient, ZkProofClientConfig,
};
use eyre::{Result, WrapErr, ensure};
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, timeout};
use url::Url;

const SAFE_L2_TIMEOUT: Duration = Duration::from_secs(300);
const SAFE_L2_POLL_INTERVAL: Duration = Duration::from_millis(500);
const PROOF_TIMEOUT: Duration = Duration::from_secs(900);
const PROOF_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// ZK proof backend mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZkBenchMode {
    /// Dry-run local proof backend.
    #[default]
    DryRun,
    /// Cluster proof backend.
    Cluster,
}

/// Runtime configuration for proving a completed load-test run.
#[derive(Clone, Debug)]
pub struct ZkBenchConfig {
    /// Proof backend mode.
    pub mode: ZkBenchMode,
    /// Rollup node RPC URL.
    pub rollup_rpc_url: Url,
    /// ZK prover RPC URL.
    pub prover_url: Url,
}

impl ZkBenchConfig {
    /// Builds ZK bench config from endpoint URLs and proof mode.
    pub const fn new(rollup_rpc_url: Url, prover_url: Url, mode: ZkBenchMode) -> Self {
        Self { mode, rollup_rpc_url, prover_url }
    }
}

/// Inclusive L2 block range selected for proof.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZkBenchTargetRange {
    /// First L2 block selected for proof.
    pub first_block: u64,
    /// Last L2 block selected for proof.
    pub last_block: u64,
    /// Human-readable selection reason.
    pub reason: String,
}

impl ZkBenchTargetRange {
    /// Builds a proof target range.
    pub fn new(first_block: u64, last_block: u64, reason: impl Into<String>) -> Result<Self> {
        ensure!(
            last_block >= first_block,
            "invalid proof target range: {first_block}..={last_block}"
        );
        Ok(Self { first_block, last_block, reason: reason.into() })
    }
}

/// Proof request summary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZkBenchProofOutcome {
    /// Proof backend mode.
    pub mode: ZkBenchMode,
    /// ZK prover session ID.
    pub session_id: String,
    /// Prover start block number.
    pub start_block_number: u64,
    /// Number of blocks requested.
    pub number_of_blocks_to_prove: u64,
    /// L1 head hash passed to the prover.
    pub l1_head: String,
}

/// JSON-serializable subset of dry-run execution stats.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZkBenchExecutionStatsSummary {
    /// Total instruction cycles.
    pub total_instruction_cycles: u64,
    /// Total SP1 gas.
    pub total_sp1_gas: u64,
    /// Cycle tracker output.
    pub cycle_tracker: std::collections::BTreeMap<String, u64>,
    /// Witness generation time in milliseconds.
    pub witness_generation_ms: f64,
    /// Execution time in milliseconds.
    pub execution_ms: f64,
}

impl From<ExecutionStats> for ZkBenchExecutionStatsSummary {
    fn from(stats: ExecutionStats) -> Self {
        Self {
            total_instruction_cycles: stats.total_instruction_cycles,
            total_sp1_gas: stats.total_sp1_gas,
            cycle_tracker: stats.cycle_tracker.into_iter().collect(),
            witness_generation_ms: stats.witness_generation_ms,
            execution_ms: stats.execution_ms,
        }
    }
}

/// Completed ZK benchmark summary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZkBenchSummary {
    /// Inclusive block range selected from the load-test run for proof.
    pub target: ZkBenchTargetRange,
    /// Proof summary.
    pub proof: ZkBenchProofOutcome,
    /// Dry-run execution stats, when the prover returned them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_stats: Option<ZkBenchExecutionStatsSummary>,
}

impl ZkBenchSummary {
    /// Builds a completed ZK benchmark summary.
    pub fn new(
        target: ZkBenchTargetRange,
        proof: ZkBenchProofOutcome,
        execution_stats: Option<ExecutionStats>,
    ) -> Self {
        Self {
            target,
            proof,
            execution_stats: execution_stats.map(ZkBenchExecutionStatsSummary::from),
        }
    }

    /// Serializes the summary to pretty JSON.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}

/// Runs ZK proof benchmarks for completed load-test summaries.
#[derive(Debug)]
pub struct ZkBenchRunner;

impl ZkBenchRunner {
    /// Selects the proof target, waits for safe L2, requests a proof, and polls for completion.
    pub async fn run(summary: &MetricsSummary, config: ZkBenchConfig) -> Result<ZkBenchSummary> {
        if let Some(error) = &summary.error {
            eyre::bail!("load test failed before ZK bench target selection: {error}");
        }

        let target = Self::select_proof_target(summary, config.mode)?;
        let rollup_provider = RpcProviders::query(config.rollup_rpc_url.clone())
            .map_err(eyre::Report::from)
            .wrap_err_with(|| format!("failed to connect rollup RPC {}", config.rollup_rpc_url))?;
        let (proof, execution_stats) = Self::prove_safe_block_range(
            &rollup_provider,
            target.first_block,
            target.last_block,
            config,
        )
        .await?;

        Ok(ZkBenchSummary::new(target, proof, execution_stats))
    }

    /// Selects the proof target from a completed load-test summary.
    pub fn select_proof_target(
        summary: &MetricsSummary,
        mode: ZkBenchMode,
    ) -> Result<ZkBenchTargetRange> {
        match mode {
            ZkBenchMode::DryRun => {
                Self::target_from_block_range(&summary.block_range, "full load-test range")
            }
            ZkBenchMode::Cluster => {
                let block = Self::select_fullest_block(&summary.block_load)?;
                ZkBenchTargetRange::new(
                    block.block_number,
                    block.block_number,
                    format!(
                        "fullest block by gas: gas={}, txs={}",
                        block.total_gas, block.confirmed_count
                    ),
                )
            }
        }
    }

    /// Converts a load-test block range into a proof target.
    pub fn target_from_block_range(
        range: &BlockRange,
        reason: impl Into<String>,
    ) -> Result<ZkBenchTargetRange> {
        let first_block =
            range.first_block.ok_or_else(|| eyre::eyre!("block range has no first block"))?;
        let last_block =
            range.last_block.ok_or_else(|| eyre::eyre!("block range has no last block"))?;
        ZkBenchTargetRange::new(first_block, last_block, reason)
    }

    /// Selects the fullest block by gas, then transaction count, then lowest block number.
    pub fn select_fullest_block(blocks: &[BlockLoadMetrics]) -> Result<BlockLoadMetrics> {
        blocks
            .iter()
            .max_by_key(|block| {
                (block.total_gas, block.confirmed_count, Reverse(block.block_number))
            })
            .cloned()
            .ok_or_else(|| eyre::eyre!("cannot select fullest block from empty load-test run"))
    }

    /// Waits for a block range to become safe, requests a proof, and polls for completion.
    pub async fn prove_safe_block_range(
        rollup_provider: &QueryProvider,
        first_block_number: u64,
        last_block_number: u64,
        config: ZkBenchConfig,
    ) -> Result<(ZkBenchProofOutcome, Option<ExecutionStats>)> {
        let l1_head = Self::wait_for_safe_l2(
            rollup_provider,
            last_block_number,
            SAFE_L2_TIMEOUT,
            SAFE_L2_POLL_INTERVAL,
        )
        .await?;

        Self::prove_block_range(first_block_number, last_block_number, l1_head, config).await
    }

    /// Waits for a workload block to become safe and returns the current L1 head.
    pub async fn wait_for_safe_l2(
        provider: &QueryProvider,
        block_number: u64,
        wait_timeout: Duration,
        poll_interval: Duration,
    ) -> Result<B256> {
        timeout(wait_timeout, async {
            loop {
                let status = provider.optimism_sync_status().await?;
                if status.safe_l2.number >= block_number {
                    provider
                        .optimism_output_at_block(BlockNumberOrTag::Number(block_number))
                        .await?;
                    return Ok::<_, eyre::Error>(status.head_l1.hash);
                }
                sleep(poll_interval).await;
            }
        })
        .await
        .wrap_err("timed out waiting for workload block to become safe")?
    }

    /// Requests a proof for a block range and polls for completion.
    pub async fn prove_block_range(
        first_block_number: u64,
        last_block_number: u64,
        l1_head: B256,
        config: ZkBenchConfig,
    ) -> Result<(ZkBenchProofOutcome, Option<ExecutionStats>)> {
        ensure!(
            last_block_number >= first_block_number,
            "invalid workload block range: {first_block_number}..={last_block_number}"
        );
        let start_block_number = first_block_number
            .checked_sub(1)
            .ok_or_else(|| eyre::eyre!("cannot prove genesis block with one-block range"))?;
        let number_of_blocks_to_prove = last_block_number - first_block_number + 1;
        let client = ZkProofClient::new(&ZkProofClientConfig {
            endpoint: config.prover_url,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
        })?;
        let response = client
            .prove_block(ProveBlockRequest {
                start_block_number,
                number_of_blocks_to_prove,
                sequence_window: None,
                proof_type: ProofType::Compressed.into(),
                session_id: None,
                prover_address: None,
                l1_head: Some(l1_head.to_string()),
                intermediate_root_interval: None,
            })
            .await?;

        let execution_stats = Self::poll_proof(
            &client,
            response.session_id.clone(),
            config.mode,
            PROOF_TIMEOUT,
            PROOF_POLL_INTERVAL,
        )
        .await?;
        let outcome = ZkBenchProofOutcome {
            mode: config.mode,
            session_id: response.session_id,
            start_block_number,
            number_of_blocks_to_prove,
            l1_head: l1_head.to_string(),
        };

        Ok((outcome, execution_stats))
    }

    /// Polls a proof job until it succeeds or fails.
    pub async fn poll_proof(
        client: &ZkProofClient,
        session_id: String,
        mode: ZkBenchMode,
        proof_timeout: Duration,
        poll_interval: Duration,
    ) -> Result<Option<ExecutionStats>> {
        let timeout_session_id = session_id.clone();
        timeout(proof_timeout, async {
            let start = Instant::now();
            loop {
                let response = client
                    .get_proof(GetProofRequest {
                        session_id: session_id.clone(),
                        receipt_type: Self::receipt_type_for_mode(mode),
                    })
                    .await?;
                let status = ProofJobStatus::try_from(response.status)
                    .unwrap_or(ProofJobStatus::Unspecified);

                match status {
                    ProofJobStatus::Succeeded => return Ok(response.execution_stats),
                    ProofJobStatus::Failed => {
                        return Err(eyre::eyre!(
                            "proof request failed after {:?}: {}",
                            start.elapsed(),
                            response
                                .error_message
                                .unwrap_or_else(|| "missing error message".to_string())
                        ));
                    }
                    _ => sleep(poll_interval).await,
                }
            }
        })
        .await
        .wrap_err_with(|| format!("timed out waiting for proof request {timeout_session_id}"))?
    }

    /// Returns the receipt type requested while polling a proof mode.
    pub const fn receipt_type_for_mode(mode: ZkBenchMode) -> Option<i32> {
        match mode {
            ZkBenchMode::DryRun => Some(ReceiptType::Stark as i32),
            ZkBenchMode::Cluster => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dry_run_targets_full_load_test_range() {
        let summary = summary(
            10,
            15,
            vec![block_load(10, 1, 100), block_load(12, 1, 200), block_load(15, 1, 100)],
        );

        let target = ZkBenchRunner::select_proof_target(&summary, ZkBenchMode::DryRun).unwrap();

        assert_eq!(target.first_block, 10);
        assert_eq!(target.last_block, 15);
        assert_eq!(target.reason, "full load-test range");
    }

    #[test]
    fn cluster_targets_fullest_load_test_block_by_gas() {
        let summary = summary(
            10,
            12,
            vec![block_load(10, 100, 1_000), block_load(11, 5, 2_000), block_load(12, 3, 2_000)],
        );

        let target = ZkBenchRunner::select_proof_target(&summary, ZkBenchMode::Cluster).unwrap();

        assert_eq!(target.first_block, 11);
        assert_eq!(target.last_block, 11);
        assert_eq!(target.reason, "fullest block by gas: gas=2000, txs=5");
    }

    #[test]
    fn fullest_block_prefers_lowest_block_after_gas_and_tx_tie() {
        let blocks = vec![block_load(12, 5, 2_000), block_load(11, 5, 2_000)];

        let block = ZkBenchRunner::select_fullest_block(&blocks).unwrap();

        assert_eq!(block.block_number, 11);
    }

    #[test]
    fn cluster_polling_does_not_request_receipt_type() {
        assert_eq!(ZkBenchRunner::receipt_type_for_mode(ZkBenchMode::Cluster), None);
    }

    fn summary(
        first_block: u64,
        last_block: u64,
        block_load: Vec<BlockLoadMetrics>,
    ) -> MetricsSummary {
        MetricsSummary {
            block_range: BlockRange {
                first_block: Some(first_block),
                last_block: Some(last_block),
                block_count: last_block - first_block + 1,
            },
            block_load,
            ..MetricsSummary::default()
        }
    }

    const fn block_load(
        block_number: u64,
        confirmed_count: u64,
        total_gas: u64,
    ) -> BlockLoadMetrics {
        BlockLoadMetrics { block_number, confirmed_count, reverted_count: 0, total_gas }
    }
}
