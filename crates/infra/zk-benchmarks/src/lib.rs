#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub use base_load_tests::{
    LoadTestCleanupSummary, LoadTestExecutor, LoadTestRunOptions, LoadTestRunOutput,
    MetricsSummary, TestConfig,
};

mod zk_bench;
pub use zk_bench::{
    ZkBenchConfig, ZkBenchExecutionStatsSummary, ZkBenchMode, ZkBenchProofOutcome, ZkBenchRunner,
    ZkBenchSummary, ZkBenchTargetRange,
};
