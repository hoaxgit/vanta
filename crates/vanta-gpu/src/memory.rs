//! Title memory by address, for translated shaders that read and write
//! through raw 40-bit addresses (`vanta_shader::emit::SLOT_MEMORY`: the
//! console's ray-tracing structure builder and the programs that walk its
//! structures; the structures themselves lie in plain title memory, in no
//! buffer object).
//!
//! The copies are the ones every other access uses ([`crate::shadow`]: a
//! title buffer's copy, or the 4 MiB window of memory outside buffers), so
//! a root view and a raw address reach the same host bytes. This module
//! keeps what a shader needs to find them:
//!
//! - the **directory**: per 64 KiB page of title address space the number
//!   of the copy that holds the page (0: none) and the page's byte offset
//!   in it; one buffer for all lists, entries only ever go from empty to
//!   filled;
//! - per view heap a run of slots with a raw unordered access view of each
//!   numbered copy (slot 0 of the run: a null view, for pages without a
//!   copy);
//! - the **faults**: the page numbers shaders found empty, read back here
//!   once a frame, so the copies exist from the next frame on. A pointer a
//!   shader takes out of memory cannot be known before it runs; the
//!   addresses a dispatch is given as root views are made ready before it.
//!
//! **A page that is not there when a shader touches it is a lost access**:
//! the read gives 0, the store is dropped, and the fault note only helps
//! the next frame. For the structure builder that is the same as a dropped
//! stage, so before a dispatch the host makes ready everything it can see
//! the dispatch name: the whole copy around each root view, around each
//! pair of root constants that is a title address and around each root
//! constant that is one in 256-byte units (the builder passes structures
//! all three ways), and the 4 MiB after each (a structure may cross a
//! window's end).
//! shortcut: what a stage reaches only through pointers it reads out of
//! memory (InitHeaders' table of structure addresses, LoadInstances'
//! instance records) is not made ready unless something else named it;
//! the fault count in the log says whether that happens. The whole answer
//! is the extent of the title's allocation around an address, which the
//! memory module of the layer knows and this one does not ask yet.
//!
//! shortcut: a copy that is numbered is kept for the rest of the run, and
//! there are at most [`COPIES`] of them (4 MiB windows mostly: 4 GiB at
//! most). Every dispatch of such a shader brings all numbered copies up to
//! date and, if it can store, declares all of them written (the bytes are
//! compared on the CPU afterwards, as for any unordered access view): a
//! dirty-page mask written by the shader would bound both.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use vanta_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES};
use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::host::HostHeap;
use crate::host::{HostList, host};
use crate::shadow::Shadow;

/// Copies that can be numbered (the run of heap slots is one longer). A
/// listed frame of the opening cutscene names about 50 windows.
const COPIES: u32 = 1023;
/// A window of memory outside buffers (`shadow.rs`).
const CELL: u64 = 4 << 20;
/// A directory page.
const PAGE: u64 = 1 << 16;

#[derive(Clone, Copy)]
struct RemapRange {
    end: usize,
    epoch: u64,
}

#[derive(Default)]
struct Remaps {
    epoch: u64,
    /// Disjoint ranges, each tagged with their most recent remap epoch.
    ranges: BTreeMap<usize, RemapRange>,
}

static REMAPS: LazyLock<Mutex<Remaps>> = LazyLock::new(|| Mutex::new(Remaps::default()));
static REMAP_EPOCH: AtomicU64 = AtomicU64::new(0);

fn insert_remap(
    ranges: &mut BTreeMap<usize, RemapRange>,
    mut start: usize,
    mut end: usize,
    epoch: u64,
) {
    if let Some((&previous, range)) = ranges.range(..start).next_back()
        && range.end == start
        && range.epoch == epoch
    {
        start = previous;
        ranges.remove(&previous);
    }
    if let Some((&next, range)) = ranges.range(start..).next()
        && next == end
        && range.epoch == epoch
    {
        end = range.end;
        ranges.remove(&next);
    }
    ranges.insert(start, RemapRange { end, epoch });
}

fn record_remap(ranges: &mut Remaps, start: usize, end: usize) -> u64 {
    ranges.epoch = ranges.epoch.wrapping_add(1).max(1);
    let epoch = ranges.epoch;
    // Keep disjoint history in place: mapping changes are frequent, while
    // each one generally touches only a few of the tracked intervals.
    let first = ranges
        .ranges
        .range(..=start)
        .next_back()
        .filter(|(_, range)| range.end > start)
        .map_or(start, |(&from, _)| from);
    let overlapping: Vec<_> = ranges
        .ranges
        .range(first..end)
        .map(|(&from, &range)| (from, range))
        .collect();
    for &(from, _) in &overlapping {
        ranges.ranges.remove(&from);
    }
    for (from, range) in overlapping {
        if from < start {
            insert_remap(&mut ranges.ranges, from, start, range.epoch);
        }
        if range.end > end {
            insert_remap(&mut ranges.ranges, end, range.end, range.epoch);
        }
    }
    insert_remap(&mut ranges.ranges, start, end, epoch);
    epoch
}

/// The current title-memory mapping epoch, captured when a GPU write is
/// declared. A later remap makes only its overlapping bytes ineligible for
/// publication from that flight.
pub(crate) fn writeback_epoch() -> u64 {
    REMAP_EPOCH.load(Ordering::Acquire)
}

/// A title-memory mapping changed. Track page-rounded ranges: the contents
/// and owner of a physical page change together, even if the caller supplied
/// a sub-page view interval.
pub(crate) fn title_memory_remapped(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let mask = PAGE as usize - 1;
    let start = at & !mask;
    let raw_end = at.saturating_add(len);
    let end = raw_end.saturating_add(mask) & !mask;
    if start < end {
        let mut remaps = REMAPS.lock().unwrap_or_else(|e| e.into_inner());
        let epoch = record_remap(&mut remaps, start, end);
        REMAP_EPOCH.store(epoch, Ordering::Release);
    }
}

fn remapped_since(epoch: u64) -> (u64, Vec<(usize, usize)>) {
    let remaps = REMAPS.lock().unwrap_or_else(|e| e.into_inner());
    let ranges = remaps
        .ranges
        .iter()
        .filter_map(|(&start, range)| (range.epoch > epoch).then_some((start, range.end)))
        .collect();
    (remaps.epoch, ranges)
}

/// The parts of `[start,end)` that still have the mapping captured by
/// `epoch`. Caller holds the title-memory read lock through publication.
pub(crate) fn publishable_ranges(epoch: u64, start: usize, end: usize) -> Vec<(usize, usize)> {
    let remaps = REMAPS.lock().unwrap_or_else(|e| e.into_inner());
    publishable_ranges_in(&remaps, epoch, start, end)
}

fn publishable_ranges_in(
    remaps: &Remaps,
    epoch: u64,
    start: usize,
    end: usize,
) -> Vec<(usize, usize)> {
    if start >= end {
        return Vec::new();
    }
    let mut cursor = start;
    let mut safe = Vec::new();
    if let Some((&from, range)) = remaps.ranges.range(..=start).next_back()
        && from < end
        && range.end > start
        && range.epoch > epoch
    {
        cursor = range.end.min(end);
    }
    for (&from, range) in remaps.ranges.range(start..end) {
        if range.end <= cursor || range.epoch <= epoch {
            continue;
        }
        if from > cursor {
            safe.push((cursor, from.min(end)));
        }
        cursor = range.end.min(end);
        if cursor >= end {
            break;
        }
    }
    if cursor < end {
        safe.push((cursor, end));
    }
    safe
}

fn clear_directory_pages(directory: &mut [u32], pages: u32, ranges: &[(usize, usize)]) {
    for &(start, end) in ranges {
        let first = (start as u64 / PAGE) as usize;
        let last = (end as u64)
            .div_ceil(PAGE)
            .min(u64::from(pages.saturating_sub(1))) as usize;
        for page in first..last {
            let at = page * 2;
            if at + 1 < directory.len() {
                directory[at] = 0;
                directory[at + 1] = 0;
            }
        }
    }
}

struct Memory {
    /// Buffer, its mapping, its GPU address.
    directory: (ID3D12Resource, *mut u32, u64),
    faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`.
    copies: Vec<Arc<Shadow>>,
    /// Per host view heap (by address): the first slot of its run, and how
    /// many copies have their view there.
    heaps: HashMap<usize, (u32, u32)>,
    /// The frame the faults were last read in.
    frame: u64,
    /// Pages asked for this frame that are not the title's memory.
    refused: std::collections::HashSet<u64>,
    /// Remaps already reflected in `directory`.
    directory_epoch: u64,
}

// SAFETY: the mappings are of buffers the struct owns; all access is under
// the mutex.
unsafe impl Send for Memory {}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// What a draw binds.
pub(crate) struct Ready {
    pub(crate) directory: u64,
    pub(crate) faults: u64,
    /// The view heap slot of the null view; copy `n`'s view is `n` further.
    pub(crate) base: u32,
}

impl Memory {
    fn new() -> Option<Memory> {
        let host = host()?;
        let make = |dwords: u64, what: &str| {
            let (buffer, data) = host.shared_buffer(dwords * 4)?;
            crate::host::name(&buffer, || format!("title memory by address: {what}"));
            // SAFETY: a fresh mapping of at least that many bytes.
            unsafe { std::ptr::write_bytes(data, 0, (dwords * 4) as usize) };
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Some((buffer, data.cast::<u32>(), gpu))
        };
        tracing::info!("title memory by address: a shader reads or writes through raw addresses");
        Some(Memory {
            directory: make(u64::from(MEMORY_PAGES) * 2, "directory")?,
            faults: make(u64::from(MEMORY_FAULTS), "faults")?,
            copies: Vec::new(),
            heaps: HashMap::new(),
            frame: u64::MAX,
            refused: Default::default(),
            directory_epoch: writeback_epoch(),
        })
    }

    /// Publish remaps to a new directory version. Lists already recorded
    /// keep their previous resource and therefore retain the owners they
    /// observed; new lists see the cleared entries and resolve the new owner.
    fn apply_remaps(&mut self, host: &crate::host::Host) -> bool {
        if writeback_epoch() == self.directory_epoch {
            return true;
        }
        let (epoch, changed) = remapped_since(self.directory_epoch);
        if changed.is_empty() {
            self.directory_epoch = epoch;
            return true;
        }
        let dwords = u64::from(MEMORY_PAGES) * 2;
        let Some((resource, data)) = host.shared_buffer(dwords * 4) else {
            return false;
        };
        let directory = data.cast::<u32>();
        // SAFETY: both mappings hold the full directory and are distinct
        // shared buffers; the new buffer is not visible to submitted lists.
        unsafe { std::ptr::copy_nonoverlapping(self.directory.1, directory, dwords as usize) };
        // SAFETY: the new shared mapping contains `dwords` writable entries.
        let entries = unsafe { std::slice::from_raw_parts_mut(directory, dwords as usize) };
        clear_directory_pages(entries, MEMORY_PAGES, &changed);
        // SAFETY: the new resource is live.
        let gpu = unsafe { resource.GetGPUVirtualAddress() };
        self.directory = (resource, directory, gpu);
        self.directory_epoch = epoch;
        tracing::debug!(
            pages = changed.len(),
            "raw memory directory remapped with copy-on-write"
        );
        true
    }

    /// Gives the page of `address` its copy, if it has none and the memory
    /// is the title's.
    fn map(&mut self, address: u64) {
        let page = address / PAGE;
        // The last entry stays empty: addresses past the directory read it.
        if page >= u64::from(MEMORY_PAGES) - 1 {
            return;
        }
        // SAFETY: inside the directory's mapping.
        let entry = unsafe { self.directory.1.add(page as usize * 2) };
        // SAFETY: as above.
        if unsafe { entry.read() } != 0 {
            return;
        }
        let start = page * PAGE;
        // A shader's pointer, or a constant taken for an address, may be
        // anything: only memory the title has.
        if self.refused.contains(&page) {
            return;
        }
        if !crate::texture::committed(start as *const u8, PAGE as usize) {
            self.refused.insert(page);
            return;
        }
        // The copy a root view at that address would get; a buffer that
        // ends inside the page is passed over for what holds all of it.
        let whole = |s: &Arc<Shadow>| s.offset_in(start).is_some_and(|_| s.left(start) >= PAGE);
        let Some(shadow) = crate::shadow::find(start, 1)
            .filter(whole)
            .or_else(|| crate::shadow::find(start, PAGE).filter(whole))
        else {
            return;
        };
        // SAFETY: a live buffer.
        let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
        if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            return;
        }
        let n = match self.copies.iter().position(|c| Arc::ptr_eq(c, &shadow)) {
            Some(n) => n,
            None if self.copies.len() < COPIES as usize => {
                self.copies.push(shadow.clone());
                let n = self.copies.len() - 1;
                // Every page the copy holds whole, not only the one asked
                // for: a structure is more than 64 KiB.
                // shortcut: a page of a title buffer inside this one that
                // has a copy of its own is given this copy too.
                let (first, len) = shadow.span();
                let mut at = first.next_multiple_of(PAGE);
                while at + PAGE <= first + len && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
                    // SAFETY: inside the directory's mapping.
                    let other = unsafe { self.directory.1.add((at / PAGE) as usize * 2) };
                    // SAFETY: as above; the offset first, as below.
                    unsafe {
                        if other.read() == 0 {
                            other.add(1).write_volatile((at - first) as u32);
                            other.write_volatile(n as u32 + 1);
                        }
                    }
                    at += PAGE;
                }
                n
            }
            None => {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::warn!(COPIES, "title memory by address: no number left for another copy; its pages read zeros")
                });
                return;
            }
        };
        let offset = shadow.offset_in(start).unwrap_or(0);
        // The offset first: a shader that sees the number sees it too.
        // SAFETY: inside the directory's mapping.
        unsafe {
            entry.add(1).write_volatile(offset as u32);
            entry.write_volatile(n as u32 + 1);
        }
    }

    /// The pages shaders found empty since the last frame.
    fn take_faults(&mut self) {
        let mut pages = Vec::new();
        for i in 1..MEMORY_FAULTS as usize {
            // SAFETY: inside the faults' mapping; the GPU may be writing,
            // a number missed now is written again next frame.
            let at = unsafe { self.faults.1.add(i) };
            // SAFETY: as above.
            let page = unsafe { at.read_volatile() };
            if page != 0 {
                pages.push(page);
                // SAFETY: as above.
                unsafe { at.write_volatile(0) };
            }
        }
        if !pages.is_empty() {
            // Each is an access that read 0 or was dropped.
            static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 50 {
                tracing::warn!(
                    pages = pages.len(),
                    first = format_args!("{:#x}", u64::from(pages[0]) * PAGE),
                    "title memory by address: shaders touched pages that had no copy (those accesses read 0 or were dropped); the copies are made now"
                );
            }
        }
        for page in pages {
            self.map(u64::from(page) * PAGE);
        }
    }

    /// What a dispatch names: the copy around `address` and the 4 MiB
    /// after it.
    fn name(&mut self, address: u64) {
        self.map(address);
        self.map((address & !(CELL - 1)) + CELL);
    }
}

/// Before a draw whose shader reads title memory by address: what its
/// root arguments name gets its copies (`addresses`: its root views;
/// `constants`: its root constants, see the module), every numbered copy
/// has its view in `heap`'s run and is brought up to date when `list` is
/// submitted (and, with `writes`, copied back after). None if the host
/// cannot hold it; the draw is then dropped.
pub(crate) fn ready(
    list: &HostList,
    heap: &HostHeap,
    addresses: &[u64],
    constants: &[u32],
    writes: bool,
) -> Option<Ready> {
    let _mapping = crate::watch::title_memory_reading();
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let m = guard.as_mut()?;
    let Some(host) = host() else { return None };
    if !m.apply_remaps(&host) {
        tracing::warn!(
            "raw memory directory could not be versioned after a remap; dropping this dispatch"
        );
        return None;
    }
    let frame = crate::frame::current();
    if m.frame != frame {
        m.frame = frame;
        m.refused.clear();
        m.take_faults();
    }
    let numbered = m.copies.len();
    for &address in addresses {
        m.name(address);
    }
    // A title address in two constants (low dword, then bits 32 to 39), or
    // in one in 256-byte units. A constant that only looks like one names
    // memory nothing reads; one that is not the title's memory is refused.
    for (i, &c) in constants.iter().enumerate() {
        if let Some(&high) = constants.get(i + 1)
            && (1..=0xFF).contains(&high)
            && c >= 0x1_0000
        {
            m.name(u64::from(c) | u64::from(high) << 32);
        }
        if c >= 0x0100_0000 {
            m.name(u64::from(c) << 8);
        }
    }
    let key = std::ptr::from_ref(heap) as usize;
    let (base, made) = match m.heaps.get(&key) {
        Some(&run) => run,
        None => {
            // Held for the heap's life: not given back with a list's slots.
            let base = heap.dynamic(COPIES + 1)?;
            heap.null_raw_uav_at(base);
            (base, 0)
        }
    };
    for (n, shadow) in m.copies.iter().enumerate().skip(made as usize) {
        let (_, len) = shadow.span();
        heap.raw_uav_at(base + 1 + n as u32, &shadow.buffer, 0, len);
    }
    m.heaps.insert(key, (base, m.copies.len() as u32));
    // Once per list, and for copies numbered since: the builder alone
    // dispatches hundreds of times a frame.
    // shortcut: a copy another list numbers meanwhile is brought up to date
    // by that list's submission, which may run after this one's.
    const READ: usize = usize::MAX - 0x6d;
    const WRITE: usize = usize::MAX - 0x6e;
    let first_read = list.first_use(READ);
    let first_write = writes && list.first_use(WRITE);
    for (n, shadow) in m.copies.iter().enumerate() {
        let (start, len) = shadow.span();
        if first_read || n >= numbered {
            shadow.refresh(list, start, len);
        }
        if writes && (first_write || n >= numbered) {
            shadow.gpu_writes(list, start, len);
        }
    }
    if list.first_use(m.directory.2 as usize) {
        list.keep(&m.directory.0);
    }
    if first_read {
        list.keep(&m.faults.0);
    }
    Some(Ready {
        directory: m.directory.2,
        faults: m.faults.2,
        base,
    })
}

#[cfg(test)]
mod tests {
    use super::{Remaps, clear_directory_pages, publishable_ranges_in, record_remap};

    #[test]
    fn remaps_only_suppress_overlapping_bytes_from_older_flights() {
        let mut remaps = Remaps::default();
        let before = remaps.epoch;
        let first = record_remap(&mut remaps, 0x1_0000, 0x2_0000);
        assert_eq!(
            publishable_ranges_in(&remaps, before, 0x8000, 0x28000),
            vec![(0x8000, 0x1_0000), (0x2_0000, 0x28000)]
        );
        assert_eq!(
            publishable_ranges_in(&remaps, before, 0x1_8000, 0x2_8000),
            vec![(0x2_0000, 0x2_8000)]
        );
        assert_eq!(
            publishable_ranges_in(&remaps, first, 0x8000, 0x28000),
            vec![(0x8000, 0x28000)]
        );

        let second = record_remap(&mut remaps, 0x1_8000, 0x2_0000);
        assert_eq!(
            publishable_ranges_in(&remaps, first, 0x1_0000, 0x2_8000),
            vec![(0x1_0000, 0x1_8000), (0x2_0000, 0x2_8000)]
        );
        assert_eq!(
            publishable_ranges_in(&remaps, second, 0x1_0000, 0x2_8000),
            vec![(0x1_0000, 0x2_8000)]
        );
    }

    #[test]
    fn remaps_preserve_both_sides_of_an_overlapped_interval() {
        let mut remaps = Remaps::default();
        let before = record_remap(&mut remaps, 0x1_0000, 0x5_0000);
        record_remap(&mut remaps, 0x2_0000, 0x3_0000);

        assert_eq!(
            publishable_ranges_in(&remaps, before, 0x1_0000, 0x5_0000),
            vec![(0x1_0000, 0x2_0000), (0x3_0000, 0x5_0000)]
        );
    }

    #[test]
    fn remaps_preserve_disjoint_history() {
        let mut remaps = Remaps::default();
        let before = remaps.epoch;
        record_remap(&mut remaps, 0x1_0000, 0x2_0000);
        let second = record_remap(&mut remaps, 0x4_0000, 0x5_0000);

        assert_eq!(
            publishable_ranges_in(&remaps, before, 0, 0x6_0000),
            vec![(0, 0x1_0000), (0x2_0000, 0x4_0000), (0x5_0000, 0x6_0000)]
        );
        assert_eq!(
            publishable_ranges_in(&remaps, second, 0, 0x6_0000),
            vec![(0, 0x6_0000)]
        );
    }

    #[test]
    fn remap_directory_clear_only_zeros_overlapping_guest_pages() {
        let mut directory = (1..=16).collect::<Vec<u32>>();
        clear_directory_pages(&mut directory, 8, &[(0x1_0000, 0x2_0000)]);
        assert_eq!(&directory[0..2], &[1, 2]);
        assert_eq!(&directory[2..4], &[0, 0]);
        assert_eq!(
            &directory[4..],
            &[5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }
}
