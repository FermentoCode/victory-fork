//! Sparse direct-queue timings. Every sample owns its timestamps until
//! the corresponding list is reset or destroyed after flight completion.
//! The readback's (completed_before, signalled_before] fence interval contains
//! previously accepted work still pending at its snapshot. Concurrent or
//! uninstrumented commands can add to it. Samples are sparse, and a full
//! query pool preferentially omits commands recorded during a backlog.
use super::relay_timing::Timestamps;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex, OnceLock};
use windows::Win32::Graphics::Direct3D12::*;

pub(super) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VICTORY_QUEUE_PROFILE").is_some_and(|v| v == "1") && super::relay()
    })
}

const LIMIT: usize = 128;
const LABEL_LIMIT: usize = 160;
static POOL: Mutex<(usize, Vec<Timestamps>)> = Mutex::new((0, Vec::new()));
pub(crate) enum Group {
    Title,
    Host,
}
impl Group {
    fn rate(&self) -> u64 {
        static RATES: OnceLock<[u64; 2]> = OnceLock::new();
        let rates = RATES.get_or_init(|| {
            [
                configured_rate(
                    std::env::var("VICTORY_QUEUE_PROFILE_RATE").ok().as_deref(),
                    256,
                ),
                configured_rate(
                    std::env::var("VICTORY_QUEUE_PROFILE_HOST_RATE")
                        .ok()
                        .as_deref(),
                    32,
                ),
            ]
        });
        match self {
            Self::Title => rates[0],
            Self::Host => rates[1],
        }
    }
}
fn configured_rate(value: Option<&str>, default: u64) -> u64 {
    value
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n.is_power_of_two() && n <= 65536)
        .unwrap_or(default)
}
static SEQUENCES: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Published after the queue has accepted a late submission and its signal.
pub(super) static LAST_TITLE_FENCE: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static INVALID: AtomicU64 = AtomicU64::new(0);
static UNLABELED: AtomicU64 = AtomicU64::new(0);
static AMBIGUOUS: AtomicU64 = AtomicU64::new(0);
type Label = (&'static str, String);
#[derive(Default)]
struct Bucket {
    n: usize,
    sum: f64,
    peak: f64,
}
static BUCKETS: LazyLock<Mutex<HashMap<Label, Bucket>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// A mixed counter avoids always picking the same draw in regularly repeating passes.
fn selected(n: u64, rate: u64) -> bool {
    let mut v = n.wrapping_mul(0x9e3779b97f4a7c15);
    v = (v ^ (v >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    v = (v ^ (v >> 27)).wrapping_mul(0x94d049bb133111eb);
    (v ^ (v >> 31)) & (rate - 1) == 0
}

pub(crate) struct Sample {
    stamps: Option<Timestamps>,
    label: Label,
}

/// A closed list may be executed more than once, overwriting the same
/// timestamp pair. Claim it under its recording lock before every queue
/// submission, including a synchronous one. Reading holds that same lock.
#[derive(Default, PartialEq, Debug)]
enum Owner {
    #[default]
    Fresh,
    Flight(u64),
    Read,
    Omitted,
}
impl Owner {
    fn submit(&mut self, fence: Option<u64>) {
        *self = match (&*self, fence) {
            (Self::Fresh, Some(value)) => Self::Flight(value),
            _ => Self::Omitted,
        };
    }
    fn take(&mut self, fence: u64) -> bool {
        if *self != Self::Flight(fence) {
            return false;
        }
        *self = Self::Read;
        true
    }
}

#[derive(Default)]
pub(super) struct Recording {
    pub(super) samples: Vec<Sample>,
    owner: Owner,
}
impl Recording {
    pub(super) fn submitted(&mut self, fence: Option<u64>) {
        if matches!(self.owner, Owner::Flight(_)) {
            AMBIGUOUS.fetch_add(self.samples.len() as u64, Relaxed);
        }
        self.owner.submit(fence);
    }
    pub(super) fn collect(
        &mut self,
        fence: &ID3D12Fence,
        value: u64,
        rows: &mut Vec<(&'static str, String, f64)>,
    ) {
        // SAFETY: query only. An early poll cannot consume the claim.
        let completed = unsafe { fence.GetCompletedValue() };
        if completed != u64::MAX && completed >= value && self.owner.take(value) {
            for sample in &self.samples {
                sample.collect(fence, value, rows);
            }
        }
    }
}

impl Drop for Sample {
    fn drop(&mut self) {
        if let Some(stamps) = self.stamps.take() {
            POOL.lock()
                .unwrap_or_else(|e| e.into_inner())
                .1
                .push(stamps);
        }
    }
}

pub(crate) fn begin(
    list: &ID3D12GraphicsCommandList,
    class: &'static str,
    group: Group,
    label: impl FnOnce() -> String,
) -> Option<Sample> {
    if !enabled() {
        return None;
    }
    let counter = match group {
        Group::Title => &SEQUENCES[0],
        Group::Host => &SEQUENCES[1],
    };
    if !selected(counter.fetch_add(1, Relaxed), group.rate()) {
        return None;
    }
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let stamps = if let Some(s) = pool.1.pop() {
        s
    } else {
        if pool.0 >= LIMIT {
            DROPPED.fetch_add(1, Relaxed);
            return None;
        }
        let host = super::host()?;
        let Some(s) = Timestamps::new(&host.device, &host.queue) else {
            INVALID.fetch_add(1, Relaxed);
            return None;
        };
        pool.0 += 1;
        s
    };
    drop(pool);
    let label = (class, label());
    stamps.start(list);
    Some(Sample {
        stamps: Some(stamps),
        label,
    })
}

impl Sample {
    pub(crate) fn finish(&self, list: &ID3D12GraphicsCommandList) {
        self.stamps.as_ref().unwrap().finish(list);
    }

    pub(crate) fn collect(
        &self,
        fence: &ID3D12Fence,
        completed: u64,
        rows: &mut Vec<(&'static str, String, f64)>,
    ) {
        let Some(ms) = self.stamps.as_ref().unwrap().read(fence, completed) else {
            INVALID.fetch_add(1, Relaxed);
            return;
        };
        rows.push((self.label.0, self.label.1.clone(), ms));
        let mut buckets = BUCKETS.lock().unwrap_or_else(|e| e.into_inner());
        if buckets.len() >= LABEL_LIMIT && !buckets.contains_key(&self.label) {
            UNLABELED.fetch_add(1, Relaxed);
            return;
        }
        let bucket = buckets.entry(self.label.clone()).or_default();
        bucket.n += 1;
        bucket.sum += ms;
        bucket.peak = bucket.peak.max(ms);
    }
}

pub(super) fn report() {
    if !enabled() {
        return;
    }
    let readings = std::mem::take(&mut *BUCKETS.lock().unwrap_or_else(|e| e.into_inner()));
    let mut rows: Vec<_> = readings.into_iter().collect();
    rows.sort_unstable_by(|a, b| b.1.sum.total_cmp(&a.1.sum));
    let samples: usize = rows.iter().map(|r| r.1.n).sum();
    static PREVIOUS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
    let eligible = [0, 1].map(|i| {
        let current = SEQUENCES[i].load(Relaxed);
        current.wrapping_sub(PREVIOUS[i].swap(current, Relaxed))
    });
    tracing::info!(
        samples,
        labels = rows.len(),
        eligible_title = eligible[0],
        eligible_host = eligible[1],
        title_one_in = Group::Title.rate(),
        host_one_in = Group::Host.rate(),
        dropped = DROPPED.swap(0, Relaxed),
        ambiguous = AMBIGUOUS.swap(0, Relaxed),
        invalid = INVALID.swap(0, Relaxed),
        omitted_from_summary = UNLABELED.swap(0, Relaxed),
        "queue profile interval"
    );
    // Per-flight rows retain the rest for offline aggregation and fence joins.
    for ((class, pipeline), b) in rows.into_iter().take(6) {
        tracing::info!(
            class,
            pipeline,
            n = b.n,
            gpu_ms = b.sum,
            peak_ms = b.peak,
            "queue profile sample"
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn overwriting_or_replaying_a_recording_never_relabels_its_timestamps() {
        use super::Owner;
        let mut owner = Owner::default();
        owner.submit(Some(7));
        assert!(!owner.take(6));
        owner.submit(Some(8));
        assert!(!owner.take(7));
        assert!(!owner.take(8));
        let mut twice = Owner::default();
        twice.submit(Some(9));
        twice.submit(Some(9));
        assert!(!twice.take(9));
        let mut sync = Owner::default();
        sync.submit(Some(10));
        sync.submit(None);
        assert!(!sync.take(10));
        let mut finished = Owner::default();
        finished.submit(Some(11));
        assert!(finished.take(11));
        assert!(!finished.take(11));
        finished.submit(Some(12));
        assert!(!finished.take(12));
    }

    #[test]
    fn rates_are_bounded_powers_of_two() {
        use super::configured_rate;
        for invalid in [None, Some("0"), Some("3"), Some("131072"), Some("x")] {
            assert_eq!(configured_rate(invalid, 256), 256);
        }
        for n in [1, 32, 256, 65536] {
            assert_eq!(configured_rate(Some(&n.to_string()), 32), n);
        }
    }
    #[test]
    fn sampled_commands_keep_their_label_and_wait_for_the_flight_fence() {
        const NAME: &str = "host::queue_profile::tests::sampled_commands_keep_their_label_and_wait_for_the_flight_fence";
        if std::env::var_os("VICTORY_TEST_PROFILE_CHILD").is_none() {
            let run = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env("VICTORY_TEST_PROFILE_CHILD", "1")
                .env("VICTORY_D3D12_WARP", "1")
                .env("VICTORY_RELAY", "2")
                .env("VICTORY_QUEUE_PROFILE", "1")
                .env("VICTORY_QUEUE_PROFILE_RATE", "1")
                .env_remove("VICTORY_LATE_WAIT")
                .env_remove("VICTORY_GPU_TIMING")
                .output()
                .unwrap();
            assert!(
                run.status.success(),
                "{}{}",
                String::from_utf8_lossy(&run.stdout),
                String::from_utf8_lossy(&run.stderr)
            );
            return;
        }
        use super::*;
        use windows::core::Interface;
        let host = super::super::host().expect("WARP adapter");
        let (allocator, list) = host.make_prelude().unwrap();
        let (src, src_ptr) = host.shared_buffer(65536).unwrap();
        let (dst, dst_ptr) = host.shared_buffer(65536).unwrap();
        // SAFETY: all resources belong to this WARP device. The test blocks
        // for its own fence before checking bytes or returning the query pair.
        unsafe {
            src_ptr.write_bytes(0xa7, 65536);
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            for value in [1, 2] {
                allocator.Reset().unwrap();
                list.Reset(&allocator, None).unwrap();
                let sample = begin(&list, "test-copy", Group::Title, || "fixture".into()).unwrap();
                list.CopyBufferRegion(&dst, 0, &src, 0, 65536);
                sample.finish(&list);
                list.Close().unwrap();
                let mut recording = Recording {
                    samples: vec![sample],
                    owner: Owner::Fresh,
                };
                recording.submitted(Some(value));
                let mut rows = Vec::new();
                recording.collect(&fence, value, &mut rows);
                assert!(rows.is_empty());
                host.queue
                    .ExecuteCommandLists(&[Some(list.cast().unwrap())]);
                host.queue.Signal(&fence, value).unwrap();
                fence
                    .SetEventOnCompletion(value, windows::Win32::Foundation::HANDLE::default())
                    .unwrap();
                if value == 2 {
                    recording.submitted(Some(3));
                    host.queue
                        .ExecuteCommandLists(&[Some(list.cast().unwrap())]);
                    host.queue.Signal(&fence, 3).unwrap();
                    fence
                        .SetEventOnCompletion(3, windows::Win32::Foundation::HANDLE::default())
                        .unwrap();
                    recording.collect(&fence, 2, &mut rows);
                    recording.collect(&fence, 3, &mut rows);
                    assert!(rows.is_empty());
                    continue;
                }
                recording.collect(&fence, value, &mut rows);
                recording.collect(&fence, value, &mut rows);
                assert_eq!(rows.len(), 1);
                assert_eq!((rows[0].0, rows[0].1.as_str()), ("test-copy", "fixture"));
                assert!(rows[0].2.is_finite() && rows[0].2 >= 0.0);
                assert!(
                    std::slice::from_raw_parts(dst_ptr, 65536)
                        .iter()
                        .all(|&b| b == 0xa7)
                );
            }
            assert_eq!(POOL.lock().unwrap().0, 1);
        }
    }

    #[test]
    fn sampler_covers_repeating_command_positions() {
        let mut counts = [0; 64];
        for n in 0..65536 {
            if super::selected(n, 32) {
                counts[n as usize % 64] += 1;
            }
        }
        assert!(counts.iter().all(|&n| n > 8));
        assert!((1800..2300).contains(&counts.iter().sum::<u32>()));
    }
}
