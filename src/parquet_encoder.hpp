// Minimal Parquet writer: v1 data pages, PLAIN, UNCOMPRESSED, one page per
// column chunk, any number of row groups. No statistics, CRC or indexes.
// Ported from research/parquet_proto/mini_parquet.py, which round-trips in
// polars, pyarrow and DuckDB.
#pragma once
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>
#include <vector>
#include <fcntl.h>
#include <sys/uio.h>
#include <unistd.h>

namespace pq {

enum PType { INT32 = 1, INT64 = 2, BYTE_ARRAY = 6 };
enum Conv { NONE = -1, UTF8 = 0, DATE = 6 };

struct Col {
    const char* name;
    PType type;
    Conv conv;
    bool optional;
};

// One column chunk's bytes for a row group.
struct Part {
    const void* levels = nullptr;   // optional columns: RLE/bit-packed hybrid def levels (no length prefix)
    size_t levels_len = 0;
    const void* values = nullptr;   // PLAIN-encoded values (non-null only)
    size_t values_len = 0;
};

// Thrift compact protocol.
struct Thrift {
    std::vector<uint8_t> b;
    int last[8];
    int depth = 0;
    Thrift() { last[0] = 0; }
    void varint(uint64_t n) {
        while (n >= 0x80) { b.push_back(uint8_t((n & 0x7F) | 0x80)); n >>= 7; }
        b.push_back(uint8_t(n));
    }
    void header(int id, int type) {
        int delta = id - last[depth];
        if (delta > 0 && delta <= 15) b.push_back(uint8_t((delta << 4) | type));
        else { b.push_back(uint8_t(type)); varint(uint64_t((uint32_t(id) << 1) ^ uint32_t(id >> 15))); }
        last[depth] = id;
    }
    void i32(int id, int32_t v) { header(id, 5); varint(uint32_t((uint32_t(v) << 1) ^ uint32_t(v >> 31))); }
    void i64(int id, int64_t v) { header(id, 6); varint((uint64_t(v) << 1) ^ uint64_t(v >> 63)); }
    void str(int id, const char* s) { header(id, 8); raw_str(s); }
    void raw_str(const char* s) { size_t n = strlen(s); varint(n); b.insert(b.end(), s, s + n); }
    void raw_i32(int32_t v) { varint(uint32_t((uint32_t(v) << 1) ^ uint32_t(v >> 31))); }
    void begin_struct(int id) { header(id, 12); last[++depth] = 0; }
    void begin_elem() { last[++depth] = 0; }          // a struct inside a list
    void end_struct() { b.push_back(0); depth--; }
    void begin_list(int id, size_t n, int elem_type) {
        header(id, 9);
        if (n < 15) b.push_back(uint8_t((n << 4) | elem_type));
        else { b.push_back(uint8_t(0xF0 | elem_type)); varint(n); }
    }
};

struct File {
    struct Chunk { uint64_t offset, size, values; };
    struct Group { uint64_t rows, bytes; std::vector<Chunk> chunks; };

    int fd = -1;
    uint64_t pos = 0;
    uint64_t rows = 0;
    std::vector<Col> cols;
    std::vector<Group> groups;

    static void fail(const char* what) { perror(what); _exit(4); }

    void write_all(const struct iovec* iov_in, int count) {
        struct iovec iov[40];
        memcpy(iov, iov_in, sizeof(struct iovec) * count);
        int at = 0;
        while (at < count) {
            ssize_t w = writev(fd, iov + at, count - at);
            if (w < 0) { if (errno == EINTR) continue; fail("writev"); }
            pos += uint64_t(w);
            while (at < count && size_t(w) >= iov[at].iov_len) { w -= iov[at].iov_len; at++; }
            if (at < count) { iov[at].iov_base = (char*)iov[at].iov_base + w; iov[at].iov_len -= w; }
        }
    }

    void open(const std::string& path, std::vector<Col> columns) {
        cols = std::move(columns);
        fd = ::open(path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (fd < 0) fail(path.c_str());
        struct iovec iov = {(void*)"PAR1", 4};
        write_all(&iov, 1);
    }

    // parts[i] holds column i's bytes for `n` rows. The whole row group goes out in one write.
    void row_group(uint64_t n, const Part* parts) {
        enum { MAX_COLS = 8 };
        if (cols.size() > MAX_COLS) fail("too many columns");
        Group group;
        group.rows = n;
        group.bytes = 0;
        Thrift headers[MAX_COLS];
        uint32_t level_len[MAX_COLS];
        struct iovec iov[MAX_COLS * 4];
        int k = 0;
        uint64_t at = pos;
        for (size_t c = 0; c < cols.size(); c++) {
            const Part& part = parts[c];
            level_len[c] = uint32_t(part.levels_len);
            size_t body = part.values_len + (cols[c].optional ? 4 + part.levels_len : 0);
            Thrift& h = headers[c];
            h.i32(1, 0);                       // DATA_PAGE
            h.i32(2, int32_t(body));           // uncompressed size
            h.i32(3, int32_t(body));           // compressed size
            h.begin_struct(5);                 // DataPageHeader
            h.i32(1, int32_t(n));              // num_values, nulls included
            h.i32(2, 0);                       // PLAIN
            h.i32(3, 3);                       // definition levels: RLE
            h.i32(4, 3);                       // repetition levels: RLE
            h.end_struct();
            h.b.push_back(0);
            Chunk chunk{at, h.b.size() + body, n};
            iov[k++] = {h.b.data(), h.b.size()};
            if (cols[c].optional) {
                iov[k++] = {&level_len[c], 4};
                if (part.levels_len) iov[k++] = {(void*)part.levels, part.levels_len};
            }
            if (part.values_len) iov[k++] = {(void*)part.values, part.values_len};
            at += chunk.size;
            group.bytes += chunk.size;
            group.chunks.push_back(chunk);
        }
        write_all(iov, k);
        rows += n;
        groups.push_back(std::move(group));
    }

    void close() {
        Thrift m;
        m.i32(1, 1);                                           // version
        m.begin_list(2, cols.size() + 1, 12);                  // schema
        m.begin_elem();
        m.str(4, "schema");
        m.i32(5, int32_t(cols.size()));
        m.end_struct();
        for (const Col& col : cols) {
            m.begin_elem();
            m.i32(1, col.type);
            m.i32(3, col.optional ? 1 : 0);
            m.str(4, col.name);
            if (col.conv != NONE) {
                m.i32(6, col.conv);
                m.begin_struct(10);                            // LogicalType
                m.begin_struct(col.conv == UTF8 ? 1 : 6);      // STRING or DATE
                m.end_struct();
                m.end_struct();
            }
            m.end_struct();
        }
        m.i64(3, int64_t(rows));
        m.begin_list(4, groups.size(), 12);
        for (const Group& group : groups) {
            m.begin_elem();
            m.begin_list(1, cols.size(), 12);
            for (size_t c = 0; c < cols.size(); c++) {
                const Chunk& chunk = group.chunks[c];
                m.begin_elem();
                m.i64(2, int64_t(chunk.offset));               // file_offset
                m.begin_struct(3);                             // ColumnMetaData
                m.i32(1, cols[c].type);
                if (cols[c].optional) { m.begin_list(2, 2, 5); m.raw_i32(0); m.raw_i32(3); }
                else { m.begin_list(2, 1, 5); m.raw_i32(0); }
                m.begin_list(3, 1, 8);
                m.raw_str(cols[c].name);
                m.i32(4, 0);                                   // UNCOMPRESSED
                m.i64(5, int64_t(chunk.values));
                m.i64(6, int64_t(chunk.size));
                m.i64(7, int64_t(chunk.size));
                m.i64(9, int64_t(chunk.offset));               // data_page_offset
                m.end_struct();
                m.end_struct();
            }
            m.i64(2, int64_t(group.bytes));
            m.i64(3, int64_t(group.rows));
            m.end_struct();
        }
        m.str(6, "shopping-sessions");
        m.b.push_back(0);
        uint32_t len = uint32_t(m.b.size());
        struct iovec iov[3] = {{m.b.data(), m.b.size()}, {&len, 4}, {(void*)"PAR1", 4}};
        write_all(iov, 3);
        ::close(fd);
        fd = -1;
    }
};

}  // namespace pq
