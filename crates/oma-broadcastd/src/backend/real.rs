//! The media backend of the running daemon: the persistent loopback writer
//! and, while active, a CUDA [`Session`] whose output forwards to it.
//!
//! Before the first session nothing here touches CUDA or loads the
//! `nvcodec` plugin: the writer is `appsrc ! v4l2sink` on the CPU, and the
//! camera scan reads sysfs and queries nodes without streaming. NVML (for
//! `gpu_percent`) is initialised only while a session runs and shut down
//! with it.
//!
//! Every output is built with the effect element (`obceffect`), in
//! passthrough for `none`, so the effect can change while it streams. Its
//! models are loaded only while an effect is requested and the session
//! runs; stopping the session unloads them before the CUDA context goes
//! (`docs/adr/0012-video-effects-and-maxine-backend.md`, "Lifecycle").

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gst::prelude::*;
use obc_core::config::EffectConfig;
use obc_core::{EffectMode, EffectOnFailure, ModeRequest, OutputContract, SinkSelector};
use obc_ipc::Stats;
use obc_pipeline::gpu::GpuSampler;
use obc_pipeline::pipeline::names;
use obc_pipeline::stats::{cpu_usage, online_cpus, self_cpu_ticks, ticks_per_sec};
use obc_pipeline::{
    DiscoveryRoute, EffectFallback, EffectKind, EffectSettings, IngestConfig, LiveStats,
    LoopbackWriter, Output, OutputConfig, OutputSink, PipelineError, Session, WriterCounts,
    WriterSink, choose_mode, effect_backend_available, list_cameras, scan_sysfs, select_sink,
};

use super::{EffectReport, EffectStatus, Failure, MediaBackend, SessionParams};
use crate::camera::{Camera, stable_id};
use crate::consumers::{NodeId, count_holders};

/// Environment variable that makes the daemon stamp every frame it
/// forwards to the writer with `obc_pipeline::stamp` (the frame's PTS in
/// its first 16 luma bytes), so the end-to-end test can tell camera frames
/// from placeholders whatever the picture. A test hook: it alters the
/// picture; never set it in normal use.
pub const STAMP_ENV: &str = "OBC_TEST_STAMP_FORWARDED";

/// NVML device sampled for `gpu_percent` (the device the CUDA elements use
/// by default).
const GPU_INDEX: u32 = 0;
/// NVML sampling period while a session runs.
const GPU_INTERVAL: Duration = Duration::from_millis(500);
/// The sampler keeps every sample; it is restarted after this many so its
/// memory stays bounded over a long call.
const GPU_SAMPLES_MAX: usize = 1200;

struct Running {
    session: Session,
    live: Arc<LiveStats>,
    /// The current ingest's source id.
    source: Option<u64>,
    gpu: Option<GpuSampler>,
    gpu_seen: usize,
    window_start: Instant,
    cpu_ticks: Option<u64>,
    /// The writer's counters at the start of the stats window.
    counts: WriterCounts,
}

/// The media backend over GStreamer (see the module docs).
pub struct GstBackend {
    writer: Option<LoopbackWriter>,
    running: Option<Running>,
    stamp: bool,
}

impl GstBackend {
    /// Initialises GStreamer (no plugin is loaded by that).
    pub fn new() -> Result<Self, String> {
        gst::init().map_err(|e| format!("GStreamer initialisation failed: {e}"))?;
        Ok(GstBackend {
            writer: None,
            running: None,
            stamp: std::env::var_os(STAMP_ENV).is_some_and(|v| !v.is_empty() && v != "0"),
        })
    }

    fn writer_counts(&self) -> WriterCounts {
        self.writer
            .as_ref()
            .map_or_else(WriterCounts::default, LoopbackWriter::counts)
    }

    fn ingest_config(params: &SessionParams, camera: &Camera) -> IngestConfig {
        let mut cfg = IngestConfig::new(camera.path.clone(), camera.mode);
        cfg.aspect = params.aspect;
        cfg.fixed_framerate = params.fixed_framerate;
        cfg
    }

    fn start_ingest(
        running: &mut Running,
        params: &SessionParams,
        camera: &Camera,
    ) -> Result<(), String> {
        let cfg = Self::ingest_config(params, camera);
        let ingest = running
            .session
            .build_ingest(&cfg)
            .map_err(|e| format!("{}: {e}", camera.label()))?;
        running
            .live
            .attach(&ingest)
            .map_err(|e| format!("{}: {e}", camera.label()))?;
        running.source = None;
        let timing = running
            .session
            .switch_to(ingest)
            .map_err(|e| format!("{}: {e}", camera.label()))?;
        running.source = Some(timing.source_id);
        Ok(())
    }
}

/// The effect element's settings for the daemon's effect settings.
fn effect_settings(e: &EffectConfig) -> EffectSettings {
    EffectSettings {
        kind: match e.mode {
            EffectMode::None => EffectKind::None,
            EffectMode::Blur => EffectKind::Blur,
            EffectMode::Replace => EffectKind::Replace,
        },
        // Validated to 0..=1 before it gets here.
        blur_strength: e.blur_strength as f32,
        background: e.background_image.clone(),
        fallback: match e.on_failure {
            EffectOnFailure::Black => EffectFallback::Black,
            EffectOnFailure::None => EffectFallback::None,
        },
    }
}

/// The reason of an effect error, without the `effect: ` prefix of
/// [`PipelineError::Effect`].
fn effect_error(e: PipelineError) -> String {
    match e {
        PipelineError::Effect(m) => m,
        other => other.to_string(),
    }
}

/// The frames that reached the loopback between two snapshots of the
/// writer's counters: those `appsrc` accepted (forwarded or placeholder),
/// less those it dropped from its full queue.
fn delivered_between(from: &WriterCounts, to: &WriterCounts) -> u64 {
    let accepted =
        (to.forwarded + to.placeholder).saturating_sub(from.forwarded + from.placeholder);
    let dropped = to.queue_dropped.saturating_sub(from.queue_dropped);
    accepted.saturating_sub(dropped)
}

/// Stamps every buffer reaching the output's forwarding `appsink`
/// ([`STAMP_ENV`]).
fn stamp_forwarded(output: &Output) {
    let Some(pad) = output
        .pipeline()
        .element(names::LOOPBACK)
        .and_then(|e| e.static_pad("sink"))
    else {
        crate::warning!(
            "{STAMP_ENV}: the output has no {} sink pad",
            names::LOOPBACK
        );
        return;
    };
    pad.add_probe(gst::PadProbeType::BUFFER, |_, info| {
        if let Some(buffer) = info.buffer_mut() {
            let pts = buffer.pts().map_or(0, |p| p.nseconds());
            if let Ok(mut map) = buffer.make_mut().map_writable() {
                obc_pipeline::stamp::write_stamp(map.as_mut_slice(), pts);
            }
        }
        gst::PadProbeReturn::Ok
    });
}

impl MediaBackend for GstBackend {
    fn scan_cameras(&mut self, request: &ModeRequest) -> Vec<Camera> {
        // The sysfs route: no DeviceMonitor (it would talk to PipeWire),
        // loopback nodes are never opened.
        list_cameras(DiscoveryRoute::Sysfs, Some(request))
            .devices
            .into_iter()
            .filter(|d| d.is_colour_camera())
            .filter_map(|d| {
                let choice = d
                    .capabilities
                    .as_ref()
                    .and_then(|c| choose_mode(c, request))?;
                Some(Camera {
                    id: stable_id(&d.path),
                    name: d.card.trim().to_string(),
                    usb: d.usb,
                    path: d.path.clone(),
                    mode: choice.mode,
                })
            })
            .collect()
    }

    fn start_writer(
        &mut self,
        sink: &SinkSelector,
        contract: &OutputContract,
    ) -> Result<PathBuf, String> {
        self.stop_writer();
        let devices = scan_sysfs();
        let device = select_sink(&devices, sink).map_err(|e| e.to_string())?;
        let path = device.path.clone();
        let mut writer = LoopbackWriter::build(*contract, WriterSink::Loopback(path.clone()))
            .map_err(|e| format!("loopback {}: {e}", path.display()))?;
        writer
            .start()
            .map_err(|e| format!("loopback {}: {e}", path.display()))?;
        self.writer = Some(writer);
        Ok(path)
    }

    fn stop_writer(&mut self) {
        self.stop_session();
        if let Some(mut w) = self.writer.take()
            && let Err(e) = w.stop()
        {
            crate::warning!("stopping the loopback writer: {e}");
        }
    }

    fn writer_failure(&mut self) -> Option<String> {
        let w = self.writer.as_ref()?;
        w.health()
            .err()
            .map(|e| format!("loopback writer failed: {e}"))
    }

    fn loopback_holders(&mut self, node: &Path) -> u32 {
        // The daemon's own descriptors (the writer) are not counted.
        count_holders(Path::new("/proc"), &NodeId::of(node), std::process::id())
    }

    fn start_session(&mut self, params: &SessionParams) -> Result<(), String> {
        self.stop_session();
        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| "no loopback writer is running".to_string())?;
        let mut ocfg = OutputConfig::new(params.contract, OutputSink::Writer(writer.handle()));
        ocfg.gap_threshold = params.input_gap;
        ocfg.hold_timeout = params.hold_timeout;
        // Always with the element: an output built without it cannot take
        // an effect later.
        ocfg.effect = Some(effect_settings(&params.effect));
        let mut output = Output::build(ocfg).map_err(|e| format!("output pipeline: {e}"))?;
        let live = LiveStats::new();
        output.set_observer(Some(live.observer()));
        if self.stamp {
            stamp_forwarded(&output);
        }
        let session = Session::new(output).map_err(|e| format!("output pipeline: {e}"))?;
        let epoch = Instant::now();
        let gpu = match GpuSampler::start(
            GPU_INDEX,
            GPU_INTERVAL,
            Arc::new(move || u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)),
        ) {
            Ok(s) => Some(s),
            Err(e) => {
                crate::debug!("GPU statistics unavailable: {e}");
                None
            }
        };
        let running = Running {
            session,
            live,
            source: None,
            gpu,
            gpu_seen: 0,
            window_start: Instant::now(),
            cpu_ticks: self_cpu_ticks(),
            counts: self.writer_counts(),
        };
        self.running = Some(running);
        Ok(())
    }

    fn switch_camera(&mut self, params: &SessionParams, camera: &Camera) -> Result<(), String> {
        let running = self
            .running
            .as_mut()
            .ok_or_else(|| "no session is running".to_string())?;
        Self::start_ingest(running, params, camera)
    }

    fn stop_session(&mut self) {
        let Some(running) = self.running.take() else {
            return;
        };
        if let Some(gpu) = running.gpu {
            let (_, res) = gpu.stop();
            if let Err(e) = res {
                crate::debug!("NVML shutdown: {e}");
            }
        }
        if let Err(e) = running.session.stop() {
            crate::warning!("stopping the session: {e}");
        }
    }

    fn first_frame(&mut self) -> bool {
        self.running.as_ref().is_some_and(|r| {
            r.source
                .is_some_and(|id| r.session.output().first_fresh(id).is_some())
        })
    }

    fn session_failure(&mut self) -> Option<Failure> {
        let r = self.running.as_ref()?;
        if let Err(e) = r.session.output().health() {
            return Some(Failure::Output(format!("output pipeline failed: {e}")));
        }
        r.session.ingest_failure().map(Failure::Camera)
    }

    fn take_stats(&mut self) -> Stats {
        let counts = self.writer_counts();
        let Some(r) = self.running.as_mut() else {
            return Stats::default();
        };
        let now = Instant::now();
        let secs = now.saturating_duration_since(r.window_start).as_secs_f64();
        r.window_start = now;
        let fps = if secs > 0.0 {
            delivered_between(&r.counts, &counts) as f64 / secs
        } else {
            Stats::NOT_MEASURABLE
        };
        r.counts = counts;
        let window = r.live.take_window();
        let latency = window.latency_p50_ms.unwrap_or(Stats::NOT_MEASURABLE);
        // What the latency depends on (ADR-0010): the camera's own rate and
        // whether the output clock is phase-locked to it. Debug only (`-v`).
        let out = r.session.output().counts();
        crate::debug!(
            "pacing: camera {} fresh fps, output clock {} (offset {:.2} ms)",
            window
                .fresh_fps()
                .map_or_else(|| "n/a".to_string(), |f| format!("{f:.2}")),
            if out.phase_locked {
                "phase locked"
            } else {
                "not phase locked"
            },
            out.phase_offset_ns as f64 / 1e6
        );
        let ticks = self_cpu_ticks();
        let cpu = match (r.cpu_ticks, ticks) {
            (Some(a), Some(b)) if secs > 0.0 => {
                cpu_usage(b.saturating_sub(a), ticks_per_sec(), secs, online_cpus()).one_core_pct
            }
            _ => Stats::NOT_MEASURABLE,
        };
        r.cpu_ticks = ticks;
        let gpu = match &r.gpu {
            Some(s) => {
                let samples = s.samples();
                let new = samples.get(r.gpu_seen..).unwrap_or(&[]);
                r.gpu_seen = samples.len();
                if new.is_empty() {
                    Stats::NOT_MEASURABLE
                } else {
                    new.iter().map(|s| f64::from(s.util_gpu_pct)).sum::<f64>() / new.len() as f64
                }
            }
            None => Stats::NOT_MEASURABLE,
        };
        if r.gpu_seen >= GPU_SAMPLES_MAX
            && let Some(old) = r.gpu.take()
        {
            let _ = old.stop();
            let epoch = Instant::now();
            r.gpu = GpuSampler::start(
                GPU_INDEX,
                GPU_INTERVAL,
                Arc::new(move || u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)),
            )
            .ok();
            r.gpu_seen = 0;
        }
        Stats {
            fps,
            latency_p50_ms: latency,
            cpu_percent: cpu,
            gpu_percent: gpu,
        }
    }

    fn effect_available(&mut self, effect: &EffectConfig) -> Result<(), String> {
        if effect.mode == EffectMode::None {
            return Ok(());
        }
        effect_backend_available()?;
        obc_pipeline::effect::check_settings(&effect_settings(effect)).map_err(effect_error)
    }

    fn set_effect(&mut self, effect: &EffectConfig) -> Result<(), String> {
        let Some(r) = self.running.as_ref() else {
            return Ok(());
        };
        r.session
            .output()
            .set_effect(&effect_settings(effect))
            .map_err(effect_error)
    }

    fn effect_report(&mut self) -> Option<EffectReport> {
        let output = self.running.as_ref()?.session.output();
        let status = match output.effect_status() {
            obc_pipeline::EffectStatus::Off => EffectStatus::Off,
            obc_pipeline::EffectStatus::Loading => EffectStatus::Loading,
            obc_pipeline::EffectStatus::Ready => EffectStatus::Ready,
            obc_pipeline::EffectStatus::Failed(r) => EffectStatus::Failed(r),
        };
        let stats = output.effect_stats().unwrap_or_default();
        Some(EffectReport {
            status,
            load_ms: stats.last_load_ms,
            reused: stats.last_load_reused,
            gpu_ms_p50: stats.gpu_ms_p50,
        })
    }
}

impl Drop for GstBackend {
    fn drop(&mut self) {
        self.stop_writer();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(forwarded: u64, placeholder: u64, queue_dropped: u64) -> WriterCounts {
        WriterCounts {
            forwarded,
            placeholder,
            queue_dropped,
            ..WriterCounts::default()
        }
    }

    #[test]
    fn delivered_frames_exclude_those_dropped_from_the_queue() {
        // 30 accepted in the window (25 forwarded, 5 placeholders), 4 of
        // them dropped by the full queue: 26 reached the loopback.
        assert_eq!(
            delivered_between(&counts(100, 10, 7), &counts(125, 15, 11)),
            26
        );
        // Nothing dropped: every accepted frame.
        assert_eq!(delivered_between(&counts(0, 0, 0), &counts(30, 0, 0)), 30);
        // Drops counted before the window are not subtracted again.
        assert_eq!(
            delivered_between(&counts(50, 0, 20), &counts(80, 0, 20)),
            30
        );
        // The drop counter read a frame the accepted counters had not yet
        // (two reads, not one snapshot): never below zero.
        assert_eq!(delivered_between(&counts(10, 0, 0), &counts(10, 0, 1)), 0);
    }
}
