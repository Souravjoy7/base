# Base ZK Benchmarks

ZK proof benchmarking utilities for completed Base load-test runs.

This crate reuses `base-load-tests` summaries to select proof targets. Dry-run benchmarks prove the
full confirmed load-test range. Cluster benchmarks prove the single fullest confirmed block by gas,
using transaction count and then the lowest block number as tie-breakers.

## Usage

```bash
cargo run -p base-zk-benchmarks-bin --bin base-zk-benchmarks -- path/to/config.yaml --mode dry-run
```

Override the default local endpoints with `--rollup-rpc-url` for the op-node RPC and
`--zk-prover-url` for the prover RPC.
