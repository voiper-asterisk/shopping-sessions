// Shopping Sessions: clean click-stream CSVs into two daily Parquet tables.
// The rules are in rules.md; reference/ref_impl.py is the slow oracle this
// program is tested against.
//
// Phases:
//   1. parse   links on thread 0 (union-find -> alias map), events and clicks on
//              every thread; events are scattered by event_id hash, clicks by person
//   2. dedup   per event bucket: keep the smallest ingest_ts copy, drop it if
//              invalid, resolve the person, scatter by person
//   3. people  per person bucket: order by time, sessions, click credit and
//              assists, daily rows; each worker writes its own Parquet file
#include <algorithm>
#include <atomic>
#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <string>
#include <string_view>
#include <thread>
#include <unordered_map>
#include <vector>

#include <dirent.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#ifdef __linux__
#include <sys/vfs.h>
#endif
#include <time.h>
#ifdef __linux__
#include <sched.h>
#endif
#include <unistd.h>

#if defined(__AVX2__)
#include <immintrin.h>
#elif defined(__ARM_NEON)
#include <arm_neon.h>
#endif

#include "parquet_encoder.hpp"

#ifndef SS_UNIT
#define SS_UNIT 128         // bytes staged per bucket before they are moved to its block chain
#endif
// Build variants kept for measurement. Defaults: the carry-add field-start
// scanner and the sortable ingest key. SS_SCAN1 / SS_SCAN2 / SS_SCAN3 select
// the older scanners, SS_ING1 the full ingest_ts conversion.
#if !defined(SS_SCAN1) && !defined(SS_SCAN2) && !defined(SS_SCAN3) && !defined(SS_SCAN4)
#define SS_SCAN4
#endif
#if !defined(SS_ING1) && !defined(SS_ING2)
#define SS_ING2
#endif
// Linked IDs: by default every surviving event and every click has its person
// looked up when it is routed (SS_ALIAS1). SS_ALIAS2 routes by the ID as written
// and settles each ID once inside the person buckets, passing the records of
// people with several IDs on to a second round. On the scoring machine the two
// tie at 20M events and SS_ALIAS2 is 4% slower at 60M: its second set of lists
// needs staging memory that no longer fits the cache beside the rest.
#if !defined(SS_ALIAS1) && !defined(SS_ALIAS2)
#define SS_ALIAS1
#endif
#if defined(SS_PEOPLE1) && !defined(SS_ALIAS1)
#error "SS_PEOPLE1 needs SS_ALIAS1"
#endif
// ID keys: the lean form (an ID's bytes as loaded) unless SS_ID1 asks for the
// 7-bit packed form.
#if !defined(SS_ID1) && !defined(SS_ID2)
#define SS_ID2
#endif
#ifdef SS_NOFORCEINLINE
#define SS_INLINE
#else
#define SS_INLINE __attribute__((always_inline))
#endif
// The per-row helpers (ID packing, timestamps). Left to itself GCC keeps them
// as separate functions, called several times for every row.
#ifdef SS_NOHOTINLINE
#define SS_HOT
#else
#define SS_HOT __attribute__((always_inline))
#endif
// The per-session and per-row steps of the people phase, likewise.
#ifdef SS_NOWALKINLINE
#define SS_WALK
#else
#define SS_WALK __attribute__((always_inline))
#endif
#ifndef SS_X
#define SS_X 0            // measurement builds only: skip one step (see probe branch)
#endif
#ifndef SS_LVL
#define SS_LVL 0          // measurement builds only: stop the event row handler after step N
#endif
#ifndef SS_SCANMAX
#define SS_SCANMAX 16     // a person with at most this many clicks has them scanned for every session
#endif
#ifndef SS_CLVL
#define SS_CLVL 0         // measurement builds only: stop the click row handler after step N
#endif
#ifndef SS_PROBE
#define SS_PROBE 0
#endif
#ifndef SS_DX
#define SS_DX 0           // measurement builds only: leave one step of the dense walk out (dense.h)
#endif

typedef uint8_t u8;
typedef uint16_t u16;
typedef uint32_t u32;
typedef uint64_t u64;
typedef int32_t i32;
typedef int64_t i64;

static const i64 DAY = 86'400'000;
static const i64 SESSION_GAP = 1'800'000;
static const i64 REACH = 7 * DAY;
static const i64 TS_BAD = INT64_MIN;
static const u32 DEAD = 0xFFFFFFFFu;        // RecA.ts: invalid copy, or a copy that lost dedup

// Present only in an instrumented (profile-generating) build; null otherwise.
// _exit skips the normal profile write-out, so it is called by hand.
#ifdef __linux__
extern "C" void __gcov_dump(void) __attribute__((weak));
#endif

[[noreturn]] static void die(const char* what) {
    fprintf(stderr, "fatal: %s\n", what);
    _exit(3);
}

static double now() {
    timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return double(t.tv_sec) + double(t.tv_nsec) * 1e-9;
}

static inline u64 rd64(const u8* p) { u64 v; memcpy(&v, p, 8); return v; }
static inline u32 rd32(const u8* p) { u32 v; memcpy(&v, p, 4); return v; }
static inline u16 rd16(const u8* p) { u16 v; memcpy(&v, p, 2); return v; }

[[maybe_unused]] static inline u64 mix64(u64 x) {
    x ^= x >> 33; x *= 0xff51afd7ed558ccdULL;
    x ^= x >> 33; x *= 0xc4ceb9fe1a85ec53ULL;
    x ^= x >> 33;
    return x;
}

// The hash used for every table and bucket choice: one multiply. Only its high
// bits are well mixed, so indexes are always taken from the top down.
static inline u64 hash64(u64 x) {
#ifdef SS_MIXHASH
    return mix64(x);
#else
    return x * 0x9E3779B97F4A7C15ULL;
#endif
}

// The dense path (dense.h): whether this run uses it, and its hook in the general dedup.
static bool g_dense_on = false;
static inline bool dense_extra(u32, u64, u32, u32) { return false; }

static inline i64 floor_div(i64 a, i64 b) { i64 q = a / b; return q - ((a % b != 0) && ((a < 0) != (b < 0))); }

// ---------------------------------------------------------------- IDs
// An ID of 1 to 9 ASCII bytes (none NUL) packs into 63 bits, 7 bits a byte,
// first byte highest, so integer order is byte-wise order. Anything else is
// interned: bit 63 plus an index into a shared table.
static const u64 INTERN_BIT = 1ULL << 63;

// The interned IDs: every distinct one is stored once, and its key is bit 63
// plus the address of that copy, so a key leads straight to the bytes. The
// table is split in 256 parts by hash. A look-up reads its part without a lock
// (slots only ever go from empty to filled, and a part that grows leaves its old
// slots in place for readers still on them); only adding a new ID takes the
// part's lock. Since scorer version 5 one ID in eight is this long, so this is
// on the path of every eighth row, not an oddity: a look-up is one hash and,
// nearly always, one slot.
//
// A slot holds the ID's whole 64-bit hash, and a look-up that finds its hash
// takes the slot without reading the stored bytes (that second read is a cache
// miss on every long ID). Two different IDs with one hash would become one ID:
// with three million long IDs that is one chance in a few million. Build with
// -DSS_INTERN_VERIFY to compare the bytes as well.
struct InternEntry { u32 len; u8 bytes[4]; };                    // bytes run on for len
struct InternSlot { std::atomic<u64> hash; std::atomic<u64> entry; };
struct InternPart {
    std::atomic<u64> table{0};                                   // address of the slots (64-aligned) + log2 of their number
    std::atomic_flag busy = ATOMIC_FLAG_INIT;
    u64 count = 0;
    u8* room = nullptr;
    size_t room_left = 0;
    char pad[24];
};
alignas(64) static InternPart g_intern[256];
static u32 g_intern_start = 10;                                  // log2 of a part's first table

static inline u64 mix128(u64 a, u64 b) { const unsigned __int128 r = (unsigned __int128)a * b; return u64(r) ^ u64(r >> 64); }
// A 64-bit hash of n bytes, never 0; reads only those bytes.
static inline SS_HOT u64 hash_bytes(const u8* p, size_t n) {
    u64 h = 0x9E3779B97F4A7C15ULL ^ (n * 0xD6E8FEB86659FD93ULL);
    while (n >= 16) {
        h = mix128(rd64(p) ^ 0xA0761D6478BD642FULL, rd64(p + 8) ^ h);
        p += 16; n -= 16;
    }
    u64 a = 0, b = 0;
    if (n >= 8) { a = rd64(p); b = rd64(p + n - 8); }
    else if (n >= 4) { a = rd32(p); b = rd32(p + n - 4); }
    else if (n) { a = (u64(p[0]) << 16) | (u64(p[n >> 1]) << 8) | p[n - 1]; }
    h = mix128(a ^ 0xE7037ED1A0B428DBULL, b ^ h);
    h = mix128(h, 0x8EBC6AF09C88C6E3ULL);
    return h ? h : 1;
}

// Sizes the parts' first tables for an input of this many bytes (about one long
// ID per 500 bytes of the usual data), so that they seldom have to grow.
static void intern_reserve(u64 input_bytes) {
    u64 per_part = input_bytes / 500 / 256 * 2;
    u32 bits = 10;
    while (bits < 20 && (1ULL << bits) < per_part) bits++;
    g_intern_start = bits;
}

static InternSlot* intern_new_slots(u32 bits) {
    void* mem = nullptr;
    const size_t bytes = sizeof(InternSlot) << bits;
    if (posix_memalign(&mem, 64, bytes) != 0 || !mem) die("out of memory for the ID table");
#ifdef MADV_HUGEPAGE
    if (bytes >= (2u << 20)) madvise(mem, bytes, MADV_HUGEPAGE);
#endif
    memset(mem, 0, bytes);
    return (InternSlot*)mem;
}

static inline SS_HOT const InternEntry* intern_find(u64 table, u64 h, [[maybe_unused]] const u8* p, [[maybe_unused]] size_t n) {
    const InternSlot* slots = (const InternSlot*)uintptr_t(table & ~63ULL);
    const u64 mask = (1ULL << (table & 63)) - 1;
    for (u64 i = (h >> 8) & mask;; i = (i + 1) & mask) {
        const u64 seen = slots[i].hash.load(std::memory_order_acquire);
        if (seen == h) {
            const InternEntry* e = (const InternEntry*)uintptr_t(slots[i].entry.load(std::memory_order_relaxed));
#ifdef SS_INTERN_VERIFY
            if (e->len != n || memcmp(e->bytes, p, n) != 0) continue;
#endif
            return e;
        }
        if (!seen) return nullptr;
    }
}

__attribute__((noinline)) static u64 intern_add(InternPart& part, u64 h, const u8* p, size_t n) {
    while (part.busy.test_and_set(std::memory_order_acquire)) { }
    u64 table = part.table.load(std::memory_order_relaxed);
    const InternEntry* found = table ? intern_find(table, h, p, n) : nullptr;      // added by another thread meanwhile?
    if (!found) {
        const u64 size = table ? 1ULL << (table & 63) : 0;
        if (!table || part.count * 2 >= size) {
            // Grow: the new slots take every entry; the old ones stay for readers still on them.
            const u32 bits = table ? u32(table & 63) + 2 : g_intern_start;
            InternSlot* bigger = intern_new_slots(bits);
            const u64 mask = (1ULL << bits) - 1;
            const InternSlot* old = (const InternSlot*)uintptr_t(table & ~63ULL);
            for (u64 i = 0; i < size; i++) {
                const u64 hh = old[i].hash.load(std::memory_order_relaxed);
                if (!hh) continue;
                u64 j = (hh >> 8) & mask;
                while (bigger[j].hash.load(std::memory_order_relaxed)) j = (j + 1) & mask;
                bigger[j].entry.store(old[i].entry.load(std::memory_order_relaxed), std::memory_order_relaxed);
                bigger[j].hash.store(hh, std::memory_order_relaxed);
            }
            table = u64(uintptr_t(bigger)) | bits;
            part.table.store(table, std::memory_order_release);
        }
        const size_t need = (sizeof(u32) + n + 8 + 7) & ~size_t(7);         // 8 spare bytes: an entry can be read a word at a time
        if (part.room_left < need) {
            const size_t block = std::max<size_t>(need, 1 << 18);
            part.room = (u8*)malloc(block);
            if (!part.room) die("out of memory for interned IDs");
            part.room_left = block;
        }
        InternEntry* e = (InternEntry*)part.room;
        part.room += need; part.room_left -= need;
        e->len = u32(n);
        memcpy(e->bytes, p, n);
        memset(e->bytes + n, 0, 8);
        InternSlot* slots = (InternSlot*)uintptr_t(table & ~63ULL);
        const u64 mask = (1ULL << (table & 63)) - 1;
        u64 j = (h >> 8) & mask;
        while (slots[j].hash.load(std::memory_order_relaxed)) j = (j + 1) & mask;
        slots[j].entry.store(u64(uintptr_t(e)), std::memory_order_relaxed);        // the entry first: a reader that sees the hash finds it
        slots[j].hash.store(h, std::memory_order_release);
        part.count++;
        found = e;
    }
    part.busy.clear(std::memory_order_release);
    return INTERN_BIT | u64(uintptr_t(found));
}

static inline SS_HOT u64 intern_id(const u8* p, size_t n) {
    const u64 h = hash_bytes(p, n);
    InternPart& part = g_intern[h & 255];
    if (const u64 table = part.table.load(std::memory_order_acquire))
        if (const InternEntry* e = intern_find(table, h, p, n)) return INTERN_BIT | u64(uintptr_t(e));
    return intern_add(part, h, p, n);
}
// The same in two steps, for a parser that can ask for the slot now and read it
// a few rows later: the look-up of a long ID is then no longer a wait on memory.
static inline SS_HOT void intern_prefetch(u64 h) {
    const u64 table = g_intern[h & 255].table.load(std::memory_order_acquire);
    if (table) __builtin_prefetch((const InternSlot*)uintptr_t(table & ~63ULL) + ((h >> 8) & ((1ULL << (table & 63)) - 1)));
}
static inline SS_HOT u64 intern_hashed(u64 h, const u8* p, size_t n) {
    InternPart& part = g_intern[h & 255];
    if (const u64 table = part.table.load(std::memory_order_acquire))
        if (const InternEntry* e = intern_find(table, h, p, n)) return INTERN_BIT | u64(uintptr_t(e));
    return intern_add(part, h, p, n);
}
static inline const InternEntry* intern_entry(u64 key) { return (const InternEntry*)uintptr_t(key & ~INTERN_BIT); }

// An ID that is only ever compared for equality (event_id) and is too long to
// pack: a 63-bit hash of its bytes stands for it. See pack_event_id.
static inline u64 long_event_key(const u8* p, size_t n) {
#ifdef SS_EVENT_INTERN
    return intern_id(p, n);
#else
    return INTERN_BIT | (hash_bytes(p, n) >> 1);
#endif
}

[[maybe_unused]] static inline SS_HOT u64 compact7(u64 x) {
#if defined(__BMI2__)
    return _pext_u64(x, 0x7F7F7F7F7F7F7F7FULL);
#else
    x = ((x & 0x7F007F007F007F00ULL) >> 1) | (x & 0x007F007F007F007FULL);
    x = ((x & 0x3FFF00003FFF0000ULL) >> 2) | (x & 0x00003FFF00003FFFULL);
    x = ((x & 0x0FFFFFFF00000000ULL) >> 4) | (x & 0x000000000FFFFFFFULL);
    return x;
#endif
}

// p must be readable for 9 bytes whatever n is. `prefix` is the letter IDs of
// this column normally start with ('e', 'u' or 'k').
#ifdef SS_ID2
// Lean form: an ID that is its column's letter plus 1 to 8 ASCII bytes (none
// NUL) is keyed by those bytes as loaded, zero-padded. Byte order is then the
// order of the byte-swapped keys.
// True, with its key, for an ID that packs; false for one that has to be interned.
static inline SS_HOT bool pack_short(const u8* p, size_t n, u8 prefix, u64& key) {
#ifndef SS_FORCE_SLOW
    if (p[0] == prefix && n - 2 < 8) {
        const u64 mask = ~0ULL >> (8 * (9 - n));
        const u64 v = rd64(p + 1) & mask;
        const u64 t = v | ~mask;
        const u64 high = 0x8080808080808080ULL;
        if (!((v & high) | ((t - 0x0101010101010101ULL) & ~t & high))) { key = v; return true; }
    }
#endif
    return false;
}
static inline SS_HOT u64 pack_id(const u8* p, size_t n, u8 prefix) {
    u64 key;
    if (pack_short(p, n, prefix, key)) return key;
    return prefix == 'e' ? long_event_key(p, n) : intern_id(p, n);
}

static std::string id_string(u64 key, u8 prefix) {
    if (key & INTERN_BIT) {
        const InternEntry* e = intern_entry(key);
        return std::string((const char*)e->bytes, e->len);
    }
    char out[9];
    int n = 0;
    out[n++] = char(prefix);
    for (; key; key >>= 8) out[n++] = char(key & 0xFF);
    return std::string(out, n);
}

// The bytes of an ID without building a string: an interned one where it is
// stored, a packed one written out into buf.
static inline size_t id_view(u64 key, u8 prefix, u8* buf, const u8*& bytes) {
    if (key & INTERN_BIT) { const InternEntry* e = intern_entry(key); bytes = e->bytes; return e->len; }
    size_t n = 0;
    buf[n++] = prefix;
    for (; key; key >>= 8) buf[n++] = u8(key & 0xFF);
    bytes = buf;
    return n;
}
static bool id_less_slow(u64 a, u64 b, u8 prefix) {             // byte-wise order, a prefix before the longer one
    u8 ba[16], bb[16];
    const u8 *pa, *pb;
    const size_t na = id_view(a, prefix, ba, pa), nb = id_view(b, prefix, bb, pb);
    const int c = memcmp(pa, pb, std::min(na, nb));
    return c < 0 || (c == 0 && na < nb);
}
static inline bool id_less(u64 a, u64 b, u8 prefix) {
    if (!((a | b) & INTERN_BIT)) return __builtin_bswap64(a) < __builtin_bswap64(b);
    return id_less_slow(a, b, prefix);
}
#else
static inline SS_HOT u64 pack_id(const u8* p, size_t n, u8) {
#ifdef SS_FORCE_SLOW
    return intern_id(p, n);
#endif
    if (n - 1 < 9) {
        u64 v = __builtin_bswap64(rd64(p));
        u64 ninth = 0;
        u64 fill = 0;
        if (n == 9) {
            ninth = p[8];
            if (ninth - 1 >= 127) return intern_id(p, n);
        } else if (n < 8) {
            v &= ~0ULL << (8 * (8 - n));
            fill = 0x0101010101010101ULL >> (8 * n);
        }
        u64 t = v | fill;
        const u64 high = 0x8080808080808080ULL;
        if ((v & high) | ((t - 0x0101010101010101ULL) & ~t & high)) return intern_id(p, n);
        return (compact7(v) << 7) | ninth;
    }
    return intern_id(p, n);
}

static inline SS_HOT bool pack_short(const u8* p, size_t n, u8 prefix, u64& key) { key = pack_id(p, n, prefix); return true; }

static std::string id_string(u64 key, u8) {
    if (key & INTERN_BIT) {
        const InternEntry* e = intern_entry(key);
        return std::string((const char*)e->bytes, e->len);
    }
    char out[9];
    int n = 0;
    for (int shift = 56; shift >= 0; shift -= 7) {
        char c = char((key >> shift) & 0x7F);
        if (!c) break;
        out[n++] = c;
    }
    return std::string(out, n);
}

static bool id_less_slow(u64 a, u64 b, u8 prefix) { return id_string(a, prefix) < id_string(b, prefix); }   // std::string compares bytes as unsigned
static inline bool id_less(u64 a, u64 b, u8 prefix) {
    if (!((a | b) & INTERN_BIT)) return a < b;
    return id_less_slow(a, b, prefix);
}
#endif

// ---------------------------------------------------------------- timestamps
static inline i64 days_from_civil(i64 y, i64 m, i64 d) {
    y -= m <= 2;
    i64 era = (y >= 0 ? y : y - 399) / 400;
    i64 yoe = y - era * 400;
    i64 doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    return era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
}

// Remembers `YYYY-MM-DD` texts already validated. A hit needs all 10 bytes equal.
struct DateCache {
    u64 a[64];
    u16 b[64];
    i64 ms[64];
    DateCache() {
        memset(a, 0, sizeof a);
        memset(b, 0, sizeof b);
        for (i64& v : ms) v = TS_BAD;       // ten NUL bytes match an empty slot, and are not a date
    }
};

// `YYYY-MM-DD` at p -> epoch ms of that midnight, or TS_BAD.
static inline i64 parse_date(const u8* p, DateCache& cache) {
    u64 a = rd64(p);
    u16 b = rd16(p + 8);
    u32 slot = ((p[9] & 15) + (p[8] & 3) * 10 + (p[6] & 1) * 32) & 63;
    if (cache.a[slot] == a && cache.b[slot] == b) return cache.ms[slot];
    u32 bad = (p[4] != '-') | (p[7] != '-');
    u32 d0 = p[0] - '0', d1 = p[1] - '0', d2 = p[2] - '0', d3 = p[3] - '0';
    u32 d5 = p[5] - '0', d6 = p[6] - '0', d8 = p[8] - '0', d9 = p[9] - '0';
    bad |= (d0 > 9) | (d1 > 9) | (d2 > 9) | (d3 > 9) | (d5 > 9) | (d6 > 9) | (d8 > 9) | (d9 > 9);
    if (bad) return TS_BAD;
    i64 year = d0 * 1000 + d1 * 100 + d2 * 10 + d3;
    i64 month = d5 * 10 + d6, day = d8 * 10 + d9;
    if (month < 1 || month > 12 || day < 1) return TS_BAD;
    static const int DIM[13] = {0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31};
    int dim = DIM[month];
    if (month == 2 && year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)) dim = 29;
    if (day > dim) return TS_BAD;
    i64 ms = days_from_civil(year, month, day) * DAY;
    cache.a[slot] = a; cache.b[slot] = b; cache.ms[slot] = ms;
    return ms;
}

// `HH:MM:SS` at p -> ms since midnight, or -1.
static inline i64 parse_tod(const u8* p) {
    u64 x = rd64(p) ^ 0x30303A30303A3030ULL;      // digits become 0..9, colons 0
    u64 over = ((x + 0x7676767676767676ULL) | x) & 0x8080808080808080ULL;
    u32 h = u32(x & 0xFF) * 10 + u32((x >> 8) & 0xFF);
    u32 m = u32((x >> 24) & 0xFF) * 10 + u32((x >> 32) & 0xFF);
    u32 s = u32((x >> 48) & 0xFF) * 10 + u32((x >> 56) & 0xFF);
    u32 colons = u32((x >> 16) & 0xFF) | u32((x >> 40) & 0xFF);
    if (over | colons | (h > 23) | (m > 59) | (s > 59)) return -1;
    return i64(h * 3600 + m * 60 + s) * 1000;
}

// `HH:MM:SS` at p -> a code that orders times of day (under 2^24), or -1.
[[maybe_unused]] static inline SS_HOT i64 tod_code(const u8* p) {
#if defined(__BMI2__)
    const u64 raw = rd64(p);
    const u64 x = raw ^ 0x30303A30303A3030ULL;
    if ((((x + 0x7676767676767676ULL) | x) & 0x8080808080808080ULL) | (x & 0x0000FF0000FF0000ULL)) return -1;
    const u32 bcd = u32(_pext_u64(__builtin_bswap64(raw), 0x0F0F000F0F000F0FULL));      // 0xHHMMSS
    if (((bcd >> 16) > 0x23) | (((bcd >> 8) & 0xFF) > 0x59) | ((bcd & 0xFF) > 0x59)) return -1;
    return bcd;
#else
    const i64 ms = parse_tod(p);
    return ms < 0 ? -1 : ms / 1000;
#endif
}

// F1, exactly `YYYY-MM-DDTHH:MM:SSZ` (20 bytes at p).
static inline i64 parse_f1(const u8* p, DateCache& cache) {
    if ((p[10] != 'T') | (p[19] != 'Z')) return TS_BAD;
    i64 date = parse_date(p, cache);
    i64 tod = parse_tod(p + 11);
    if (date == TS_BAD || tod < 0) return TS_BAD;
    return date + tod;
}

static inline SS_HOT bool parse8(const u8* p, u64& out) {
    u64 v = rd64(p);
    u64 x = v ^ 0x3030303030303030ULL;
    if (((x + 0x7676767676767676ULL) | x) & 0x8080808080808080ULL) return false;
    x = (x * 2561) >> 8;
    x = ((x & 0x00FF00FF00FF00FFULL) * 6553601) >> 16;
    out = ((x & 0x0000FFFF0000FFFFULL) * 42949672960001ULL) >> 32;
    return true;
}

static i64 parse_f3_slow(const u8* p, size_t n) {
    if (n == 0) return TS_BAD;
    u64 v = 0;
    bool huge = false;
    for (size_t i = 0; i < n; i++) {
        u32 d = p[i] - '0';
        if (d > 9) return TS_BAD;
        if (v > (u64(INT64_MAX) - 9) / 10) huge = true; else v = v * 10 + d;
    }
    return huge ? INT64_MAX : i64(v);       // too long is not malformed, only outside any window
}

// The dates near the window, keyed by their text `YYYY-MM-DDT` through a hash
// chosen at start-up to have no collisions among them. A hit is an exact
// 11-byte match and gives that midnight in epoch ms; any other text misses and
// goes to the general parser.
struct DateTable {
    struct Entry { u64 a; u32 b; u32 used; i64 ms; i64 sec; };       // sec: that midnight in epoch seconds
    Entry slot[256];
    u64 seed = 0;

    static inline u32 index(u64 a, u32 b, u64 seed) { return u32(((a ^ (u64(b) << 24)) * seed) >> 56); }

    void build(i64 first_day, i64 last_day) {       // days since the epoch, inclusive
        memset(slot, 0, sizeof slot);
        seed = 0;
        const i64 count = last_day - first_day + 1;
        if (count < 1 || count > 72) return;         // too many to place without collisions: no table
        std::vector<Entry> all;
        for (i64 day = first_day; day <= last_day; day++) {
            i64 z = day + 719468;
            i64 era = (z >= 0 ? z : z - 146096) / 146097, doe = z - era * 146097;
            i64 yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
            i64 doy = doe - (365 * yoe + yoe / 4 - yoe / 100), mp = (5 * doy + 2) / 153;
            i64 dd = doy - (153 * mp + 2) / 5 + 1, mm = mp < 10 ? mp + 3 : mp - 9, yy = yoe + era * 400 + (mm <= 2);
            if (yy < 0 || yy > 9999) continue;
            char text[16];
            snprintf(text, sizeof text, "%04lld-%02lld-%02lldT", (long long)yy, (long long)mm, (long long)dd);
            all.push_back({rd64((const u8*)text), rd32((const u8*)text + 8) & 0xFFFFFF, 1, day * DAY, day * 86400});
        }
        u64 state = 0x2545F4914F6CDD1DULL;
        for (int attempt = 0; attempt < 200000; attempt++) {
            state ^= state << 13; state ^= state >> 7; state ^= state << 17;
            const u64 candidate = state | 1;
            memset(slot, 0, sizeof slot);
            bool clash = false;
            for (const Entry& e : all) {
                Entry& home = slot[index(e.a, e.b, candidate)];
                if (home.used) { clash = true; break; }
                home = e;
            }
            if (!clash) { seed = candidate; return; }
        }
        memset(slot, 0, sizeof slot);
    }
    // The entry of the date text at p, or null.
    inline SS_HOT const Entry* find(const u8* p) const {
        const u64 a = rd64(p);
        const u32 b = rd32(p + 8) & 0xFFFFFF;
        const Entry& e = slot[index(a, b, seed)];
        return (u32(e.a == a) & u32(e.b == b) & e.used) ? &e : nullptr;
    }
    inline SS_HOT i64 lookup(const u8* p) const {
        const u64 a = rd64(p);
        const u32 b = rd32(p + 8) & 0xFFFFFF;
        const Entry& e = slot[index(a, b, seed)];
        return (u32(e.a == a) & u32(e.b == b) & e.used) ? e.ms : TS_BAD;
    }
};
static DateTable g_dates;

// `HH:MM:SS` at p -> seconds since midnight, or -1. One add checks every digit,
// both colons and the tens ranges; one multiply forms the three numbers.
static inline SS_HOT i32 tod_seconds(const u8* p) {
    const u64 w = rd64(p) ^ 0x30303A30303A3030ULL;
    if (((w + 0x060A0F060A0F060DULL) | w) & 0xF0F0F0F0F0F0F0F0ULL) return -1;
    const u64 t = w * 2561;
    const u32 h = u32(t >> 8) & 0xFF, m = u32(t >> 32) & 0xFF, sec = u32(t >> 56);
    if (h > 23) return -1;
    return i32(h * 3600 + m * 60 + sec);
}

// Two `HH:MM:SS` texts at once: seconds since midnight for each, -1 for one that
// is not a time of day (exactly what tod_seconds says of each).
static inline SS_HOT void tod_pair(const u8* pa, const u8* pb, i32& sa, i32& sb) {
#if defined(__SSE4_1__) && !defined(SS_NOTODPAIR)
    const __m128i x = _mm_sub_epi8(_mm_set_epi64x((long long)rd64(pb), (long long)rd64(pa)), _mm_set1_epi8('0'));
    // Per place: the largest digit allowed (2, 9, 5, 9, 5, 9); the colons must be exactly 10 above '0'.
    const __m128i limit = _mm_setr_epi8(2, 9, 10, 5, 9, 10, 5, 9, 2, 9, 10, 5, 9, 10, 5, 9);
    const __m128i colon = _mm_setr_epi8(0, 0, 10, 0, 0, 10, 0, 0, 0, 0, 10, 0, 0, 10, 0, 0);
    const __m128i at_colon = _mm_setr_epi8(0, 0, -1, 0, 0, -1, 0, 0, 0, 0, -1, 0, 0, -1, 0, 0);
    const __m128i ok = _mm_and_si128(_mm_cmpeq_epi8(_mm_min_epu8(x, limit), x), _mm_cmpeq_epi8(_mm_and_si128(x, at_colon), colon));
    const u32 good = u32(_mm_movemask_epi8(ok));
    const __m128i digits = _mm_shuffle_epi8(x, _mm_setr_epi8(0, 1, 3, 4, 6, 7, -1, -1, 8, 9, 11, 12, 14, 15, -1, -1));
    const __m128i pairs = _mm_maddubs_epi16(digits, _mm_setr_epi8(10, 1, 10, 1, 10, 1, 0, 0, 10, 1, 10, 1, 10, 1, 0, 0));    // HH, MM, SS
    const __m128i parts = _mm_madd_epi16(pairs, _mm_setr_epi16(3600, 60, 1, 0, 3600, 60, 1, 0));
    const __m128i seconds = _mm_add_epi32(parts, _mm_srli_epi64(parts, 32));
    const u32 a = u32(_mm_cvtsi128_si32(seconds)), b = u32(_mm_extract_epi32(seconds, 2));
    sa = (((good & 0xFF) == 0xFF) & (a < 86400)) ? i32(a) : -1;        // under 86,400 seconds: the hour is at most 23
    sb = (((good >> 8) == 0xFF) & (b < 86400)) ? i32(b) : -1;
#else
    sa = tod_seconds(pa);
    sb = tod_seconds(pb);
#endif
}

// tod_pair without its verdicts: the two sums as they come, and one bit per byte
// that is the digit or colon its place asks for (all 16 set: both texts are well
// formed, and each is a time of day if its sum is under 86,400).
#if defined(__SSE4_1__)
static inline SS_HOT u32 tod_pair_raw(const u8* pa, const u8* pb, u32& a, u32& b) {
    const __m128i x = _mm_sub_epi8(_mm_set_epi64x((long long)rd64(pb), (long long)rd64(pa)), _mm_set1_epi8('0'));
    const __m128i limit = _mm_setr_epi8(2, 9, 10, 5, 9, 10, 5, 9, 2, 9, 10, 5, 9, 10, 5, 9);
    const __m128i colon = _mm_setr_epi8(0, 0, 10, 0, 0, 10, 0, 0, 0, 0, 10, 0, 0, 10, 0, 0);
    const __m128i at_colon = _mm_setr_epi8(0, 0, -1, 0, 0, -1, 0, 0, 0, 0, -1, 0, 0, -1, 0, 0);
    const __m128i ok = _mm_and_si128(_mm_cmpeq_epi8(_mm_min_epu8(x, limit), x), _mm_cmpeq_epi8(_mm_and_si128(x, at_colon), colon));
    const __m128i digits = _mm_shuffle_epi8(x, _mm_setr_epi8(0, 1, 3, 4, 6, 7, -1, -1, 8, 9, 11, 12, 14, 15, -1, -1));
    const __m128i pairs = _mm_maddubs_epi16(digits, _mm_setr_epi8(10, 1, 10, 1, 10, 1, 0, 0, 10, 1, 10, 1, 10, 1, 0, 0));
    const __m128i parts = _mm_madd_epi16(pairs, _mm_setr_epi16(3600, 60, 1, 0, 3600, 60, 1, 0));
    const __m128i seconds = _mm_add_epi32(parts, _mm_srli_epi64(parts, 32));
    a = u32(_mm_cvtsi128_si32(seconds));
    b = u32(_mm_extract_epi32(seconds, 2));
    return u32(_mm_movemask_epi8(ok));
}
#endif

// Any `ts`: F1, F2 or F3 -> epoch ms, or TS_BAD. p readable for n + 8 bytes.
static inline SS_HOT i64 parse_ts(const u8* p, size_t n, DateCache& cache) {
#ifndef SS_FORCE_SLOW
    if (n == 20) {
        if (p[19] == 'Z') {
#ifndef SS_TS1
            const i64 day = g_dates.lookup(p);
            if (day != TS_BAD) {
                const i32 seconds = tod_seconds(p + 11);
                return seconds < 0 ? TS_BAD : day + i64(seconds) * 1000;
            }
#endif
            return parse_f1(p, cache);
        }
    } else if (n == 13) {
        u64 hi, lo;
        if (parse8(p, hi) && parse8(p + 5, lo)) return i64(hi * 100000 + lo % 100000);
        return TS_BAD;
    }
#else
    if (n == 20 && p[19] == 'Z') return parse_f1(p, cache);
#endif
    if (n == 25 && p[10] == 'T' && (p[19] == '+' || p[19] == '-') && p[22] == ':') {
        i64 date, tod;
#if !defined(SS_FORCE_SLOW) && !defined(SS_TS1)
        date = g_dates.lookup(p);
        if (date != TS_BAD) {
            const i32 seconds = tod_seconds(p + 11);
            tod = seconds < 0 ? -1 : i64(seconds) * 1000;
        } else
#endif
        {
            date = parse_date(p, cache);
            tod = parse_tod(p + 11);
        }
        u32 h0 = p[20] - '0', h1 = p[21] - '0', m0 = p[23] - '0', m1 = p[24] - '0';
        if (date == TS_BAD || tod < 0 || h0 > 9 || h1 > 9 || m0 > 9 || m1 > 9) return TS_BAD;
        u32 oh = h0 * 10 + h1, om = m0 * 10 + m1;
        if (oh > 23 || om > 59) return TS_BAD;
        i64 offset = i64(oh * 60 + om) * 60000;
        return p[19] == '+' ? date + tod - offset : date + tod + offset;
    }
    return parse_f3_slow(p, n);
}

// ---------------------------------------------------------------- CSV scan
struct Masks { u64 comma, nl, quote; };

static inline Masks classify(const u8* p) {
    Masks m;
#if defined(__AVX512BW__) && !defined(SS_NO_AVX512)
    __m512i v = _mm512_loadu_si512((const void*)p);
    m.comma = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(','));
    m.nl = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8('\n'));
    m.quote = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8('"'));
#elif defined(__AVX2__)
    __m256i a = _mm256_loadu_si256((const __m256i*)p), b = _mm256_loadu_si256((const __m256i*)(p + 32));
    __m256i c = _mm256_set1_epi8(','), n = _mm256_set1_epi8('\n'), q = _mm256_set1_epi8('"');
    m.comma = u32(_mm256_movemask_epi8(_mm256_cmpeq_epi8(a, c))) | (u64(u32(_mm256_movemask_epi8(_mm256_cmpeq_epi8(b, c)))) << 32);
    m.nl = u32(_mm256_movemask_epi8(_mm256_cmpeq_epi8(a, n))) | (u64(u32(_mm256_movemask_epi8(_mm256_cmpeq_epi8(b, n)))) << 32);
    m.quote = u32(_mm256_movemask_epi8(_mm256_cmpeq_epi8(a, q))) | (u64(u32(_mm256_movemask_epi8(_mm256_cmpeq_epi8(b, q)))) << 32);
#elif defined(__ARM_NEON)
    const uint8x16_t bits = {1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128};
    uint8x16_t v0 = vld1q_u8(p), v1 = vld1q_u8(p + 16), v2 = vld1q_u8(p + 32), v3 = vld1q_u8(p + 48);
    auto mask = [&](u8 ch) -> u64 {
        uint8x16_t c = vdupq_n_u8(ch);
        uint8x16_t s0 = vpaddq_u8(vandq_u8(vceqq_u8(v0, c), bits), vandq_u8(vceqq_u8(v1, c), bits));
        uint8x16_t s1 = vpaddq_u8(vandq_u8(vceqq_u8(v2, c), bits), vandq_u8(vceqq_u8(v3, c), bits));
        s0 = vpaddq_u8(s0, s1);
        s0 = vpaddq_u8(s0, s0);
        return vgetq_lane_u64(vreinterpretq_u64_u8(s0), 0);
    };
    m.comma = mask(','); m.nl = mask('\n'); m.quote = mask('"');
#else
    m.comma = m.nl = m.quote = 0;
    for (int i = 0; i < 64; i++) {
        m.comma |= u64(p[i] == ',') << i;
        m.nl |= u64(p[i] == '\n') << i;
        m.quote |= u64(p[i] == '"') << i;
    }
#endif
    return m;
}

// Calls row(first_byte, comma_positions, newline_position, quotes_in_line) for
// every line that starts in [start, stop) and has exactly NF fields. buf[start]
// starts a line; `last` is the index of the first '\n' at or after stop - 1,
// which ends the last such line; 64 readable bytes follow it.
#ifdef SS_SCAN4
static inline u32 tz64(u64 x) {
#if defined(__BMI__)
    return u32(_tzcnt_u64(x));
#else
    return x ? u32(__builtin_ctzll(x)) : 64;
#endif
}

// Calls row(starts, newline) for every line that starts in [start, stop) and has
// exactly NF well-formed fields; starts[i] is where field i's content begins
// (past an opening quote), and the byte before it is a quote exactly when the
// field is quoted.
//
// Per 64-byte block, with S the separators (commas and newlines) and Q the
// quotes: a quote directly after a separator opens a field; any other quote
// must close one, so the next byte must be a separator; an opener must not be
// followed by a separator (a field that is one quote); and a field that opened
// with a quote must end with one. The last test uses one addition: adding the
// openers to ~S makes a carry run through each opened field's bytes and stop on
// the separator that ends it, marking exactly the separators that end quoted
// fields. Lines with any fault, or the wrong field count, are decided byte by
// byte, so the result is exact.
template <int NF, class Row>
static inline SS_INLINE void scan_rows(const u8* buf, size_t start, size_t stop, size_t last, Row&& row) {
    if (start >= stop) return;
    enum { BLOCKS = 16, CAP = BLOCKS * 64 + 2 * NF + 48 };
    u32 sp[CAP];                            // content start of every field, in order
    u32 nlp[BLOCKS * 64 + 8];               // newline positions of this batch
    u32 bp[BLOCKS * 64 + 8];                // fault positions of this batch
    u32 n = 0;
    u64 after_sep = 1;                      // the byte before `start` is a newline
    u64 after_open = 0, after_other = 0, carry = 0;
    bool carry_bad = false;                 // the unfinished line already has a fault
    size_t line = start;
    size_t block = start;
    for (;;) {
        u32 newlines = 0, faults = 0;
        for (u32 done = 0; done < BLOCKS && block <= last; done++, block += 64) {
#ifndef SS_NOSCANPF
            __builtin_prefetch(buf + block + 1024);
#endif
            const Masks m = classify(buf + block);
            const u64 S = m.comma | m.nl, Q = m.quote;
            const u64 first = (S << 1) | after_sep;             // first byte of each field
            const u64 open = Q & first, other = Q & ~first;
            const u64 u = (open << 1) | after_open;             // byte after an opening quote
            const u64 t = (other << 1) | after_other;           // byte after any other quote
            const u64 plain = ~S;
            const u64 partial = plain + open;
            const u64 sum = partial + carry;
            carry = u64(partial < plain) | u64(sum < partial);
            u64 bad = (t & ~S) | (u & S) | ((sum ^ (t | u)) & S);
            u64 starts = (first & ~Q) | u;
            after_sep = S >> 63;
            after_open = open >> 63;
            after_other = other >> 63;
            const u32 base = u32(block);

            const u32 count = u32(__builtin_popcountll(starts));
            u32* out = sp + n;
            for (int i = 0; i < 8; i++) { out[i] = base + tz64(starts); starts &= starts - 1; }
            if (count > 8) {
                for (int i = 8; i < 16; i++) { out[i] = base + tz64(starts); starts &= starts - 1; }
                for (u32 i = 16; i < count; i++) { out[i] = base + tz64(starts); starts &= starts - 1; }
            }
            n += count;

            u64 nl = m.nl;
            const u32 lines = u32(__builtin_popcountll(nl));
            nlp[newlines] = base + tz64(nl); nl &= nl - 1;
            nlp[newlines + 1] = base + tz64(nl); nl &= nl - 1;
            for (u32 i = 2; i < lines; i++) { nlp[newlines + i] = base + tz64(nl); nl &= nl - 1; }
            newlines += lines;

            for (; bad; bad &= bad - 1) bp[faults++] = base + u32(__builtin_ctzll(bad));
        }
        for (int i = 0; i <= NF; i++) sp[n + i] = 0xFFFFFFFFu;  // so that a look past the end reads "beyond any line"
        bp[faults] = 0xFFFFFFFFu;
        u32 k = 0, b = 0;
        for (u32 j = 0; j < newlines; j++) {
            const u32 nl = nlp[j];
            if (__builtin_expect((sp[k + NF - 1] <= nl) & (sp[k + NF] > nl) & (bp[b] > nl) & !carry_bad, 1)) {
                row(sp + k, nl);
                k += NF;
            } else {
                // A fault or the wrong number of fields: decide this line exactly from its bytes.
                u32 ls[NF + 1];
                const u8* a = buf + line;
                const u8* end = buf + nl;
                int f = 0;
                bool ok = true;
                for (;;) {
                    const u8* comma = (const u8*)memchr(a, ',', size_t(end - a));
                    const u8* stop_at = comma ? comma : end;
                    if (f == NF) { ok = false; break; }
                    u32 wrapped = 0;
                    if (memchr(a, '"', size_t(stop_at - a))) {
                        if (stop_at - a < 2 || a[0] != '"' || stop_at[-1] != '"' || memchr(a + 1, '"', size_t(stop_at - a - 2))) { ok = false; break; }
                        wrapped = 1;
                    }
                    ls[f++] = u32(a - buf) + wrapped;
                    if (!comma) break;
                    a = comma + 1;
                }
                if (ok && f == NF) row(ls, nl);
                while (sp[k] <= nl) k++;
                while (bp[b] <= nl) b++;
                carry_bad = false;
            }
            line = size_t(nl) + 1;
            if (nl >= last) return;
        }
        if (block > last && !newlines) return;                  // cannot happen: `last` is a newline
        // Keep the unfinished line's starts; beyond NF + 1 the line is malformed anyway.
        carry_bad = carry_bad || b < faults;
        u32 keep = n - k;
        if (keep > u32(NF) + 2) keep = u32(NF) + 2;
        memmove(sp, sp + k, keep * sizeof(u32));
        n = keep;
    }
}

// Handlers receive the content starts and the newline; a field is quoted exactly
// when the byte before its start is a quote, and its end follows from the next start.
#define ROW_PARAMS const u32* a, u32 nlpos
#define QUOTED(i) const u32 q##i = u32(buf[a[i] - 1] == '"');
#define FIELD(i, j) const u8 *s##i = buf + a[i], *e##i = buf + a[j] - q##j - 1 - q##i;
#define FIELD_LAST(i) const u8 *s##i = buf + a[i], *e##i = buf + nlpos - q##i;
#elif defined(SS_SCAN3)
static inline u32 tz64(u64 x) {
#if defined(__BMI__)
    return u32(_tzcnt_u64(x));
#else
    return x ? u32(__builtin_ctzll(x)) : 64;
#endif
}
static inline u64 pext64(u64 value, u64 mask) {
#if defined(__BMI2__)
    return _pext_u64(value, mask);
#else
    u64 out = 0;
    for (u64 bit = 1; mask; mask &= mask - 1, bit <<= 1)
        if (value & mask & -mask) out |= bit;
    return out;
#endif
}
static inline u64 pdep64(u64 value, u64 mask) {
#if defined(__BMI2__)
    return _pdep_u64(value, mask);
#else
    u64 out = 0;
    for (u64 bit = 1; mask; mask &= mask - 1, bit <<= 1)
        if (value & bit) out |= mask & -mask;
    return out;
#endif
}

// Calls row(starts, ends) for every line that starts in [start, stop) and has
// exactly NF well-formed fields; field i is buf[starts[i] .. ends[i]) with any
// wrapping quotes already removed.
//
// Quotes are settled a whole 64-byte block at a time. With D the delimiter
// bits and Q the quote bits, a quote is an opener if it directly follows a
// delimiter and a closer if it directly precedes one. A line is well-quoted
// exactly when every quote is one or the other (not both, which would be a
// field that is a single quote) and every field's opener and closer come
// together. Those conditions are bit tests on the block; any line they flag is
// re-examined byte by byte, so the result is exact.
template <int NF, class Row>
static inline SS_INLINE void scan_rows(const u8* buf, size_t start, size_t stop, size_t last, Row&& row) {
    if (start >= stop) return;
    enum { BLOCKS = 16, CAP = BLOCKS * 64 + NF + 48 };
    // Entry 0 always describes the delimiter just before the unfinished line.
    u32 pos[CAP], fs[CAP], fe[CAP];
    u32 nl_at[BLOCKS * 64 + 1];
    u8 nl_bad[BLOCKS * 64 + 1];
    u32 n = 1;
    bool carry_bad = false;                 // the unfinished line already has a quote fault
    size_t block = start;
    Masks cur = classify(buf + block);
    u64 prev_delim = 1;                     // the byte before `start` is a newline
    u64 prev_quote = 0;
    u64 prev_open = cur.quote & 1;          // does the field after the last delimiter open with a quote
    pos[0] = u32(start - 1);
    fs[0] = u32(start) + u32(prev_open);
    fe[0] = 0;
    const Masks none{0, 0, 0};
    for (;;) {
        u32 newlines = 0;
        for (u32 done = 0; done < BLOCKS && block <= last; done++, block += 64) {
#ifndef SS_NOSCANPF
            __builtin_prefetch(buf + block + 1024);
#endif
            const Masks next = block + 64 <= last ? classify(buf + block + 64) : none;
            const u64 Q = cur.quote, D = cur.comma | cur.nl;
            const u64 next_delim = (next.comma | next.nl) & 1, next_quote = next.quote & 1;
            const u64 openers = Q & ((D << 1) | prev_delim);
            const u64 closers = Q & ((D >> 1) | (next_delim << 63));
            u64 faults = (Q & ~(openers | closers)) | (openers & closers);
            const u32 count = u32(__builtin_popcountll(D));
            // One bit per delimiter, in order: is the byte before it a closer, the byte after it an opener.
            const u64 cb = pext64((closers << 1) | prev_quote, D);
            const u64 ob = pext64((openers >> 1) | (next_quote << 63), D);
            u64 unpaired = ((ob << 1) | prev_open) ^ cb;
            if (count < 64) unpaired &= (1ULL << count) - 1;
            if (unpaired) faults |= pdep64(unpaired, D);
            if (count) prev_open = (ob >> (count - 1)) & 1;

            u64 rest = cur.nl, pending = faults;
            for (; rest; rest &= rest - 1) {
                const u64 upto = (2ULL << __builtin_ctzll(rest)) - 1;
                nl_at[newlines] = n + u32(__builtin_popcountll(D & upto)) - 1;
                nl_bad[newlines] = u8(carry_bad | ((pending & upto) != 0));
                pending &= ~upto;
                carry_bad = false;
                newlines++;
            }
            carry_bad |= pending != 0;

            u32* out = pos + n;
            const u32 base = u32(block);
            u64 bits = D;
            for (int i = 0; i < 8; i++) { out[i] = base + tz64(bits); bits &= bits - 1; }
            if (count > 8) {
                for (int i = 8; i < 16; i++) { out[i] = base + tz64(bits); bits &= bits - 1; }
                for (u32 i = 16; i < count; i++) { out[i] = base + tz64(bits); bits &= bits - 1; }
            }
#if defined(__AVX2__)
            const __m256i select = _mm256_setr_epi32(1, 2, 4, 8, 16, 32, 64, 128);
            const __m256i one = _mm256_set1_epi32(1);
            for (u32 i = 0; i < count; i += 8) {
                const __m256i p = _mm256_loadu_si256((const __m256i*)(out + i));
                const __m256i o = _mm256_cmpeq_epi32(_mm256_and_si256(_mm256_set1_epi32(int((ob >> i) & 0xFF)), select), select);
                const __m256i c = _mm256_cmpeq_epi32(_mm256_and_si256(_mm256_set1_epi32(int((cb >> i) & 0xFF)), select), select);
                _mm256_storeu_si256((__m256i*)(fs + n + i), _mm256_sub_epi32(_mm256_add_epi32(p, one), o));
                _mm256_storeu_si256((__m256i*)(fe + n + i), _mm256_add_epi32(p, c));
            }
#else
            for (u32 i = 0; i < count; i++) {
                fs[n + i] = out[i] + 1 + u32((ob >> i) & 1);
                fe[n + i] = out[i] - u32((cb >> i) & 1);
            }
#endif
            n += count;
            prev_delim = D >> 63;
            prev_quote = Q >> 63;
            cur = next;
        }
        u32 from = 1;                       // first delimiter of the line being walked
        for (u32 j = 0; j < newlines; j++) {
            const u32 at = nl_at[j];
            if (at - from == NF - 1) {
                if (!nl_bad[j]) {
                    row(fs + from - 1, fe + from);
                } else {
                    // A quote fault somewhere in this line: decide it exactly.
                    u32 ls[NF], le[NF];
                    bool ok = true;
                    for (int f = 0; f < NF; f++) {
                        const u8* a = buf + pos[from - 1 + f] + 1;
                        const u8* b = buf + pos[from + f];
                        if (memchr(a, '"', size_t(b - a))) {
                            if (b - a < 2 || a[0] != '"' || b[-1] != '"' || memchr(a + 1, '"', size_t(b - a - 2))) { ok = false; break; }
                            a++; b--;
                        }
                        ls[f] = u32(a - buf);
                        le[f] = u32(b - buf);
                    }
                    if (ok) row(ls, le);
                }
            }
            from = at + 1;
            if (pos[at] >= last) return;
        }
        if (block > last && !newlines) return;      // cannot happen: `last` is a newline
        // Keep the previous delimiter and the unfinished line's own; beyond NF the line is malformed anyway.
        u32 keep = n - (from - 1);
        if (keep > u32(NF) + 2) keep = u32(NF) + 2;
        memmove(pos, pos + from - 1, keep * sizeof(u32));
        memmove(fs, fs + from - 1, keep * sizeof(u32));
        memmove(fe, fe + from - 1, keep * sizeof(u32));
        n = keep;
    }
}

// Handlers receive finished field bounds.
#define ROW_PARAMS const u32* fs, const u32* fe
#define FIELD(i) const u8 *s##i = buf + fs[i], *e##i = buf + fe[i];
#else
#ifdef SS_SCAN2
static inline u32 tz64(u64 x) {
#if defined(__BMI__)
    return u32(_tzcnt_u64(x));
#else
    return x ? u32(__builtin_ctzll(x)) : 64;
#endif
}

// Two stages per batch of 16 blocks: first every delimiter position is written
// to an array with no data-dependent branch per delimiter (eight at a time,
// more only when a block holds more), and each newline notes how many
// delimiters and quotes came before it; then the rows are walked from that.
template <int NF, class Row>
static inline SS_INLINE void scan_rows(const u8* buf, size_t start, size_t stop, size_t last, Row&& row) {
    if (start >= stop) return;
    static const u32 BLOCKS = 16;
    u32 pos[BLOCKS * 64 + NF + 32];
    u32 nl_at[BLOCKS * 64 + 1];
    u32 nl_quotes[BLOCKS * 64 + 1];
    u32 n = 0;                          // delimiters held in pos (those of the unfinished line first)
    u32 quotes = 0;                     // quotes seen since the unfinished line began
    size_t line = start;
    size_t block = start;
    for (;;) {
        u32 newlines = 0;
        for (u32 done = 0; done < BLOCKS && block <= last; done++, block += 64) {
            Masks m = classify(buf + block);
            u64 delims = m.comma | m.nl;
            const u32 count = u32(__builtin_popcountll(delims));
            for (u64 nl = m.nl; nl; nl &= nl - 1) {
                u64 below = (2ULL << __builtin_ctzll(nl)) - 1;
                nl_at[newlines] = n + u32(__builtin_popcountll(delims & below)) - 1;
                nl_quotes[newlines] = quotes + u32(__builtin_popcountll(m.quote & below));
                newlines++;
            }
            quotes += u32(__builtin_popcountll(m.quote));
            u32* out = pos + n;
            const u32 base = u32(block);
            for (int i = 0; i < 8; i++) { out[i] = base + tz64(delims); delims &= delims - 1; }
            if (count > 8) {
                for (int i = 8; i < 16; i++) { out[i] = base + tz64(delims); delims &= delims - 1; }
                for (u32 i = 16; i < count; i++) { out[i] = base + tz64(delims); delims &= delims - 1; }
            }
            n += count;
        }
        u32 from = 0, counted = 0;
        for (u32 j = 0; j < newlines; j++) {
            const u32 at = nl_at[j];
            const size_t end = pos[at];
            if (at - from == NF - 1) row(line, pos + from, end, nl_quotes[j] - counted);
            counted = nl_quotes[j];
            line = end + 1;
            from = at + 1;
            if (end >= last) return;
        }
        if (block > last && !newlines) return;      // cannot happen: `last` is a newline
        // Keep the unfinished line's delimiters; beyond NF the line is malformed anyway.
        u32 keep = n - from;
        if (keep > u32(NF)) keep = u32(NF);
        memmove(pos, pos + from, keep * sizeof(u32));
        n = keep;
        quotes -= counted;
    }
}
#else
template <int NF, class Row>
static inline void scan_rows(const u8* buf, size_t start, size_t stop, size_t, Row&& row) {
    if (start >= stop) return;
    size_t line = start;
    size_t commas[NF];
    u32 found = 0;
    u32 quotes = 0;
    for (size_t block = start;; block += 64) {
        Masks m = classify(buf + block);
        u64 delims = m.comma | m.nl;
        u64 q = m.quote;
        while (delims) {
            u32 bit = u32(__builtin_ctzll(delims));
            delims &= delims - 1;
            size_t pos = block + bit;
            if (!((m.nl >> bit) & 1)) {
                if (found < NF - 1) commas[found] = pos;
                found++;
                continue;
            }
            u64 below = (2ULL << bit) - 1;
            u32 line_quotes = quotes + u32(__builtin_popcountll(q & below));
            q &= ~below;
            quotes = 0;
            if (found == NF - 1) row(line, commas, pos, line_quotes);
            found = 0;
            line = pos + 1;
            if (line >= stop) return;
        }
        quotes += u32(__builtin_popcountll(q));
    }
}
#endif

// A field wrapped in quotes loses them and counts 2 toward the line's quotes.
// If the count does not reach the line's total, a quote sits somewhere else.
#define UNQUOTE(s, e) \
    { u32 w = u32((e) - (s) >= 2) & u32(*(s) == '"') & u32((e)[-1] == '"'); (s) += w; (e) -= w; wrapped += 2 * w; }

// Handlers receive the raw delimiter positions and strip the quotes themselves.
#define ROW_PARAMS size_t line, const auto* commas, size_t nl, u32 quotes
#endif

// ---------------------------------------------------------------- input chunks
struct InputFile { int fd; size_t size; const u8* map; };
struct Task { u32 file; size_t begin, end; };

static int g_release_input = 0;             // SS_RELEASE: 1 drops a mapped chunk's page-table entries once it is parsed,
                                            // 2 drops a whole file's when its last chunk is parsed
static std::atomic<u32>* g_chunks_left = nullptr;   // per input file, for SS_RELEASE=2
static bool g_populate_input = false;       // SS_POPULATE: map a chunk's pages in one call before it is parsed

// Asks the kernel to fill in the page-table entries of a mapped chunk at once,
// instead of taking a fault every few pages while scanning it. Advice only.
static inline void populate_chunk(const u8* map, size_t from, size_t to) {
#ifdef __linux__
    if (!g_populate_input) return;
    const size_t page = 4096;
    from &= ~(page - 1);
    madvise((void*)(map + from), to - from, 22 /* MADV_POPULATE_READ */);
#else
    (void)map; (void)from; (void)to;
#endif
}

// The whole pages inside a finished chunk of a mapped file. The file stays
// cached; this only spreads the unmapping over the workers instead of leaving
// it all to the single-threaded exit.
static inline void release_chunk(const InputFile& f, const Task& task) {
    if (!g_release_input || !f.map) return;
    if (g_release_input == 2) {
        // One call per file: the cost of dropping mappings is mostly per call
        // (every other thread is interrupted), not per page.
        if (g_chunks_left && g_chunks_left[task.file].fetch_sub(1, std::memory_order_acq_rel) == 1)
            madvise((void*)f.map, f.size, MADV_DONTNEED);
        return;
    }
    const size_t page = 4096;
    const size_t from = (task.begin + page - 1) & ~(page - 1), to = task.end & ~(page - 1);
    if (to > from) madvise((void*)(f.map + from), to - from, MADV_DONTNEED);
}

// A thread's read buffer. data() has 64 readable bytes before and after.
struct Reader {
    u8* mem = nullptr;
    size_t cap = 0;
    static const size_t LEAD = 64;

    void reserve(size_t n) {
        if (n + 2 * LEAD + 1 <= cap) return;
        size_t want = std::max(n + 2 * LEAD + 1, cap * 2);
        u8* fresh = (u8*)malloc(want);
        if (!fresh) die("out of memory for a read buffer");
        if (mem) { memcpy(fresh, mem, cap); free(mem); }
        else memset(fresh, 0, LEAD);
        mem = fresh;
        cap = want;
    }
    u8* data() { return mem + LEAD; }

    // Loads the lines of `task`. Returns false when no line starts inside it.
    bool load(const InputFile& f, const Task& task, const u8*& buf, size_t& start, size_t& stop, size_t& last_nl) {
        size_t from = task.begin ? task.begin - 1 : 0;
        if (f.map) {
            // Mapped input: usable in place when the chunk's last line ends in a
            // newline with 64 readable bytes after it; otherwise copy as usual.
            populate_chunk(f.map, from, std::min(f.size, task.end + 256));
            const u8* last = (const u8*)memchr(f.map + task.end - 1, '\n', f.size - (task.end - 1));
            if (last && size_t(last - f.map) + 65 <= f.size) {
                buf = f.map + from;
                const u8* first = (const u8*)memchr(buf, '\n', size_t(last - buf) + 1);
                start = size_t(first - buf) + 1;
                stop = task.end - from;
                last_nl = size_t(last - buf);
                return start < stop;
            }
        }
        size_t have = 0;
        size_t slack = 4096;
        size_t need = task.end - 1 - from;                 // a '\n' is needed at or after this index
        for (;;) {
            size_t upto = std::min(f.size, task.end + slack);
            reserve(upto - from);
            while (from + have < upto) {
                ssize_t got = pread(f.fd, data() + have, upto - from - have, off_t(from + have));
                if (got < 0) { if (errno == EINTR) continue; die("pread failed"); }
                if (got == 0) die("input file shrank while reading");
                have += size_t(got);
            }
            if (const u8* found = (const u8*)memchr(data() + need, '\n', have - need)) { last_nl = size_t(found - data()); break; }
            if (from + have == f.size) { last_nl = have; data()[have++] = '\n'; break; }   // last line without a newline
            slack *= 4;
        }
        memset(data() + have, 0, LEAD);
        buf = data();
        const u8* first = (const u8*)memchr(buf, '\n', have);              // ends the header, or the line before this chunk
        if (!first) return false;
        start = size_t(first - buf) + 1;
        stop = task.end - from;
        return start < stop;
    }
};

// Zero-filled memory straight from the kernel, on 2 MB pages where it will give
// them: for tables that are probed at random, where small pages would mean a
// TLB miss on nearly every access, and so that nothing has to be cleared by hand.
template <class T>
static T* big_alloc(size_t count) {
    const size_t huge = 2u << 20;
    size_t bytes = (count * sizeof(T) + huge - 1) & ~(huge - 1);
    if (!bytes) bytes = huge;
    void* p = mmap(nullptr, bytes + huge, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) die("mmap failed for a table");
    void* aligned = (void*)((uintptr_t(p) + huge - 1) & ~(uintptr_t(huge) - 1));
#ifdef MADV_HUGEPAGE
    if (!getenv("SS_NOHUGE")) madvise(aligned, bytes, MADV_HUGEPAGE);
#endif
    return (T*)aligned;
}

// The working tables of phases 2 and 3 (dedup slots, sort keys, counters, the
// output columns) are std::vectors on this allocator: anything of a page or
// more comes from a per-thread region on 2 MB pages and is never handed back
// (a vector that grows leaves its old storage behind, at most as much again).
// On the scoring machine a small-page miss costs far more than a large-page one.
struct HugeRegion { u8* cur = nullptr; u8* end = nullptr; };
static thread_local HugeRegion t_huge;
static void* huge_take(size_t bytes) {
    bytes = (bytes + 63) & ~size_t(63);
    HugeRegion& a = t_huge;
    if (size_t(a.end - a.cur) < bytes) {
        const size_t huge = 2u << 20;
        const size_t region = (std::max<size_t>(bytes, 16u << 20) + huge - 1) & ~(huge - 1);
        a.cur = big_alloc<u8>(region);
        a.end = a.cur + region;
    }
    void* p = a.cur;
    a.cur += bytes;
    return p;
}
template <class T>
struct Huge {
    using value_type = T;
    Huge() = default;
    template <class U> Huge(const Huge<U>&) {}
    T* allocate(size_t n) {
        const size_t bytes = n * sizeof(T);
#ifndef SS_NOHUGEVEC
        if (bytes >= 4096) return (T*)huge_take(bytes);
#endif
        void* p = malloc(bytes ? bytes : 1);
        if (!p) die("out of memory");
        return (T*)p;
    }
    void deallocate(T* p, size_t n) {
#ifndef SS_NOHUGEVEC
        if (n * sizeof(T) >= 4096) return;
#endif
        free(p);
    }
    template <class U> bool operator==(const Huge<U>&) const { return true; }
    template <class U> bool operator!=(const Huge<U>&) const { return false; }
};
template <class T> using hvec = std::vector<T, Huge<T>>;

// ---------------------------------------------------------------- arena and scatter lists
static const size_t SLAB = 2u << 20;
static const size_t BLOCK = 8192;

static std::mutex g_arena_lock;
static u8* g_arena_cur = nullptr;
static u8* g_arena_end = nullptr;
static std::vector<std::pair<u8*, size_t>> g_arena_regions;
static size_t g_arena_region = 1ULL << 30;

static u8* slab_alloc() {
    std::lock_guard<std::mutex> guard(g_arena_lock);
    if (size_t(g_arena_end - g_arena_cur) < SLAB) {
        size_t len = g_arena_region;
        void* p = mmap(nullptr, len + SLAB, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS
#ifdef MAP_NORESERVE
                       | MAP_NORESERVE
#endif
                       , -1, 0);
        if (p == MAP_FAILED) die("mmap failed for the record arena");
        u8* base = (u8*)((uintptr_t(p) + SLAB - 1) & ~(uintptr_t(SLAB) - 1));
#ifdef MADV_HUGEPAGE
        if (!getenv("SS_NOHUGE")) madvise(base, len, MADV_HUGEPAGE);
#endif
        g_arena_cur = base;
        g_arena_end = base + len;
        g_arena_regions.emplace_back(base, len);
    }
    u8* out = g_arena_cur;
    g_arena_cur += SLAB;
    return out;
}

struct BlockAlloc {
    u8* cur = nullptr;
    u8* end = nullptr;
    std::vector<u8*> spare;
    void recycle(u8* block) { spare.push_back(block); }
    inline u8* get() {
        if (!spare.empty()) { u8* out = spare.back(); spare.pop_back(); return out; }
        if (cur == end) { cur = slab_alloc(); end = cur + SLAB; }
        u8* out = cur;
        cur += BLOCK;
        return out;
    }
};

struct Block { Block* next; u32 n; };

// Records headed for many buckets are staged one cache-line unit per bucket
// and moved to the bucket's block chain a whole unit at a time with streaming
// stores, so the scatter touches each destination line once and never reads it.
// Blocks are BLOCK-aligned and their header is sized so the records end exactly
// at the block's end: a cursor on a BLOCK boundary means "needs a new block".
template <class T, size_t U = SS_UNIT>
struct Scatter {
    static constexpr size_t PER = sizeof(T) == 24 ? 8 : U / sizeof(T);
    static constexpr size_t UNIT = PER * sizeof(T);
    static constexpr size_t HDR = BLOCK - ((BLOCK - 64) / UNIT) * UNIT;
    static constexpr u32 CAP = u32((BLOCK - HDR) / sizeof(T));

    u32 buckets = 0;
    u8* stage = nullptr;
    u8* fill = nullptr;
    T** cur = nullptr;
    Block** head = nullptr;
    u32* nblocks = nullptr;                 // blocks handed to each bucket
    BlockAlloc* alloc = nullptr;

    static T* items(Block* b) { return (T*)((u8*)b + HDR); }

    void init(u32 n, BlockAlloc* a) {
        buckets = n;
        alloc = a;
        // One allocation: staging units first (64-byte aligned), then the small per-bucket arrays.
        const size_t stage_bytes = size_t(n) * UNIT;
        u8* base = big_alloc<u8>(stage_bytes + size_t(n) * (1 + sizeof(T*) + sizeof(Block*) + sizeof(u32)) + 64);
        stage = base;
        cur = (T**)(base + stage_bytes);
        head = (Block**)(cur + n);
        nblocks = (u32*)(head + n);
        fill = (u8*)(nblocks + n);
    }
    inline void push(u32 b, const T& v) {
        T* unit = (T*)(stage + size_t(b) * UNIT);
        u32 f = fill[b];
        unit[f] = v;
        if (++f == PER) { flush_unit(b); f = 0; }
        fill[b] = u8(f);
    }
    T* grow(u32 b, T* c) {
        Block* fresh = (Block*)alloc->get();
        fresh->next = nullptr;
        fresh->n = CAP;
        nblocks[b]++;
        if (c) ((Block*)((uintptr_t(c) - 1) & ~(uintptr_t(BLOCK) - 1)))->next = fresh;
        else head[b] = fresh;
        return items(fresh);
    }
    inline void flush_unit(u32 b) {
        T* c = cur[b];
        if (!(uintptr_t(c) & (BLOCK - 1))) c = grow(b, c);
        const u8* src = stage + size_t(b) * UNIT;
#if defined(__AVX2__) && !defined(SS_NO_STREAM)
        for (size_t k = 0; k < UNIT; k += 32)
            _mm256_stream_si256((__m256i*)((u8*)c + k), _mm256_load_si256((const __m256i*)(src + k)));
#else
        memcpy((void*)c, src, UNIT);
#endif
        cur[b] = c + PER;
    }
    void finish() {
        for (u32 b = 0; b < buckets; b++) {
            u32 f = fill[b];
            if (f) {
                T* c = cur[b];
                if (!(uintptr_t(c) & (BLOCK - 1))) c = grow(b, c);
                memcpy((void*)c, stage + size_t(b) * UNIT, f * sizeof(T));
                cur[b] = c + f;
                fill[b] = 0;
            }
            if (cur[b]) {
                Block* tail = (Block*)((uintptr_t(cur[b]) - 1) & ~(uintptr_t(BLOCK) - 1));
                tail->n = u32(cur[b] - items(tail));
            }
        }
#if defined(__AVX2__)
        _mm_sfence();
#endif
    }
};

struct RecA { u64 ekey; u64 ukey; i64 ingest; u32 ts; u32 payload; };      // one event copy, 32 bytes
struct RecB { u64 person; u32 ts; u32 payload; };                          // one surviving event, 16 bytes
struct RecC { u64 person; u64 ckey; u32 ts; u32 camp; };                   // one valid click, 24 bytes
// The dense path's records (dense.h says what the words hold).
struct RecD { u64 a, b; };                                                 // one event copy with a decoded number, 16 bytes
struct RecK { u64 x, y; };                                                 // one valid click of a decoded user, 16 bytes

// ---------------------------------------------------------------- catalogs
// One line of the dialect split the plain way. False if the line is malformed.
static bool split_line(std::string_view line, std::vector<std::string>& fields) {
    fields.clear();
    if (line.empty()) return false;
    size_t s = 0;
    for (;;) {
        size_t c = line.find(',', s);
        std::string_view field = line.substr(s, c == std::string_view::npos ? std::string_view::npos : c - s);
        if (field.find('"') != std::string_view::npos) {
            if (field.size() < 2 || field.front() != '"' || field.back() != '"' ||
                field.substr(1, field.size() - 2).find('"') != std::string_view::npos) return false;
            field = field.substr(1, field.size() - 2);
        }
        fields.emplace_back(field);
        if (c == std::string_view::npos) return true;
        s = c + 1;
    }
}

// Small files (campaigns): the same dialect, parsed the plain way.
static std::vector<std::vector<std::string>> read_small_csv(const std::string& path, size_t width) {
    std::vector<std::vector<std::string>> rows;
    FILE* f = fopen(path.c_str(), "rb");
    if (!f) return rows;
    std::string data;
    char tmp[1 << 16];
    size_t got;
    while ((got = fread(tmp, 1, sizeof tmp, f)) > 0) data.append(tmp, got);
    fclose(f);
    size_t at = 0;
    bool header = true;
    while (at < data.size()) {
        size_t nl = data.find('\n', at);
        if (nl == std::string::npos) nl = data.size();
        std::string_view line(data.data() + at, nl - at);
        at = nl + 1;
        if (header) { header = false; continue; }
        std::vector<std::string> fields;
        if (split_line(line, fields) && fields.size() == width) rows.push_back(std::move(fields));
    }
    return rows;
}

// `prefix` then a decimal number with no leading zero -> the number, else -1.
static inline i64 canonical_number(const u8* s, const u8* e, size_t prefix) {
    size_t n = size_t(e - s);
    if (n <= prefix || n > prefix + 8) return -1;
    const u8* p = s + prefix;
    if (*p == '0' && e - p > 1) return -1;
    u32 v = 0;
    for (; p < e; p++) {
        u32 d = *p - '0';
        if (d > 9) return -1;
        v = v * 10 + d;
    }
    return v;
}

struct Catalogs {
    // products
    std::vector<std::string> categories;       // byte-wise sorted, `unknown` included
    u32 unknown = 0;
    std::vector<u8> product_dense;             // p<N> -> category + 1, 0 = not in the file
    std::unordered_map<std::string, u8> product_other;
    // campaigns
    std::vector<std::string> campaign_ids, channels;
    std::vector<i32> campaign_dense;           // c_<N> -> index, -1 = not in the file
    // IDs of up to 8 bytes, keyed by their text as one zero-padded word plus the length
    struct ShortId { u64 text; u32 len; i32 index; };
    std::vector<ShortId> campaign_short;
    int campaign_shift = 63;
    std::unordered_map<std::string, i32> campaign_other;

    void load(const std::string& dir) {
        // Products: one pass over the file, numbering categories as they first
        // appear; a repeated product ID keeps its last row. Then renumber in byte order.
        std::vector<std::string> names{"unknown"};
        auto category_id = [&](const char* p, size_t n) -> u32 {
            for (size_t i = names.size(); i-- > 0;)
                if (names[i].size() == n && memcmp(names[i].data(), p, n) == 0) return u32(i);
            if (names.size() >= 255) die("more than 255 categories");
            names.emplace_back(p, n);
            return u32(names.size() - 1);
        };
        std::vector<u32> dense;                    // provisional category + 1
        std::unordered_map<std::string, u32> other;
        auto add_product = [&](const char* id, size_t id_len, const char* cat, size_t cat_len) {
            const u8* s = (const u8*)id;
            i64 number = (id_len >= 2 && s[0] == 'p') ? canonical_number(s, s + id_len, 1) : -1;
            u32 category = category_id(cat, cat_len);
            if (number >= 0) {
                if (size_t(number) >= dense.size()) dense.resize(size_t(number) + 1 + dense.size() / 2, 0);
                dense[size_t(number)] = category + 1;
            } else {
                other[std::string(id, id_len)] = category;
            }
        };
        {
            std::string data;
            if (FILE* f = fopen((dir + "/products.csv").c_str(), "rb")) {
                char tmp[1 << 16];
                size_t got;
                while ((got = fread(tmp, 1, sizeof tmp, f)) > 0) data.append(tmp, got);
                fclose(f);
            }
            const char* p = data.data();
            const char* end = p + data.size();
            const char* header = (const char*)memchr(p, '\n', size_t(end - p));
            p = header ? header + 1 : end;
            while (p < end) {
                const char* nl = (const char*)memchr(p, '\n', size_t(end - p));
                if (!nl) nl = end;
                const char* comma = (const char*)memchr(p, ',', size_t(nl - p));
                // The plain shape `id,category` with no quote and one comma is handled here.
                if (comma && !memchr(p, '"', size_t(nl - p)) && !memchr(comma + 1, ',', size_t(nl - comma - 1))) {
                    add_product(p, size_t(comma - p), comma + 1, size_t(nl - comma - 1));
                } else {
                    std::string_view line(p, size_t(nl - p));
                    std::vector<std::string> fields;
                    if (split_line(line, fields) && fields.size() == 2)
                        add_product(fields[0].data(), fields[0].size(), fields[1].data(), fields[1].size());
                }
                p = nl + 1;
            }
        }
        if (names.size() > 255) die("more than 255 categories");
        std::vector<u32> rank(names.size());
        for (u32 i = 0; i < names.size(); i++) rank[i] = i;
        std::sort(rank.begin(), rank.end(), [&](u32 x, u32 y) { return names[x] < names[y]; });
        std::vector<u8> final_id(names.size());
        for (u32 i = 0; i < rank.size(); i++) { final_id[rank[i]] = u8(i); categories.push_back(names[rank[i]]); }
        unknown = final_id[0];
        product_dense.assign(dense.size(), 0);
        for (size_t i = 0; i < dense.size(); i++) if (dense[i]) product_dense[i] = u8(final_id[dense[i] - 1] + 1);
        for (auto& kv : other) product_other[kv.first] = final_id[kv.second];

        i64 largest = -1;
        auto campaigns = read_small_csv(dir + "/campaigns.csv", 2);
        std::unordered_map<std::string, i32> seen;
        for (auto& row : campaigns) {
            auto it = seen.find(row[0]);
            if (it != seen.end()) { channels[size_t(it->second)] = row[1]; continue; }
            seen[row[0]] = i32(campaign_ids.size());
            campaign_ids.push_back(row[0]);
            channels.push_back(row[1]);
        }
        largest = -1;
        auto number_of = [](const std::string& id) -> i64 {
            const u8* s = (const u8*)id.data();
            return (id.size() >= 3 && s[0] == 'c' && s[1] == '_') ? canonical_number(s, s + id.size(), 2) : -1;
        };
        for (auto& id : campaign_ids) largest = std::max(largest, number_of(id));
        campaign_dense.assign(size_t(largest + 1), -1);
        for (size_t i = 0; i < campaign_ids.size(); i++) {
            i64 number = number_of(campaign_ids[i]);
            if (number >= 0) campaign_dense[size_t(number)] = i32(i);
            else campaign_other[campaign_ids[i]] = i32(i);
        }
        size_t cap = 16;
        while (cap < campaign_ids.size() * 4) cap <<= 1;
        campaign_short.assign(cap, ShortId{0, 0, -1});
        campaign_shift = 64 - __builtin_ctzll(cap);
        for (size_t i = 0; i < campaign_ids.size(); i++) {
            const std::string& id = campaign_ids[i];
            if (id.empty() || id.size() > 8) continue;
            u64 text = 0;
            memcpy(&text, id.data(), id.size());
            size_t at = hash64(text ^ id.size()) >> campaign_shift;
            while (campaign_short[at].len) at = (at + 1) & (cap - 1);
            campaign_short[at] = {text, u32(id.size()), i32(i)};
        }
    }

    inline u32 category(const u8* s, const u8* e) const {
#ifndef SS_FORCE_SLOW
        if (*s == 'p') {
            i64 number = canonical_number(s, e, 1);
            if (number >= 0) {
                if (size_t(number) < product_dense.size() && product_dense[size_t(number)]) return product_dense[size_t(number)] - 1u;
                return unknown;
            }
        }
        if (product_other.empty()) return unknown;
#else
        if (*s == 'p') {
            i64 number = canonical_number(s, e, 1);
            if (number >= 0) return (size_t(number) < product_dense.size() && product_dense[size_t(number)]) ? product_dense[size_t(number)] - 1u : unknown;
        }
#endif
        auto it = product_other.find(std::string((const char*)s, size_t(e - s)));
        return it == product_other.end() ? unknown : it->second;
    }

    inline i32 campaign(const u8* s, const u8* e) const {
#if !defined(SS_FORCE_SLOW) && !defined(SS_CAMP1)
        // The usual case, an ID of 1 to 8 bytes: one masked load, one probe, no loop over its digits.
        const size_t n = size_t(e - s);
        if (n - 1 < 8) {
            const u64 text = rd64(s) & (~0ULL >> (8 * (8 - n)));
            const size_t mask = campaign_short.size() - 1;
            size_t at = hash64(text ^ n) >> campaign_shift;
            for (;;) {
                const ShortId& slot = campaign_short[at];
                if (slot.text == text && slot.len == n) return slot.index;
                if (!slot.len) return -1;
                at = (at + 1) & mask;
            }
        }
#endif
        if (e - s >= 3 && s[0] == 'c' && s[1] == '_') {
            i64 number = canonical_number(s, e, 2);
            if (number >= 0) return size_t(number) < campaign_dense.size() ? campaign_dense[size_t(number)] : -1;
        }
        if (campaign_other.empty()) return -1;
        auto it = campaign_other.find(std::string((const char*)s, size_t(e - s)));
        return it == campaign_other.end() ? -1 : it->second;
    }
};

// ---------------------------------------------------------------- linked IDs
// Maps every ID that is not its person's name to that name. An ID absent
// from the table is its own person.
static u32 g_alias_space = 300;             // SS_ASPACE: alias map slots per 100 entries

struct AliasMap {
    struct Slot { u64 key, name; };
    Slot* slots = nullptr;
    u64 cap = 16;                           // any size: a hash's slot is the high half of hash * cap
    // One bit per hash prefix, about 16 bits an entry: most IDs that are not
    // aliases are answered from this small table without touching `slots`.
    u64* filter = nullptr;
    int filter_shift = 64;

    inline u64 slot_of(u64 hash) const { return u64((unsigned __int128)hash * cap >> 64); }

    void build(const std::pair<u64, u64>* entries, size_t count) {
        cap = std::max<u64>(16, u64(count) * std::max<u32>(g_alias_space, 110) / 100);
        slots = big_alloc<Slot>(cap + 1);
        size_t bits = 1 << 16;
        while (bits < count * 16) bits <<= 1;
        filter = big_alloc<u64>(bits / 64);
        filter_shift = 64 - __builtin_ctzll(bits);
        // Inserts land all over a table far larger than the cache, so the
        // slots are requested a batch ahead of being written.
        const size_t AHEAD = 32;
        for (size_t i = 0; i < count; i++) {
            if (i + AHEAD < count) {
                u64 ahead = hash64(entries[i + AHEAD].first);
                __builtin_prefetch(&slots[slot_of(ahead)], 1);
                __builtin_prefetch(&filter[(ahead >> filter_shift) >> 6], 1);
            }
            u64 h = hash64(entries[i].first);
            u64 at = slot_of(h);
            while (slots[at].key) at = at + 1 == cap ? 0 : at + 1;
            slots[at] = {entries[i].first, entries[i].second};
            u64 bit = h >> filter_shift;
            filter[bit >> 6] |= 1ULL << (bit & 63);
        }
    }
    inline void prefetch_filter(u64 hash) const { __builtin_prefetch(&filter[(hash >> filter_shift) >> 6]); }
    // False means the key is certainly not an alias.
    inline bool maybe(u64 hash) const {
        u64 bit = hash >> filter_shift;
        return (filter[bit >> 6] >> (bit & 63)) & 1;
    }
    inline void prefetch_hash(u64 hash) const { __builtin_prefetch(&slots[slot_of(hash)]); }
    // IDs whose hash starts with the `bits`-bit number `bucket` all sit in one
    // stretch of the table and of the filter. Asking for the whole stretch up
    // front lets the lookups of a bucket's IDs find it in cache.
    void prefetch_bucket(u32 bucket, u32 bits) const {
#ifndef SS_NOSLICEPF
        const int filter_bits = 64 - filter_shift;
        {
            const u64 first = slot_of(u64(bucket) << (64 - bits));
            const u64 bytes = std::min<u64>(((cap >> bits) + 1) * sizeof(Slot) + 256, 1u << 20);
            const char* base = (const char*)&slots[first];
            const char* limit = (const char*)(slots + cap);
            for (u64 off = 0; off < bytes && base + off < limit; off += 64) __builtin_prefetch(base + off);
        }
        if (filter_bits >= int(bits) + 6) {
            const u64 first = (u64(bucket) << (filter_bits - int(bits))) >> 6;
            const u64 words = std::min<u64>(1ULL << (filter_bits - int(bits) - 6), 1u << 14);
            for (u64 w = 0; w < words; w += 8) __builtin_prefetch(&filter[first + w]);
        }
#else
        (void)bucket; (void)bits;
#endif
    }
    // True when `key` is in the map; `name` is then its person's name.
    inline bool find(u64 key, u64 hash, u64& name) const {
        if (!maybe(hash)) return false;
        u64 at = slot_of(hash);
        for (;;) {
            u64 k = slots[at].key;
            if (k == key) { name = slots[at].name; return true; }
            if (!k) return false;
            at = at + 1 == cap ? 0 : at + 1;
        }
    }
    inline u64 person_hashed(u64 key, u64 hash) const {
        u64 at = slot_of(hash);
        for (;;) {
            u64 k = slots[at].key;
            if (k == key) return slots[at].name;
            if (!k) return key;
            at = at + 1 == cap ? 0 : at + 1;
        }
    }
    inline void prefetch(u64 key) const { __builtin_prefetch(&slots[slot_of(hash64(key))]); }
    inline u64 person(u64 key) const {
        u64 hash = hash64(key);
        return maybe(hash) ? person_hashed(key, hash) : key;
    }
};

// Resolves user IDs to people a fixed distance behind where they were queued:
// the filter word is requested when an item arrives, tested DIST items later,
// and for a likely alias the table slot is requested then and read DIST items
// after that. Both memory accesses are thus under way long before they are needed.
template <class T, class Emit>
struct Resolver {
#ifndef SS_DIST
#define SS_DIST 16
#endif
    static const u32 DIST = SS_DIST, SIZE = SS_DIST < 48 ? 64 : 256;    // SIZE is a power of two above DIST
    const AliasMap& alias;
    Emit emit;                                       // emit(person, hash64(person), item)
    T a_item[SIZE]; u64 a_hash[SIZE]; u64 a_key[SIZE];
    T b_item[SIZE]; u64 b_hash[SIZE]; u64 b_key[SIZE];
    u32 a_in = 0, a_out = 0, b_in = 0, b_out = 0;

    Resolver(const AliasMap& map, Emit e) : alias(map), emit(e) {}
    inline void step_b() {
        u32 i = b_out++ & (SIZE - 1);
        const u64 person = alias.person_hashed(b_key[i], b_hash[i]);
        emit(person, person == b_key[i] ? b_hash[i] : hash64(person), b_item[i]);
    }
    inline void step_a() {
        u32 i = a_out++ & (SIZE - 1);
        if (!alias.maybe(a_hash[i])) { emit(a_key[i], a_hash[i], a_item[i]); return; }     // not an alias: the ID is the person
        alias.prefetch_hash(a_hash[i]);
        u32 j = b_in++ & (SIZE - 1);
        b_item[j] = a_item[i]; b_hash[j] = a_hash[i]; b_key[j] = a_key[i];
        if (b_in - b_out > DIST) step_b();
    }
#ifdef SS_NOFILTER
    // Variant: no filter. The table slot is requested on arrival and read DIST items later.
    inline void step_direct() {
        u32 i = a_out++ & (SIZE - 1);
        const u64 person = alias.person_hashed(a_key[i], a_hash[i]);
        emit(person, person == a_key[i] ? a_hash[i] : hash64(person), a_item[i]);
    }
    inline void push(u64 key, const T& item) {
        const u64 hash = hash64(key);
        alias.prefetch_hash(hash);
        u32 i = a_in++ & (SIZE - 1);
        a_item[i] = item; a_hash[i] = hash; a_key[i] = key;
        if (a_in - a_out > DIST) step_direct();
    }
    void finish() {
        while (a_out != a_in) step_direct();
    }
#else
    inline void push(u64 key, const T& item) {
        const u64 hash = hash64(key);
        alias.prefetch_filter(hash);
        u32 i = a_in++ & (SIZE - 1);
        a_item[i] = item; a_hash[i] = hash; a_key[i] = key;
        if (a_in - a_out > DIST) step_a();
    }
    void finish() {
        while (a_out != a_in) step_a();
        while (b_out != b_in) step_b();
    }
#endif
};

struct Links {
    std::vector<std::pair<u64, u64>> pairs;
    double solved_at = 0;

    void add(u64 a, u64 b) { pairs.emplace_back(a, b); }

    void solve(AliasMap& out) {
        size_t cap = 16;
        while (cap < pairs.size() * 4) cap <<= 1;
        struct Slot { u64 key; u32 index; u32 used; };
        struct Node { u64 key; u32 parent; u32 pad; };      // an ID and its union-find parent, on one cache line
        Slot* table = big_alloc<Slot>(cap);                  // zero-filled: used == 0 means empty
        Node* nodes = big_alloc<Node>(pairs.size() * 2 + 1);
        u32 nnodes = 0;
        const u64 mask = cap - 1;
        const int top = 64 - __builtin_ctzll(cap);
        auto index_of = [&](u64 key) -> u32 {
            u64 at = hash64(key) >> top;
            for (;;) {
                Slot& slot = table[at];
                if (!slot.used) {
                    slot = {key, nnodes, 1};
                    nodes[nnodes] = {key, nnodes, 0};
                    return nnodes++;
                }
                if (slot.key == key) return slot.index;
                at = (at + 1) & mask;
            }
        };
        auto find = [&](u32 x) -> u32 {
            while (nodes[x].parent != x) { nodes[x].parent = nodes[nodes[x].parent].parent; x = nodes[x].parent; }
            return x;
        };
        // Three steps a fixed distance apart, so the table slots and the nodes
        // are already on their way when they are needed: request the slots of
        // pair i + 2*GAP, number the IDs of pair i + GAP and request their
        // nodes, join pair i.
        const size_t GAP = 12, total = pairs.size();
        std::pair<u32, u32>* numbered = big_alloc<std::pair<u32, u32>>(total + 1);
        for (size_t i = 0; i < total + GAP; i++) {
            if (i + GAP < total) {
                __builtin_prefetch(&table[hash64(pairs[i + GAP].first) >> top], 1);
                __builtin_prefetch(&table[hash64(pairs[i + GAP].second) >> top], 1);
            }
            if (i < total) {
                const u32 a = index_of(pairs[i].first), b = index_of(pairs[i].second);
                numbered[i] = {a, b};
                __builtin_prefetch(&nodes[a], 1);
                __builtin_prefetch(&nodes[b], 1);
            }
            if (i >= GAP) {
                u32 a = find(numbered[i - GAP].first), b = find(numbered[i - GAP].second);
                if (a == b) continue;
                // The root is always the byte-wise smallest ID, so it is the person's name.
                if (id_less(nodes[b].key, nodes[a].key, 'u')) std::swap(a, b);
                nodes[b].parent = a;
            }
        }
        solved_at = now();
        std::pair<u64, u64>* entries = big_alloc<std::pair<u64, u64>>(nnodes + 1);
        size_t nentries = 0;
#ifdef SS_ALIAS1
        for (u32 i = 0; i < nnodes; i++) {
            u32 root = find(i);
            if (root != i) entries[nentries++] = {nodes[i].key, nodes[root].key};
        }
#else
        // Every ID of a person with more than one ID, the name included (it maps
        // to itself): the people phase asks "is this one of several linked IDs?".
        for (u32 i = 0; i < nnodes; i++) {
            u32 root = find(i);
            if (root != i) nodes[root].pad = 1;
        }
        for (u32 i = 0; i < nnodes; i++) {
            u32 root = nodes[i].parent == i ? i : find(i);
            if (root != i || nodes[i].pad) entries[nentries++] = {nodes[i].key, nodes[root].key};
        }
#endif
        out.build(entries, nentries);
    }
};

// ---------------------------------------------------------------- shared state
struct Config {
    std::string data, out;
    u32 threads = 8;
    size_t chunk = 256 << 10;
    // 1: map an input file and parse it in place when it lives in memory (tmpfs);
    // files on disk are read into buffers, which copes far better with a cold cache.
    // 0: always read. 2: always map (tests).
    int use_mmap = 1;
};

struct Shared {
    Config cfg;
    Catalogs cat;
    i64 ws = 0, we = 0;                     // event window, epoch ms
    i64 day0 = 0;                           // first session day (days since the epoch)
    i64 day_base = 0;                       // that day's midnight, epoch ms
    std::vector<u64> day_text8;             // `YYYY-MM-DD` of each window day: first 8 bytes
    std::vector<u16> day_text2;             //   and the last 2
    u32 ndays = 0;
    u32 abits = 6, bbits = 6;               // log2 of event buckets and person buckets
    std::vector<InputFile> files;
    std::vector<Task> event_tasks, click_tasks, link_tasks;
    std::atomic<size_t> next_event{0}, next_click{0}, next_bucket{0}, next_person{0};
    std::atomic<bool> links_ready{false};
    AliasMap alias;
    // one per thread
    std::vector<Scatter<RecA>> a;
    std::vector<Scatter<RecB>> b;
    std::vector<Scatter<RecC>> c;
    // Events and clicks of people with several linked IDs, by the person's name:
    // filled by the first round of the people phase, consumed by the second.
    std::vector<Scatter<RecB>> lb;
    std::vector<Scatter<RecC>> lc;
    // The dense path: event copies by event number, clicks by person number.
    std::vector<Scatter<RecD, 64>> d;
    std::vector<Scatter<RecK, 64>> k;
    Links odd_links;                        // links the dense path leaves to the general one
    u32 part_base = 0;                      // first output file and totals slot of the general people phase
    std::vector<BlockAlloc> alloc;
    std::vector<u32> person_order;
    // per thread, indexed campaign * ndays + day
    struct Totals { hvec<u32> sessions, assisted, purchasing; hvec<i64> revenue; };
    std::vector<Totals> campaign_totals;
    std::atomic<u64> user_rows{0};
    double links_seconds = 0, links_parse = 0, links_union = 0;
};

static void list_csv(Shared& sh, const std::string& folder, std::vector<Task>& tasks) {
    std::string dir = sh.cfg.data + "/" + folder;
    DIR* d = opendir(dir.c_str());
    if (!d) return;
    while (dirent* entry = readdir(d)) {
        std::string name = entry->d_name;
        if (name.size() < 4 || name[0] == '.' || name.compare(name.size() - 4, 4, ".csv") != 0) continue;
        std::string path = dir + "/" + name;
        struct stat st;
        if (stat(path.c_str(), &st) != 0 || !S_ISREG(st.st_mode) || st.st_size == 0) continue;
        int fd = open(path.c_str(), O_RDONLY);
        if (fd < 0) die("cannot open an input file");
        u32 index = u32(sh.files.size());
        const u8* map = nullptr;
        bool in_memory = sh.cfg.use_mmap == 2;
#ifdef __linux__
        struct statfs fs;
        if (sh.cfg.use_mmap == 1 && fstatfs(fd, &fs) == 0)
            in_memory = fs.f_type == 0x01021994 || fs.f_type == 0x858458f6;      // tmpfs, ramfs
#endif
        if (in_memory) {
            void* p = mmap(nullptr, size_t(st.st_size), PROT_READ, MAP_PRIVATE, fd, 0);
            if (p != MAP_FAILED) map = (const u8*)p;
        }
        sh.files.push_back({fd, size_t(st.st_size), map});
        for (size_t at = 0; at < size_t(st.st_size); at += sh.cfg.chunk)
            tasks.push_back({index, at, std::min(size_t(st.st_size), at + sh.cfg.chunk)});
    }
    closedir(d);
}

#include "tokenizer.hpp"

// ---------------------------------------------------------------- phase 1: parse
static void parse_links(Shared& sh, Reader& reader) {
    const double began = now();
    Links links;
    links.pairs.reserve(1 << 16);
    for (const Task& task : sh.link_tasks) {
        const u8* buf; size_t start, stop, last;
        if (!reader.load(sh.files[task.file], task, buf, start, stop, last)) { release_chunk(sh.files[task.file], task); continue; }
        scan_rows<2>(buf, start, stop, last, [&](ROW_PARAMS) SS_INLINE {
#if defined(SS_SCAN4)
            QUOTED(0) QUOTED(1)
            FIELD(0, 1) FIELD_LAST(1)
#elif defined(SS_SCAN3)
            FIELD(0) FIELD(1)
#else
            const u8 *s0 = buf + line, *e0 = buf + commas[0], *s1 = e0 + 1, *e1 = buf + nl;
            u32 wrapped = 0;
            UNQUOTE(s0, e0) UNQUOTE(s1, e1)
            if (wrapped != quotes) return;
#endif
            if (s0 == e0 || s1 == e1) return;
            links.add(pack_id(s0, size_t(e0 - s0), 'u'), pack_id(s1, size_t(e1 - s1), 'u'));
        });
        release_chunk(sh.files[task.file], task);
    }
    const double parsed = now();
    links.solve(sh.alias);
    sh.links_seconds = now() - began;
    sh.links_parse = parsed - began;
    sh.links_union = links.solved_at - parsed;
    sh.links_ready.store(true, std::memory_order_release);
}

static void parse_events(Shared& sh, u32 tid, Reader& reader) {
    const u32 VIEW = rd32((const u8*)"view"), CART = rd32((const u8*)"cart");
    const u64 PURCHASE = rd64((const u8*)"purchase");
    const i64 ws = sh.ws, we = sh.we;
    const int shift = 64 - int(sh.abits);
    const Catalogs& cat = sh.cat;
    Scatter<RecA>& lists = sh.a[tid];
    DateCache dates;
    u64 sink = 0;
    enum { WAIT = 8 };
    struct EventWait { RecA rec; const u8* id; u64 hash; u32 len; };
    EventWait waiting[WAIT];
    for (EventWait& w : waiting) w.id = nullptr;
    u32 wait_at = 0;
    for (;;) {
        size_t at = sh.next_event.fetch_add(1, std::memory_order_relaxed);
        if (at >= sh.event_tasks.size()) break;
        const Task& task = sh.event_tasks[at];
        const u8* buf; size_t start, stop, last;
        if (!reader.load(sh.files[task.file], task, buf, start, stop, last)) { release_chunk(sh.files[task.file], task); continue; }
#if SS_PROBE == 1 || SS_LVL == 1
        continue;
#endif
#if !defined(SS_FORCE_SLOW) && !defined(SS_BLOCKSCAN)
        // One line at a time (lines.h); a line the reader does not take is split byte by byte.
        line_rows<7>(buf, start, stop, last, line7w, [&](const Fields<7>& f) SS_INLINE {
            const u8 *s0 = f.s[0], *e0 = f.e[0], *s1 = f.s[1], *e1 = f.e[1], *s2 = f.s[2], *e2 = f.e[2], *s3 = f.s[3], *e3 = f.e[3];
            const u8 *s4 = f.s[4], *e4 = f.e[4], *s5 = f.s[5], *e5 = f.e[5], *s6 = f.s[6], *e6 = f.e[6];
#else
        scan_rows<7>(buf, start, stop, last, [&](ROW_PARAMS) SS_INLINE {
#endif
#if !defined(SS_FORCE_SLOW) && !defined(SS_BLOCKSCAN)
#elif defined(SS_SCAN4)
#if SS_LVL == 2
            sink += a[0] + a[6] + nlpos; return;
#endif
            QUOTED(0) QUOTED(1) QUOTED(2) QUOTED(3) QUOTED(4) QUOTED(5) QUOTED(6)
            FIELD(0, 1) FIELD(1, 2) FIELD(2, 3) FIELD(3, 4) FIELD(4, 5) FIELD(5, 6) FIELD_LAST(6)
#elif defined(SS_SCAN3)
#if SS_LVL == 2
            sink += fs[0] + fe[6]; return;
#endif
            FIELD(0) FIELD(1) FIELD(2) FIELD(3) FIELD(4) FIELD(5) FIELD(6)
#else
#if SS_LVL == 2
            sink += commas[0] + commas[5] + nl + quotes + line; return;
#endif
            const u8 *s0 = buf + line, *e0 = buf + commas[0];
            const u8 *s1 = e0 + 1, *e1 = buf + commas[1];
            const u8 *s2 = e1 + 1, *e2 = buf + commas[2];
            const u8 *s3 = e2 + 1, *e3 = buf + commas[3];
            const u8 *s4 = e3 + 1, *e4 = buf + commas[4];
            const u8 *s5 = e4 + 1, *e5 = buf + commas[5];
            const u8 *s6 = e5 + 1, *e6 = buf + nl;
            u32 wrapped = 0;
            UNQUOTE(s0, e0) UNQUOTE(s1, e1) UNQUOTE(s2, e2) UNQUOTE(s3, e3) UNQUOTE(s4, e4) UNQUOTE(s5, e5) UNQUOTE(s6, e6)
            if (wrapped != quotes) return;                      // a quote somewhere it may not be
#endif
            if (s0 == e0) return;                               // no event_id
#if SS_LVL == 3
            sink += size_t(e0 - s0) + size_t(e1 - s1) + size_t(e2 - s2) + size_t(e3 - s3) + size_t(e4 - s4) + size_t(e5 - s5) + size_t(e6 - s6); return;
#endif

            RecA rec;
            rec.ekey = pack_id(s0, size_t(e0 - s0), 'e');
#if SS_LVL == 4
            sink += rec.ekey + size_t(e6 - s6); return;
#endif
#ifdef SS_ING2
            // Only two things are needed from ingest_ts: whether the event is late, and
            // an order among copies of one event. So it is kept as a sortable key,
            // (days since the epoch) * 2^24 + a time-of-day code. Nearly always it
            // carries the event's own UTC date: then its date text equals that day's
            // text, the event cannot be late, and the code comes straight from the digits.
            rec.ingest = INT64_MAX;                 // rules.md: always valid F1; if not, the copy can never be kept
            rec.ukey = 0;
            rec.ts = DEAD;
            rec.payload = 0;
            const i64 ts = parse_ts(s2, size_t(e2 - s2), dates);
            const bool in_window = ts != TS_BAD && ts >= ws && ts < we;
            bool timely = false;                    // ingest_ts valid and not over 24 h after ts
            bool settled = false;
#ifndef SS_FORCE_SLOW
            if (in_window && e3 - s3 == 20) {
                const u32 d = u32(u64(ts - sh.day_base) / u64(DAY));
                if (u32(rd64(s3) == sh.day_text8[d]) & u32(rd16(s3 + 8) == sh.day_text2[d]) & u32(s3[10] == 'T') & u32(s3[19] == 'Z')) {
                    const i64 code = tod_code(s3 + 11);
                    if (code >= 0) { rec.ingest = (sh.day0 + d) * (1LL << 24) + code; timely = true; }
                    settled = true;
                }
            }
#endif
            if (!settled) {
                const i64 ingest = (e3 - s3 == 20) ? parse_f1(s3, dates) : TS_BAD;
                if (ingest != TS_BAD) {
                    rec.ingest = floor_div(ingest, DAY) * (1LL << 24) + tod_code(s3 + 11);
                    timely = in_window && ingest - ts <= DAY;
                }
            }
#if SS_LVL == 5 || SS_LVL == 6
            sink += rec.ekey + u64(rec.ingest) + u64(ts) + size_t(e6 - s6); return;
#endif
            if (timely && s1 != e1 && s5 != e5) {
                {
#else
            // rules.md: ingest_ts is always valid F1. If it is not, the copy can never be kept.
            i64 ingest = (e3 - s3 == 20) ? parse_f1(s3, dates) : TS_BAD;
            rec.ingest = ingest == TS_BAD ? INT64_MAX : ingest;
            rec.ukey = 0;
            rec.ts = DEAD;
            rec.payload = 0;
#if SS_LVL == 5
            sink += rec.ekey + u64(rec.ingest) + size_t(e6 - s6); return;
#endif
            if (ingest != TS_BAD && s1 != e1 && s5 != e5) {
                i64 ts = parse_ts(s2, size_t(e2 - s2), dates);
#if SS_LVL == 6
                sink += rec.ekey + u64(rec.ingest) + u64(ts) + size_t(e6 - s6); return;
#endif
                if (ts != TS_BAD && ts >= ws && ts < we && ingest - ts <= DAY) {
#endif
                    size_t kind = size_t(e4 - s4);
                    bool ok = false;
                    if (kind == 4) {
                        u32 word = rd32(s4);
                        ok = (word == VIEW) | (word == CART);
                    } else if (kind == 8 && rd64(s4) == PURCHASE) {
                        const u8* p = s6;
                        while (p < e6 && *p == '0') p++;
                        u32 price = 0;
                        bool digits = e6 > s6 && e6 - p <= 8;
                        for (; digits && p < e6; p++) {
                            u32 d = *p - '0';
                            if (d > 9) digits = false;
                            price = price * 10 + d;
                        }
                        if (digits && price >= 1 && price <= 10'000'000) {
                            ok = true;
                            rec.payload = (price << 8) | cat.category(s5, e5);
                        }
                    }
                    if (ok) {
                        rec.ts = u32(ts - ws);
#ifdef SS_NOWAIT                     // measurement: every long ID looked up on the spot
                        rec.ukey = pack_id(s1, size_t(e1 - s1), 'u');
                        if (false) {
#else
                        if (!pack_short(s1, size_t(e1 - s1), 'u', rec.ukey)) {
#endif
                            // A long user ID: its table slot is asked for now and read
                            // a few rows on, when this row takes the place of an older one.
                            const u64 h = hash_bytes(s1, size_t(e1 - s1));
                            intern_prefetch(h);
                            EventWait& w = waiting[wait_at++ & (WAIT - 1)];
                            if (w.id) { w.rec.ukey = intern_hashed(w.hash, w.id, w.len); lists.push(u32(hash64(w.rec.ekey) >> shift), w.rec); }
                            w.rec = rec; w.id = s1; w.len = u32(e1 - s1); w.hash = h;
                            return;
                        }
                    }
                }
            }
#if SS_PROBE == 3 || SS_LVL == 7
            sink += rec.ekey + rec.ukey + rec.ts + rec.payload + u64(rec.ingest); return;
#endif
            lists.push(u32(hash64(rec.ekey) >> shift), rec);
        });
        for (EventWait& w : waiting)                            // the rows still waiting point into this chunk
            if (w.id) { w.rec.ukey = intern_hashed(w.hash, w.id, w.len); lists.push(u32(hash64(w.rec.ekey) >> shift), w.rec); w.id = nullptr; }
        release_chunk(sh.files[task.file], task);
    }
    if (sink == 1) fprintf(stderr, "sink\n");
}

static void parse_clicks(Shared& sh, u32 tid, Reader& reader) {
#if SS_PROBE == 5
    return;
#endif
    const i64 lo = sh.ws - REACH, hi = sh.we;
    const int shift = 64 - int(sh.bbits);
    const Catalogs& cat = sh.cat;
    Scatter<RecC>& lists = sh.c[tid];
    DateCache dates;
    struct Pending { u64 ckey; u32 ts; u32 camp; };
    auto emit = [&lists, shift](u64 person, u64 hash, const Pending& click) {
        lists.push(u32(hash >> shift), RecC{person, click.ckey, click.ts, click.camp});
    };
#ifdef SS_ALIAS1
    while (!sh.links_ready.load(std::memory_order_acquire)) std::this_thread::yield();
    const AliasMap& alias = sh.alias;
    Resolver<Pending, decltype(emit)> resolver(alias, emit);
#endif
    [[maybe_unused]] u64 sink = 0;
    // Where a click goes once both its IDs have keys.
    auto deliver = [&](u64 who, u64 ckey, u32 ts32, u32 camp) SS_INLINE {
#ifdef SS_ALIAS1
        resolver.push(who, Pending{ckey, ts32, camp});
#else
        // The click goes to its user ID's bucket as it is; whose it is gets
        // settled there, once for each ID (see process_people).
        emit(who, hash64(who), Pending{ckey, ts32, camp});
#endif
    };
    enum { WAIT = 8 };
    struct ClickWait { u64 who, ckey, user_hash, click_hash; const u8 *user, *click; u32 user_len, click_len, ts, camp; bool live; };
    ClickWait waiting[WAIT];
    for (ClickWait& w : waiting) w.live = false;
    u32 wait_at = 0;
    auto settle = [&](ClickWait& w) {
        if (w.user) w.who = intern_hashed(w.user_hash, w.user, w.user_len);
        if (w.click) w.ckey = intern_hashed(w.click_hash, w.click, w.click_len);
        w.live = false;
        deliver(w.who, w.ckey, w.ts, w.camp);
    };
    for (;;) {
        size_t at = sh.next_click.fetch_add(1, std::memory_order_relaxed);
        if (at >= sh.click_tasks.size()) break;
        const Task& task = sh.click_tasks[at];
        const u8* buf; size_t start, stop, last;
        if (!reader.load(sh.files[task.file], task, buf, start, stop, last)) { release_chunk(sh.files[task.file], task); continue; }
#if !defined(SS_FORCE_SLOW) && !defined(SS_BLOCKSCAN)
        line_rows<4>(buf, start, stop, last, line4w, [&](const Fields<4>& f) SS_INLINE {
            const u8 *s0 = f.s[0], *e0 = f.e[0], *s1 = f.s[1], *e1 = f.e[1], *s2 = f.s[2], *e2 = f.e[2], *s3 = f.s[3], *e3 = f.e[3];
#else
        scan_rows<4>(buf, start, stop, last, [&](ROW_PARAMS) SS_INLINE {
#endif
#if !defined(SS_FORCE_SLOW) && !defined(SS_BLOCKSCAN)
#elif defined(SS_SCAN4)
#if SS_CLVL == 2
            sink += a[0] + a[3] + nlpos; return;
#endif
            QUOTED(0) QUOTED(1) QUOTED(2) QUOTED(3)
            FIELD(0, 1) FIELD(1, 2) FIELD(2, 3) FIELD_LAST(3)
#elif defined(SS_SCAN3)
            FIELD(0) FIELD(1) FIELD(2) FIELD(3)
#else
            const u8 *s0 = buf + line, *e0 = buf + commas[0];
            const u8 *s1 = e0 + 1, *e1 = buf + commas[1];
            const u8 *s2 = e1 + 1, *e2 = buf + commas[2];
            const u8 *s3 = e2 + 1, *e3 = buf + nl;
            u32 wrapped = 0;
            UNQUOTE(s0, e0) UNQUOTE(s1, e1) UNQUOTE(s2, e2) UNQUOTE(s3, e3)
            if (wrapped != quotes) return;
#endif
            if (s0 == e0 || s1 == e1) return;
            i64 ts = parse_ts(s3, size_t(e3 - s3), dates);
            if (ts == TS_BAD || ts < lo || ts >= hi) return;
#if SS_CLVL == 3
            sink += u64(ts); return;
#endif
            i32 campaign = cat.campaign(s2, e2);
            if (campaign < 0) return;
#if SS_CLVL == 4
            sink += u64(ts) + u64(campaign); return;
#elif SS_CLVL == 5
            sink += u64(ts) + u64(campaign) + pack_id(s1, size_t(e1 - s1), 'u') + pack_id(s0, size_t(e0 - s0), 'k'); return;
#elif SS_CLVL == 6
            { const u64 who = pack_id(s1, size_t(e1 - s1), 'u');       // no alias lookup: the ID stands for the person
              emit(who, hash64(who), Pending{pack_id(s0, size_t(e0 - s0), 'k'), u32(ts - lo), u32(campaign)}); return; }
#endif
            // Copies of a click are identical (rules.md), so they change neither
            // the credited campaign nor the set of assisting campaigns: no dedup.
            u64 who, ckey;
#ifdef SS_NOWAIT
            who = pack_id(s1, size_t(e1 - s1), 'u'); ckey = pack_id(s0, size_t(e0 - s0), 'k');
            const bool short_user = true, short_click = true;
#else
            const bool short_user = pack_short(s1, size_t(e1 - s1), 'u', who), short_click = pack_short(s0, size_t(e0 - s0), 'k', ckey);
#endif
            if (!(short_user & short_click)) {
                // A long ID or two: the slots are asked for now, the row waits a few rows.
                ClickWait& w = waiting[wait_at++ & (WAIT - 1)];
                if (w.live) settle(w);
                w.live = true; w.who = who; w.ckey = ckey; w.ts = u32(ts - lo); w.camp = u32(campaign);
                w.user = short_user ? nullptr : s1; w.click = short_click ? nullptr : s0;
                w.user_len = u32(e1 - s1); w.click_len = u32(e0 - s0);
                if (!short_user) { w.user_hash = hash_bytes(s1, size_t(e1 - s1)); intern_prefetch(w.user_hash); }
                if (!short_click) { w.click_hash = hash_bytes(s0, size_t(e0 - s0)); intern_prefetch(w.click_hash); }
                return;
            }
            deliver(who, ckey, u32(ts - lo), u32(campaign));
        });
        for (ClickWait& w : waiting) if (w.live) settle(w);     // the rows still waiting point into this chunk
        release_chunk(sh.files[task.file], task);
    }
#ifdef SS_ALIAS1
    resolver.finish();
#endif
    if (sink == 1) fprintf(stderr, "sink\n");
}

// Record blocks are 8 KB pieces strung into lists, so the hardware prefetcher
// starts cold at every block. Asking for the first lines of the next block
// while the current one is processed covers the gap.
static inline void prefetch_block(const void* block) {
#ifndef SS_NOBLOCKPF
    if (!block) return;
    for (int k = 0; k < 4; k++) __builtin_prefetch((const char*)block + 64 * k);
#else
    (void)block;
#endif
}

// Where the time inside phases 2 and 3 goes: CPU seconds per step, per thread.
// Two clock reads per step per bucket; printed with the other figures on stderr.
static double g_steps[256][12];

// ---------------------------------------------------------------- phase 2: dedup
static void dedup_buckets(Shared& sh, u32 tid) {
    double* steps = g_steps[tid & 255];
    const u32 nbuckets = 1u << sh.abits;
    const u32 abits = sh.abits;
    const int shift = 64 - int(sh.bbits);
    Scatter<RecB>& out = sh.b[tid];
    const bool dense_on = g_dense_on;
    std::vector<Block*> blocks;
    hvec<u64> table;                        // wide slots: (hash tag << 32) | reference
    hvec<u32> small;                        // narrow slots: (hash tag << 16) | reference
#ifdef SS_ALIAS1
    const AliasMap& alias = sh.alias;
    struct Survivor { u32 ts; u32 payload; };
    auto emit = [&out, shift](u64 person, u64 hash, const Survivor& event) {
        out.push(u32(hash >> shift), RecB{person, event.ts, event.payload});
    };
    Resolver<Survivor, decltype(emit)> resolver(alias, emit);
#endif
    for (;;) {
        u32 bucket = u32(sh.next_bucket.fetch_add(1, std::memory_order_relaxed));
        if (bucket >= nbuckets) break;
        blocks.clear();
        size_t total = 0;
        for (u32 t = 0; t < sh.cfg.threads; t++)
            for (Block* b = sh.a[t].head[bucket]; b; b = b->next) { blocks.push_back(b); total += b->n; }
        if (!total) continue;
        const double step0 = now();
        // Smallest ingest_ts wins; equal ingest_ts means identical copies.
        // A slot is a hash tag plus (block << 8 | index) + 1. With at most 256
        // blocks the reference fits 16 bits and the whole slot 4 bytes.
#if SS_X == 7
        if (false) {
#else
        if (blocks.size() <= 256) {
#endif
            size_t cap = 16;
            while (cap * 2 < total * 3) cap <<= 1;
            small.assign(cap, 0);
            const u64 mask = cap - 1;
            const int index_shift = 64 - __builtin_ctzll(cap), tag_shift = int(abits) + __builtin_ctzll(cap);
            for (u32 bi = 0; bi < blocks.size(); bi++) {
                RecA* recs = Scatter<RecA>::items(blocks[bi]);
                if (bi + 1 < blocks.size()) prefetch_block(blocks[bi + 1]);
                for (u32 i = 0; i < blocks[bi]->n; i++) {
                    RecA& rec = recs[i];
                    const u64 m = hash64(rec.ekey);
                    const u32 mine = (u32((m << tag_shift) >> 48) << 16) | ((bi << 8 | i) + 1);
                    u64 at = (m << abits) >> index_shift;
                    for (;;) {
                        u32 slot = small[at];
                        if (!slot) { small[at] = mine; break; }
                        if ((slot ^ mine) >> 16 == 0) {
                            u32 ref = (slot & 0xFFFF) - 1;
                            RecA& best = Scatter<RecA>::items(blocks[ref >> 8])[ref & 255];
                            if (best.ekey == rec.ekey) {
                                if (rec.ingest < best.ingest) { best.ts = DEAD; small[at] = mine; }
                                else rec.ts = DEAD;
                                break;
                            }
                        }
                        at = (at + 1) & mask;
                    }
                }
            }
        } else {
#if SS_X == 7
            if (true) goto emit;
#endif
            size_t cap = 16;
            while (cap < total * 2) cap <<= 1;
            table.assign(cap, 0);
            const u64 mask = cap - 1;
            const int index_shift = 64 - __builtin_ctzll(cap), tag_shift = int(abits) + __builtin_ctzll(cap);
            for (u32 bi = 0; bi < blocks.size(); bi++) {
                RecA* recs = Scatter<RecA>::items(blocks[bi]);
                if (bi + 1 < blocks.size()) prefetch_block(blocks[bi + 1]);
                for (u32 i = 0; i < blocks[bi]->n; i++) {
                    RecA& rec = recs[i];
                    const u64 m = hash64(rec.ekey);
                    const u64 mine = ((m << tag_shift) >> 32 << 32) | ((bi << 8 | i) + 1);
                    u64 at = (m << abits) >> index_shift;
                    for (;;) {
                        u64 slot = table[at];
                        if (!slot) { table[at] = mine; break; }
                        if ((slot ^ mine) >> 32 == 0) {
                            u32 ref = u32(slot) - 1;
                            RecA& best = Scatter<RecA>::items(blocks[ref >> 8])[ref & 255];
                            if (best.ekey == rec.ekey) {
                                if (rec.ingest < best.ingest) { best.ts = DEAD; table[at] = mine; }
                                else rec.ts = DEAD;
                                break;
                            }
                        }
                        at = (at + 1) & mask;
                    }
                }
            }
        }
#if SS_X == 7
    emit:
#endif
#if SS_X == 3
        continue;
#endif
        const double step1 = now();
        // Survivors that are valid get their person and move to a person bucket.
        for (Block* b : blocks) {
            RecA* recs = Scatter<RecA>::items(b);
            for (u32 i = 0; i < b->n; i++) {
                if (recs[i].ts == DEAD) continue;
                if (dense_on && dense_extra(tid, recs[i].ukey, recs[i].ts, recs[i].payload)) continue;
#if SS_X == 4 || !defined(SS_ALIAS1)
                // To the bucket of the user ID as it is; whose it is gets settled
                // there, once for each ID (see process_people).
                out.push(u32(hash64(recs[i].ukey) >> shift), RecB{recs[i].ukey, recs[i].ts, recs[i].payload});
#else
                resolver.push(recs[i].ukey, Survivor{recs[i].ts, recs[i].payload});
#endif
            }
        }
#ifdef SS_ALIAS1
        resolver.finish();
#endif
        // This bucket's blocks are finished with: reuse them for person buckets.
        for (Block* b : blocks) sh.alloc[tid].recycle((u8*)b);
        const double step2 = now();
        steps[0] += step1 - step0;
        steps[1] += step2 - step1;
    }
}

// ---------------------------------------------------------------- phase 3: people
struct Click { u32 ts; u32 camp; u64 key; };

#ifdef SS_DIGEST
// Measurement builds only: a sum over every output row of a hash of its values.
// It does not depend on the order or the files the rows land in, so two runs
// (dense and general) can be compared on a machine with nothing but a shell.
static std::atomic<u64> g_digest_users{0}, g_digest_campaigns{0};
static inline u64 digest_bytes(const char* p, size_t n, u64 h) {
    for (size_t i = 0; i < n; i++) h = (h ^ u8(p[i])) * 0x100000001B3ULL;
    return h;
}
#endif

struct UserWriter {
#ifndef SS_GROUP
#define SS_GROUP 2048                   // rows in a row group: small, so the column buffers being filled stay in cache
#endif                                  // (measured on the scoring machine: 2,048 and 8,192 rows beat 131,072 by about 1%)
    static const u32 GROUP = SS_GROUP;
    pq::File file;
    bool opened = false;
    std::string path;
    u32 n = 0;
    hvec<u8> ids, cats, nulls;
    hvec<i32> days, sessions, events;
    hvec<i64> revenue;
    size_t ids_len = 0, cats_len = 0;

    void init(const std::string& p) {
        path = p;
        ids.resize(size_t(GROUP) * 16 + 4096);
        cats.resize(1 << 16);
        nulls.assign(GROUP / 8 + 8, 0);
        days.resize(GROUP); sessions.resize(GROUP); events.resize(GROUP); revenue.resize(GROUP);
    }
    static std::vector<pq::Col> schema() {
        return {{"user_id", pq::BYTE_ARRAY, pq::UTF8, false}, {"day", pq::INT32, pq::DATE, false},
                {"sessions", pq::INT32, pq::NONE, false}, {"events", pq::INT32, pq::NONE, false},
                {"revenue_cents", pq::INT64, pq::NONE, false}, {"top_category", pq::BYTE_ARRAY, pq::UTF8, true}};
    }
    inline void add(const char* id, u32 id_len, i32 day, i32 nsessions, i32 nevents, i64 cents, const std::string* category) {
#ifdef SS_DIGEST
        {
            u64 h = digest_bytes(id, id_len, 0xCBF29CE484222325ULL);
            h = mix64(h ^ u64(u32(day)) ^ (u64(u32(nsessions)) << 20) ^ (u64(u32(nevents)) << 40));
            h = mix64(h ^ u64(cents));
            if (category) h = digest_bytes(category->data(), category->size(), h ^ 0x9E3779B97F4A7C15ULL);
            g_digest_users.fetch_add(mix64(h), std::memory_order_relaxed);
        }
#endif
        if (ids_len + id_len + 4 > ids.size()) ids.resize(std::max(ids.size() * 2, ids_len + id_len + 4));
        memcpy(&ids[ids_len], &id_len, 4);
        memcpy(&ids[ids_len + 4], id, id_len);
        ids_len += id_len + 4;
        days[n] = day; sessions[n] = nsessions; events[n] = nevents; revenue[n] = cents;
        if (category) {
            u32 len = u32(category->size());
            if (cats_len + len + 4 > cats.size()) cats.resize(std::max(cats.size() * 2, cats_len + len + 4));
            memcpy(&cats[cats_len], &len, 4);
            memcpy(&cats[cats_len + 4], category->data(), len);
            cats_len += len + 4;
            nulls[n >> 3] |= u8(1u << (n & 7));
        }
        if (++n == GROUP) flush();
    }
    void flush() {
        if (!n) return;
        if (!opened) { file.open(path, schema()); opened = true; }
        // Definition levels: one bit-packed run, 1 bit a row.
        u32 groups = (n + 7) / 8;
        std::vector<u8> levels;
        u64 header = (u64(groups) << 1) | 1;
        while (header >= 0x80) { levels.push_back(u8((header & 0x7F) | 0x80)); header >>= 7; }
        levels.push_back(u8(header));
        levels.insert(levels.end(), nulls.begin(), nulls.begin() + groups);
        pq::Part parts[6];
        parts[0].values = ids.data(); parts[0].values_len = ids_len;
        parts[1].values = days.data(); parts[1].values_len = size_t(n) * 4;
        parts[2].values = sessions.data(); parts[2].values_len = size_t(n) * 4;
        parts[3].values = events.data(); parts[3].values_len = size_t(n) * 4;
        parts[4].values = revenue.data(); parts[4].values_len = size_t(n) * 8;
        parts[5].levels = levels.data(); parts[5].levels_len = levels.size();
        parts[5].values = cats.data(); parts[5].values_len = cats_len;
#if SS_DX != 6
        file.row_group(n, parts);
#endif
        memset(nulls.data(), 0, groups);
        n = 0; ids_len = 0; cats_len = 0;
    }
    void finish() {
        flush();
        if (opened) file.close();
    }
};

// All of one person's events inside one 30-minute slice of the window. Events
// in a slice are under 30 minutes apart, so they always share a session, and a
// session boundary can only fall between two slices: sessions come from
// walking a person's slices in order, with no sort of the events.
struct Cell { u64 person; u32 bin; u32 count; u32 first; u32 last; u32 purchases; u32 pad; };
struct Purchase { u32 payload; u32 next; };

[[maybe_unused]] static void process_people_v1(Shared& sh, u32 tid) {
    const u32 nthreads = sh.cfg.threads;
    const u32 ncamp = u32(sh.cat.campaign_ids.size());
    const u32 organic = ncamp;
    const u32 ndays = sh.ndays;
    const i64 ws = sh.ws, day0 = sh.day0;
    const u32 ncat = u32(sh.cat.categories.size());
    const u32 bbits = sh.bbits;
    Shared::Totals& totals = sh.campaign_totals[tid];
    totals.sessions.assign(size_t(ncamp + 1) * ndays, 0);
    totals.assisted.assign(size_t(ncamp + 1) * ndays, 0);
    totals.purchasing.assign(size_t(ncamp + 1) * ndays, 0);
    totals.revenue.assign(size_t(ncamp + 1) * ndays, 0);

    UserWriter writer;
    char name[64];
    snprintf(name, sizeof name, "/user_daily/part-%03u.parquet", tid);
    writer.init(sh.cfg.out + name);

    std::vector<u32> cell_table, person_table, cell_person, person_cells, order, cursor, click_start;
    std::vector<Cell> cells;
    std::vector<Purchase> purchases;
    std::vector<u64> persons;
    struct Found { u32 person; Click click; };
    std::vector<Found> found;
    std::vector<Click> clicks;
    std::vector<i64> cat_cents(ncat, 0);
    std::vector<u32> cat_touched;
    std::vector<u32> camp_count(ncamp + 1, 0), camp_slot(ncamp + 1, 0), active;
    u64 rows = 0;

    for (;;) {
        size_t turn = sh.next_person.fetch_add(1, std::memory_order_relaxed);
        if (turn >= sh.person_order.size()) break;
        u32 bucket = sh.person_order[turn];

        size_t nev = 0;
        for (u32 t = 0; t < nthreads; t++)
            for (Block* b = sh.b[t].head[bucket]; b; b = b->next) nev += b->n;
        if (!nev) continue;

        // 1. Fold the bucket's events into (person, slice) cells.
        size_t cap = 64;
        while (cap < nev) cap <<= 1;
        cell_table.assign(cap, 0);
        u64 mask = cap - 1;
        int cell_shift = 64 - __builtin_ctzll(cap);
        cells.clear();
        purchases.clear();
        purchases.push_back({0, 0});                              // index 0 means "none"
        auto cell_home = [&](u64 person, u32 bin) -> u64 {
            return (((hash64(person) << bbits) >> cell_shift) ^ (u64(bin) * 0x9E3779B1ULL)) & mask;
        };
        for (u32 t = 0; t < nthreads; t++)
            for (Block* b = sh.b[t].head[bucket]; b; b = b->next) {
                const RecB* recs = Scatter<RecB>::items(b);
                for (u32 i = 0; i < b->n; i++) {
                    const u64 person = recs[i].person;
                    const u32 ts = recs[i].ts;
                    const u32 bin = ts / u32(SESSION_GAP);
                    u64 at = cell_home(person, bin);
                    u32 id;
                    for (;;) {
                        u32 slot = cell_table[at];
                        if (!slot) {
                            if ((cells.size() + 1) * 2 > cap) {            // keep the table at most half full
                                cap <<= 1;
                                mask = cap - 1;
                                cell_shift--;
                                cell_table.assign(cap, 0);
                                for (u32 k = 0; k < cells.size(); k++) {
                                    u64 h = cell_home(cells[k].person, cells[k].bin);
                                    while (cell_table[h]) h = (h + 1) & mask;
                                    cell_table[h] = k + 1;
                                }
                                at = cell_home(person, bin);
                                while (cell_table[at]) at = (at + 1) & mask;
                            }
                            id = u32(cells.size());
                            cells.push_back({person, bin, 0, ts, ts, 0, 0});
                            cell_table[at] = id + 1;
                            break;
                        }
                        const Cell& seen = cells[slot - 1];
                        if (seen.person == person && seen.bin == bin) { id = slot - 1; break; }
                        at = (at + 1) & mask;
                    }
                    Cell& cell = cells[id];
                    cell.count++;
                    if (ts < cell.first) cell.first = ts;
                    if (ts > cell.last) cell.last = ts;
                    if (recs[i].payload) {
                        purchases.push_back({recs[i].payload, cell.purchases});
                        cell.purchases = u32(purchases.size() - 1);
                    }
                }
            }
        const u32 ncells = u32(cells.size());
#if SS_X == 2
        rows += ncells; continue;
#endif

        // 2. Number the people and list each one's cells in slice order.
        size_t pcap = 64;
        while (pcap < size_t(ncells) * 2) pcap <<= 1;
        const u64 pmask = pcap - 1;
        const int person_shift = 64 - __builtin_ctzll(pcap);
        person_table.assign(pcap, 0);
        persons.clear();
        person_cells.clear();
        cell_person.resize(ncells);
        for (u32 k = 0; k < ncells; k++) {
            const u64 person = cells[k].person;
            u64 at = (hash64(person) << bbits) >> person_shift;
            u32 id;
            for (;;) {
                u32 slot = person_table[at];
                if (!slot) {
                    id = u32(persons.size());
                    persons.push_back(person);
                    person_cells.push_back(0);
                    person_table[at] = id + 1;
                    break;
                }
                if (persons[slot - 1] == person) { id = slot - 1; break; }
                at = (at + 1) & pmask;
            }
            person_cells[id]++;
            cell_person[k] = id;
        }
        const u32 npeople = u32(persons.size());
        person_cells.push_back(0);
        for (u32 p = 0, sum = 0; p <= npeople; p++) { u32 n = person_cells[p]; person_cells[p] = sum; sum += n; }
        cursor.assign(person_cells.begin(), person_cells.end() - 1);
        order.resize(ncells);
        for (u32 k = 0; k < ncells; k++) order[cursor[cell_person[k]]++] = k;

        // 3. Clicks of people who have events, grouped the same way.
        auto lookup = [&](u64 person) -> i64 {
            u64 at = (hash64(person) << bbits) >> person_shift;
            for (;;) {
                u32 slot = person_table[at];
                if (!slot) return -1;
                if (persons[slot - 1] == person) return slot - 1;
                at = (at + 1) & pmask;
            }
        };
        click_start.assign(npeople + 1, 0);
        found.clear();
        for (u32 t = 0; t < nthreads; t++)
            for (Block* b = sh.c[t].head[bucket]; b; b = b->next) {
                const RecC* recs = Scatter<RecC>::items(b);
                for (u32 i = 0; i < b->n; i++) {
                    i64 id = lookup(recs[i].person);
                    if (id >= 0) {
                        click_start[size_t(id)]++;
                        found.push_back({u32(id), {recs[i].ts, recs[i].camp, recs[i].ckey}});
                    }
                }
            }
        const size_t nclicks = found.size();
        if (nclicks) {
            for (u32 p = 0, sum = 0; p <= npeople; p++) { u32 n = click_start[p]; click_start[p] = sum; sum += n; }
            cursor.assign(click_start.begin(), click_start.end() - 1);
            clicks.resize(nclicks);
            for (const Found& f : found) clicks[cursor[f.person]++] = f.click;
        }

#if SS_X == 1
        rows += nclicks; continue;
#endif
        // 4. Each person: sessions, click credit and assists, daily rows.
        for (u32 p = 0; p < npeople; p++) {
            u32* mine = order.data() + person_cells[p];
            const u32 n = person_cells[p + 1] - person_cells[p];
            if (n <= 32) {
                for (u32 i = 1; i < n; i++) {
                    u32 v = mine[i];
                    u32 bin = cells[v].bin;
                    u32 j = i;
                    while (j > 0 && cells[mine[j - 1]].bin > bin) { mine[j] = mine[j - 1]; j--; }
                    mine[j] = v;
                }
            } else {
                std::sort(mine, mine + n, [&](u32 x, u32 y) { return cells[x].bin < cells[y].bin; });
            }
            Click* ck = nclicks ? clicks.data() + click_start[p] : nullptr;
#if SS_X == 5
            const u32 m = 0;
#else
            const u32 m = nclicks ? click_start[p + 1] - click_start[p] : 0;
#endif
            // Time ascending; at one instant the smallest click_id comes last, so
            // the last click at or before a session start is the one to credit.
            auto before = [](const Click& x, const Click& y) {
                return x.ts < y.ts || (x.ts == y.ts && id_less(y.key, x.key, 'k'));
            };
            if (m <= 32) {
                for (u32 i = 1; i < m; i++) {
                    Click v = ck[i];
                    u32 j = i;
                    while (j > 0 && before(v, ck[j - 1])) { ck[j] = ck[j - 1]; j--; }
                    ck[j] = v;
                }
            } else {
                std::sort(ck, ck + m, before);
            }

            char name_buf[16];
            u32 name_len = 0;
            std::string name_long;
            const char* person_name = name_buf;
            if (persons[p] & INTERN_BIT) {
                name_long = id_string(persons[p], 'u');
                person_name = name_long.data();
                name_len = u32(name_long.size());
            } else {
#ifdef SS_ID2
                name_buf[name_len++] = 'u';
                for (u64 key = persons[p]; key; key >>= 8) name_buf[name_len++] = char(key & 0xFF);
#else
                for (int shift = 56; shift >= 0; shift -= 7) {
                    char c = char((persons[p] >> shift) & 0x7F);
                    if (!c) break;
                    name_buf[name_len++] = c;
                }
#endif
            }
            i64 cur_day = -1;
            i32 d_sessions = 0, d_events = 0;
            i64 d_revenue = 0;
            auto emit = [&]() {
                const std::string* top = nullptr;
                if (!cat_touched.empty()) {
                    std::sort(cat_touched.begin(), cat_touched.end());   // categories are numbered in byte order
                    i64 best = 0;
                    for (u32 c : cat_touched) {
                        if (cat_cents[c] > best) { best = cat_cents[c]; top = &sh.cat.categories[c]; }
                        cat_cents[c] = 0;
                    }
                    cat_touched.clear();
                }
#if SS_X != 6
                writer.add(person_name, name_len, i32(day0 + cur_day), d_sessions, d_events, d_revenue, top);
#endif
                rows++;
            };
            u32 left = 0, right = 0;
            u32 i = 0;
            while (i < n) {
                const Cell* cell = &cells[mine[i]];
                const u32 start = cell->first;
                const i64 day = floor_div(ws + start, DAY) - day0;
                if (day != cur_day) {
                    if (cur_day >= 0) emit();
                    cur_day = day; d_sessions = 0; d_events = 0; d_revenue = 0;
                }
                i64 cents = 0;
                u32 nevents = 0;
                for (;;) {
                    nevents += cell->count;
                    for (u32 k = cell->purchases; k; k = purchases[k].next) {
                        u32 c = purchases[k].payload & 0xFF;
                        i64 price = purchases[k].payload >> 8;
                        if (!cat_cents[c]) cat_touched.push_back(c);
                        cat_cents[c] += price;
                        cents += price;
                    }
                    if (++i == n) break;
                    const Cell* next = &cells[mine[i]];
                    // A later slice continues the session only if it is the very next
                    // slice and its first event is within 30 minutes of the last one.
                    if (next->bin != cell->bin + 1 || i64(next->first) - i64(cell->last) > SESSION_GAP) break;
                    cell = next;
                }
                // purchases have a price of at least 1, so cents > 0 means "has a purchase"
                d_sessions++; d_events += i32(nevents); d_revenue += cents;

                u32 credited = organic;
                if (m) {
                    // Click times count from window_start - 7 days, event times from
                    // window_start, so the clicks in reach are those in [start, start + 7 days].
                    const u64 hi = u64(start) + u64(REACH);
                    while (right < m && ck[right].ts <= hi) {
                        u32 c = ck[right++].camp;
                        if (camp_count[c]++ == 0) { camp_slot[c] = u32(active.size()); active.push_back(c); }
                    }
                    while (left < right && ck[left].ts < start) {
                        u32 c = ck[left++].camp;
                        if (--camp_count[c] == 0) {
                            u32 last = active.back();
                            active[camp_slot[c]] = last;
                            camp_slot[last] = camp_slot[c];
                            active.pop_back();
                        }
                    }
                    if (right > left) credited = ck[right - 1].camp;
                    for (u32 c : active) totals.assisted[size_t(c) * ndays + size_t(day)]++;
                }
                const size_t slot = size_t(credited) * ndays + size_t(day);
                totals.sessions[slot]++;
                if (cents > 0) { totals.purchasing[slot]++; totals.revenue[slot] += cents; }
            }
            if (cur_day >= 0) emit();
            for (u32 c : active) camp_count[c] = 0;
            active.clear();
        }
    }
    writer.finish();
    sh.user_rows.fetch_add(rows);
}

// The people phase. Per bucket: every person gets a local number; each event
// becomes one 64-bit key (person number, 30-minute slice, time within the
// slice); two counting passes order the keys by person and slice with no
// comparisons. Events in one slice are under 30 minutes apart, so they always
// share a session and a session boundary can only fall between two slices:
// walking a person's slices in order gives the sessions without sorting events.
#ifndef SS_PEOPLE1
// The people phase runs twice. Round 0 takes the buckets of user IDs: an ID that
// is not linked to another is a whole person and is finished there; the events
// and clicks of an ID that is one of several are passed on, under the person's
// name, to that name's bucket in the "linked" lists. Round 1 takes those. An ID
// is looked up in the alias map once, when the bucket first meets it.
// (With SS_ALIAS1 the records arrive resolved and there is only round 0.)
static void process_people(Shared& sh, u32 tid, u32 round) {
    const u32 nthreads = sh.cfg.threads;
#ifdef SS_ALIAS1
    const bool route = false;
#else
    const bool route = round == 0;
#endif
    std::vector<Scatter<RecB>>& src_b = round ? sh.lb : sh.b;
    std::vector<Scatter<RecC>>& src_c = round ? sh.lc : sh.c;
    const int person_shift = 64 - int(sh.bbits);
    const u32 LINKED = 0x80000000u, NOBODY = 0xFFFFFFFFu;
    const u32 ncamp = u32(sh.cat.campaign_ids.size());
    const u32 organic = ncamp;
    const u32 ndays = sh.ndays;
    const i64 ws = sh.ws, day0 = sh.day0;
    const u32 ncat = u32(sh.cat.categories.size());
    const u32 bbits = sh.bbits;
    const u32 SLICE = u32(SESSION_GAP);
    const u32 nslices = u32((sh.we - sh.ws) / SESSION_GAP) + 2;
    const u32 part = tid + (round ? nthreads : sh.part_base);
    Shared::Totals& totals = sh.campaign_totals[part];
    totals.sessions.assign(size_t(ncamp + 1) * ndays, 0);
    totals.assisted.assign(size_t(ncamp + 1) * ndays, 0);
    totals.purchasing.assign(size_t(ncamp + 1) * ndays, 0);
    totals.revenue.assign(size_t(ncamp + 1) * ndays, 0);

    UserWriter writer;
    char name[64];
    snprintf(name, sizeof name, "/user_daily/part-%03u.parquet", part);
    writer.init(sh.cfg.out + name);
    double* steps = g_steps[tid & 255];

    // An ID met in this bucket: a person's number within the bucket, or LINKED | n
    // for one of several linked IDs, whose person's name is link_root[n].
    struct Local { u64 key; u32 index; u32 used; };
    hvec<u64> met_key, link_root;                             // everything in the table, to rebuild it when it grows
    hvec<u32> met_code;
    hvec<RecB> passing(Scatter<RecB>::CAP + 1);               // a block's events on their way to the linked lists
    struct Bought { u64 key; u32 payload; u32 pad; };        // key = person number << 32 | ts
    struct Found { u32 person; Click click; };
    hvec<Local> table;
    hvec<u64> persons, keys, sorted;
    hvec<u32> per_person, per_slice(nslices + 1), person_start, click_start, cursor;
    hvec<Bought> bought;
    hvec<Found> found;
    hvec<Click> clicks;
    hvec<i64> cat_cents(ncat, 0);
    hvec<u32> cat_touched;
    hvec<u32> camp_count(ncamp + 1, 0), camp_slot(ncamp + 1, 0), active;
    hvec<u32> stamp(ncamp + 1, 0);                     // last session that counted this campaign as an assist
    // For a person with many clicks: the sessions so far cut into runs of one day
    // each (the day, and the number of the run's first session).
    hvec<u32> run_day(ndays + 2, 0), run_first(ndays + 2, 0);
    u32 session_id = 0;
    u64 rows = 0;

    for (;;) {
        size_t turn = sh.next_person.fetch_add(1, std::memory_order_relaxed);
        if (turn >= sh.person_order.size()) break;
        u32 bucket = sh.person_order[turn];

        size_t nev = 0;
        for (u32 t = 0; t < nthreads; t++)
            for (Block* b = src_b[t].head[bucket]; b; b = b->next) nev += b->n;
        if (!nev) {
            // No events, but in round 0 there may be clicks to pass on.
            bool clicks_here = false;
            for (u32 t = 0; route && t < nthreads; t++) clicks_here = clicks_here || src_c[t].head[bucket];
            if (!clicks_here) continue;
        }
        const double step0 = now();
        if (route) sh.alias.prefetch_bucket(bucket, bbits);

        // 1. Number the people and key the events.
        size_t cap = 64;
        while (cap < nev / 8) cap <<= 1;
        table.assign(cap, Local{0, 0, 0});
        u64 mask = cap - 1;
        int shift = 64 - __builtin_ctzll(cap);
        persons.clear();
        per_person.clear();
        met_key.clear();
        met_code.clear();
        link_root.clear();
        link_root.push_back(0);                                   // number 0 is never given out: see the event loop
        bought.clear();
        keys.resize(nev + 1);                                     // one spare place for a write that is not kept
        memset(per_slice.data(), 0, per_slice.size() * sizeof(u32));
        auto remember = [&](u64 id, u32 code) {
            if ((met_key.size() + 1) * 2 > cap) {                 // keep the table at most half full
                cap <<= 1;
                mask = cap - 1;
                shift--;
                table.assign(cap, Local{0, 0, 0});
                for (size_t i = 0; i < met_key.size(); i++) {
                    u64 h = (hash64(met_key[i]) << bbits) >> shift;
                    while (table[h].used) h = (h + 1) & mask;
                    table[h] = {met_key[i], met_code[i], 1};
                }
            }
            u64 at = (hash64(id) << bbits) >> shift;
            while (table[at].used) at = (at + 1) & mask;
            table[at] = {id, code, 1};
            met_key.push_back(id);
            met_code.push_back(code);
        };
        // The ID of an event: its person's number here, or LINKED | n.
        auto number = [&](u64 id) -> u32 {
            const u64 h = hash64(id);
            u64 at = (h << bbits) >> shift;
            for (;;) {
                const Local& slot = table[at];
                if (!slot.used) break;
                if (slot.key == id) return slot.index;
                at = (at + 1) & mask;
            }
            u32 code;
            u64 root;
            if (route && sh.alias.find(id, h, root)) {
                code = LINKED | u32(link_root.size());
                link_root.push_back(root);
            } else {
                code = u32(persons.size());
                persons.push_back(id);
                per_person.push_back(0);
            }
            remember(id, code);
            return code;
        };
        size_t j = 0, passed_on = 0;
        for (u32 t = 0; t < nthreads; t++)
            for (Block* b = src_b[t].head[bucket]; b; b = b->next) {
                const RecB* recs = Scatter<RecB>::items(b);
                prefetch_block(b->next);
#if !defined(SS_ALIAS1) && !defined(SS_ROUTE1)
                if (route) {
                    // Whether an event stays or is passed on is close to a coin toss, so
                    // nothing branches on it: both forms are written and only the right
                    // cursor moves. The events passed on leave after the block is read.
                    RecB* out = passing.data();
                    u32 m = 0;
                    for (u32 i = 0; i < b->n; i++) {
                        const u32 who = number(recs[i].person);
                        const u32 linked = who >> 31, stays = linked ^ 1;
                        const u32 ts = recs[i].ts;
                        const u32 slice = ts / SLICE;
                        out[m] = RecB{link_root[linked ? (who & ~LINKED) : 0], ts, recs[i].payload};
                        m += linked;
                        keys[j] = (u64(who) << 33) | (u64(slice) << 21) | (ts - slice * SLICE);
                        j += stays;
                        per_slice[slice] += stays;
                        u32 nowhere = 0;
                        u32* count = linked ? &nowhere : per_person.data() + who;
                        *count += 1;
                        if (recs[i].payload && stays) bought.push_back({(u64(who) << 32) | ts, recs[i].payload, 0});
                    }
                    for (u32 k = 0; k < m; k++) sh.lb[tid].push(u32(hash64(out[k].person) >> person_shift), out[k]);
                    passed_on += m;
                    continue;
                }
#endif
                for (u32 i = 0; i < b->n; i++) {
                    const u32 who = number(recs[i].person);
                    if (who & LINKED) {
                        const u64 root = link_root[who & ~LINKED];
                        sh.lb[tid].push(u32(hash64(root) >> person_shift), RecB{root, recs[i].ts, recs[i].payload});
                        passed_on++;
                        continue;
                    }
                    const u32 ts = recs[i].ts;
                    const u32 slice = ts / SLICE;
                    keys[j++] = (u64(who) << 33) | (u64(slice) << 21) | (ts - slice * SLICE);
                    per_slice[slice]++;
                    per_person[who]++;
                    if (recs[i].payload) bought.push_back({(u64(who) << 32) | ts, recs[i].payload, 0});
                }
            }
        const u32 npeople = u32(persons.size());
        nev = j;                                                  // without the events passed on
        const double step1 = now();

        // 2. Order the keys by slice, then (keeping that order) by person.
        sorted.resize(nev);
        for (u32 k = 0, sum = 0; k < per_slice.size(); k++) { u32 n = per_slice[k]; per_slice[k] = sum; sum += n; }
        for (size_t k = 0; k < nev; k++) sorted[per_slice[(keys[k] >> 21) & 0xFFF]++] = keys[k];
        person_start.resize(npeople + 1);
        for (u32 k = 0, sum = 0; k < npeople; k++) { person_start[k] = sum; sum += per_person[k]; }
        person_start[npeople] = u32(nev);
        cursor.assign(person_start.begin(), person_start.end() - 1);
        for (size_t k = 0; k < nev; k++) keys[cursor[sorted[k] >> 33]++] = sorted[k];
        if (bought.size() > 1) std::sort(bought.begin(), bought.end(), [](const Bought& x, const Bought& y) { return x.key < y.key; });
        bought.push_back({~0ULL, 0, 0});                          // sentinel
        const double step2 = now();

        // 3. Clicks of people who have events, grouped by person.
        // The ID of a click: as for an event, or NOBODY for an ID with no events
        // here that is not linked to another (its clicks reach no session).
        auto lookup = [&](u64 id) -> u32 {
            const u64 h = hash64(id);
            u64 at = (h << bbits) >> shift;
            for (;;) {
                const Local& slot = table[at];
                if (!slot.used) break;
                if (slot.key == id) return slot.index;
                at = (at + 1) & mask;
            }
            u64 root;
            if (route && sh.alias.find(id, h, root)) {
                const u32 code = LINKED | u32(link_root.size());
                link_root.push_back(root);
                remember(id, code);
                return code;
            }
            return NOBODY;
        };
        click_start.assign(npeople + 1, 0);
        found.clear();
        for (u32 t = 0; t < nthreads; t++)
            for (Block* b = src_c[t].head[bucket]; b; b = b->next) {
                const RecC* recs = Scatter<RecC>::items(b);
                prefetch_block(b->next);
                for (u32 i = 0; i < b->n; i++) {
                    const u32 id = lookup(recs[i].person);
                    if (id == NOBODY) continue;
                    if (id & LINKED) {
                        const u64 root = link_root[id & ~LINKED];
                        sh.lc[tid].push(u32(hash64(root) >> person_shift), RecC{root, recs[i].ckey, recs[i].ts, recs[i].camp});
                        continue;
                    }
                    click_start[size_t(id)]++;
                    found.push_back({id, {recs[i].ts, recs[i].camp, recs[i].ckey}});
                }
            }
        const size_t nclicks = found.size();
        if (nclicks) {
            for (u32 p = 0, sum = 0; p <= npeople; p++) { u32 n = click_start[p]; click_start[p] = sum; sum += n; }
            cursor.assign(click_start.begin(), click_start.end() - 1);
            clicks.resize(nclicks);
            for (const Found& f : found) clicks[cursor[f.person]++] = f.click;
        }

        // 4. Each person: sessions, click credit and assists, daily rows.
        const double step3 = now();
        size_t next_bought = 0;
        for (u32 p = 0; p < npeople; p++) {
            const u64* mine = keys.data() + person_start[p];
            const u32 n = person_start[p + 1] - person_start[p];
            Click* ck = nclicks ? clicks.data() + click_start[p] : nullptr;
#if SS_X == 5
            const u32 m = 0;
#else
            const u32 m = nclicks ? click_start[p + 1] - click_start[p] : 0;
#endif
            // A handful of clicks are simply scanned for every session. More are put in
            // time order (at one instant the smallest click_id last) for a sliding window.
            const bool scan_clicks = m <= SS_SCANMAX;
            if (!scan_clicks) {
                auto before = [](const Click& x, const Click& y) {
                    return x.ts < y.ts || (x.ts == y.ts && id_less(y.key, x.key, 'k'));
                };
                std::sort(ck, ck + m, before);
            }

            char name_buf[16];
            u32 name_len = 0;
            std::string name_long;
            const char* person_name = name_buf;
            if (persons[p] & INTERN_BIT) {
                name_long = id_string(persons[p], 'u');
                person_name = name_long.data();
                name_len = u32(name_long.size());
            } else {
#ifdef SS_ID2
                name_buf[name_len++] = 'u';
                for (u64 key = persons[p]; key; key >>= 8) name_buf[name_len++] = char(key & 0xFF);
#else
                for (int sh7 = 56; sh7 >= 0; sh7 -= 7) {
                    char c = char((persons[p] >> sh7) & 0x7F);
                    if (!c) break;
                    name_buf[name_len++] = c;
                }
#endif
            }
            i64 cur_day = -1;
            i32 d_sessions = 0, d_events = 0;
            i64 d_revenue = 0;
            auto emit = [&]() SS_WALK {
                const std::string* top = nullptr;
                if (!cat_touched.empty()) {
                    std::sort(cat_touched.begin(), cat_touched.end());   // categories are numbered in byte order
                    i64 best = 0;
                    for (u32 c : cat_touched) {
                        if (cat_cents[c] > best) { best = cat_cents[c]; top = &sh.cat.categories[c]; }
                        cat_cents[c] = 0;
                    }
                    cat_touched.clear();
                }
#if SS_X != 6
                writer.add(person_name, name_len, i32(day0 + cur_day), d_sessions, d_events, d_revenue, top);
#endif
                rows++;
            };
            u32 left = 0, right = 0;
#ifndef SS_ASSIST1
            // A campaign assists every session from the one where its first click comes
            // into reach to the one where its last click drops out. Counting that span
            // when it ends, a day at a time, replaces one addition per campaign in reach
            // per session. Sessions are numbered from 0 in time order (`ord` is the next
            // number); camp_slot holds the number at which a campaign's span began.
            u32 ord = 0, nruns = 0;
            auto close_span = [&](u32 c, u32 from) {
                u32 upper = ord;                                  // sessions [from, ord) had this campaign in reach
                for (u32 j = nruns; j-- > 0;) {
                    const u32 first = run_first[j];
                    const u32 lo = from > first ? from : first;
                    if (upper > lo) totals.assisted[size_t(c) * ndays + run_day[j]] += upper - lo;
                    upper = first;
                    if (first <= from) break;
                }
            };
#endif
            // One session: first event at `start`, last at `last` (ms from window_start).
            auto session = [&](u32 start, u32 last, u32 nevents) SS_WALK {
#if SS_X == 9
                rows += start + last + nevents; return;
#endif
                const i64 day = floor_div(ws + start, DAY) - day0;
                if (day != cur_day) {
                    if (cur_day >= 0) emit();
                    cur_day = day; d_sessions = 0; d_events = 0; d_revenue = 0;
                }
                i64 cents = 0;
                const u64 upto = (u64(p) << 32) | last;
                while (bought[next_bought].key <= upto) {         // this person's purchases up to the session's end
                    const u32 payload = bought[next_bought++].payload;
                    const u32 c = payload & 0xFF;
                    const i64 price = payload >> 8;
                    if (!cat_cents[c]) cat_touched.push_back(c);
                    cat_cents[c] += price;
                    cents += price;
                }
                // purchases have a price of at least 1, so cents > 0 means "has a purchase"
                d_sessions++; d_events += i32(nevents); d_revenue += cents;

                u32 credited = organic;
                // Click times count from window_start - 7 days, event times from
                // window_start, so the clicks in reach are those in [start, start + 7 days].
                if (scan_clicks) {
                    session_id++;
                    i64 best = -1;
                    for (u32 k = 0; k < m; k++) {
                        const u32 t = ck[k].ts;
                        if (u32(t - start) > u32(REACH)) continue;            // before start, or over 7 days back
                        const u32 c = ck[k].camp;
                        if (stamp[c] != session_id) { stamp[c] = session_id; totals.assisted[size_t(c) * ndays + size_t(day)]++; }
                        if (best < 0 || t > ck[best].ts || (t == ck[best].ts && id_less(ck[k].key, ck[best].key, 'k'))) best = k;
                    }
                    if (best >= 0) credited = ck[best].camp;
                } else {
                    const u64 hi = u64(start) + u64(REACH);
#ifndef SS_ASSIST1
                    while (right < m && ck[right].ts <= hi) {
                        u32 c = ck[right++].camp;
                        if (camp_count[c]++ == 0) camp_slot[c] = ord;
                    }
                    while (left < right && ck[left].ts < start) {
                        u32 c = ck[left++].camp;
                        if (--camp_count[c] == 0) close_span(c, camp_slot[c]);
                    }
                    if (right > left) credited = ck[right - 1].camp;
                    if (nruns == 0 || run_day[nruns - 1] != u32(day)) { run_day[nruns] = u32(day); run_first[nruns] = ord; nruns++; }
                    ord++;
#else
                    while (right < m && ck[right].ts <= hi) {
                        u32 c = ck[right++].camp;
                        if (camp_count[c]++ == 0) { camp_slot[c] = u32(active.size()); active.push_back(c); }
                    }
                    while (left < right && ck[left].ts < start) {
                        u32 c = ck[left++].camp;
                        if (--camp_count[c] == 0) {
                            u32 tail = active.back();
                            active[camp_slot[c]] = tail;
                            camp_slot[tail] = camp_slot[c];
                            active.pop_back();
                        }
                    }
                    if (right > left) credited = ck[right - 1].camp;
                    for (u32 c : active) totals.assisted[size_t(c) * ndays + size_t(day)]++;
#endif
                }
                const size_t slot = size_t(credited) * ndays + size_t(day);
                totals.sessions[slot]++;
                if (cents > 0) { totals.purchasing[slot]++; totals.revenue[slot] += cents; }
            };

            // Walk the slices. Within a slice the keys are in no order, so take min and max.
            // The loop takes one more turn after the last slice, to close the last
            // session at the same place as every other one.
            u32 i = 0;
            u32 start = 0, last = 0, nevents = 0, prev_slice = 0;
            bool open = false;
            for (;;) {
                const bool more = i < n;
                u32 slice = 0, base = 0, lo = 0, hi = 0, count = 0;
                bool joins = false;
                if (more) {
                    slice = u32(mine[i] >> 21) & 0xFFF;
                    base = slice * SLICE;
                    lo = u32(mine[i]) & 0x1FFFFF; hi = lo; count = 1;
                    for (i++; i < n && (u32(mine[i] >> 21) & 0xFFF) == slice; i++) {
                        const u32 v = u32(mine[i]) & 0x1FFFFF;
                        lo = v < lo ? v : lo;
                        hi = v > hi ? v : hi;
                        count++;
                    }
                    // A later slice continues the session only if it is the very next
                    // slice and its first event is within 30 minutes of the last one.
                    joins = open && slice == prev_slice + 1 && i64(base + lo) - i64(last) <= SESSION_GAP;
                }
                if (open && !joins) session(start, last, nevents);
                if (!more) break;
                if (joins) {
                    last = base + hi;
                    nevents += count;
                } else {
                    start = base + lo; last = base + hi; nevents = count;
                    open = true;
                }
                prev_slice = slice;
            }
            if (cur_day >= 0) emit();
#ifndef SS_ASSIST1
            for (u32 k = left; k < right; k++) {                  // the spans still open at the person's last session
                const u32 c = ck[k].camp;
                if (camp_count[c]) { close_span(c, camp_slot[c]); camp_count[c] = 0; }
            }
#else
            for (u32 c : active) camp_count[c] = 0;
            active.clear();
#endif
        }
        if (route) {
            // This bucket's blocks are finished with: reuse them for the linked lists.
            for (u32 t = 0; t < nthreads; t++) {
                for (Block* b = src_b[t].head[bucket]; b;) { Block* next = b->next; sh.alloc[tid].recycle((u8*)b); b = next; }
                for (Block* b = src_c[t].head[bucket]; b;) { Block* next = b->next; sh.alloc[tid].recycle((u8*)b); b = next; }
            }
        }
        const double step4 = now();
        steps[2] += step1 - step0;
        steps[3] += step2 - step1;
        steps[4] += step3 - step2;
        steps[5] += step4 - step3;
        steps[7] += double(npeople);
        steps[8] += double(passed_on);
        steps[9] += double(link_root.size() - 1);
    }
    const double step5 = now();
    writer.finish();
    steps[6] += now() - step5;
    sh.user_rows.fetch_add(rows);
}
#else
static void process_people(Shared& sh, u32 tid, u32) { process_people_v1(sh, tid); }
#endif

// ---------------------------------------------------------------- main
static double thread_cpu() {
    timespec t;
    clock_gettime(CLOCK_THREAD_CPUTIME_ID, &t);
    return double(t.tv_sec) + double(t.tv_nsec) * 1e-9;
}

static std::vector<int> g_pin;              // SS_PIN: the CPUs worker t is tied to, in order

static void pin_thread(u32 t) {
#ifdef __linux__
    if (g_pin.empty()) return;
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(g_pin[t % g_pin.size()], &set);
    sched_setaffinity(0, sizeof set, &set);
#endif
}

// Runs body(0..n-1) on n threads and returns the CPU seconds they used.
template <class F>
static double run_threads(u32 n, F&& body) {
    std::vector<std::thread> threads;
    std::vector<double> cpu(n, 0);
    auto timed = [&](u32 t) { pin_thread(t); double c0 = thread_cpu(); body(t); cpu[t] = thread_cpu() - c0; };
    for (u32 t = 1; t < n; t++) threads.emplace_back([&timed, t] { timed(t); });
    timed(0);
    for (auto& th : threads) th.join();
    double sum = 0;
    for (double c : cpu) sum += c;
    return sum;
}



static i64 manifest_time(const std::string& text, const char* key) {
    size_t at = text.find(key);
    if (at == std::string::npos) die("manifest.json lacks a window bound");
    at = text.find(':', at);
    size_t open = text.find('"', at);
    size_t close = text.find('"', open + 1);
    if (at == std::string::npos || open == std::string::npos || close == std::string::npos) die("manifest.json is not in the expected form");
    std::string value = text.substr(open + 1, close - open - 1);
    value.append(32, '\0');
    DateCache cache;
    i64 ts = parse_ts((const u8*)value.data(), close - open - 1, cache);
    if (ts == TS_BAD) die("manifest.json has an unreadable window bound");
    return ts;
}

static void write_campaigns(Shared& sh) {
    const u32 ncamp = u32(sh.cat.campaign_ids.size());
    const u32 ndays = sh.ndays;
    std::vector<i64> total(size_t(ncamp + 1) * ndays * 4, 0);
    for (auto& part : sh.campaign_totals)
        for (size_t i = 0; i < part.sessions.size(); i++) {
            total[i * 4] += part.sessions[i];
            total[i * 4 + 1] += part.purchasing[i];
            total[i * 4 + 2] += part.revenue[i];
            total[i * 4 + 3] += part.assisted[i];
        }
    std::vector<u8> ids, channels;
    std::vector<i32> days;
    std::vector<i64> sessions, purchasing, revenue, assisted;
    auto put = [](std::vector<u8>& out, const std::string& s) {
        u32 len = u32(s.size());
        out.insert(out.end(), (u8*)&len, (u8*)&len + 4);
        out.insert(out.end(), s.begin(), s.end());
    };
    const std::string organic = "organic";
    for (u32 c = 0; c <= ncamp; c++)
        for (u32 d = 0; d < ndays; d++) {
            const i64* cell = &total[(size_t(c) * ndays + d) * 4];
            if (!cell[0] && !cell[3]) continue;
            put(ids, c == ncamp ? organic : sh.cat.campaign_ids[c]);
            put(channels, c == ncamp ? organic : sh.cat.channels[c]);
            days.push_back(i32(sh.day0 + d));
            sessions.push_back(cell[0]); purchasing.push_back(cell[1]); revenue.push_back(cell[2]);
#ifdef SS_DIGEST
            {
                const std::string& text = c == ncamp ? organic : sh.cat.campaign_ids[c];
                u64 h = digest_bytes(text.data(), text.size(), 0xCBF29CE484222325ULL);
                h = mix64(h ^ u64(d)) ^ mix64(u64(cell[0]) * 3 + 1) ^ mix64(u64(cell[1]) * 5 + 2) ^ mix64(u64(cell[2]) * 7 + 3) ^ mix64(u64(c == ncamp ? 0 : cell[3]) * 11 + 4);
                g_digest_campaigns.fetch_add(mix64(h), std::memory_order_relaxed);
            }
#endif
            assisted.push_back(c == ncamp ? 0 : cell[3]);
        }
    pq::File file;
    file.open(sh.cfg.out + "/campaign_daily/part-000.parquet",
              {{"campaign_id", pq::BYTE_ARRAY, pq::UTF8, false}, {"channel", pq::BYTE_ARRAY, pq::UTF8, false},
               {"day", pq::INT32, pq::DATE, false}, {"sessions", pq::INT64, pq::NONE, false},
               {"purchasing_sessions", pq::INT64, pq::NONE, false}, {"revenue_cents", pq::INT64, pq::NONE, false},
               {"assisted_sessions", pq::INT64, pq::NONE, false}});
    size_t n = days.size();
    if (n) {
        pq::Part parts[7];
        parts[0].values = ids.data(); parts[0].values_len = ids.size();
        parts[1].values = channels.data(); parts[1].values_len = channels.size();
        parts[2].values = days.data(); parts[2].values_len = n * 4;
        parts[3].values = sessions.data(); parts[3].values_len = n * 8;
        parts[4].values = purchasing.data(); parts[4].values_len = n * 8;
        parts[5].values = revenue.data(); parts[5].values_len = n * 8;
        parts[6].values = assisted.data(); parts[6].values_len = n * 8;
        file.row_group(n, parts);
    }
    file.close();
}

int main(int, char** argv) {
    double t0 = now();
    Shared sh;
    Config& cfg = sh.cfg;
    auto env = [](const char* name, const char* fallback) { const char* v = getenv(name); return std::string(v && *v ? v : fallback); };
    cfg.data = env("DATA_DIR", "/data");
    cfg.out = env("OUTPUT_DIR", "/output");
    std::string threads = env("SS_THREADS", env("NCPU", "").c_str());
    cfg.threads = threads.empty() ? std::thread::hardware_concurrency() : u32(atoi(threads.c_str()));
    if (cfg.threads < 1) cfg.threads = 1;
    if (cfg.threads > 256) cfg.threads = 256;
    std::string chunk = env("SS_CHUNK", "");
    if (!chunk.empty()) cfg.chunk = size_t(std::max(1LL, atoll(chunk.c_str())));
    cfg.use_mmap = atoi(env("SS_MMAP", "1").c_str());
    g_alias_space = u32(std::max(110, atoi(env("SS_ASPACE", "300").c_str())));
    {
        const std::string mode = env("SS_RELEASE", "2");            // 0 never, 1 chunk by chunk, 2 file by file
        g_release_input = mode == "0" ? 0 : mode == "1" ? 1 : 2;
    }
    g_populate_input = !env("SS_POPULATE", "").empty();
#ifdef __linux__
    if (!env("SS_PIN", "").empty()) {
        cpu_set_t allowed;
        if (sched_getaffinity(0, sizeof allowed, &allowed) == 0)
            for (int c = 0; c < CPU_SETSIZE; c++) if (CPU_ISSET(c, &allowed)) g_pin.push_back(c);
    }
#endif

    {
        std::string text;
        FILE* f = fopen((cfg.data + "/manifest.json").c_str(), "rb");
        if (!f) die("cannot open manifest.json");
        char tmp[4096];
        size_t got;
        while ((got = fread(tmp, 1, sizeof tmp, f)) > 0) text.append(tmp, got);
        fclose(f);
        sh.ws = manifest_time(text, "\"window_start\"");
        sh.we = manifest_time(text, "\"window_end\"");
    }
    if (sh.we - sh.ws + REACH >= i64(0xFFFFFFF0u)) die("window longer than this build supports");
    sh.day0 = floor_div(sh.ws, DAY);
    sh.ndays = sh.we > sh.ws ? u32(floor_div(sh.we - 1, DAY) - sh.day0 + 1) : 1;
    g_dates.build(sh.day0 - 9, sh.day0 + i64(sh.ndays) + 8);
    sh.day_base = sh.day0 * DAY;
    for (u32 d = 0; d <= sh.ndays; d++) {
        // civil date of day0 + d (Howard Hinnant's civil_from_days)
        i64 z = sh.day0 + d + 719468;
        i64 era = (z >= 0 ? z : z - 146096) / 146097, doe = z - era * 146097;
        i64 yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        i64 doy = doe - (365 * yoe + yoe / 4 - yoe / 100), mp = (5 * doy + 2) / 153;
        i64 day = doy - (153 * mp + 2) / 5 + 1, month = mp < 10 ? mp + 3 : mp - 9, year = yoe + era * 400 + (month <= 2);
        char text[16];
        snprintf(text, sizeof text, "%04lld-%02lld-%02lld", (long long)year, (long long)month, (long long)day);
        sh.day_text8.push_back(rd64((const u8*)text));
        sh.day_text2.push_back(rd16((const u8*)text + 8));
    }
    sh.cat.load(cfg.data);

    list_csv(sh, "links", sh.link_tasks);
    list_csv(sh, "events", sh.event_tasks);
    list_csv(sh, "clicks", sh.click_tasks);
    { u64 input_bytes = 0; for (const InputFile& f : sh.files) input_bytes += f.size; intern_reserve(input_bytes); }
    if (g_release_input == 2) {
        g_chunks_left = new std::atomic<u32>[sh.files.size() + 1];
        for (size_t i = 0; i <= sh.files.size(); i++) g_chunks_left[i].store(0);
        for (const std::vector<Task>* tasks : {&sh.link_tasks, &sh.event_tasks, &sh.click_tasks})
            for (const Task& task : *tasks) g_chunks_left[task.file].fetch_add(1);
    }
    size_t event_bytes = 0;
    for (const Task& task : sh.event_tasks) event_bytes += task.end - task.begin;
    // Up to about 32K events a bucket and at most 2,048 buckets. Fewer, larger
    // buckets cost the dedup a little and save the parse more: every bucket has
    // staging memory, and all of it has to stay in cache beside the scan.
    // Measured on the scoring machine: 1,024 buckets at 20M events and 2,048 at
    // 60M each beat twice as many by about 1%.
    size_t rows = event_bytes / 64;
    while (sh.abits < 11 && (rows >> sh.abits) > 32768) sh.abits++;
    sh.bbits = sh.abits;
    if (!env("SS_ABITS", "").empty()) sh.abits = u32(std::min(14, std::max(1, atoi(env("SS_ABITS", "").c_str()))));
    if (!env("SS_BBITS", "").empty()) sh.bbits = u32(std::min(14, std::max(1, atoi(env("SS_BBITS", "").c_str()))));
    g_arena_region = std::max<size_t>(64u << 20, std::min<size_t>(4ULL << 30, event_bytes / 2 + (64u << 20)));
    g_arena_region = (g_arena_region + SLAB - 1) & ~(SLAB - 1);

    const u32 T = cfg.threads;
    bool dense = false;
#if defined(SS_DENSE_BUILD) && defined(SS_SELFTEST)
    if (!env("SS_LINETEST", "").empty()) line_selftest(u64(atoll(env("SS_LINETEST", "0").c_str())));
#endif
#ifdef SS_DENSE_BUILD
    g_dense.argv = argv;
    dense = dense_setup(sh);
    if (dense) {
        // The general path now only carries the few IDs that do not decode and the
        // people handed over to it: a few buckets are enough.
        sh.abits = 4;
        sh.bbits = 6;
        if (!env("SS_ABITS", "").empty()) sh.abits = u32(std::min(14, std::max(1, atoi(env("SS_ABITS", "").c_str()))));
        if (!env("SS_BBITS", "").empty()) sh.bbits = u32(std::min(14, std::max(1, atoi(env("SS_BBITS", "").c_str()))));
        sh.part_base = T;
        sh.d.resize(T);
        sh.k.resize(T);
    }
#else
    (void)argv;
#endif
    sh.a.resize(T);
    sh.b.resize(T);
    sh.c.resize(T);
    sh.lb.resize(T);
    sh.lc.resize(T);
    sh.alloc.resize(T);
    sh.campaign_totals.resize(2 * size_t(T) + 1);
    mkdir(cfg.out.c_str(), 0777);
    mkdir((cfg.out + "/user_daily").c_str(), 0777);
    mkdir((cfg.out + "/campaign_daily").c_str(), 0777);
    double t1 = now();
    double t2 = 0, t3 = 0, t4 = 0, c1 = 0, c2 = 0, c3 = 0;

    auto order_buckets = [&](std::vector<Scatter<RecB>>& lists) {
        // Largest person buckets first, so a heavy one never ends up running alone at the end.
        const u32 nb = 1u << sh.bbits;
        std::vector<u64> size(nb, 0);
        for (u32 t = 0; t < T; t++)
            for (u32 k = 0; k < nb; k++)
                size[k] += lists[t].nblocks[k];
        sh.person_order.resize(nb);
        for (u32 k = 0; k < nb; k++) sh.person_order[k] = k;
        std::sort(sh.person_order.begin(), sh.person_order.end(), [&](u32 x, u32 y) { return size[x] > size[y]; });
        sh.next_person.store(0);
    };
#ifdef SS_DENSE_BUILD
    if (dense) {
        Dense& D = g_dense;
        c1 = run_threads(T, [&](u32 tid) {
            Reader reader;
            sh.a[tid].init(1u << sh.abits, &sh.alloc[tid]);
            sh.d[tid].init(D.nparts, &sh.alloc[tid]);
            if (tid == 0) parse_links_dense(sh, reader);
#ifdef SS_STAGE
            parse_events_staged(sh, tid, reader);
#else
            parse_events_dense(sh, tid, reader);
#endif
            sh.a[tid].finish();
            sh.d[tid].finish();
            sh.k[tid].init(D.cparts, &sh.alloc[tid]);
            parse_clicks_dense(sh, tid, reader);
            sh.k[tid].finish();
        });
        if (env("SS_DENSE_BAIL", "") == "2") dense_bail(9);
        if (D.bail.load()) dense_restart(sh, false);
        if (!env("SS_STOP", "").empty()) {                    // measurement: the parse alone
            fprintf(stderr, "threads %u buckets %u | setup %.3f parse %.3f dedup %.3f people %.3f write %.3f | total %.3f s | cpu parse %.2f dedup %.2f people %.2f | user rows 0 | links 0\n",
                    T, 1u << sh.abits, t1 - t0, now() - t1, 0.0, 0.0, 0.0, now() - t0, c1, 0.0, 0.0);
            _exit(0);
        }
        t2 = now();
        D.wall[0] = sh.links_union;
        // Phase 2: the general dedup (few events) and the clicks grouped by person.
        c2 = run_threads(T, [&](u32 tid) {
            DenseCtx& cx = D.ctx[tid];
            sh.b[tid].init(1u << sh.bbits, &sh.alloc[tid]);
            sh.c[tid].init(1u << sh.bbits, &sh.alloc[tid]);
            const int shift = 64 - int(sh.bbits);
            for (const RecC& click : cx.pending) {
                const u64 name = sh.alias.person(click.person);
                sh.c[tid].push(u32(hash64(name) >> shift), RecC{name, click.ckey, click.ts, click.camp});
            }
            dedup_buckets(sh, tid);
            dense_group_clicks(sh, tid);
            sh.c[tid].finish();
        });
        dense_gather_extras();
        t3 = now();
        // Phase 3: events by number. Phase 4: the stretches that cross partitions, then the general people phase.
        c3 = run_threads(T, [&](u32 tid) {
            dense_walk(sh, tid);
            sh.b[tid].finish();
        });
        const double w1 = now();
        dense_groups();
        order_buckets(sh.b);
        const double w2 = now();
        D.wall[1] = w1 - t3;
        D.wall[2] = w2 - w1;
        c3 += run_threads(T, [&](u32 tid) {
            dense_stitch(sh, tid);
            dense_ctx_finish(sh, D.ctx[tid]);
            process_people(sh, tid, 0);
        });
        const double w3 = now();
        dense_leftovers(sh);
        t4 = now();
        D.wall[3] = w3 - w2;
        if (env("SS_DENSE_BAIL", "") == "1") dense_bail(9);
        if (D.bail.load()) dense_restart(sh, true);
    } else
#endif
    {
    c1 = run_threads(T, [&](u32 tid) {
        Reader reader;
        sh.a[tid].init(1u << sh.abits, &sh.alloc[tid]);
        if (tid == 0) parse_links(sh, reader);
        parse_events(sh, tid, reader);
        sh.a[tid].finish();
        sh.c[tid].init(1u << sh.bbits, &sh.alloc[tid]);
        parse_clicks(sh, tid, reader);
        sh.c[tid].finish();
    });
    t2 = now();

    // SS_T2 / SS_T3: fewer threads for the dedup or the people phase (measurement).
    const u32 T2 = std::min<u32>(T, std::max<u32>(1, u32(atoi(env("SS_T2", "0").c_str())) ? u32(atoi(env("SS_T2", "0").c_str())) : T));
    const u32 T3 = std::min<u32>(T, std::max<u32>(1, u32(atoi(env("SS_T3", "0").c_str())) ? u32(atoi(env("SS_T3", "0").c_str())) : T));
    for (u32 t = T2; t < T; t++) { sh.b[t].init(1u << sh.bbits, &sh.alloc[t]); sh.b[t].finish(); }
    c2 = run_threads(T2, [&](u32 tid) {
        sh.b[tid].init(1u << sh.bbits, &sh.alloc[tid]);
        dedup_buckets(sh, tid);
        sh.b[tid].finish();
    });
    t3 = now();

    order_buckets(sh.b);
#ifdef SS_ALIAS1
    c3 = run_threads(T3, [&](u32 tid) { process_people(sh, tid, 0); });
#else
    for (u32 t = T3; t < T; t++) {
        sh.lb[t].init(1u << sh.bbits, &sh.alloc[t]); sh.lb[t].finish();
        sh.lc[t].init(1u << sh.bbits, &sh.alloc[t]); sh.lc[t].finish();
    }
    c3 = run_threads(T3, [&](u32 tid) {
        sh.lb[tid].init(1u << sh.bbits, &sh.alloc[tid]);
        sh.lc[tid].init(1u << sh.bbits, &sh.alloc[tid]);
        process_people(sh, tid, 0);
        sh.lb[tid].finish();
        sh.lc[tid].finish();
    });
    order_buckets(sh.lb);
    c3 += run_threads(T3, [&](u32 tid) { process_people(sh, tid, 1); });
#endif
    t4 = now();
    }

    write_campaigns(sh);
    if (sh.user_rows.load() == 0) {                       // an empty table still needs a file with the schema
        pq::File file;
        file.open(cfg.out + "/user_daily/part-000.parquet", UserWriter::schema());
        file.close();
    }
    double t5 = now();
    size_t mapped = 0;
    for (const InputFile& f : sh.files) mapped += f.map != nullptr;
    rusage ru;
    getrusage(RUSAGE_SELF, &ru);
    fprintf(stderr, "user %.2f sys %.2f faults %ld/%ld maxrss %ld MB\n",
            double(ru.ru_utime.tv_sec) + double(ru.ru_utime.tv_usec) * 1e-6, double(ru.ru_stime.tv_sec) + double(ru.ru_stime.tv_usec) * 1e-6,
            long(ru.ru_minflt), long(ru.ru_majflt), long(ru.ru_maxrss >> 10));
#ifdef SS_DIGEST
    fprintf(stderr, "digest users %016llx campaigns %016llx rows %llu\n", (unsigned long long)g_digest_users.load(), (unsigned long long)g_digest_campaigns.load(), (unsigned long long)sh.user_rows.load());
#endif
    fprintf(stderr, "threads %u buckets %u | setup %.3f parse %.3f dedup %.3f people %.3f write %.3f | total %.3f s | cpu parse %.2f dedup %.2f people %.2f | user rows %llu | mapped %zu of %zu | links parse %.3f union %.3f | links %.3f\n",
            T, 1u << sh.abits, t1 - t0, t2 - t1, t3 - t2, t4 - t3, t5 - t4, t5 - t0, c1, c2, c3, (unsigned long long)sh.user_rows.load(), mapped, sh.files.size(), sh.links_parse, sh.links_union, sh.links_seconds);
    {
        double sum[12] = {};
        for (u32 t = 0; t < T && t < 256; t++) for (int k = 0; k < 12; k++) sum[k] += g_steps[t][k];
        fprintf(stderr, "steps | dedup table %.3f move %.3f | people key %.3f order %.3f clicks %.3f walk %.3f finish %.3f | people %.0f | linked events %.0f ids %.0f\n",
                sum[0], sum[1], sum[2], sum[3], sum[4], sum[5], sum[6], sum[7], sum[8], sum[9]);
    }
#ifdef SS_DENSE_BUILD
    if (dense) {
        Dense& D = g_dense;
        u64 people = 0, split = 0, sorted = 0, handed = 0, stitched = 0, odd_users = 0, odd_clicks = 0, pending = 0, marked = 0;
        double cpu[4] = {};
        for (const DenseCtx& cx : D.ctx) {
            people += cx.n_people; split += cx.n_split; sorted += cx.n_sorted; handed += cx.n_handed; stitched += cx.n_stitched;
            odd_users += cx.odd_users.size(); odd_clicks += cx.odd_clicks.size(); pending += cx.pending.size(); marked += cx.marked.size();
            for (int k = 0; k < 4; k++) cpu[k] += cx.cpu[k];
        }
        fprintf(stderr, "dense keys e %llx u %llx\n", (unsigned long long)D.ekey, (unsigned long long)D.ukey);
        fprintf(stderr, "dense | setup %.4f | numbers %llu parts %u people %u | walked %llu split %llu sorted %llu stitched events %llu groups %zu | handed events %llu odd users %llu odd clicks %llu pending clicks %llu marked %llu | cpu place %.3f sweep %.3f walk %.3f stitch %.3f | wall settle %.4f walk %.4f join %.4f stitch %.4f\n",
                D.setup_seconds, (unsigned long long)D.nidx, D.nparts, D.pmax, (unsigned long long)people, (unsigned long long)split, (unsigned long long)sorted,
                (unsigned long long)stitched, D.gstart.size(), (unsigned long long)handed, (unsigned long long)odd_users, (unsigned long long)odd_clicks,
                (unsigned long long)pending, (unsigned long long)marked, cpu[0], cpu[1], cpu[2], cpu[3], D.wall[0], D.wall[1], D.wall[2], D.wall[3]);
    }
#endif
#ifdef __linux__
    if (__gcov_dump) __gcov_dump();
#endif
    if (getenv("SS_ZAP")) {
        // Hand the record arena back on all threads instead of leaving it to a single-threaded exit.
        std::vector<std::pair<u8*, size_t>> pieces;
        for (auto& region : g_arena_regions)
            for (size_t at = 0; at < region.second; at += 16u << 20)
                pieces.emplace_back(region.first + at, std::min<size_t>(16u << 20, region.second - at));
        std::atomic<size_t> next{0};
        run_threads(T, [&](u32) {
            for (;;) {
                size_t i = next.fetch_add(1);
                if (i >= pieces.size()) return;
                madvise(pieces[i].first, pieces[i].second, MADV_DONTNEED);
            }
        });
    }
    _exit(0);
}
