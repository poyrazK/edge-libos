//! 256 KiB arena matching CPython's `obmalloc` (arenas are 256 KiB).
//!
//! Bump-allocate with a free-list for holes from `munmap`. Each arena is
//! embedded in a contiguous slice of guest linear memory; the
//! `LinearAllocator` (in `super`) owns the actual `Memory` and the byte
//! content — this struct only tracks which offsets within the 256 KiB range
//! are in use.
//!
//! Invariants:
//! - `base + used <= base + ARENA_SIZE`
//! - `free_list` entries are non-overlapping and within `[base, base + ARENA_SIZE)`
//! - `alloc` first tries the free list (best-fit), then bumps the pointer
//! - `free` does NOT zero memory; the host zero-fills on `mmap`, and CPython
//!   expects fresh allocations to be zero (so we never reuse dirty memory
//!   unless the caller explicitly chose to via realloc)

pub const ARENA_SIZE: usize = 256 * 1024; // MUST match CPython's obmalloc

/// A 256 KiB arena tracked by offset ranges.
#[derive(Debug, Clone)]
pub struct Arena {
    /// Offset in linear memory where this arena starts.
    pub base: u32,
    /// Bump pointer — bytes consumed so far (from `base`).
    pub used: usize,
    /// Free-list entries: (offset_from_base, length). Best-fit search order.
    pub free_list: Vec<(usize, usize)>,
}

impl Arena {
    pub fn new(base: u32) -> Self {
        Self {
            base,
            used: 0,
            free_list: Vec::new(),
        }
    }

    /// Allocate `len` bytes with `align`-byte alignment. Returns the absolute
    /// offset (i.e. `base + intra_arena_offset`) on success, or `None` if the
    /// arena is full.
    pub fn alloc(&mut self, len: usize, align: usize) -> Option<u32> {
        debug_assert!(align > 0 && (align & (align - 1)) == 0, "align must be power of two");
        debug_assert!(len <= ARENA_SIZE, "single allocation must fit in one arena");

        // Best-fit free-list search: smallest hole that fits.
        let align_mask = align - 1;
        let mut best: Option<(usize, usize, usize)> = None; // (idx, hole_off, hole_len)
        for (i, (off, hlen)) in self.free_list.iter().enumerate() {
            // Round `off` up to the next multiple of `align`.
            let aligned_off = (*off + align_mask) & !align_mask;
            let slack = aligned_off - *off;
            if slack + len > *hlen {
                continue; // doesn't fit
            }
            let waste = *hlen - len - slack;
            match best {
                None => best = Some((i, aligned_off, waste)),
                Some((_, _, bw)) if waste < bw => best = Some((i, aligned_off, waste)),
                _ => {}
            }
        }

        if let Some((i, alloc_off, _)) = best {
            let (hole_off, hole_len) = self.free_list[i];
            // Carve the allocation out of the free-list entry.
            let alloc_end = alloc_off + len;
            let hole_end = hole_off + hole_len;
            if alloc_off == hole_off && alloc_end == hole_end {
                // Exact fit: drop the entry.
                self.free_list.swap_remove(i);
            } else if alloc_off == hole_off {
                // Allocated from the start of the hole.
                self.free_list[i] = (alloc_end, hole_end - alloc_end);
            } else if alloc_end == hole_end {
                // Allocated from the end of the hole.
                self.free_list[i] = (hole_off, alloc_off - hole_off);
            } else {
                // Split: hole becomes two pieces around the allocation.
                self.free_list[i] = (hole_off, alloc_off - hole_off);
                self.free_list.push((alloc_end, hole_end - alloc_end));
            }
            return Some(self.base + alloc_off as u32);
        }

        // No free-list hit: bump.
        let bump_aligned = (self.used + align_mask) & !align_mask;
        let new_used = bump_aligned + len;
        if new_used > ARENA_SIZE {
            return None;
        }
        self.used = new_used;
        Some(self.base + bump_aligned as u32)
    }

    /// Free a previously-allocated range. Returns true if the range was
    /// inside this arena. Does NOT zero memory; the host zero-fills on
    /// `mmap` (or the caller is expected to if reusing).
    pub fn free(&mut self, off: u32, len: usize) -> bool {
        if len == 0 {
            return false;
        }
        let o: usize = match off.checked_sub(self.base) {
            Some(v) => v as usize,
            None => return false,
        };
        let end = match o.checked_add(len) {
            Some(e) => e,
            None => return false,
        };
        if end > ARENA_SIZE {
            return false;
        }

        // Coalesce with adjacent free-list entries.
        // Sort by offset; insert; merge with neighbours.
        self.free_list.push((o, len));
        self.free_list.sort_by_key(|&(off, _)| off);
        let mut i = 0;
        while i + 1 < self.free_list.len() {
            let (a_off, a_len) = self.free_list[i];
            let (b_off, b_len) = self.free_list[i + 1];
            if a_off + a_len == b_off {
                self.free_list[i] = (a_off, a_len + b_len);
                self.free_list.remove(i + 1);
            } else {
                i += 1;
            }
        }
        true
    }

    /// True if `[off, off+len)` is entirely inside this arena's range.
    pub fn contains(&self, off: u32, len: usize) -> bool {
        let end = match off.checked_add(len as u32) {
            Some(e) => e,
            None => return false,
        };
        off >= self.base && end <= self.base + ARENA_SIZE as u32
    }

    /// Bytes available for new allocations (free-list + tail).
    pub fn available(&self) -> usize {
        let in_free: usize = self.free_list.iter().map(|(_, l)| l).sum();
        ARENA_SIZE - self.used + in_free
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_arena_is_empty() {
        let a = Arena::new(0x1000_0000);
        assert_eq!(a.used, 0);
        assert!(a.free_list.is_empty());
        assert_eq!(a.available(), ARENA_SIZE);
    }

    #[test]
    fn bump_alloc_grows_used() {
        let mut a = Arena::new(0x1000_0000);
        let p1 = a.alloc(64, 8).unwrap();
        assert_eq!(p1, 0x1000_0000);
        let p2 = a.alloc(64, 8).unwrap();
        assert_eq!(p2, 0x1000_0040);
        assert_eq!(a.used, 128);
    }

    #[test]
    fn bump_aligns_to_boundary() {
        let mut a = Arena::new(0x1000_0000);
        let p1 = a.alloc(7, 8).unwrap();
        assert_eq!(p1, 0x1000_0000);
        let p2 = a.alloc(8, 8).unwrap();
        // p2 must be aligned to 8; the 7-byte allocation leaves 1 byte slack.
        assert_eq!(p2, 0x1000_0008);
    }

    #[test]
    fn arena_full_returns_none() {
        let mut a = Arena::new(0);
        // ARENA_SIZE is 256 KiB. Ask for exactly that — should succeed.
        let p = a.alloc(ARENA_SIZE, 1).unwrap();
        assert_eq!(p, 0);
        // Now arena is full.
        assert!(a.alloc(1, 1).is_none());
    }

    #[test]
    fn free_then_alloc_reuses_the_hole() {
        let mut a = Arena::new(0);
        let p1 = a.alloc(64, 8).unwrap();
        let p2 = a.alloc(64, 8).unwrap();
        assert!(a.free(p2, 64));
        // Next alloc should reuse p2's slot (best-fit, only hole available).
        let p3 = a.alloc(32, 8).unwrap();
        assert_eq!(p3, p2, "should reuse freed hole");
        // p1 untouched.
        assert!(a.contains(p1, 64));
    }

    #[test]
    fn free_coalesces_adjacent_holes() {
        let mut a = Arena::new(0);
        let p1 = a.alloc(64, 8).unwrap();
        let p2 = a.alloc(64, 8).unwrap();
        let p3 = a.alloc(64, 8).unwrap();
        // Free p2 first, then p1 → should coalesce into one 128-byte hole.
        assert!(a.free(p2, 64));
        assert!(a.free(p1, 64));
        // Now a single big alloc should fit.
        let p = a.alloc(120, 1).unwrap();
        assert!(a.contains(p, 120));
        // p3 still allocated.
        assert!(a.contains(p3, 64));
    }

    #[test]
    fn alloc_uses_best_fit() {
        // Two holes: 100 bytes and 50 bytes. Asking for 30 should pick the
        // 50-byte hole (less waste).
        let mut a = Arena::new(0);
        let p1 = a.alloc(100, 1).unwrap();
        let _p2 = a.alloc(100, 1).unwrap();
        let _p3 = a.alloc(100, 1).unwrap();
        a.free(p1, 100);
        // Manually inject a smaller hole so we can test best-fit.
        a.free_list.push((300, 50));
        let p = a.alloc(30, 1).unwrap();
        // The 30-byte alloc should land in the 50-byte hole (off=300),
        // not the 100-byte hole.
        assert_eq!(p, 300, "best-fit should pick the 50-byte hole");
    }

    #[test]
    fn contains_checks_range() {
        let a = Arena::new(0x1000_0000);
        assert!(a.contains(0x1000_0000, 1));
        assert!(a.contains(0x1000_0000, ARENA_SIZE));
        assert!(!a.contains(0x1000_0000 - 1, 1), "before base");
        assert!(!a.contains(0x1000_0000, ARENA_SIZE + 1), "past end");
    }
}
