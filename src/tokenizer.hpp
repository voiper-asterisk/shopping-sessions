// The line-at-a-time CSV reader: part of main.cpp (included there once).
#ifndef SS_LINES_H
#define SS_LINES_H

// The parsers read one line at a time. A line of the usual shape is
// settled from the comma, newline and quote bits of one or two 64-byte windows:
// the separators come out by position, and the quote rules are checked with the
// same carry trick as scan_rows (an opening quote directly after a separator, a
// closing one directly before the next, none elsewhere). Any other line (too
// long for its windows, the wrong number of fields, a quote out of place) is
// decided byte by byte, so the result is exact.
template <int NF> struct Fields { const u8* s[NF]; const u8* e[NF]; };      // each field's content, quotes removed

// One bit for every place where the quote rules are broken, given the
// separators S and quotes Q of a window that starts at the start of a field.
// A fault always shows at or before the separator that ends its field.
static inline SS_HOT u64 quote_faults(u64 S, u64 Q) {
    const u64 first = (S << 1) | 1;
    const u64 open = Q & first, other = Q & ~first;
    const u64 u = open << 1, t = other << 1;
    const u64 sum = ~S + open;
    return (t & ~S) | (u & S) | ((sum ^ (t | u)) & S);
}

// Decides a line byte by byte: [a, end) without its newline. False if malformed.
template <int NF>
static bool split_exact(const u8* a, const u8* end, Fields<NF>& f) {
    int n = 0;
    for (;;) {
        const u8* comma = (const u8*)memchr(a, ',', size_t(end - a));
        const u8* stop_at = comma ? comma : end;
        if (n == NF) return false;
        const u8 *s = a, *e = stop_at;
        if (memchr(a, '"', size_t(stop_at - a))) {
            if (stop_at - a < 2 || a[0] != '"' || stop_at[-1] != '"' || memchr(a + 1, '"', size_t(stop_at - a - 2))) return false;
            s = a + 1;
            e = stop_at - 1;
        }
        f.s[n] = s;
        f.e[n] = e;
        n++;
        if (!comma) break;
        a = comma + 1;
    }
    return n == NF;
}

// An event line: the first three fields end within 64 bytes of the line's
// start, the other four and the newline within 64 bytes of the fourth's start.
static inline SS_HOT bool line7(const u8* p, Fields<7>& f, const u8*& next) {
    const Masks A = classify(p);
    u64 ca = A.comma;
    const u32 c1 = tz64(ca); ca &= ca - 1;
    const u32 c2 = tz64(ca); ca &= ca - 1;
    const u32 c3 = tz64(ca);
    if (c3 >= 64 || (A.nl & ((1ULL << c3) - 1))) return false;
    const u8* q = p + c3 + 1;
    const Masks B = classify(q);
    u64 cb = B.comma;
    const u32 c4 = tz64(cb); cb &= cb - 1;
    const u32 c5 = tz64(cb); cb &= cb - 1;
    const u32 c6 = tz64(cb); cb &= cb - 1;
    const u32 c7 = tz64(cb);
    const u32 e = tz64(B.nl);
    const u64 QA = A.quote, QB = B.quote;
    const u64 bad_a = quote_faults(A.comma | A.nl, QA) & (~0ULL >> (63 - c3));
    const u64 bad_b = quote_faults(B.comma | B.nl, QB) & (~0ULL >> ((63 - e) & 63));
    if ((c6 >= e) | (e >= 64) | (c7 <= e) | ((bad_a | bad_b) != 0)) return false;
    const u32 o0 = u32(QA & 1), o1 = u32(QA >> (c1 + 1)) & 1, o2 = u32(QA >> (c2 + 1)) & 1;
    const u32 o3 = u32(QB & 1), o4 = u32(QB >> (c4 + 1)) & 1, o5 = u32(QB >> (c5 + 1)) & 1, o6 = u32(QB >> (c6 + 1)) & 1;
    f.s[0] = p + o0;          f.e[0] = p + c1 - o0;
    f.s[1] = p + c1 + 1 + o1; f.e[1] = p + c2 - o1;
    f.s[2] = p + c2 + 1 + o2; f.e[2] = p + c3 - o2;
    f.s[3] = q + o3;          f.e[3] = q + c4 - o3;
    f.s[4] = q + c4 + 1 + o4; f.e[4] = q + c5 - o4;
    f.s[5] = q + c5 + 1 + o5; f.e[5] = q + c6 - o5;
    f.s[6] = q + c6 + 1 + o6; f.e[6] = q + e - o6;
    next = q + e + 1;
    return true;
}

// A click line: all four fields and the newline within 64 bytes.
static inline SS_HOT bool line4(const u8* p, Fields<4>& f, const u8*& next) {
    const Masks A = classify(p);
    u64 ca = A.comma;
    const u32 c1 = tz64(ca); ca &= ca - 1;
    const u32 c2 = tz64(ca); ca &= ca - 1;
    const u32 c3 = tz64(ca); ca &= ca - 1;
    const u32 c4 = tz64(ca);
    const u32 e = tz64(A.nl);
    const u64 Q = A.quote;
    const u64 bad = quote_faults(A.comma | A.nl, Q) & (~0ULL >> ((63 - e) & 63));
    if ((c3 >= e) | (e >= 64) | (c4 <= e) | (bad != 0)) return false;
    const u32 o0 = u32(Q & 1), o1 = u32(Q >> (c1 + 1)) & 1, o2 = u32(Q >> (c2 + 1)) & 1, o3 = u32(Q >> (c3 + 1)) & 1;
    f.s[0] = p + o0;          f.e[0] = p + c1 - o0;
    f.s[1] = p + c1 + 1 + o1; f.e[1] = p + c2 - o1;
    f.s[2] = p + c2 + 1 + o2; f.e[2] = p + c3 - o2;
    f.s[3] = p + c3 + 1 + o3; f.e[3] = p + e - o3;
    next = p + e + 1;
    return true;
}

// A link line: both fields and the newline within 64 bytes.
static inline SS_HOT bool line2(const u8* p, Fields<2>& f, const u8*& next) {
    const Masks A = classify(p);
    u64 ca = A.comma;
    const u32 c1 = tz64(ca); ca &= ca - 1;
    const u32 c2 = tz64(ca);
    const u32 e = tz64(A.nl);
    const u64 Q = A.quote;
    const u64 bad = quote_faults(A.comma | A.nl, Q) & (~0ULL >> ((63 - e) & 63));
    if ((c1 >= e) | (e >= 64) | (c2 <= e) | (bad != 0)) return false;
    const u32 o0 = u32(Q & 1), o1 = u32(Q >> (c1 + 1)) & 1;
    f.s[0] = p + o0;          f.e[0] = p + c1 - o0;
    f.s[1] = p + c1 + 1 + o1; f.e[1] = p + e - o1;
    next = p + e + 1;
    return true;
}

// The same three readers for a line whose newline is already known (used by the
// batched variants, which find the newlines of a group of lines first). Only
// commas and quotes are looked for.
// An event line read through three windows, for IDs of any usual length: the
// first holds the two IDs, the second ts, ingest_ts and the type, the third the
// product and the price. (Two long IDs do not fit one window: then the second
// ID gets a window of its own.)
static inline SS_HOT bool line7w(const u8* p, Fields<7>& f, const u8*& next) {
    const Masks A = classify(p);
    u64 ca = A.comma;
    const u32 c1 = tz64(ca); ca &= ca - 1;
    u32 c2 = tz64(ca);
    if (c1 >= 63) return false;
    u64 bad = 0;
    const u8* second = p + c1 + 1;                               // where the second field starts
    u32 o1;
    const u8* q;                                                 // where the third field starts
    if (__builtin_expect(c2 < 64, 1)) {
        if (A.nl & ((1ULL << c2) - 1)) return false;
        bad = quote_faults(A.comma | A.nl, A.quote) & (~0ULL >> (63 - c2));
        o1 = u32(A.quote >> (c1 + 1)) & 1;
        q = p + c2 + 1;
        c2 -= c1 + 1;                                            // from here on counted from `second`
    } else {
        if (A.nl & ((1ULL << c1) - 1)) return false;
        const Masks A2 = classify(second);
        c2 = tz64(A2.comma);
        if (c2 >= 64 || (A2.nl & ((1ULL << c2) - 1))) return false;
        bad = (quote_faults(A.comma | A.nl, A.quote) & (~0ULL >> (63 - c1))) | (quote_faults(A2.comma | A2.nl, A2.quote) & (~0ULL >> (63 - c2)));
        o1 = u32(A2.quote & 1);
        q = second + c2 + 1;
    }
    const Masks B = classify(q);
    u64 cb = B.comma;
    const u32 c3 = tz64(cb); cb &= cb - 1;
    const u32 c4 = tz64(cb); cb &= cb - 1;
    const u32 c5 = tz64(cb);
    if (c5 >= 64 || (B.nl & ((1ULL << c5) - 1))) return false;
    bad |= quote_faults(B.comma | B.nl, B.quote) & (~0ULL >> (63 - c5));
    const u8* r = q + c5 + 1;
    const Masks C = classify(r);
    u64 cc = C.comma;
    const u32 c6 = tz64(cc); cc &= cc - 1;
    const u32 c7 = tz64(cc);
    const u32 e = tz64(C.nl);
    bad |= quote_faults(C.comma | C.nl, C.quote) & (~0ULL >> ((63 - e) & 63));
    if ((c6 >= e) | (e >= 64) | (c7 <= e) | (bad != 0)) return false;
    const u64 QA = A.quote, QB = B.quote, QC = C.quote;
    const u32 o0 = u32(QA & 1);
    const u32 o2 = u32(QB & 1), o3 = u32(QB >> (c3 + 1)) & 1, o4 = u32(QB >> (c4 + 1)) & 1;
    const u32 o5 = u32(QC & 1), o6 = u32(QC >> (c6 + 1)) & 1;
    f.s[0] = p + o0;          f.e[0] = p + c1 - o0;
    f.s[1] = second + o1;     f.e[1] = second + c2 - o1;
    f.s[2] = q + o2;          f.e[2] = q + c3 - o2;
    f.s[3] = q + c3 + 1 + o3; f.e[3] = q + c4 - o3;
    f.s[4] = q + c4 + 1 + o4; f.e[4] = q + c5 - o4;
    f.s[5] = r + o5;          f.e[5] = r + c6 - o5;
    f.s[6] = r + c6 + 1 + o6; f.e[6] = r + e - o6;
    next = r + e + 1;
    return true;
}

// A click line through two windows: the two IDs, then the campaign and the time.
static inline SS_HOT bool line4w(const u8* p, Fields<4>& f, const u8*& next) {
    const Masks A = classify(p);
    u64 ca = A.comma;
    const u32 c1 = tz64(ca); ca &= ca - 1;
    u32 c2 = tz64(ca);
    if (c1 >= 63) return false;
    u64 bad = 0;
    const u8* second = p + c1 + 1;
    u32 o1;
    const u8* q;
    if (__builtin_expect(c2 < 64, 1)) {
        if (A.nl & ((1ULL << c2) - 1)) return false;
        bad = quote_faults(A.comma | A.nl, A.quote) & (~0ULL >> (63 - c2));
        o1 = u32(A.quote >> (c1 + 1)) & 1;
        q = p + c2 + 1;
        c2 -= c1 + 1;
    } else {
        if (A.nl & ((1ULL << c1) - 1)) return false;
        const Masks A2 = classify(second);
        c2 = tz64(A2.comma);
        if (c2 >= 64 || (A2.nl & ((1ULL << c2) - 1))) return false;
        bad = (quote_faults(A.comma | A.nl, A.quote) & (~0ULL >> (63 - c1))) | (quote_faults(A2.comma | A2.nl, A2.quote) & (~0ULL >> (63 - c2)));
        o1 = u32(A2.quote & 1);
        q = second + c2 + 1;
    }
    const Masks B = classify(q);
    u64 cb = B.comma;
    const u32 c3 = tz64(cb); cb &= cb - 1;
    const u32 c4 = tz64(cb);
    const u32 e = tz64(B.nl);
    bad |= quote_faults(B.comma | B.nl, B.quote) & (~0ULL >> ((63 - e) & 63));
    if ((c3 >= e) | (e >= 64) | (c4 <= e) | (bad != 0)) return false;
    const u32 o0 = u32(A.quote & 1), o2 = u32(B.quote & 1), o3 = u32(B.quote >> (c3 + 1)) & 1;
    f.s[0] = p + o0;          f.e[0] = p + c1 - o0;
    f.s[1] = second + o1;     f.e[1] = second + c2 - o1;
    f.s[2] = q + o2;          f.e[2] = q + c3 - o2;
    f.s[3] = q + c3 + 1 + o3; f.e[3] = q + e - o3;
    next = q + e + 1;
    return true;
}

struct CommaQuote { u64 comma, quote; };
static inline SS_HOT CommaQuote comma_quote(const u8* p) {
#if defined(__AVX2__)
    const __m256i a = _mm256_loadu_si256((const __m256i*)p), b = _mm256_loadu_si256((const __m256i*)(p + 32));
    const __m256i c = _mm256_set1_epi8(','), q = _mm256_set1_epi8('"');
    CommaQuote m;
    m.comma = u32(_mm256_movemask_epi8(_mm256_cmpeq_epi8(a, c))) | (u64(u32(_mm256_movemask_epi8(_mm256_cmpeq_epi8(b, c)))) << 32);
    m.quote = u32(_mm256_movemask_epi8(_mm256_cmpeq_epi8(a, q))) | (u64(u32(_mm256_movemask_epi8(_mm256_cmpeq_epi8(b, q)))) << 32);
    return m;
#else
    const Masks all = classify(p);
    return {all.comma, all.quote};
#endif
}
static inline SS_HOT u64 newline_mask(const u8* p) {
#if defined(__AVX2__)
    const __m256i n = _mm256_set1_epi8('\n');
    return u32(_mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i*)p), n))) |
           (u64(u32(_mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256((const __m256i*)(p + 32)), n)))) << 32);
#else
    return classify(p).nl;
#endif
}

// [p, nl) is the line, nl its newline (the first one at or after p).
static inline SS_HOT bool line_known(const u8* p, const u8* nl, Fields<7>& f) {
    const size_t len = size_t(nl - p);
    const CommaQuote A = comma_quote(p);
    u64 ca = A.comma;
    const u32 c1 = tz64(ca); ca &= ca - 1;
    const u32 c2 = tz64(ca); ca &= ca - 1;
    const u32 c3 = tz64(ca);
    if (c3 >= 64 || c3 >= len) return false;                    // the third comma within 64 bytes and within the line
    const u8* q = p + c3 + 1;
    const size_t e = size_t(nl - q);
    if (e >= 64) return false;
    const CommaQuote B = comma_quote(q);
    u64 cb = B.comma;
    const u32 c4 = tz64(cb); cb &= cb - 1;
    const u32 c5 = tz64(cb); cb &= cb - 1;
    const u32 c6 = tz64(cb); cb &= cb - 1;
    const u32 c7 = tz64(cb);
    const u64 QA = A.quote, QB = B.quote;
    const u64 bad_a = quote_faults(A.comma, QA) & (~0ULL >> (63 - c3));
    const u64 bad_b = quote_faults(B.comma | (1ULL << e), QB) & (~0ULL >> (63 - e));
    if ((c6 >= e) | (c7 <= e) | ((bad_a | bad_b) != 0)) return false;
    const u32 o0 = u32(QA & 1), o1 = u32(QA >> (c1 + 1)) & 1, o2 = u32(QA >> (c2 + 1)) & 1;
    const u32 o3 = u32(QB & 1), o4 = u32(QB >> (c4 + 1)) & 1, o5 = u32(QB >> (c5 + 1)) & 1, o6 = u32(QB >> (c6 + 1)) & 1;
    f.s[0] = p + o0;          f.e[0] = p + c1 - o0;
    f.s[1] = p + c1 + 1 + o1; f.e[1] = p + c2 - o1;
    f.s[2] = p + c2 + 1 + o2; f.e[2] = p + c3 - o2;
    f.s[3] = q + o3;          f.e[3] = q + c4 - o3;
    f.s[4] = q + c4 + 1 + o4; f.e[4] = q + c5 - o4;
    f.s[5] = q + c5 + 1 + o5; f.e[5] = q + c6 - o5;
    f.s[6] = q + c6 + 1 + o6; f.e[6] = nl - o6;
    return true;
}
static inline SS_HOT bool line_known(const u8* p, const u8* nl, Fields<4>& f) {
    const size_t e = size_t(nl - p);
    if (e >= 64) return false;
    const CommaQuote A = comma_quote(p);
    u64 ca = A.comma;
    const u32 c1 = tz64(ca); ca &= ca - 1;
    const u32 c2 = tz64(ca); ca &= ca - 1;
    const u32 c3 = tz64(ca); ca &= ca - 1;
    const u32 c4 = tz64(ca);
    const u64 Q = A.quote;
    const u64 bad = quote_faults(A.comma | (1ULL << e), Q) & (~0ULL >> (63 - e));
    if ((c3 >= e) | (c4 <= e) | (bad != 0)) return false;
    const u32 o0 = u32(Q & 1), o1 = u32(Q >> (c1 + 1)) & 1, o2 = u32(Q >> (c2 + 1)) & 1, o3 = u32(Q >> (c3 + 1)) & 1;
    f.s[0] = p + o0;          f.e[0] = p + c1 - o0;
    f.s[1] = p + c1 + 1 + o1; f.e[1] = p + c2 - o1;
    f.s[2] = p + c2 + 1 + o2; f.e[2] = p + c3 - o2;
    f.s[3] = p + c3 + 1 + o3; f.e[3] = nl - o3;
    return true;
}
static inline SS_HOT bool line_known(const u8* p, const u8* nl, Fields<2>& f) {
    const size_t e = size_t(nl - p);
    if (e >= 64) return false;
    const CommaQuote A = comma_quote(p);
    u64 ca = A.comma;
    const u32 c1 = tz64(ca); ca &= ca - 1;
    const u32 c2 = tz64(ca);
    const u64 Q = A.quote;
    const u64 bad = quote_faults(A.comma | (1ULL << e), Q) & (~0ULL >> (63 - e));
    if ((c1 >= e) | (c2 <= e) | (bad != 0)) return false;
    const u32 o0 = u32(Q & 1), o1 = u32(Q >> (c1 + 1)) & 1;
    f.s[0] = p + o0;          f.e[0] = p + c1 - o0;
    f.s[1] = p + c1 + 1 + o1; f.e[1] = nl - o1;
    return true;
}

// Calls rows(fields, n) with the well-formed lines of NF fields that start in
// [start, stop), up to GROUP at a time: the caller then works through a group
// one step at a time (all the timestamps, then all the IDs, ...), so that each
// step is a short loop whose turns do not wait on one another.
template <int NF, class Rows>
static inline SS_INLINE void line_groups(const u8* buf, size_t start, size_t stop, size_t last, Rows&& rows) {
    enum { BATCH = 64, GROUP = 32 };
    u32 ends[BATCH + 72];
    Fields<NF> fs[GROUP];
    size_t block = start, line = start;
    u32 n = 0;
    for (;;) {
        u32 count = 0;
        while (count < BATCH && block <= last) {
            __builtin_prefetch(buf + block + 1024);
            u64 nl = newline_mask(buf + block);
            const u32 found = u32(__builtin_popcountll(nl));
            ends[count] = u32(block) + tz64(nl); nl &= nl - 1;
            ends[count + 1] = u32(block) + tz64(nl); nl &= nl - 1;
            for (u32 i = 2; i < found; i++) { ends[count + i] = u32(block) + tz64(nl); nl &= nl - 1; }
            count += found;
            block += 64;
        }
        for (u32 k = 0; k < count; k++) {
            if (line >= stop) break;
            const u8* p = buf + line;
            const u8* nl = buf + ends[k];
            line = size_t(ends[k]) + 1;
            if (__builtin_expect(!line_known(p, nl, fs[n]), 0) && !split_exact<NF>(p, nl, fs[n])) continue;
            if (++n == GROUP) { rows(fs, n); n = 0; }
        }
        if (line >= stop || block > last) break;
    }
    if (n) rows(fs, n);
}

// Calls row(fields) for every well-formed line of NF fields that starts in
// [start, stop); `last` is the newline that ends the last such line, with 64
// readable bytes after it (as for scan_rows).
template <int NF, class Fast, class Row>
static inline SS_INLINE void line_rows(const u8* buf, size_t start, size_t stop, size_t last, Fast&& fast, Row&& row) {
#ifdef SS_LINE0
    // Measurement: the block scanner underneath, as before.
    (void)fast;
    scan_rows<NF>(buf, start, stop, last, [&](ROW_PARAMS) SS_INLINE {
        Fields<NF> f;
        u32 quoted[NF];
        for (int i = 0; i < NF; i++) quoted[i] = u32(buf[a[i] - 1] == '"');
        for (int i = 0; i < NF; i++) {
            f.s[i] = buf + a[i];
            f.e[i] = i + 1 < NF ? buf + a[i + 1] - quoted[i + 1] - 1 - quoted[i] : buf + nlpos - quoted[i];
        }
        row(f);
    });
#elif defined(SS_LINE2)
    // Measurement: the newlines of a batch of lines first, 64 bytes at a time; then
    // each line of the batch on its own, its end already known. On the scoring
    // machine this is slower than reading line after line (the default below).
    (void)fast;
    enum { BATCH = 64 };
    u32 ends[BATCH + 72];
    size_t block = start, line = start;
    for (;;) {
        u32 count = 0;
        while (count < BATCH && block <= last) {
            __builtin_prefetch(buf + block + 1024);
            u64 nl = newline_mask(buf + block);
            const u32 found = u32(__builtin_popcountll(nl));
            ends[count] = u32(block) + tz64(nl); nl &= nl - 1;
            ends[count + 1] = u32(block) + tz64(nl); nl &= nl - 1;
            for (u32 i = 2; i < found; i++) { ends[count + i] = u32(block) + tz64(nl); nl &= nl - 1; }
            count += found;
            block += 64;
        }
        for (u32 k = 0; k < count; k++) {
            if (line >= stop) return;
            const u8* p = buf + line;
            const u8* nl = buf + ends[k];
            line = size_t(ends[k]) + 1;
            Fields<NF> f;
            if (__builtin_expect(!line_known(p, nl, f), 0) && !split_exact<NF>(p, nl, f)) continue;
            row(f);
        }
        if (block > last) return;                               // every newline up to `last` has been used
    }
#else
    const u8* p = buf + start;
    const u8* const stop_at = buf + stop;
    const u8* const last_nl = buf + last;
    while (p < stop_at) {
#ifndef SS_LPF
#define SS_LPF 1024                  // how far ahead of the line being read the input is asked for
#endif
#if SS_LPF > 0
        __builtin_prefetch(p + SS_LPF);
#endif
        Fields<NF> f;
        const u8* next;
        if (__builtin_expect(!fast(p, f, next), 0)) {
            const u8* nl = (const u8*)memchr(p, '\n', size_t(last_nl - p) + 1);    // found: `last` is a newline
            next = nl + 1;
            if (!split_exact<NF>(p, nl, f)) { p = next; continue; }
        }
        p = next;
        row(f);
    }
#endif
}


#endif // SS_LINES_H
