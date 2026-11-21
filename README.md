# Shopping Sessions (Team FAST)

Rust implementation of the Databricks Shopping Sessions rules (`rules.md`), written from scratch:
memory-mapped CSV, parallel parse with SIMD field splitting, hash-partitioned dedup,
per-person sessionization with interval-merged assists, and a hand-written Parquet writer.

    docker build --platform linux/amd64 -t sessions .
