# Shopping Sessions Data Processing Engine

High-performance native pipeline for the Databricks Shopping Sessions track (Buildathon 2026).
Cleans and transforms 60 million click-stream events into exact daily user and campaign Parquet tables.

## Pipeline Architecture

1. **Vectorized Line & Field Tokenizer (`src/tokenizer.hpp`):**
   - Employs 64-byte AVX-512BW / AVX2 register classifications to locate commas, newlines, and quotation boundaries.
   - Quote-pair parity validation via branchless bitmask arithmetic.

2. **Opaque Identifier Resolution:**
   - 7-bit injective packing for short alphanumeric IDs into 63-bit integer keys.
   - Lock-free 256-shard hash open-addressing table for variable-width 32-character hexadecimal suffix IDs (conforming to `shopping-sessions-5`).

3. **Identity Union-Find & Session Attribution:**
   - Disjoint-set union (UF) for connected shopper identifiers with path compression.
   - Per-person interval attribution with a 7-day sliding lookback window.

4. **Direct Zero-Copy Parquet Serialization (`src/parquet_encoder.hpp`):**
   - Direct PLAIN uncompressed columnar encoding for `user_daily` and `campaign_daily` tables.
   - Zero dynamic runtime dependencies; single static binary running `FROM scratch`.
