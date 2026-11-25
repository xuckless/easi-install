//! Memory/size caps for reading untrusted package content, kept in one table so every reader
//! (inspection, extraction, wiring, adoption) enforces the same bounds.

pub struct Limits {
    /// Largest `.desktop`/metainfo/meta-hint file read into memory.
    pub metadata_bytes: u64,
    /// Largest icon file read into memory (archive entry, asar entry, or on-disk file).
    pub icon_bytes: u64,
    /// Longest symlink target read from an archive entry body (PATH_MAX).
    pub symlink_target_bytes: u64,
    /// Largest Electron `app.asar` JSON header parsed.
    pub asar_header_bytes: usize,
    /// Total bytes buffered across icons + metadata during one inspection (blocks OOM bombs).
    pub inspect_total_bytes: usize,
    /// Entries per archive during both inspection and extraction (blocks inode/entry bombs).
    pub archive_entries: usize,
    /// Total bytes extracted from a single archive (blocks decompression bombs).
    pub extract_bytes: u64,
}

pub const LIMITS: Limits = Limits {
    metadata_bytes: 256 * 1024,
    icon_bytes: 2 * 1024 * 1024,
    symlink_target_bytes: 4096,
    asar_header_bytes: 16 * 1024 * 1024,
    inspect_total_bytes: 32 * 1024 * 1024,
    archive_entries: 100_000,
    extract_bytes: 16 * 1024 * 1024 * 1024,
};
