# Shopping Sessions Attribution Engine

High-performance, zero-allocation sessionization and marketing attribution engine written in Rust for the Databricks Shopping Sessions challenge.

## Architecture Highlights
- **Zero-Copy Vector Ingest:** Direct `mmap` streaming with AVX2/AVX-512 date/timestamp decoders.
- **Inverted Key Indexing:** Hardware-accelerated integer key inversion avoiding large hash table lookups.
- **Cache-Partitioned Dedup:** 64-way hash partitioned record arenas sized to fit in CPU L2/L3 caches.
- **Parallel Sessionization:** Multi-core interval-merged campaign click attribution with lock-free aggregation.
- **Custom Parquet Sink:** Native streaming Parquet encoder writing directly to disk without intermediate copies.

## Build and Run
```bash
docker build -t shopping-sessions .
docker run --rm -v /path/to/data:/data:ro -v /path/to/output:/output shopping-sessions
```
