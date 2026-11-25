//! Times the relay's direct-queue readbacks when VICTORY_RELAY_FETCH_TIMING=1.
//! GPU time brackets the copy commands. CPU time includes recording, acquiring
//! the queue-order lock, and submitting through the existing fence wait.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::Direct3D12::*;

pub(super) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RELAY_FETCH_TIMING").is_some_and(|v| v == "1"))
}

pub(crate) struct Timestamps {
    heap: ID3D12QueryHeap,
    readback: ID3D12Resource,
    frequency: u64,
}

impl Timestamps {
    pub(crate) fn new(device: &ID3D12Device, queue: &ID3D12CommandQueue) -> Option<Self> {
        let mut heap = None;
        let desc = D3D12_QUERY_HEAP_DESC {
            Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
            Count: 2,
            NodeMask: 0,
        };
        // SAFETY: timestamp queries are recorded on this direct queue.
        let frequency = unsafe {
            let hz = queue.GetTimestampFrequency().ok()?;
            if hz == 0 {
                return None;
            }
            device.CreateQueryHeap(&desc, &mut heap).ok()?;
            hz
        };
        Some(Self {
            heap: heap?,
            readback: super::Host::readback_on(device, 16)?,
            frequency,
        })
    }

    pub(crate) fn start(&self, list: &ID3D12GraphicsCommandList) {
        // SAFETY: the relay owns this open list and its two query slots.
        unsafe { list.EndQuery(&self.heap, D3D12_QUERY_TYPE_TIMESTAMP, 0) };
    }

    pub(crate) fn finish(&self, list: &ID3D12GraphicsCommandList) {
        // SAFETY: both queries are ended before resolving to the 16-byte
        // COPY_DEST buffer. The pair is reused only after this list's fence.
        unsafe {
            list.EndQuery(&self.heap, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            list.ResolveQueryData(
                &self.heap,
                D3D12_QUERY_TYPE_TIMESTAMP,
                0,
                2,
                &self.readback,
                0,
            );
        }
    }

    pub(crate) fn read(&self, fence: &ID3D12Fence, value: u64) -> Option<f64> {
        // SAFETY: the map is read only after the exact list is done. MAX is
        // D3D12's device-removed sentinel. Mapping adds no GPU wait.
        unsafe {
            let completed = fence.GetCompletedValue();
            if completed == u64::MAX || completed < value {
                return None;
            }
            let mut data = std::ptr::null_mut();
            self.readback
                .Map(0, Some(&D3D12_RANGE { Begin: 0, End: 16 }), Some(&mut data))
                .ok()?;
            let pair = (!data.is_null()).then(|| data.cast::<[u64; 2]>().read_unaligned());
            self.readback
                .Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            let [start, end] = pair?;
            if end == 0 {
                return None;
            }
            Some(end.checked_sub(start)? as f64 * 1000.0 / self.frequency as f64)
        }
    }
}

/// Completed fetches since the previous frame-rate report. Never locked
/// while recording or submitting commands, or while waiting on the fence.
static SAMPLES: Mutex<Option<Samples>> = Mutex::new(None);

pub(super) fn record(gpu: Option<f64>, bytes: u64, start: Instant, times: [Instant; 4]) {
    SAMPLES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Samples::default)
        .add(gpu, bytes, start, times);
}

pub(super) fn report(frames: f64) {
    if enabled() {
        let interval = SAMPLES.lock().unwrap_or_else(|e| e.into_inner()).take();
        interval.unwrap_or_default().log(frames);
    }
}

#[derive(Default)]
struct Samples {
    gpu_ms: f64,
    wall: Duration,
    record: Duration,
    lock: Duration,
    submit_to_fence: Duration,
    event_wait: Duration,
    bytes: u64,
    fetches: u64,
    invalid: u64,
}

impl Samples {
    fn add(&mut self, gpu: Option<f64>, bytes: u64, start: Instant, times: [Instant; 4]) {
        let [before_lock, acquired, before_wait, finished] = times;
        self.gpu_ms += gpu.unwrap_or(0.0);
        self.wall += finished - start;
        self.record += before_lock - start;
        self.lock += acquired - before_lock;
        self.submit_to_fence += finished - acquired;
        self.event_wait += finished - before_wait;
        self.bytes += bytes;
        self.fetches += 1;
        self.invalid += u64::from(gpu.is_none());
    }

    fn log(&self, frames: f64) {
        let ms = |time: Duration| time.as_secs_f64() * 1000.0 / frames;
        tracing::info!(
            frames,
            gpu_copy_ms = self.gpu_ms / frames,
            cpu_wall_ms = ms(self.wall),
            record_ms = ms(self.record),
            lock_ms = ms(self.lock),
            submit_to_fence_ms = ms(self.submit_to_fence),
            event_wait_ms = ms(self.event_wait),
            bytes_per_frame = self.bytes as f64 / frames,
            fetches_per_frame = self.fetches as f64 / frames,
            invalid = self.invalid,
            fetches = self.fetches,
            "relay fetch timing per frame"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_phases_partition_wall_and_invalid_samples_keep_their_work() {
        let mut samples = Samples::default();
        let start = Instant::now();
        let times = [2, 5, 6, 10].map(|ms| start + Duration::from_millis(ms));
        samples.add(Some(1.25), 1024, start, times);
        samples.add(None, 4096, start, times);
        assert_eq!(samples.fetches, 2);
        assert_eq!(samples.invalid, 1);
        assert_eq!(samples.bytes, 5120);
        assert_eq!(samples.gpu_ms, 1.25);
        assert_eq!(samples.wall, Duration::from_millis(20));
        assert_eq!(
            samples.wall,
            samples.record + samples.lock + samples.submit_to_fence
        );
        assert_eq!(samples.event_wait, Duration::from_millis(8));
    }
}
