//! GPU sampling through NVML (the library behind `nvidia-smi dmon`).
//!
//! The sampler owns NVML on its own thread: [`GpuSampler::stop`] joins the
//! thread, which calls `nvmlShutdown` before exiting. NVML holds
//! `/dev/nvidia*` open while initialised, so it must be stopped before the
//! pipeline teardown is judged (ADR-0006: idle means no GPU handles).
//!
//! Initialising NVML wakes a runtime-suspended dGPU.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nvml_wrapper::Nvml;
use nvml_wrapper::enums::device::UsedGpuMemory;
use serde::Serialize;

/// Static facts about the sampled GPU.
#[derive(Debug, Clone, Default, Serialize)]
pub struct GpuInfo {
    pub index: u32,
    pub name: String,
    pub pci_bus_id: String,
    pub driver_version: String,
    pub memory_total_bytes: u64,
    /// The CUDA compute capability `(major, minor)`, when NVML reports it.
    /// Not serialised: the measurement reports keep their shape.
    #[serde(skip)]
    pub compute_capability: Option<(u32, u32)>,
}

/// One NVML sample.
#[derive(Debug, Clone, Serialize)]
pub struct GpuSample {
    /// Nanoseconds on the caller's epoch.
    pub t_ns: u64,
    /// `nvmlDeviceGetUtilizationRates().gpu`: % of time a kernel ran.
    pub util_gpu_pct: u32,
    /// `.memory`: % of time device memory was read or written.
    pub util_mem_pct: u32,
    pub mem_used_bytes: u64,
    /// Device memory attributed to this process, if NVML reports it.
    pub proc_mem_bytes: Option<u64>,
    /// This process's SM utilisation since the previous sample, if reported.
    pub proc_sm_pct: Option<u32>,
    /// Other processes with a compute or graphics context on this GPU
    /// (they contaminate device-wide utilisation and memory figures).
    pub other_pids: Vec<u32>,
}

/// Background NVML sampler.
pub struct GpuSampler {
    stop_tx: mpsc::Sender<()>,
    handle: JoinHandle<Result<(), String>>,
    samples: Arc<Mutex<Vec<GpuSample>>>,
    info: GpuInfo,
}

impl GpuSampler {
    /// Initialises NVML on a new thread and samples device `index` every
    /// `interval`, stamping samples with `epoch` (shared with the recorder).
    pub fn start(
        index: u32,
        interval: Duration,
        epoch_now: Arc<dyn Fn() -> u64 + Send + Sync>,
    ) -> Result<Self, String> {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (info_tx, info_rx) = mpsc::channel::<Result<GpuInfo, String>>();
        let samples = Arc::new(Mutex::new(Vec::new()));
        let out = Arc::clone(&samples);
        let handle = std::thread::Builder::new()
            .name("obc-nvml".into())
            .spawn(move || -> Result<(), String> {
                let nvml = match Nvml::builder()
                    .lib_path(std::ffi::OsStr::new("libnvidia-ml.so.1"))
                    .init()
                {
                    Ok(n) => n,
                    Err(e) => {
                        let msg = format!("NVML init failed: {e}");
                        let _ = info_tx.send(Err(msg.clone()));
                        return Err(msg);
                    }
                };
                let result = sample_loop(
                    &nvml,
                    index,
                    interval,
                    &stop_rx,
                    &info_tx,
                    &out,
                    &*epoch_now,
                );
                // Explicit shutdown (rather than relying on Drop) so errors surface.
                let shut = nvml
                    .shutdown()
                    .map_err(|e| format!("nvmlShutdown failed: {e}"));
                result.and(shut)
            })
            .map_err(|e| e.to_string())?;
        let info = info_rx
            .recv()
            .map_err(|_| "NVML thread exited early".to_string())??;
        Ok(GpuSampler {
            stop_tx,
            handle,
            samples,
            info,
        })
    }

    pub fn info(&self) -> &GpuInfo {
        &self.info
    }

    /// Samples collected so far.
    pub fn samples(&self) -> Vec<GpuSample> {
        self.samples.lock().expect("sampler poisoned").clone()
    }

    /// Stops sampling, shuts NVML down and joins the thread. Returns all
    /// samples and the shutdown result.
    pub fn stop(self) -> (Vec<GpuSample>, Result<(), String>) {
        let _ = self.stop_tx.send(());
        let res = self
            .handle
            .join()
            .unwrap_or_else(|_| Err("NVML thread panicked".into()));
        let samples = std::mem::take(&mut *self.samples.lock().expect("sampler poisoned"));
        (samples, res)
    }
}

/// The memory of a GPU now, as NVML reports it ([`memory_now`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryNow {
    pub total_bytes: u64,
    pub used_bytes: u64,
    /// Device memory attributed to this process, if NVML reports it.
    pub proc_mem_bytes: Option<u64>,
}

/// Asks NVML for the memory of device `index` now: one query, not a
/// sample of the [`GpuSampler`], which may be up to its interval old.
/// NVML is initialised for the call and shut down after it (NVML counts
/// its initialisations: a sampler running meanwhile keeps its own), so
/// the call wakes a runtime-suspended dGPU as [`GpuSampler::start`] does;
/// it makes no CUDA call.
pub fn memory_now(index: u32) -> Result<MemoryNow, String> {
    let nvml = Nvml::builder()
        .lib_path(std::ffi::OsStr::new("libnvidia-ml.so.1"))
        .init()
        .map_err(|e| format!("NVML init failed: {e}"))?;
    let device = nvml
        .device_by_index(index)
        .map_err(|e| format!("NVML device {index}: {e}"))?;
    let mem = device
        .memory_info()
        .map_err(|e| format!("NVML device {index} memory: {e}"))?;
    let pid = std::process::id();
    let proc_mem_bytes = device
        .running_compute_processes()
        .unwrap_or_default()
        .into_iter()
        .find(|p| p.pid == pid)
        .and_then(|p| match p.used_gpu_memory {
            UsedGpuMemory::Used(b) => Some(b),
            UsedGpuMemory::Unavailable => None,
        });
    let now = MemoryNow {
        total_bytes: mem.total,
        used_bytes: mem.used,
        proc_mem_bytes,
    };
    nvml.shutdown()
        .map_err(|e| format!("nvmlShutdown failed: {e}"))?;
    Ok(now)
}

fn sample_loop(
    nvml: &Nvml,
    index: u32,
    interval: Duration,
    stop_rx: &mpsc::Receiver<()>,
    info_tx: &mpsc::Sender<Result<GpuInfo, String>>,
    out: &Mutex<Vec<GpuSample>>,
    now: &(dyn Fn() -> u64 + Send + Sync),
) -> Result<(), String> {
    let device = match nvml.device_by_index(index) {
        Ok(d) => d,
        Err(e) => {
            let msg = format!("NVML device {index}: {e}");
            let _ = info_tx.send(Err(msg.clone()));
            return Err(msg);
        }
    };
    let info = GpuInfo {
        index,
        name: device.name().unwrap_or_default(),
        pci_bus_id: device.pci_info().map(|p| p.bus_id).unwrap_or_default(),
        driver_version: nvml.sys_driver_version().unwrap_or_default(),
        memory_total_bytes: device.memory_info().map(|m| m.total).unwrap_or(0),
        compute_capability: device
            .cuda_compute_capability()
            .ok()
            .and_then(|c| Some((u32::try_from(c.major).ok()?, u32::try_from(c.minor).ok()?))),
    };
    let _ = info_tx.send(Ok(info));
    let pid = std::process::id();
    let mut last_seen: Option<u64> = None;
    loop {
        let started = Instant::now();
        let util = device.utilization_rates().ok();
        let mem = device.memory_info().ok();
        let compute = device.running_compute_processes().unwrap_or_default();
        let mut other_pids: Vec<u32> = compute
            .iter()
            .chain(
                device
                    .running_graphics_processes()
                    .unwrap_or_default()
                    .iter(),
            )
            .map(|p| p.pid)
            .filter(|&p| p != pid)
            .collect();
        other_pids.sort_unstable();
        other_pids.dedup();
        let proc_mem =
            compute
                .into_iter()
                .find(|p| p.pid == pid)
                .and_then(|p| match p.used_gpu_memory {
                    UsedGpuMemory::Used(b) => Some(b),
                    UsedGpuMemory::Unavailable => None,
                });
        let proc_sm = match device.process_utilization_stats(last_seen) {
            Ok(v) => {
                if let Some(ts) = v.iter().map(|s| s.timestamp).max() {
                    last_seen = Some(ts);
                }
                let mine: Vec<u32> = v
                    .iter()
                    .filter(|s| s.pid == pid)
                    .map(|s| s.sm_util)
                    .collect();
                (!mine.is_empty()).then(|| mine.iter().sum::<u32>() / mine.len() as u32)
            }
            Err(_) => None,
        };
        if let (Some(u), Some(m)) = (util, mem) {
            out.lock().expect("sampler poisoned").push(GpuSample {
                t_ns: now(),
                util_gpu_pct: u.gpu,
                util_mem_pct: u.memory,
                mem_used_bytes: m.used,
                proc_mem_bytes: proc_mem,
                proc_sm_pct: proc_sm,
                other_pids,
            });
        }
        let wait = interval.saturating_sub(started.elapsed());
        match stop_rx.recv_timeout(wait) {
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            _ => break,
        }
    }
    Ok(())
}
