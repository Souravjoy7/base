//! CLI argument parsing and execution for the ZK benchmark binary.

use std::path::PathBuf;

use base_cli_utils::RuntimeManager;
use base_zk_benchmarks::{
    LoadTestExecutor, LoadTestRunOptions, TestConfig, ZkBenchConfig, ZkBenchMode, ZkBenchRunner,
    ZkBenchSummary,
};
use clap::{Args, Parser, ValueEnum};
use eyre::{Result, bail};

/// The Base ZK benchmark CLI.
#[derive(Parser, Clone, Debug)]
#[command(author, version = env!("CARGO_PKG_VERSION"), about = "Base ZK benchmarks")]
pub(crate) struct Cli {
    /// ZK benchmark arguments.
    #[command(flatten)]
    args: ZkBenchArgs,
}

impl Cli {
    /// Runs the selected benchmark.
    pub(crate) fn run(self) -> Result<()> {
        RuntimeManager::new()
            .tokio_runtime()?
            .block_on(async move { run_zk_benchmark(self.args).await })
    }
}

/// ZK benchmark command arguments.
#[derive(Args, Clone, Debug)]
struct ZkBenchArgs {
    /// Run continuously until interrupted.
    #[arg(long)]
    continuous: bool,

    /// ZK proof benchmark mode.
    #[arg(long, value_enum)]
    mode: ZkBenchModeArg,

    /// Rollup node RPC URL. This is the op-node RPC, not the L2 execution RPC.
    #[arg(
        long = "rollup-rpc-url",
        env = "ROLLUP_RPC_URL",
        default_value = "http://localhost:8649"
    )]
    rollup_rpc_url: url::Url,

    /// ZK prover RPC URL.
    #[arg(long = "zk-prover-url", env = "ZK_PROVER_URL", default_value = "http://localhost:9000")]
    zk_prover_url: url::Url,

    /// Load test YAML configuration.
    #[arg(value_name = "CONFIG")]
    config: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ZkBenchModeArg {
    DryRun,
    Cluster,
}

impl From<ZkBenchModeArg> for ZkBenchMode {
    fn from(mode: ZkBenchModeArg) -> Self {
        match mode {
            ZkBenchModeArg::DryRun => Self::DryRun,
            ZkBenchModeArg::Cluster => Self::Cluster,
        }
    }
}

async fn run_zk_benchmark(args: ZkBenchArgs) -> Result<()> {
    if !args.config.exists() {
        bail!("config file not found: {}", args.config.display());
    }

    let test_config = TestConfig::load(&args.config)?;
    let zk_config = ZkBenchConfig::new(
        args.rollup_rpc_url.clone(),
        args.zk_prover_url.clone(),
        args.mode.into(),
    );

    println!("=== Base ZK Benchmark Runner ===");
    println!("Config: {}", args.config.display());
    println!(
        "Mode: {:?} | Rollup RPC: {} | Prover: {}",
        zk_config.mode, zk_config.rollup_rpc_url, zk_config.prover_url
    );
    println!();

    println!("Running load test...");
    let output = LoadTestExecutor::run(
        test_config,
        LoadTestRunOptions {
            continuous: args.continuous,
            install_signal_handler: true,
            ..Default::default()
        },
    )
    .await?;

    let summary = output.summary;
    if let Ok(output_path) = std::env::var("LOAD_TEST_OUTPUT") {
        match summary.to_json() {
            Ok(json) => match std::fs::write(&output_path, &json) {
                Ok(()) => println!("Load test results written to {output_path}"),
                Err(e) => {
                    eprintln!("Warning: failed to write load test results to {output_path}: {e}")
                }
            },
            Err(e) => eprintln!("Warning: failed to serialize load test results: {e}"),
        }
    }

    if output.cleanup.b20_teardown_attempted {
        match &output.cleanup.b20_teardown_error {
            None => println!("B-20 teardown complete."),
            Some(error) => eprintln!("Warning: B-20 teardown failed: {error}"),
        }
    }

    match &output.cleanup.drained {
        Some(drained) => println!("Drained {drained} wei back to funder."),
        None => {
            if let Some(error) = &output.cleanup.drain_error {
                eprintln!("Warning: drain failed: {error}");
            }
        }
    }

    if let Some(error) = &summary.error {
        bail!("load test failed before ZK bench: {error}");
    }

    println!();
    println!("Running ZK benchmark...");
    let zk_summary = ZkBenchRunner::run(&summary, zk_config).await?;
    print_zk_bench_summary(&zk_summary);

    if let Ok(output_path) = std::env::var("ZK_BENCH_OUTPUT") {
        match zk_summary.to_json() {
            Ok(json) => match std::fs::write(&output_path, &json) {
                Ok(()) => println!("ZK bench results written to {output_path}"),
                Err(e) => {
                    eprintln!("Warning: failed to write ZK bench results to {output_path}: {e}")
                }
            },
            Err(e) => eprintln!("Warning: failed to serialize ZK bench results: {e}"),
        }
    }

    Ok(())
}

fn print_zk_bench_summary(summary: &ZkBenchSummary) {
    println!(
        "Target: blocks {}..={} ({})",
        summary.target.first_block, summary.target.last_block, summary.target.reason
    );
    println!(
        "Proof: session={} start={} blocks={} l1_head={}",
        summary.proof.session_id,
        summary.proof.start_block_number,
        summary.proof.number_of_blocks_to_prove,
        summary.proof.l1_head
    );

    if let Some(stats) = &summary.execution_stats {
        println!(
            "Execution: cycles={} sp1_gas={} witness_ms={:.2} execution_ms={:.2}",
            stats.total_instruction_cycles,
            stats.total_sp1_gas,
            stats.witness_generation_ms,
            stats.execution_ms
        );
    }
}
