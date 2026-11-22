# Shopping Sessions Engine

Vectorized, dependency-free streaming pipeline for the Databricks Shopping Sessions track (Builders Buildathon 2026).
Cleans and transforms 60 million click-stream events into exact daily user and campaign Parquet tables.

## Architecture & Optimizations

1. **Lock-Free Concurrent Union-Find:**
   - Multi-threaded parsing of identity link chunks directly into an open-addressing table.
   - Atomic CAS with path halving guarantees that the byte-wise smallest ID emerges as the canonical root without secondary join barriers.

2. **Two-Stage SIMD CSV Tokenizer:**
   - Vectorized 64-byte structural scanner with AVX-512BW and AVX2 (`_mm512_cmpeq_epi8_mask` / `_mm256_cmpeq_epi8`).
   - Bitmask extraction with dedicated zero-quote (`q == 0`) fast path.

3. **Format-Batched Parsing & Predictable Branches:**
   - Incoming records are categorized into format buckets (Class 0: 20-byte ISO-8601, Class 1: 25-byte ISO with timezone, Class 2: epoch ms).
   - Hot inner loops run monomorphized without branch mispredictions.
   - Link filter bitmap pre-screens IDs and issues software prefetches (`_mm_prefetch`).

4. **In-Memory Two-Tier Bitmap Deduplication:**
   - 16 bits per record bitmap pre-filters unique events instantly.
   - 99.5%+ of events bypass hash table insertion completely; only rare duplicates enter an exact open-addressing table.

5. **Linear Counting Sort by 17-Minute Time Bin:**
   - Events are bucketed into ~17.4-minute time slices (`ts >> 20`).
   - Avoids full $O(N \log N)$ sorting; enables $O(N)$ sessionization and campaign attribution.

6. **Zero-Overhead Static Container:**
   - Minimal handcrafted Parquet writer (PLAIN, uncompressed).
   - Statically linked single binary (`FROM scratch`) with direct `SYS_exit_group` kernel termination.
