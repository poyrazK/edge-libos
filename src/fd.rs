//! Per-process file-descriptor table.
//!
//! P0 preloads 0/1/2 to host stdin/stdout/stderr. Every other fd comes from
//! `openat` (or `pipe2` for asyncio's self-pipe).
//!
//! Full implementation lands in Step 12; this is the skeleton the kernel
//! constructor and dispatch table need.

use std::collections::HashMap;

pub const AT_FDCWD: i64 = -100;
pub const STDIN: u32 = 0;
pub const STDOUT: u32 = 1;
pub const STDERR: u32 = 2;

/// What's behind a fd. Variants fill in as their syscalls land.
#[allow(dead_code)]
pub enum Resource {
    /// Placeholder for P0; the actual stdio wiring lands in Step 12.
    StdioPlaceholder,
}

pub struct FdTable {
    #[allow(dead_code)]
    table: HashMap<u32, Resource>,
    next_fd: u32,
}

impl FdTable {
    pub fn new() -> Self {
        let mut table = HashMap::new();
        table.insert(STDIN, Resource::StdioPlaceholder);
        table.insert(STDOUT, Resource::StdioPlaceholder);
        table.insert(STDERR, Resource::StdioPlaceholder);
        Self {
            table,
            next_fd: STDERR + 1,
        }
    }

    /// True if `fd` is currently bound. Returns `false` for unknown fds
    /// (does not distinguish "closed" from "never opened" in v1).
    pub fn contains(&self, fd: u32) -> bool {
        self.table.contains_key(&fd)
    }
}

impl Default for FdTable {
    fn default() -> Self {
        Self::new()
    }
}
