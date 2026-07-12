//! Identity stubs. Per spec §4.7 we report a fixed uid/gid of 1000.

pub const UID: i64 = 1000;
pub const GID: i64 = 1000;

pub const NR_GETUID: u32 = 102;
pub const NR_GETEUID: u32 = 107;
pub const NR_GETGID: u32 = 104;
pub const NR_GETEGID: u32 = 108;

pub fn getuid() -> i64 { UID }
pub fn geteuid() -> i64 { UID }
pub fn getgid() -> i64 { UID }
pub fn getegid() -> i64 { UID }
