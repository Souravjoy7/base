//! High-level load-test execution orchestration.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use alloy_primitives::U256;
use alloy_provider::Provider;
use alloy_signer_local::PrivateKeySigner;

use crate::{
    LoadConfig, LoadRunner, LoadTestDisplay, MetricsSummary, RealTokenSetup, Result, RpcProviders,
    RpcResultExt, TestConfig,
};

/// Runtime options for a load-test execution.
#[derive(Clone, Copy, Debug)]
pub struct LoadTestRunOptions {
    /// Run continuously until the runner is externally stopped.
    pub continuous: bool,
    /// Clear configured txpool nodes before funding and running the test.
    pub clear_txpools: bool,
    /// Burn B-20 balances and drain native ETH after the run attempt.
    pub cleanup: bool,
    /// Install a Ctrl-C handler that asks the runner to stop gracefully.
    pub install_signal_handler: bool,
}

impl Default for LoadTestRunOptions {
    fn default() -> Self {
        Self {
            continuous: false,
            clear_txpools: true,
            cleanup: true,
            install_signal_handler: false,
        }
    }
}

/// Cleanup work attempted after a load-test run.
#[derive(Clone, Debug, Default)]
pub struct LoadTestCleanupSummary {
    /// Whether B-20 teardown was attempted.
    pub b20_teardown_attempted: bool,
    /// Error returned by B-20 teardown, if any.
    pub b20_teardown_error: Option<String>,
    /// Native ETH drained back to the funder.
    pub drained: Option<U256>,
    /// Error returned by native ETH drain, if any.
    pub drain_error: Option<String>,
}

/// Completed load-test execution.
#[derive(Clone, Debug)]
pub struct LoadTestRunOutput {
    /// Metrics summary from the run. If setup or execution failed, `error` is populated.
    pub summary: MetricsSummary,
    /// Cleanup summary.
    pub cleanup: LoadTestCleanupSummary,
}

/// Executes a configured load test, including setup and optional cleanup.
#[derive(Debug)]
pub struct LoadTestExecutor;

impl LoadTestExecutor {
    /// Runs a load test from a parsed [`TestConfig`].
    pub async fn run(
        test_config: TestConfig,
        options: LoadTestRunOptions,
    ) -> Result<LoadTestRunOutput> {
        Self::run_with_display(test_config, options, None).await
    }

    /// Runs a load test from a parsed [`TestConfig`] with an optional live display.
    pub async fn run_with_display(
        test_config: TestConfig,
        options: LoadTestRunOptions,
        display: Option<LoadTestDisplay>,
    ) -> Result<LoadTestRunOutput> {
        let query_rpc = test_config.query_rpc.clone().unwrap_or_else(|| {
            test_config.primary_submission_rpc().expect("validated config").clone()
        });
        let client = RpcProviders::query(query_rpc)?;
        let rpc_chain_id = if test_config.chain_id.is_none() {
            Some(client.get_chain_id().await.rpc("chain id")?)
        } else {
            None
        };

        let load_config = {
            let config = test_config.to_load_config(rpc_chain_id)?;
            if options.continuous { config.with_continuous() } else { config }
        };
        let config_summary = test_config.to_summary();
        let funding_key = TestConfig::funder_key()?;

        let mut runner = LoadRunner::new(load_config.clone())?;
        runner.set_config_summary(config_summary.clone());
        runner.set_funder_address(funding_key.address().to_string());
        if let Some(display) = display {
            runner.set_display(display);
        }
        if options.install_signal_handler {
            Self::install_signal_handler(runner.stop_flag());
        }

        let run_result =
            Self::run_phases(&mut runner, &test_config, &funding_key, &load_config, options).await;
        let summary = match run_result {
            Ok(summary) => summary,
            Err(error) => MetricsSummary {
                config: Some(config_summary),
                error: Some(error.to_string()),
                ..MetricsSummary::default()
            },
        };

        let cleanup = if options.cleanup {
            Self::cleanup(&runner, funding_key).await
        } else {
            LoadTestCleanupSummary::default()
        };

        Ok(LoadTestRunOutput { summary, cleanup })
    }

    /// Runs txpool clearing, account funding, token setup, and the load loop.
    pub async fn run_phases(
        runner: &mut LoadRunner,
        test_config: &TestConfig,
        funding_key: &PrivateKeySigner,
        load_config: &LoadConfig,
        options: LoadTestRunOptions,
    ) -> Result<MetricsSummary> {
        if options.clear_txpools && runner.txpool_node_count() > 0 {
            runner.clear_txpools().await?;
        }

        runner.fund_accounts(funding_key.clone(), test_config.parse_funding_amount()?).await?;

        let real_token_setup = test_config.parse_real_token_setup(load_config.chain_id)?;
        Self::setup_tokens(runner, test_config, funding_key, real_token_setup.as_ref()).await?;

        runner.run().await
    }

    /// Prepares optional real-token, swap-token, and B-20 balances.
    pub async fn setup_tokens(
        runner: &mut LoadRunner,
        test_config: &TestConfig,
        funding_key: &PrivateKeySigner,
        real_token_setup: Option<&RealTokenSetup>,
    ) -> Result<()> {
        if let Some(setup) = real_token_setup {
            runner.setup_real_tokens(setup).await?;
        } else if !runner.collect_swap_tokens().is_empty() {
            runner
                .setup_swap_tokens(funding_key.clone(), test_config.parse_swap_token_amount()?)
                .await?;
        }

        if runner.needs_b20_setup() {
            runner
                .setup_b20_tokens(funding_key.clone(), test_config.parse_b20_mint_amount()?)
                .await?;
        }

        Ok(())
    }

    /// Burns B-20 balances when needed and drains native ETH back to the funder.
    pub async fn cleanup(
        runner: &LoadRunner,
        funding_key: PrivateKeySigner,
    ) -> LoadTestCleanupSummary {
        tokio::time::sleep(Duration::from_secs(2)).await;

        let mut summary = LoadTestCleanupSummary::default();
        if runner.needs_b20_setup() {
            summary.b20_teardown_attempted = true;
            if let Err(error) = runner.teardown_b20_tokens().await {
                summary.b20_teardown_error = Some(error.to_string());
            }
        }

        match runner.drain_accounts(funding_key).await {
            Ok(drained) => summary.drained = Some(drained),
            Err(error) => summary.drain_error = Some(error.to_string()),
        }

        summary
    }

    /// Installs a Ctrl-C handler that flips the runner stop flag.
    pub fn install_signal_handler(stop_flag: Arc<AtomicBool>) {
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\nReceived signal, stopping gracefully.");
                stop_flag.store(true, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }
}
