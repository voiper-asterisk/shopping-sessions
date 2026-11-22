# Shopping Sessions Engine

Vectorized, dependency-free streaming pipeline for the Databricks Shopping Sessions track (Buildathon 2026).
Cleans and transforms 60 million click-stream events into exact daily user and campaign Parquet tables.

## Architecture & Optimizations

1. **AVX-512BW Structural Tokenizer (`src/scan.rs` & `src/scan2.rs`):**
   - 64-byte vector classification using `_mm512_cmpeq_epi8_mask` to locate delimiters in a single CPU cycle.
   - Branchless quote parity and separator indexing.
   - AVX2 fallback for non-AVX-512 environments.

2. **Opaque Identifier Resolution (`src/ids.rs`):**
   - Compact 63-bit injective bit-packing for short IDs (<= 9 bytes), preserving lexicographical sort order in integer registers.
   - 63-bit hash with 48-bit zero-copy memory-mapped file locators for long 32-character hexadecimal IDs.

3. **Lock-Free Bump Allocator & Huge Pages (`src/sys.rs`):**
   - 24 GB single lazy reservation with `MAP_NORESERVE` and `MADV_HUGEPAGE`.
   - Parallel multi-core page eviction (`MADV_DONTNEED`) across worker threads before instant `_exit(0)` termination.

4. **Two-Level Bitmap Deduplication (`src/main.rs`):**
   - Two-level bloom/bitmap filters (`b1`, `b2`) that detect duplicate event keys without inserting unique records into hash tables.

5. **Streaming Parquet Sink (`src/parquet.rs`):**
   - Handcrafted columnar PLAIN uncompressed Parquet writer without runtime dependencies.
