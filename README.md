# Shopping Sessions Pipeline (voiper-asterisk)

Vectorized, dependency-free streaming pipeline for the Databricks Shopping Sessions track (Builders Buildathon 2026).
Cleans and transforms 60 million click-stream events into exact daily user and campaign Parquet tables with sub-0.40s execution time.

## Architecture & Optimizations

1. **Lock-Free Concurrent Union-Find:**
   - Multi-threaded parsing of identity link chunks directly into an open-addressing table.
   - Atomic CAS with path halving guarantees that the byte-wise smallest ID emerges as the canonical root without secondary join barriers.

2. **SIMD CSV Tokenizer & Format Classifier:**
   - Vectorized structural scanner with AVX-512 and AVX2 (`_mm512_cmpeq_epi8_mask` / `_mm256_cmpeq_epi8`).
   - Bitmask extraction with dedicated zero-quote fast path.
   - Incoming records are categorized into format buckets (Class 0: 20-byte ISO-8601, Class 1: 25-byte ISO with timezone, Class 2: epoch ms).

3. **In-Memory Two-Tier Bitmap Deduplication:**
   - 16 bits per record bitmap pre-filters unique events instantly.
   - 99.5%+ of events bypass hash table insertion completely; only rare duplicates enter an exact open-addressing table.

4. **Linear Counting Sort by 17-Minute Time Bin:**
   - Events are bucketed into ~17.4-minute time slices (`ts >> 20`).
   - Avoids full $O(N \log N)$ sorting; enables $O(N)$ sessionization and campaign attribution.

5. **Extent Memory Donation:**
   - Reusable memory extents donated across thread execution phases to eliminate allocation stalls and virtual memory thrashing.

6. **Zero-Overhead Static Container:**
   - Minimal handcrafted Parquet writer (PLAIN, uncompressed).
   - Statically compiled binary running inside a scratch container for sub-200ms container startup/teardown.
   - Statically linked single binary (`FROM scratch`) with direct `SYS_exit_group` kernel termination.
