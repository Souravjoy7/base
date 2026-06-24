//! Dry-run [`ZkProver`] for local SP1 execution statistics.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use base_proof_succinct_client_utils::client::DEFAULT_INTERMEDIATE_ROOT_INTERVAL;
use base_proof_succinct_proof_utils::get_range_elf_embedded;
use base_proof_zk_host::{ZkProofRequestKind, ZkProver, ZkProverError, ZkSessionState};
use base_prover_service_protocol::{ExecutionStats, ProofResult, ZkProofResult, ZkVm};
use sp1_sdk::{
    Elf, SP1Stdin,
    blocking::{MockProver, Prover},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::succinct::{
    L1HeadSource, OpSuccinctWitnessProvider, SuccinctRpcConfig, SuccinctZkProverBuildError,
    SuccinctZkProverBuilder, WitnessParams,
};

macro_rules! backend_error {
    ($($arg:tt)*) => {
        ZkProverError::Backend(std::io::Error::other(format!($($arg)*)).into())
    };
}

/// Prefix marking a dry-run backend session id.
pub const DRY_RUN_PREFIX: &str = "dry-run-stark-";

/// Configuration for [`DryRunZkProver`].
#[derive(Clone, Debug)]
pub struct DryRunZkProverConfig {
    /// Base consensus node RPC URL.
    pub base_consensus_url: String,
    /// L1 execution node RPC URL.
    pub l1_node_url: String,
    /// Default sequence window for L1 head calculations.
    pub default_sequence_window: u64,
}

/// [`ZkProver`] that generates a witness and executes the range program locally.
#[derive(Clone)]
pub struct DryRunZkProver {
    provider: OpSuccinctWitnessProvider,
    config: DryRunZkProverConfig,
    completed_results: Arc<Mutex<HashMap<String, ProofResult>>>,
}

impl std::fmt::Debug for DryRunZkProver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DryRunZkProver").field("config", &self.config).finish_non_exhaustive()
    }
}

impl DryRunZkProver {
    /// Create a dry-run prover with a witness provider and RPC config.
    pub fn new(provider: OpSuccinctWitnessProvider, config: DryRunZkProverConfig) -> Self {
        Self { provider, config, completed_results: Arc::new(Mutex::new(HashMap::new())) }
    }

    /// Builds a dry-run backend.
    pub async fn build_until_cancelled(
        rpc: SuccinctRpcConfig,
        cancel: &CancellationToken,
    ) -> Result<Option<Arc<dyn ZkProver>>, SuccinctZkProverBuildError> {
        let base_consensus_url = rpc.base_consensus_rpc.as_str().to_owned();
        let l1_node_url = rpc.l1_rpc.as_str().to_owned();
        let default_sequence_window = rpc.default_sequence_window;

        info!(backend = "dry_run", "using local SP1 dry-run backend");
        let Some(provider) = SuccinctZkProverBuilder::build_witness_provider(rpc, cancel).await?
        else {
            return Ok(None);
        };

        let config =
            DryRunZkProverConfig { base_consensus_url, l1_node_url, default_sequence_window };

        Ok(Some(Arc::new(Self::new(provider, config))))
    }

    /// Derive the deterministic backend session id for a request.
    pub fn backend_session_id(request: &ZkProofRequestKind, request_session_id: &str) -> String {
        match request {
            ZkProofRequestKind::Compressed(_) => format!("{DRY_RUN_PREFIX}{request_session_id}"),
            ZkProofRequestKind::SnarkGroth16(_) => {
                format!("dry-run-unsupported-snark-{request_session_id}")
            }
        }
    }

    /// Execute the range program with SP1's mock prover and return execution statistics.
    pub async fn execute_range_program(stdin: SP1Stdin) -> Result<ExecutionStats, ZkProverError> {
        let execution_start = std::time::Instant::now();
        let (_, report) = tokio::task::spawn_blocking(move || {
            info!("starting local SP1 zkVM execution");

            let prover = MockProver::new();
            prover
                .execute(Elf::Static(get_range_elf_embedded()), stdin)
                .calculate_gas(true)
                .deferred_proof_verification(false)
                .run()
        })
        .await
        .map_err(|e| backend_error!("SP1 execution task failed to join: {e}"))?
        .map_err(|e| backend_error!("SP1 execution failed: {e}"))?;

        let execution_ms = execution_start.elapsed().as_secs_f64() * 1000.0;

        Ok(ExecutionStats {
            total_instruction_cycles: report.total_instruction_count(),
            total_sp1_gas: report.gas().unwrap_or_else(|| {
                warn!("gas calculation returned None despite calculate_gas(true)");
                0
            }),
            cycle_tracker: report.cycle_tracker.into_iter().collect(),
            witness_generation_ms: 0.0,
            execution_ms,
        })
    }

    /// Generate the witness, execute it locally, and return an empty-proof dry-run result.
    pub async fn prove_range(
        &self,
        request: &base_prover_service_protocol::ZkProofRequest,
        request_session_id: &str,
    ) -> Result<ProofResult, ZkProverError> {
        if request.number_of_blocks_to_prove == 0 {
            return Err(backend_error!("number_of_blocks_to_prove must be greater than zero"));
        }

        let start_block = request.start_block_number;
        let end_block = start_block
            .checked_add(request.number_of_blocks_to_prove)
            .ok_or_else(|| backend_error!("proof range end block overflowed u64"))?;
        let sequence_window =
            request.sequence_window.unwrap_or(self.config.default_sequence_window);
        let intermediate_root_interval =
            request.intermediate_root_interval.unwrap_or(DEFAULT_INTERMEDIATE_ROOT_INTERVAL);

        info!(
            request_session_id = %request_session_id,
            start_block = start_block,
            end_block = end_block,
            number_of_blocks = request.number_of_blocks_to_prove,
            sequence_window = sequence_window,
            intermediate_root_interval = intermediate_root_interval,
            l1_head = ?request.l1_head,
            "starting dry-run SP1 execution"
        );

        let witness_start = std::time::Instant::now();
        let stdin = self
            .provider
            .generate_witness(WitnessParams {
                start_block,
                end_block,
                l1_head: request.l1_head.map_or(
                    L1HeadSource::SequenceWindow {
                        sequence_window,
                        l1_node_url: &self.config.l1_node_url,
                        base_consensus_url: &self.config.base_consensus_url,
                    },
                    L1HeadSource::Pinned,
                ),
                intermediate_root_interval,
            })
            .await
            .map_err(|e| {
                error!(
                    start_block = start_block,
                    end_block = end_block,
                    error = %e,
                    "dry-run witness generation failed"
                );
                backend_error!("witness generation failed: {e}")
            })?;
        let witness_generation_ms = witness_start.elapsed().as_secs_f64() * 1000.0;

        let mut execution_stats = Self::execute_range_program(stdin).await?;
        execution_stats.witness_generation_ms = witness_generation_ms;

        info!(
            request_session_id = %request_session_id,
            total_instruction_cycles = execution_stats.total_instruction_cycles,
            total_sp1_gas = execution_stats.total_sp1_gas,
            witness_generation_ms = witness_generation_ms,
            execution_ms = execution_stats.execution_ms,
            tracked_sections = execution_stats.cycle_tracker.len(),
            "dry-run SP1 execution completed"
        );

        Ok(ProofResult::Compressed(ZkProofResult {
            zk_vm: ZkVm::Sp1,
            proof: Vec::new().into(),
            execution_stats: Some(execution_stats),
        }))
    }
}

#[async_trait]
impl ZkProver for DryRunZkProver {
    async fn submit(
        &self,
        request: &ZkProofRequestKind,
        request_session_id: &str,
    ) -> Result<String, ZkProverError> {
        let ZkProofRequestKind::Compressed(request) = request else {
            return Err(backend_error!(
                "dry-run backend only supports compressed proof types; SNARK_GROTH16 requires a proof-producing backend"
            ));
        };

        let backend_session_id = Self::backend_session_id(
            &ZkProofRequestKind::Compressed(request.clone()),
            request_session_id,
        );
        let result = self.prove_range(request, request_session_id).await?;
        self.completed_results
            .lock()
            .map_err(|e| backend_error!("dry-run result store lock poisoned: {e}"))?
            .insert(backend_session_id.clone(), result);

        Ok(backend_session_id)
    }

    async fn poll(&self, backend_session_id: &str) -> Result<ZkSessionState, ZkProverError> {
        let contains_result = self
            .completed_results
            .lock()
            .map_err(|e| backend_error!("dry-run result store lock poisoned: {e}"))?
            .contains_key(backend_session_id);
        if contains_result { Ok(ZkSessionState::Completed) } else { Ok(ZkSessionState::NotFound) }
    }

    async fn download(&self, backend_session_id: &str) -> Result<ProofResult, ZkProverError> {
        self.completed_results
            .lock()
            .map_err(|e| backend_error!("dry-run result store lock poisoned: {e}"))?
            .get(backend_session_id)
            .cloned()
            .ok_or_else(|| ZkProverError::BackendSessionNotFound {
                backend_session_id: backend_session_id.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use base_prover_service_protocol::{SnarkGroth16ProofRequest, ZkProofRequest, ZkVm};

    use super::*;

    fn zk_request() -> ZkProofRequest {
        ZkProofRequest {
            start_block_number: 1,
            number_of_blocks_to_prove: 1,
            sequence_window: None,
            l1_head: None,
            intermediate_root_interval: None,
            zk_vm: ZkVm::Sp1,
        }
    }

    fn compressed() -> ZkProofRequestKind {
        ZkProofRequestKind::Compressed(zk_request())
    }

    fn snark_groth16() -> ZkProofRequestKind {
        ZkProofRequestKind::SnarkGroth16(SnarkGroth16ProofRequest {
            proof: zk_request(),
            prover_address: Default::default(),
        })
    }

    #[test]
    fn dry_run_session_id_is_deterministic_for_compressed_requests() {
        assert_eq!(
            DryRunZkProver::backend_session_id(&compressed(), "session-1"),
            "dry-run-stark-session-1"
        );
    }

    #[test]
    fn dry_run_session_id_marks_snark_requests_unsupported() {
        assert_eq!(
            DryRunZkProver::backend_session_id(&snark_groth16(), "session-1"),
            "dry-run-unsupported-snark-session-1"
        );
    }
}
