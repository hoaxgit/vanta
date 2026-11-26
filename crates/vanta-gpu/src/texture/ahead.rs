//! First-use indexed texture uploads on a dedicated D3D12 COPY queue.
//!
//! Only fresh, title-filled textures in the COMMON state are eligible.
//! Their indexed descriptor stays null while the copy is in flight and is
//! remade after the worker's fence completes; every other upload uses the
//! existing DIRECT-list path.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::Interface;

use super::Staging;
use crate::object::This;
use crate::resource::{AsyncResource, HostTexture};

const MAX_IN_FLIGHT: u64 = 96 << 20;
const BATCH_BYTES: u64 = 16 << 20;
const BATCH_TEXTURES: usize = 64;
const WORKERS: usize = 2;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Wait {
    Ready,
    Failed,
    DeviceLost,
}

struct Pool {
    jobs: Mutex<VecDeque<Job>>,
    more: Condvar,
}

struct Job {
    /// Pins the title object until its completion callback has run.
    resource: AsyncResource,
    /// Key only; callbacks use it to recheck the current descriptor value.
    resource_key: usize,
    copy: Arc<HostTexture>,
    staging: Arc<Staging>,
    bytes: u64,
    asked: Instant,
}

struct Worker {
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    event: HANDLE,
}

// SAFETY: the allocator, list, fence and event are owned and used by one
// worker thread; the shared command queue is free-threaded.
unsafe impl Send for Worker {}

impl Drop for Worker {
    fn drop(&mut self) {
        // SAFETY: this worker created the event and owns it.
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

static POOL: OnceLock<Option<Arc<Pool>>> = OnceLock::new();
static IN_FLIGHT_BYTES: AtomicU64 = AtomicU64::new(0);
static BROKEN: AtomicBool = AtomicBool::new(false);
static COMPLETED: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());
static FINISH_GATE: RwLock<()> = RwLock::new(());
/// Work whose COPY list was submitted but could not be fenced is retained
/// until process exit. In that case the device is unusable; retaining both
/// resources and the staging lock avoids recycling memory the GPU may touch.
static STUCK: Mutex<Vec<Job>> = Mutex::new(Vec::new());
static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CompletionPlan {
    resource_state: Option<bool>,
    publish: bool,
}

fn completion_plan(success: bool, broken: bool) -> CompletionPlan {
    if broken {
        CompletionPlan {
            resource_state: None,
            publish: false,
        }
    } else {
        CompletionPlan {
            resource_state: Some(success),
            publish: true,
        }
    }
}

fn mark_broken() {
    let _gate = FINISH_GATE.write().unwrap_or_else(|e| e.into_inner());
    BROKEN.store(true, Ordering::Release);
}

fn wait_result(state: u8, broken: bool) -> Option<Wait> {
    match state {
        crate::resource::UPLOAD_AHEAD_READY => Some(Wait::Ready),
        crate::resource::UPLOAD_AHEAD_NONE | crate::resource::UPLOAD_AHEAD_DIRECT => {
            Some(Wait::Failed)
        }
        crate::resource::UPLOAD_AHEAD_PENDING if broken => Some(Wait::DeviceLost),
        crate::resource::UPLOAD_AHEAD_PENDING => None,
        _ => Some(Wait::Failed),
    }
}

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VANTA_TEXTURE_UPLOAD_AHEAD").is_none_or(|value| value != "0")
    })
}

pub(crate) fn broken() -> bool {
    BROKEN.load(Ordering::Acquire)
}

fn pool() -> Option<&'static Arc<Pool>> {
    POOL.get_or_init(|| {
        if !enabled() || BROKEN.load(Ordering::Acquire) {
            return None;
        }
        let host = crate::host::host()?;
        // SAFETY: plain creation of a free-threaded COPY queue.
        let queue: ID3D12CommandQueue = unsafe {
            host.device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_COPY,
                    ..Default::default()
                })
                .ok()?
        };
        let pool = Arc::new(Pool {
            jobs: Mutex::new(VecDeque::new()),
            more: Condvar::new(),
        });
        let mut started = 0;
        for n in 0..WORKERS {
            let worker = match Worker::new(&host.device, queue.clone()) {
                Ok(worker) => worker,
                Err(error) => {
                    tracing::warn!(%error, "no texture upload-ahead worker");
                    continue;
                }
            };
            let shared = pool.clone();
            match std::thread::Builder::new()
                .name(format!("vanta-texture-ahead-{n}"))
                .spawn(move || worker.work(shared))
            {
                Ok(_) => started += 1,
                Err(error) => tracing::warn!(%error, "no texture upload-ahead thread"),
            }
        }
        (started > 0).then_some(pool)
    })
    .as_ref()
}

/// Schedules only a first upload that can safely use a COPY queue. The
/// caller must do this before mirroring the texture's descriptor.
pub(crate) fn schedule(resource: This) -> bool {
    if !enabled()
        || BROKEN.load(Ordering::Acquire)
        || resource.is_null()
        || !crate::resource::title_fills(resource)
        || super::host_wrote(resource as usize)
        || crate::resource::alias_of(resource).is_some()
        || super::is_staged(resource)
    {
        return false;
    }
    let Some(pool) = pool() else {
        return false;
    };
    // A queue failure and a schedule decision are mutually exclusive. Any
    // job scheduled before the failure is drained by the broken path; none
    // may reserve a new resource after BROKEN becomes visible.
    let _finish_gate = FINISH_GATE.read().unwrap_or_else(|e| e.into_inner());
    if BROKEN.load(Ordering::Acquire) {
        return false;
    }
    let Some(owner) = crate::resource::hold_async(resource) else {
        return false;
    };
    // Claim the Hosted state before `host_copy`: creating the host texture
    // publishes `texture_made` to descriptor waiters.
    if !crate::resource::claim_upload_ahead(resource) {
        return false;
    }
    let Some(copy) = crate::resource::host_copy_for_ahead(resource) else {
        crate::resource::finish_upload_ahead(resource, false);
        crate::descriptor::texture_made(owner.key());
        notify_completed();
        return false;
    };
    if copy.state.load(Ordering::Acquire) != D3D12_RESOURCE_STATE_COMMON.0 as u32 {
        crate::resource::finish_upload_ahead(resource, false);
        crate::descriptor::texture_made(owner.key());
        notify_completed();
        return false;
    }
    let Some(staging) = super::staging(resource, &copy.texture) else {
        crate::resource::finish_upload_ahead(resource, false);
        crate::descriptor::texture_made(owner.key());
        notify_completed();
        return false;
    };
    let (memory, size) = staging.memory();
    if staging.uploaded.load(Ordering::Acquire)
        || !super::committed(memory as *const u8, size)
    {
        crate::resource::finish_upload_ahead(resource, false);
        crate::descriptor::texture_made(owner.key());
        notify_completed();
        return false;
    }
    let bytes = staging.bytes.max(1);
    let in_flight = IN_FLIGHT_BYTES.fetch_add(bytes, Ordering::AcqRel);
    if in_flight.saturating_add(bytes) > MAX_IN_FLIGHT {
        IN_FLIGHT_BYTES.fetch_sub(bytes, Ordering::AcqRel);
        crate::resource::finish_upload_ahead(resource, false);
        crate::descriptor::texture_made(owner.key());
        notify_completed();
        return false;
    }
    let job = Job {
        resource_key: owner.key(),
        resource: owner,
        copy,
        staging,
        bytes,
        asked: Instant::now(),
    };
    let mut queued = pool.jobs.lock().unwrap_or_else(|e| e.into_inner());
    if BROKEN.load(Ordering::Acquire) {
        drop(queued);
        fail_jobs(vec![job]);
        return false;
    }
    queued.push_back(job);
    drop(queued);
    pool.more.notify_one();
    true
}

/// Waits only when a non-draw operation directly depends on this texture.
pub(crate) fn wait(resource: This) -> Wait {
    let mut lock = COMPLETED.0.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(result) = wait_result(
            crate::resource::upload_ahead_state(resource),
            BROKEN.load(Ordering::Acquire),
        ) {
            return result;
        }
        lock = COMPLETED.1.wait(lock).unwrap_or_else(|e| e.into_inner());
    }
}

impl Worker {
    fn new(device: &ID3D12Device, queue: ID3D12CommandQueue) -> windows::core::Result<Self> {
        unsafe {
            let allocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
            list.Close()?;
            Ok(Self {
                queue,
                allocator,
                list,
                fence: device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?,
                event: CreateEventW(None, false, false, None)?,
            })
        }
    }

    fn work(self, pool: Arc<Pool>) {
        let mut value = 0u64;
        loop {
            let Some(jobs) = take(&pool) else {
                return;
            };
            if BROKEN.load(Ordering::Acquire) {
                fail_jobs(jobs);
                continue;
            }
            value = value.wrapping_add(1).max(1);
            let stable: Vec<_> = jobs
                .iter()
                .map(|job| job.staging.refresh_for_ahead())
                .collect();
            match self.copy(&jobs, value) {
                Ok(()) => {
                    drop(stable);
                    finish(jobs, true);
                }
                Err((error, false)) => {
                    drop(stable);
                    tracing::warn!(%error, "texture upload-ahead failed before submission");
                    finish(jobs, false);
                }
                Err((error, true)) => {
                    tracing::error!(%error, "texture upload-ahead lost its COPY fence");
                    mark_broken();
                    notify_completed();
                    // The GPU may still be reading the staging buffers. Keep
                    // their locks, resources, command allocator/list and queue
                    // alive; this path is device loss.
                    std::mem::forget(stable);
                    STUCK.lock().unwrap_or_else(|e| e.into_inner()).extend(jobs);
                    fail_queued(&pool);
                    // The submitted command list's allocator, queue and event
                    // cannot be safely released without a confirmed fence.
                    std::mem::forget(self);
                    return;
                }
            }
        }
    }

    fn copy(&self, jobs: &[Job], value: u64) -> Result<(), (windows::core::Error, bool)> {
        let mut submitted = false;
        let result = unsafe {
            (|| -> windows::core::Result<()> {
                self.allocator.Reset()?;
                self.list.Reset(&self.allocator, None)?;
                for job in jobs {
                    for (i, footprint) in job.staging.footprints.iter().enumerate() {
                        let to = D3D12_TEXTURE_COPY_LOCATION {
                            // SAFETY: the job holds the live destination resource.
                            pResource: std::mem::transmute_copy(&job.copy.texture),
                            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                                SubresourceIndex: i as u32,
                            },
                        };
                        let from = D3D12_TEXTURE_COPY_LOCATION {
                            // SAFETY: the job holds the live staging resource.
                            pResource: std::mem::transmute_copy(&job.staging.buffer),
                            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                                PlacedFootprint: *footprint,
                            },
                        };
                        // SAFETY: an open COPY list, COMMON-state first-use
                        // destination, and mapped upload-heap source.
                        self.list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
                    }
                }
                self.list.Close()?;
                let lists = [Some(self.list.cast::<ID3D12CommandList>()?)];
                self.queue.ExecuteCommandLists(&lists);
                submitted = true;
                self.queue.Signal(&self.fence, value)?;
                self.fence.SetEventOnCompletion(value, self.event)?;
                Ok(())
            })()
        };
        if let Err(error) = result {
            return Err((error, submitted));
        }
        // SAFETY: the worker owns this event and the fence was signalled above.
        let waited = unsafe { WaitForSingleObject(self.event, INFINITE) };
        if waited != WAIT_OBJECT_0 {
            return Err((windows::core::Error::from_thread(), true));
        }
        // SAFETY: this worker owns a live fence.
        let completed = unsafe { self.fence.GetCompletedValue() };
        if completed == u64::MAX || completed < value {
            return Err((
                windows::core::Error::new(
                    windows::core::HRESULT(0x8000_4005u32 as i32),
                    "COPY fence did not reach its signalled value",
                ),
                true,
            ));
        }
        Ok(())
    }
}

fn take(pool: &Pool) -> Option<Vec<Job>> {
    let mut queued = pool.jobs.lock().unwrap_or_else(|e| e.into_inner());
    while queued.is_empty() && !BROKEN.load(Ordering::Acquire) {
        queued = pool.more.wait(queued).unwrap_or_else(|e| e.into_inner());
    }
    if BROKEN.load(Ordering::Acquire) && queued.is_empty() {
        return None;
    }
    let mut jobs = Vec::new();
    let mut bytes = 0u64;
    while jobs.len() < BATCH_TEXTURES && (bytes < BATCH_BYTES || jobs.is_empty()) {
        let Some(job) = queued.pop_front() else {
            break;
        };
        bytes = bytes.saturating_add(job.bytes);
        jobs.push(job);
    }
    Some(jobs)
}

fn finish(jobs: Vec<Job>, success: bool) {
    // A completed batch may publish only before the shared queue is declared
    // unusable. The write side makes this decision atomic with `mark_broken`.
    let gate = FINISH_GATE.read().unwrap_or_else(|e| e.into_inner());
    let plan = completion_plan(success, BROKEN.load(Ordering::Acquire));
    for job in jobs {
        job.staging.uploaded.store(success, Ordering::Release);
        job.staging.dirty.store(!success, Ordering::Release);
        if let Some(resource_state) = plan.resource_state {
            job.resource.finish_ahead(resource_state);
        }
        if success {
            let n = STATS[0].fetch_add(1, Ordering::Relaxed) + 1;
            STATS[1].fetch_add(job.bytes, Ordering::Relaxed);
            STATS[2].fetch_add(job.asked.elapsed().as_micros() as u64, Ordering::Relaxed);
            if n == 1 || n.is_multiple_of(5000) {
                tracing::info!(
                    uploaded = n,
                    bytes_mb = STATS[1].load(Ordering::Relaxed) >> 20,
                    mean_ms = STATS[2].load(Ordering::Relaxed) as f64 / n as f64 / 1e3,
                    "indexed textures uploaded ahead on the COPY queue"
                );
            }
        } else {
            STATS[3].fetch_add(1, Ordering::Relaxed);
        }
        IN_FLIGHT_BYTES.fetch_sub(job.bytes, Ordering::AcqRel);
        if plan.publish {
            crate::descriptor::texture_made(job.resource_key);
        }
    }
    drop(gate);
    notify_completed();
}

fn notify_completed() {
    let _lock = COMPLETED.0.lock().unwrap_or_else(|e| e.into_inner());
    COMPLETED.1.notify_all();
}

fn fail_jobs(jobs: Vec<Job>) {
    finish(jobs, false);
}

fn fail_queued(pool: &Pool) {
    let queued = {
        let mut jobs = pool.jobs.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *jobs)
    };
    if !queued.is_empty() {
        fail_jobs(queued.into());
    }
    pool.more.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_wait_reports_completion_fallback_and_unfenced_failure() {
        use crate::resource::{
            UPLOAD_AHEAD_DIRECT, UPLOAD_AHEAD_NONE, UPLOAD_AHEAD_PENDING, UPLOAD_AHEAD_READY,
        };

        assert_eq!(wait_result(UPLOAD_AHEAD_PENDING, false), None);
        assert_eq!(wait_result(UPLOAD_AHEAD_READY, false), Some(Wait::Ready));
        assert_eq!(wait_result(UPLOAD_AHEAD_NONE, false), Some(Wait::Failed));
        assert_eq!(wait_result(UPLOAD_AHEAD_DIRECT, false), Some(Wait::Failed));
        // An Execute without a confirmed fence deliberately remains pending:
        // callers must fail the device, never use or recycle the resources.
        assert_eq!(
            wait_result(UPLOAD_AHEAD_PENDING, true),
            Some(Wait::DeviceLost)
        );
    }

    #[test]
    fn broken_queue_cleanup_never_publishes_unsubmitted_textures() {
        assert_eq!(
            completion_plan(false, false),
            CompletionPlan {
                resource_state: Some(false),
                publish: true,
            }
        );
        assert_eq!(
            completion_plan(true, false),
            CompletionPlan {
                resource_state: Some(true),
                publish: true,
            }
        );
        assert_eq!(
            completion_plan(false, true),
            CompletionPlan {
                resource_state: None,
                publish: false,
            }
        );
    }
}
