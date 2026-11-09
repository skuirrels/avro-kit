//! A hostile block (a few KB that decompress to far more) must be refused without the decoder
//! holding much more than the limit: this binary counts the peak of what's allocated.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use avro_kit::Codec;

struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = NOW.fetch_add(l.size(), Ordering::SeqCst) + l.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        NOW.fetch_sub(l.size(), Ordering::SeqCst);
    }
}

#[global_allocator]
static A: Counting = Counting;

#[test]
fn hostile_blocks_stop_at_the_limit() {
    // 48 MB of zeros, compressed to a few KB each.
    let data = vec![0u8; 48 << 20];
    let limit: u64 = 1 << 20;
    for c in Codec::ALL.into_iter().filter(|c| *c != Codec::Null) {
        // the lowest levels are quickest to write in a debug build
        let packed = c.compress(&data, Some(if c == Codec::Bzip2 { 1 } else { 0 })).unwrap();
        let before = NOW.load(Ordering::SeqCst);
        PEAK.store(before, Ordering::SeqCst);
        let e = c.decompress(packed, limit).unwrap_err();
        let used = PEAK.load(Ordering::SeqCst) - before;
        assert!(e.contains("more than 1 MB"), "{}: {e}", c.name());
        // the limit, the decoder's working memory (bzip2's is ~4 MB at level 1, ~40 MB at 9);
        // never the 48 MB
        assert!(used < 16 << 20, "{} used {} MB to refuse the block", c.name(), used >> 20);
    }
}
