//! Content-defined chunking with a small-file fast path.

use fastcdc::v2020::FastCDC;

/// Files up to this size are stored as a single chunk.
pub const SMALL_FILE_MAX: usize = 64 * 1024;
pub const MIN_CHUNK: u32 = 1024 * 1024;
pub const AVG_CHUNK: u32 = 2 * 1024 * 1024;
pub const MAX_CHUNK: u32 = 4 * 1024 * 1024;

/// Split `data` into chunks. Empty input yields no chunks.
pub fn split(data: &[u8]) -> Vec<&[u8]> {
    if data.is_empty() {
        return Vec::new();
    }
    if data.len() <= SMALL_FILE_MAX {
        return vec![data];
    }
    FastCDC::new(data, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK)
        .map(|c| &data[c.offset..c.offset + c.length])
        .collect()
}
