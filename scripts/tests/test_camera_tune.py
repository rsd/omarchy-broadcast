"""Unit tests for scripts/camera-tune.py: the pure parts, no camera needed."""

import argparse
import importlib.util
import os
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import numpy as np

SCRIPT = Path(__file__).resolve().parents[1] / "camera-tune.py"
spec = importlib.util.spec_from_file_location("camera_tune", SCRIPT)
ct = importlib.util.module_from_spec(spec)
sys.modules["camera_tune"] = ct
spec.loader.exec_module(ct)

LIST_FORMATS = """\
ioctl: VIDIOC_ENUM_FMT
\tType: Video Capture

\t[0]: 'YUYV' (YUYV 4:2:2)
\t\tSize: Discrete 640x480
\t\t\tInterval: Discrete 0.033s (30.000 fps)
\t\t\tInterval: Discrete 0.050s (20.000 fps)
\t\tSize: Discrete 1280x720
\t\t\tInterval: Discrete 0.100s (10.000 fps)
\t\tSize: Discrete 1920x1080
\t\t\tInterval: Discrete 0.200s (5.000 fps)
\t\tSize: Discrete 3840x2160
\t\t\tInterval: Discrete 0.500s (2.000 fps)
\t[1]: 'MJPG' (Motion-JPEG, compressed)
\t\tSize: Discrete 640x480
\t\t\tInterval: Discrete 0.033s (30.000 fps)
\t\tSize: Discrete 1280x720
\t\t\tInterval: Discrete 0.033s (30.000 fps)
\t\tSize: Discrete 1920x1080
\t\t\tInterval: Discrete 0.033s (30.000 fps)
\t\t\tInterval: Discrete 0.040s (25.000 fps)

ioctl: VIDIOC_ENUM_FMT
\tType: Meta-Data Capture

\t[0]: 'UVCH' (UVC Payload Header Metadata)
\t\tSize: Discrete 0x0
"""

CONTROLS = """\
User Controls

                     brightness 0x00980900 (int)    : min=-10 max=10 step=1 default=2 value=2 flags=has-min-max
        white_balance_automatic 0x0098090c (bool)   : default=1 value=1
           power_line_frequency 0x00980918 (menu)   : min=0 max=2 default=2 value=2 (60 Hz)
\t\t\t\t0: Disabled
\t\t\t\t1: 50 Hz
\t\t\t\t2: 60 Hz
      white_balance_temperature 0x0098091a (int)    : min=2800 max=6500 step=1 default=6500 value=6500 flags=inactive, has-min-max

Camera Controls

                  auto_exposure 0x009a0901 (menu)   : min=0 max=3 default=3 value=3 (Aperture Priority Mode)
\t\t\t\t1: Manual Mode
\t\t\t\t3: Aperture Priority Mode
                        privacy 0x009a0910 (bool)   : default=0 value=0 flags=read-only
"""


def buf_line(seq, ts, flags="ts-monotonic, ts-src-soe", size=614400):
    return f"cap dqbuf: {seq % 4:2d} seq: {seq:6d} bytesused: {size} ts: {ts:.6f} ({flags})"


def metrics(y=100.0, u=2.0, v=-3.0, std=40.0, lap=50.0):
    return {"y": y, "u": u, "v": v, "std": std, "lap": lap, "noise": 0.8}


def point(value, m=None, ok=True, error=None):
    return {"value": value, "ok": ok, "error": error, "metrics": m if m is not None else metrics()}


class Modes(unittest.TestCase):
    def test_parse_skips_metadata_and_keeps_top_rate(self):
        modes = ct.parse_modes(LIST_FORMATS)
        self.assertEqual(len(modes), 7)
        first = modes[0]
        self.assertEqual((first.fmt, first.width, first.height, first.fps), ("YUYV", 640, 480, 30.0))
        self.assertEqual(first.rates, (30.0, 20.0))
        self.assertFalse(any(m.fmt == "UVCH" for m in modes))

    def test_select_largest_two_up_to_1080p(self):
        sel = ct.select_modes(ct.parse_modes(LIST_FORMATS), 2)
        got = [(m.fmt, m.width, m.height, m.fps) for m in sel]
        self.assertEqual(got, [
            ("YUYV", 1920, 1080, 5.0), ("YUYV", 1280, 720, 10.0),
            ("MJPG", 1920, 1080, 30.0), ("MJPG", 1280, 720, 30.0)])

    def test_stepwise_size_and_continuous_rate(self):
        text = ("\tType: Video Capture\n\t[0]: 'YUYV' (x)\n\t\tSize: Stepwise 160x120 - 1280x720 with step 8/8\n"
                "\t\t\tInterval: Stepwise 0.033s - 1.000s with step 0.001s (30.000-1.000 fps)\n")
        (m,) = ct.parse_modes(text)
        self.assertEqual((m.width, m.height, m.fps), (1280, 720, 30.0))

    def test_bandwidth(self):
        yuyv = ct.Mode("YUYV", 1280, 720, 30.0)
        self.assertEqual(ct.mode_bandwidth(yuyv), 1280 * 720 * 2 * 30)
        self.assertEqual(ct.mode_bandwidth(ct.Mode("MJPG", 1920, 1080, 30.0)), 0)
        self.assertGreater(ct.mode_bandwidth(ct.Mode("NV12", 640, 480, 30.0)), 0)


class Controls(unittest.TestCase):
    def test_parse(self):
        c = ct.parse_controls(CONTROLS)
        self.assertEqual(list(c)[:3], ["brightness", "white_balance_automatic", "power_line_frequency"])
        b = c["brightness"]
        self.assertEqual((b.cid, b.min, b.max, b.default, b.value), (0x980900, -10, 10, 2, 2))
        self.assertEqual(c["power_line_frequency"].menu, (0, 1, 2))
        self.assertEqual(c["auto_exposure"].menu, (1, 3))
        self.assertIn("inactive", c["white_balance_temperature"].flags)
        self.assertEqual(c["white_balance_automatic"].type, "bool")

    def test_sweepable_skips_inactive_readonly_and_denied(self):
        c = ct.parse_controls(CONTROLS)
        names = [n for n, x in c.items() if ct.is_sweepable(x)]
        self.assertEqual(names, ["brightness", "white_balance_automatic", "auto_exposure"])
        for n in ("privacy", "power_line_frequency", "white_balance_temperature"):
            self.assertNotIn(n, names)

    def test_never_writes_motion_led_privacy(self):
        for name in ("privacy", "led1_mode", "pan_absolute", "tilt_speed", "zoom_absolute",
                     "focus_absolute", "roll_absolute", "motion_estimation"):
            c = ct.Control(name, 1, "int", 0, 10, 1, 5, 5)
            self.assertFalse(ct.is_sweepable(c), name)

    def test_sweep_values(self):
        self.assertEqual(ct.sweep_values(ct.Control("b", 1, "int", -10, 10, 1, 2, 2)), [-10, 2, 10])
        self.assertEqual(ct.sweep_values(ct.Control("b", 1, "int", 0, 1, 1, 0, 0)), [0, 1])
        self.assertEqual(ct.sweep_values(ct.Control("w", 1, "bool", 0, 1, 1, 1, 1)), [0, 1])
        self.assertEqual(ct.sweep_values(ct.Control("a", 1, "menu", 0, 3, 1, 3, 3, menu=(1, 3))), [1, 3])


class Buffers(unittest.TestCase):
    def test_parse_line(self):
        b = ct.parse_buffer_line(buf_line(7, 4811.25))
        self.assertEqual((b["seq"], b["bytesused"], b["error"]), (7, 614400, False))
        self.assertAlmostEqual(b["ts"], 4811.25)

    def test_error_flag(self):
        b = ct.parse_buffer_line(buf_line(1, 5.0, flags="error, ts-monotonic, ts-src-soe"))
        self.assertTrue(b["error"])

    def test_extra_fields_before_flags(self):
        line = "cap dqbuf:  2 seq:      2 bytesused: 99 ts: 10.500000 delta: 33.333 ms fps: 30.00 (ts-monotonic)"
        b = ct.parse_buffer_line(line)
        self.assertEqual((b["seq"], b["bytesused"]), (2, 99))
        self.assertFalse(b["error"])

    def test_ignores_other_lines(self):
        self.assertIsNone(ct.parse_buffer_line("\tIndex    : 0"))
        self.assertEqual(ct.parse_buffers("Streaming\n" + buf_line(0, 1.0) + "\n<<<<\n"), [ct.parse_buffer_line(buf_line(0, 1.0))])

    def test_fmt_info(self):
        text = """Format Video Capture:
\tWidth/Height      : 1280/720
\tPixel Format      : 'YUYV' (YUYV 4:2:2)
\tBytes per Line    : 2560
\tSize Image        : 1843200
\tColorspace        : sRGB
\tQuantization      : Default (maps to Limited Range)
"""
        i = ct.parse_fmt_info(text)
        self.assertEqual((i["width"], i["height"], i["stride"], i["sizeimage"]), (1280, 720, 2560, 1843200))
        self.assertFalse(i["label_full"])
        self.assertTrue(ct.parse_fmt_info("Quantization : Full Range")["label_full"])
        self.assertTrue(ct.parse_fmt_info("Quantization : Default (maps to Full Range)")["label_full"])


class Timing(unittest.TestCase):
    def bufs(self, n, interval, seq_skip=None, errors=()):
        out = []
        seq = 0
        for i in range(n):
            if seq_skip is not None and i == seq_skip:
                seq += 2
            out.append({"seq": seq, "bytesused": 1, "ts": 100.0 + i * interval, "error": i in errors})
            seq += 1
        return out

    def test_fps_and_jitter_after_warmup(self):
        b = self.bufs(60, 1 / 30)
        t = ct.timing_stats(b, 30.0, t0=99.5)
        self.assertAlmostEqual(t["fps"], 30.0, places=1)
        self.assertAlmostEqual(t["interval_ms"]["std"], 0.0, places=2)
        self.assertEqual((t["error_frames"], t["dropped"], t["late_frames"]), (0, 0, 0))
        self.assertEqual(t["first_frame_s"], 0.5)

    def test_warmup_frames_excluded(self):
        b = self.bufs(60, 1 / 30)
        for i in range(10):  # a slow start must not drag the mean
            b[i]["ts"] = 100.0 + i * 0.5
        for i in range(10, 60):
            b[i]["ts"] = 110.0 + (i - 10) / 30
        t = ct.timing_stats(b, 30.0)
        self.assertAlmostEqual(t["fps"], 30.0, places=1)

    def test_gap_errors_and_late(self):
        b = self.bufs(60, 1 / 30, seq_skip=40, errors=(20, 21))
        b[50]["ts"] += 0.05
        for i in range(51, 60):
            b[i]["ts"] += 0.05
        t = ct.timing_stats(b, 30.0)
        self.assertEqual(t["error_frames"], 2)
        self.assertEqual(t["dropped"], 2)
        self.assertEqual(t["late_frames"], 1)
        self.assertGreater(t["interval_ms"]["max"], 80)

    def test_slow_camera(self):
        t = ct.timing_stats(self.bufs(60, 1 / 15), 30.0)
        self.assertAlmostEqual(t["fps"], 15.0, places=1)

    def test_empty(self):
        self.assertEqual(ct.timing_stats([], 30.0), {"frames": 0, "error_frames": 0})


class RangeRule(unittest.TestCase):
    def hist(self, parts):
        h = np.zeros(256, dtype=np.int64)
        for v, n in parts:
            h[v] += n
        return h

    def test_matches_daemon_constants(self):
        self.assertEqual((ct.RANGE_MARGIN, ct.SUSPECT_SHARE, ct.MIN_SAMPLES), (2, 0.01, 1000))
        # The Rust source is the authority; compare when the tree is there.
        rs = SCRIPT.parents[1] / "crates/obc-pipeline/src/colour_range.rs"
        if rs.exists():
            text = rs.read_text()
            self.assertIn("pub const RANGE_MARGIN: u8 = 2;", text)
            self.assertIn("pub const SUSPECT_SHARE: f64 = 0.01;", text)
            self.assertIn("pub const MIN_SAMPLES: u64 = 1000;", text)

    def test_overshoot_within_margin_is_fine(self):
        h = self.hist([(100, 9000), (14, 500), (237, 500)])  # 14 and 237 are inside the margin
        self.assertIsNone(ct.suggests_full_range(h))

    def test_outside_margin_counts(self):
        h = self.hist([(100, 9000), (13, 60), (238, 60)])  # 1.3 % outside
        e = ct.suggests_full_range(h)
        self.assertEqual((e["min"], e["max"], e["outside"]), (13, 238, 120))
        self.assertAlmostEqual(e["share"], 120 / 9120)

    def test_just_below_threshold_and_too_few_samples(self):
        self.assertIsNone(ct.suggests_full_range(self.hist([(100, 9910), (0, 90)])))  # 0.9 %
        self.assertIsNotNone(ct.suggests_full_range(self.hist([(100, 9900), (0, 100)])))  # 1.0 %
        self.assertIsNone(ct.suggests_full_range(self.hist([(100, 90), (0, 90)])))  # < 1000 samples
        self.assertIsNone(ct.range_evidence(np.zeros(256, dtype=np.int64)))

    def test_analyze_frames_flags_full_range(self):
        rng = np.random.default_rng(1)
        y = rng.integers(0, 256, size=(6, 40, 60), dtype=np.uint8)
        a = ct.analyze_frames(y)
        self.assertTrue(a["range_suspect"])
        lim = rng.integers(20, 230, size=(6, 40, 60), dtype=np.uint8)
        self.assertFalse(ct.analyze_frames(lim)["range_suspect"])


class Functions(unittest.TestCase):
    def test_immerkaer_recovers_gaussian_sigma(self):
        rng = np.random.default_rng(0)
        img = 128 + rng.normal(0, 3.0, size=(200, 300))
        self.assertAlmostEqual(ct.immerkaer(img), 3.0, delta=0.15)
        self.assertEqual(ct.immerkaer(np.full((50, 50), 7.0)), 0.0)

    def test_immerkaer_ignores_a_gradient(self):
        img = np.tile(np.linspace(0, 255, 300), (200, 1))
        self.assertLess(ct.immerkaer(img), 1e-6)

    def test_laplacian_variance_orders_sharpness(self):
        flat = np.full((100, 100), 100.0)
        edge = flat.copy()
        edge[:, 50:] = 200.0
        self.assertEqual(ct.laplacian_variance(flat), 0.0)
        self.assertGreater(ct.laplacian_variance(edge), 0.0)
        blur = np.tile(np.clip((np.arange(100) - 25) * 4.0, 0, 100) + 100, (100, 1))
        self.assertGreater(ct.laplacian_variance(edge), ct.laplacian_variance(blur))

    def test_downscale_area(self):
        img = np.arange(16, dtype=np.float64).reshape(4, 4)
        d = ct.downscale_area(img, 2, 2)
        self.assertEqual(d.shape, (2, 2))
        self.assertAlmostEqual(d[0, 0], img[:2, :2].mean())
        self.assertAlmostEqual(d[1, 1], img[2:, 2:].mean())
        big = np.ones((7, 9))
        self.assertEqual(ct.downscale_area(big, 4, 3).shape, (3, 4))
        self.assertAlmostEqual(float(ct.downscale_area(big, 4, 3).mean()), 1.0)
        self.assertEqual(ct.downscale_area(big, 640, 360).shape, (7, 9))  # never upscales

    def test_temporal_noise_on_static_scene(self):
        rng = np.random.default_rng(2)
        base = np.tile(np.linspace(70, 180, 80), (60, 1))
        frames = np.clip(base + rng.normal(0, 1.5, size=(30, 60, 80)), 0, 255)
        tn = ct.temporal_noise(frames.astype(np.float64))
        self.assertAlmostEqual(tn["midtone"], 1.5, delta=0.2)
        self.assertEqual(tn["mean_image"].shape, (60, 80))
        quiet = ct.temporal_noise(np.repeat(base[None], 10, axis=0))
        self.assertAlmostEqual(quiet["midtone"], 0.0)

    def test_midtone_none_when_no_midtones(self):
        frames = np.zeros((6, 20, 20))
        tn = ct.temporal_noise(frames)
        self.assertIsNone(tn["midtone"])
        a = {"temporal_noise": {"midtone": None, "p25": 0.4}}
        self.assertEqual(ct.midtone_noise(a), 0.4)

    def test_analyze_frames_chroma_and_noise(self):
        rng = np.random.default_rng(3)
        y = np.clip(120 + rng.normal(0, 2, size=(8, 30, 40)), 0, 255).astype(np.uint8)
        u = np.full((8, 30, 20), 118, dtype=np.uint8)
        v = np.full((8, 30, 20), 131, dtype=np.uint8)
        a = ct.analyze_frames(y, u, v)
        self.assertEqual(a["chroma"], {"U": 118.0, "V": 131.0})
        self.assertAlmostEqual(a["luma"]["mean"], 120, delta=1)
        self.assertGreater(a["temporal_noise"]["midtone"], 1.0)
        self.assertGreater(a["noise_immerkaer"], 0.5)


class Planes(unittest.TestCase):
    def test_yuyv_and_uyvy(self):
        w, h = 4, 2
        row = [10, 100, 20, 200, 30, 101, 40, 201]  # Y0 U Y1 V Y2 U Y3 V
        frame = np.array(row * h, dtype=np.uint8).reshape(1, -1)
        y, u, v = ct.split_planes(frame, "YUYV", w, h, w * 2, w * 2 * h)
        self.assertEqual(y[0, 0].tolist(), [10, 20, 30, 40])
        self.assertEqual(u[0, 0].tolist(), [100, 101])
        self.assertEqual(v[0, 0].tolist(), [200, 201])
        uyvy = np.array([100, 10, 200, 20, 101, 30, 201, 40] * h, dtype=np.uint8).reshape(1, -1)
        y, u, v = ct.split_planes(uyvy, "UYVY", w, h, w * 2, w * 2 * h)
        self.assertEqual(y[0, 1].tolist(), [10, 20, 30, 40])
        self.assertEqual((u[0, 0].tolist(), v[0, 0].tolist()), ([100, 101], [200, 201]))

    def test_nv12_with_stride_padding(self):
        w, h, stride = 4, 2, 6
        yp = np.arange(stride * h, dtype=np.uint8)
        uv = np.array([50, 60, 51, 61, 0, 0], dtype=np.uint8)  # one chroma row, padded
        frame = np.concatenate([yp, uv]).reshape(1, -1)
        y, u, v = ct.split_planes(frame, "NV12", w, h, stride, frame.shape[1])
        self.assertEqual(y[0, 1].tolist(), [6, 7, 8, 9])
        self.assertEqual((u[0, 0].tolist(), v[0, 0].tolist()), ([50, 51], [60, 61]))

    def test_unknown_or_short_returns_none(self):
        frame = np.zeros((1, 10), dtype=np.uint8)
        self.assertIsNone(ct.split_planes(frame, "BA81", 4, 2, 4, 10))
        self.assertIsNone(ct.split_planes(frame, "YUYV", 4, 2, 8, 10))


class AnalyseFile(unittest.TestCase):
    def test_yuyv_capture_file_end_to_end(self):
        w, h, n = 64, 32, 10
        rng = np.random.default_rng(5)
        luma = np.clip(120 + rng.normal(0, 2, size=(n, h, w)), 0, 255).astype(np.uint8)
        frames = np.zeros((n, h, w * 2), dtype=np.uint8)
        frames[:, :, 0::2] = luma
        frames[:, :, 1::4] = 110
        frames[:, :, 3::4] = 140
        with tempfile.TemporaryDirectory() as t:
            raw = Path(t) / "x.raw"
            raw.write_bytes(frames.tobytes())
            cam = ct.Camera("/dev/video99", ct.Runner())
            a = cam._analyse_file(str(raw), ct.Mode("YUYV", w, h, 30.0), w, h, w * 2, w * h * 2, [None] * n, 6)
        self.assertEqual(a["frames"], 6)
        self.assertEqual(a["chroma"], {"U": 110.0, "V": 140.0})
        self.assertAlmostEqual(a["luma"]["mean"], 120, delta=1)
        self.assertFalse(a["range_suspect"])


class UsableFrames(unittest.TestCase):
    W, H = 8, 4
    SIZE = W * H * 2  # one full YUYV frame

    def frame(self, luma):
        f = np.zeros((self.H, self.W * 2), dtype=np.uint8)
        f[:, 0::2] = luma
        f[:, 1::4] = 110
        f[:, 3::4] = 140
        return f.tobytes()

    def buf(self, i, size, error=False):
        return {"seq": i, "bytesused": size, "ts": 1.0 + i / 30, "error": error}

    def capture_file(self, t):
        """Frames 0..9: 3 is flagged error (luma 0), 5 is short (40 bytes of luma 0), the rest luma 120."""
        data, bufs = b"", []
        for i in range(10):
            if i == 3:
                data += self.frame(0)
                bufs.append(self.buf(i, self.SIZE, error=True))
            elif i == 5:
                data += self.frame(0)[:40]
                bufs.append(self.buf(i, 40))
            else:
                data += self.frame(120)
                bufs.append(self.buf(i, self.SIZE))
        raw = Path(t) / "x.raw"
        raw.write_bytes(data)
        return raw, bufs

    def test_offsets_follow_recorded_sizes_and_skip_short_and_error_frames(self):
        with tempfile.TemporaryDirectory() as t:
            raw, bufs = self.capture_file(t)
            size = raw.stat().st_size
            offs = ct.usable_frame_offsets(bufs, size, self.SIZE)
            # 3 is flagged error (at 192), 5 is short (at 320, 40 bytes), so 6 starts at 360.
            self.assertEqual(offs, [0, 64, 128, 256, 360, 424, 488, 552])

    def test_analysis_never_sees_the_bad_frames(self):
        with tempfile.TemporaryDirectory() as t:
            raw, bufs = self.capture_file(t)
            cam = ct.Camera("/dev/video99", ct.Runner())
            a = cam._analyse_file(str(raw), ct.Mode("YUYV", self.W, self.H, 30.0),
                                  self.W, self.H, self.W * 2, self.SIZE, bufs, 24)
            self.assertEqual(a["frames"], 8)  # the 8 good frames of 10
            self.assertEqual(a["luma"]["min"], 120)  # no all-zero frame got in
            self.assertEqual(a["chroma"], {"U": 110.0, "V": 140.0})

    def test_fixed_steps_would_have_misread_the_file(self):
        # The old fixed-size offsets: the short frame shifts everything after it.
        with tempfile.TemporaryDirectory() as t:
            raw, bufs = self.capture_file(t)
            self.assertNotEqual(raw.stat().st_size % self.SIZE, 0)

    def test_fallback_without_records_needs_whole_full_frames(self):
        n = 6
        self.assertEqual(ct.usable_frame_offsets([None] * n, n * self.SIZE, self.SIZE),
                         [i * self.SIZE for i in range(n)])
        with self.assertRaises(ValueError):
            ct.usable_frame_offsets([None] * n, n * self.SIZE - 5, self.SIZE)

    def test_records_align_from_the_end_when_they_do_not_add_up(self):
        # Two records for a file of five full frames: they are frames 3 and 4;
        # 4 is flagged error, and frames 0-2 have no record, so only 3 is usable.
        bufs = [self.buf(0, self.SIZE), self.buf(1, self.SIZE, error=True)]
        self.assertEqual(ct.usable_frame_offsets(bufs, 5 * self.SIZE, self.SIZE), [3 * self.SIZE])
        good = [self.buf(i, self.SIZE) for i in range(5)]
        self.assertEqual(ct.usable_frame_offsets(good, 5 * self.SIZE, self.SIZE), [i * self.SIZE for i in range(5)])

    def test_vx800_one_logged_dequeue_more_than_written(self):
        # Real v4l2-ctl 1.32 capture: 91 `cap dqbuf:` lines, every one 614400 bytes,
        # the first flagged error, and a file of exactly 90 full frames.
        size = 614400
        lines = [buf_line(1, 17327.490186, "error, ts-monotonic, ts-src-soe")]
        lines += [buf_line(i, 17327.490186 + (i - 1) / 30) for i in range(2, 92)]
        bufs = ct.parse_buffers("\n".join(lines))
        self.assertEqual(len(bufs), 91)
        self.assertTrue(bufs[0]["error"])
        offs = ct.usable_frame_offsets(bufs, 90 * size, size)
        # The last 90 records are clean, so all 90 frames in the file are usable.
        self.assertEqual(offs, [i * size for i in range(90)])

    def test_vx800_shape_end_to_end(self):
        # The same shape on small frames: 11 records (the first flagged error) for 10 frames.
        n = 10
        bufs = [self.buf(0, self.SIZE, error=True)] + [self.buf(i, self.SIZE) for i in range(1, n + 1)]
        with tempfile.TemporaryDirectory() as t:
            raw = Path(t) / "x.raw"
            raw.write_bytes(b"".join(self.frame(120) for _ in range(n)))
            cam = ct.Camera("/dev/video99", ct.Runner())
            a = cam._analyse_file(str(raw), ct.Mode("YUYV", self.W, self.H, 30.0),
                                  self.W, self.H, self.W * 2, self.SIZE, bufs, 24)
        self.assertEqual(a["frames"], n)
        self.assertEqual(a["luma"]["min"], 120)

    def test_error_frame_in_the_middle_with_an_extra_record(self):
        # 11 records for 10 full frames; the aligned record of frame 4 is flagged error.
        n = 10
        bufs = [self.buf(0, self.SIZE)] + [self.buf(i, self.SIZE, error=(i == 5)) for i in range(1, n + 1)]
        with tempfile.TemporaryDirectory() as t:
            raw = Path(t) / "x.raw"
            raw.write_bytes(b"".join(self.frame(0 if i == 4 else 120) for i in range(n)))
            self.assertEqual(ct.usable_frame_offsets(bufs, n * self.SIZE, self.SIZE),
                             [i * self.SIZE for i in range(n) if i != 4])
            cam = ct.Camera("/dev/video99", ct.Runner())
            a = cam._analyse_file(str(raw), ct.Mode("YUYV", self.W, self.H, 30.0),
                                  self.W, self.H, self.W * 2, self.SIZE, bufs, 24)
        self.assertEqual(a["frames"], n - 1)
        self.assertEqual(a["luma"]["min"], 120)  # the error frame stayed out

    def test_unalignable_capture_is_refused(self):
        # A partial frame in the file and records whose sizes do not add up to it.
        bufs = [self.buf(i, self.SIZE) for i in range(4)]
        with self.assertRaises(ValueError):
            ct.usable_frame_offsets(bufs, 5 * self.SIZE - 7, self.SIZE)


class Classify(unittest.TestCase):
    base = metrics()

    def test_noeffect(self):
        pts = [point(16, metrics(y=100.2)), point(64), point(200, metrics(y=99.8))]
        v, reason = ct.classify_control(pts, self.base)
        self.assertEqual(v, "noeffect", reason)

    def test_effect_on_brightness(self):
        pts = [point(-10, metrics(y=60)), point(2), point(10, metrics(y=150))]
        self.assertEqual(ct.classify_control(pts, self.base)[0], "effect")

    def test_effect_on_contrast_or_sharpness_alone(self):
        pts = [point(0, metrics(std=20)), point(10), point(20, metrics(std=60))]
        self.assertEqual(ct.classify_control(pts, self.base)[0], "effect")
        pts = [point(0, metrics(lap=10)), point(5), point(10, metrics(lap=90))]
        self.assertEqual(ct.classify_control(pts, self.base)[0], "effect")

    def test_effect_on_saturation_moves_uv(self):
        pts = [point(0, metrics(u=0, v=0)), point(4), point(10, metrics(u=9, v=-12))]
        self.assertEqual(ct.classify_control(pts, self.base)[0], "effect")

    def test_wide_tolerance_from_noisy_baseline_prevents_noeffect(self):
        tol = ct.tolerances(metrics(y=100), metrics(y=104))
        self.assertEqual(tol["y"], 8.0)
        pts = [point(0, metrics(y=98)), point(5), point(10, metrics(y=103))]
        self.assertEqual(ct.classify_control(pts, self.base)[0], "effect")
        self.assertEqual(ct.classify_control(pts, self.base, tol)[0], "noeffect")

    def test_broken_on_eproto_and_eio(self):
        for err in ("VIDIOC_S_EXT_CTRLS: failed: Protocol error", "failed: Input/output error"):
            pts = [point(2800, ok=False, error=err, m=metrics(u=-20, v=0)), point(6500)]
            v, reason = ct.classify_control(pts, self.base)
            self.assertEqual(v, "broken", reason)
            self.assertIn("2800", reason)

    def test_broken_on_chroma_jump(self):
        pts = [point(0), point(1, metrics(u=-40, v=20))]
        v, reason = ct.classify_control(pts, self.base)
        self.assertEqual(v, "broken")
        self.assertIn("chroma", reason)

    def test_chroma_just_under_jump_is_not_broken(self):
        pts = [point(0), point(1, metrics(u=2 + 24, v=-3))]
        self.assertNotEqual(ct.classify_control(pts, self.base)[0], "broken")

    def test_other_write_failure_and_single_value_are_unknown(self):
        pts = [point(0), point(9, ok=False, error="failed: Invalid argument", m=None)]
        pts[1]["metrics"] = None
        self.assertEqual(ct.classify_control(pts, self.base)[0], "unknown")
        self.assertEqual(ct.classify_control([point(1)], self.base)[0], "unknown")


class RustText(unittest.TestCase):
    identity = {"card": "GENERAL WEBCAM", "usb": {"vendor": "1b3f", "product_id": "2002",
                                                    "product": "GENERAL WEBCAM"}}

    def results(self):
        suspect = {"range_suspect": True, "range_evidence": {"min": 0, "max": 255, "share": 0.058, "outside": 1, "samples": 1}}
        return {
            "noise": 1.15,
            "modes": [
                {"fmt": "YUYV", "analysis": suspect, "label_full": False},
                {"fmt": "MJPG", "analysis": suspect, "label_full": False},  # never listed
                {"fmt": "NV12", "analysis": {"range_suspect": False}, "label_full": False},
            ],
            "sweep": {"controls": [
                {"name": "brightness", "cid": 0x980900, "default": 128, "verdict": "noeffect",
                 "reason": "nothing changes", "points": []},
                {"name": "hue", "cid": 0x980903, "default": 0, "verdict": "noeffect", "reason": "x", "points": []},
                {"name": "gain", "cid": 0x980913, "default": 34, "verdict": "effect", "reason": "x", "points": []},
            ]},
        }

    def test_usb_formatting(self):
        self.assertEqual(ct.rust_usb("1b3f", "2002"), "usb(0x1b3f, 0x2002)")
        self.assertEqual(ct.rust_usb("045E", "0766"), "usb(0x045e, 0x0766)")
        self.assertEqual(ct.rust_cid(0x980913), "0x0098_0913")
        self.assertEqual(ct.rust_str('a "b"'), '"a \\"b\\""')

    def test_range_and_noeffect_rows(self):
        text = ct.propose_rows(self.identity, self.results())
        self.assertIn("RangeQuirk {\n        usb: usb(0x1b3f, 0x2002),\n"
                      "        formats: &[PixelFormat::Yuy2],\n"
                      '        model: "GENERAL WEBCAM",\n    },', text)
        self.assertNotIn("Mjpeg", text)
        self.assertNotIn("Nv12", text)
        self.assertIn("hidden: &[crate::controls::CID_BRIGHTNESS, CID_HUE],", text)
        self.assertIn("because: HiddenBecause::NoEffect,", text)
        self.assertIn("reset_at_open: &[],", text)
        self.assertIn("noisy: true,", text)
        self.assertIn("values: &[],", text)

    def test_broken_row_with_reset_at_open(self):
        r = {"noise": 0.7, "modes": [], "sweep": {"controls": [
            {"name": "white_balance_automatic", "cid": 0x98090C, "default": 1, "verdict": "broken",
             "reason": "write of 0 failed: Protocol error",
             "points": [{"value": 0, "ok": False}, {"value": 1, "ok": True}]},
            {"name": "white_balance_temperature", "cid": 0x98091A, "default": 6500, "verdict": "broken",
             "reason": "write failed", "points": [{"value": 6500, "ok": False}]},
        ]}}
        text = ct.propose_rows(self.identity, r)
        self.assertIn("hidden: &[CID_AUTO_WHITE_BALANCE, CID_WHITE_BALANCE_TEMPERATURE],", text)
        self.assertIn("because: HiddenBecause::Broken,", text)
        self.assertIn("reset_at_open: &[CID_AUTO_WHITE_BALANCE],", text)
        self.assertIn("noisy: false,", text)
        self.assertNotIn("RangeQuirk", text)

    def test_both_control_kinds_are_flagged_for_merging(self):
        r = self.results()
        r["sweep"]["controls"].append(
            {"name": "gamma", "cid": 0x980910, "default": 1, "verdict": "broken", "reason": "r",
             "points": [{"value": 1, "ok": True}]})
        text = ct.propose_rows(self.identity, r)
        self.assertEqual(text.count("ControlQuirk {"), 2)
        self.assertIn("one row per camera", text)
        self.assertIn("0x0098_0910 /* gamma */", text)

    def test_profile_values_from_recommend(self):
        rec = ct.parse_recommend(["sharpness=5,hue=0", "gain=34"])
        self.assertEqual(rec, [("sharpness", 0x98091B, 5), ("hue", 0x980903, 0), ("gain", 0x980913, 34)])
        text = ct.propose_rows(self.identity, {"modes": [], "noise": 0.5}, rec)
        self.assertIn("values: &[(crate::controls::CID_SHARPNESS, 5), (CID_HUE, 0), (0x0098_0913 /* gain */, 34)],", text)
        self.assertIn("noisy: false,", text)

    def test_recommend_errors(self):
        with self.assertRaises(ct.ToolError):
            ct.parse_recommend(["bogus=1"])
        with self.assertRaises(ct.ToolError):
            ct.parse_recommend(["sharpness"])

    def test_noise_threshold_is_strictly_above_one(self):
        t = ct.propose_rows(self.identity, {"modes": [], "noise": 1.0})
        self.assertIn("noisy: false,", t)
        self.assertIn("noisy: true,", ct.propose_rows(self.identity, {"modes": [], "noise": 1.01}))

    def test_unmeasured_noise_and_no_usb(self):
        text = ct.propose_rows(self.identity, {"modes": []})
        self.assertIn("// noisy: not measured", text)
        self.assertNotIn("CameraProfile {", text)
        self.assertNotIn("noisy: false", text)
        self.assertNotIn("noisy: true", text)
        rec = ct.parse_recommend(["sharpness=5"])
        text = ct.propose_rows(self.identity, {"modes": []}, rec)
        self.assertNotIn("CameraProfile {", text)
        self.assertIn("(crate::controls::CID_SHARPNESS, 5)", text)
        self.assertIn("Not a USB camera", ct.propose_rows({"usb": None}, {}))

    def test_unmeasured_range_and_controls_are_said_not_assumed(self):
        r = {"modes": [{"fmt": "YUYV", "width": 640, "height": 480, "fps": 30.0,
                        "analysis_error": "x", "label_full": False},
                       {"fmt": "MJPG", "width": 640, "height": 480, "fps": 30.0}]}
        text = ct.propose_rows(self.identity, r)
        self.assertIn("// RANGE_QUIRKS: colour range not measured for YUYV 640x480", text)
        self.assertNotIn("MJPG 640x480", text)
        self.assertIn("// CONTROL_QUIRKS: controls not swept (no sweep in this run)", text)
        self.assertNotIn("RangeQuirk {", text)
        r["sweep"] = {"error": "baseline capture failed: x", "controls": []}
        self.assertIn("controls not swept (baseline capture failed: x)", ct.propose_rows(self.identity, r))
        self.assertNotIn("controls not swept", ct.propose_rows(self.identity, self.results()))

    def test_a_format_without_a_pixel_format_gets_a_note_not_a_row(self):
        suspect = {"range_suspect": True, "range_evidence": {"min": 0, "max": 255, "share": 0.06}}
        only_grey = ct.propose_rows(self.identity, {"modes": [{"fmt": "GREY", "analysis": suspect, "label_full": False}]})
        self.assertNotIn("RangeQuirk {", only_grey)
        self.assertNotIn("formats: &[]", only_grey)
        self.assertIn("no row proposed for GREY", only_grey)
        both = ct.propose_rows(self.identity, {"modes": [
            {"fmt": "YUYV", "analysis": suspect, "label_full": False},
            {"fmt": "GREY", "analysis": suspect, "label_full": False}]})
        self.assertIn("formats: &[PixelFormat::Yuy2],", both)
        self.assertIn("no row proposed for GREY", both)
        self.assertNotIn("// GREY: luma", both)

    def test_full_range_label_is_never_a_range_row(self):
        r = self.results()
        r["modes"][0]["label_full"] = True
        self.assertNotIn("RangeQuirk", ct.propose_rows(self.identity, r))


class FakeSysfs(unittest.TestCase):
    def build(self, root: Path, hub_children):
        dev = root / "devices/pci0/usb3"
        hub = dev / "3-7"
        hub.mkdir(parents=True)
        (hub / "idVendor").write_text("05e3\n")
        (hub / "idProduct").write_text("0610\n")
        cam = hub / "3-7.1"
        (cam / "3-7.1:1.0").mkdir(parents=True)
        for f, v in (("idVendor", "5986"), ("idProduct", "1193"), ("product", "FHD Camera"),
                     ("manufacturer", "Acme"), ("speed", "480")):
            (cam / f).write_text(v + "\n")
        for name, ifclass, vid in hub_children:
            d = hub / name
            (d / f"{name}:1.0").mkdir(parents=True)
            (d / "idVendor").write_text(vid + "\n")
            (d / "idProduct").write_text("0001\n")
            (d / "product").write_text("Thing " + name + "\n")
            (d / f"{name}:1.0" / "bInterfaceClass").write_text(ifclass + "\n")
        node = root / "class/video4linux/video0"
        node.mkdir(parents=True)
        (node / "device").symlink_to(cam / "3-7.1:1.0")
        return cam

    def test_identity_and_hid_siblings_behind_a_hub(self):
        with tempfile.TemporaryDirectory() as t:
            root = Path(t)
            self.build(root, [("3-7.2", "03", "046d"), ("3-7.3", "08", "0781")])
            topo = ct.usb_topology(root, "/dev/video0")
            self.assertEqual(topo["usb"]["vendor"], "5986")
            self.assertEqual(topo["usb"]["product"], "FHD Camera")
            self.assertEqual(topo["usb_path"], "3-7.1 <- 3-7 <- usb3")
            self.assertEqual(topo["parent"], "3-7")
            self.assertFalse(topo["parent_is_root_hub"])
            self.assertEqual([h["path"] for h in topo["hid_siblings"]], ["3-7.2"])
            self.assertEqual(topo["hid_siblings"][0]["hid_interfaces"], ["3-7.2:1.0"])
            self.assertTrue(topo["shared_hub_with_hid"])

    def test_hub_without_hid_is_not_shared(self):
        with tempfile.TemporaryDirectory() as t:
            root = Path(t)
            self.build(root, [("3-7.3", "08", "0781")])
            topo = ct.usb_topology(root, "/dev/video0")
            self.assertEqual(topo["hid_siblings"], [])
            self.assertFalse(topo["shared_hub_with_hid"])

    def test_root_hub_is_reported_but_not_gated(self):
        with tempfile.TemporaryDirectory() as t:
            root = Path(t)
            dev = root / "devices/pci0/usb3"
            cam = dev / "3-4"
            (cam / "3-4:1.0").mkdir(parents=True)
            (cam / "idVendor").write_text("5986\n")
            (cam / "idProduct").write_text("1193\n")
            kb = dev / "3-1"
            (kb / "3-1:1.0").mkdir(parents=True)
            (kb / "idVendor").write_text("046d\n")
            (kb / "idProduct").write_text("c52b\n")
            (kb / "3-1:1.0" / "bInterfaceClass").write_text("03\n")
            node = root / "class/video4linux/video0"
            node.mkdir(parents=True)
            (node / "device").symlink_to(cam / "3-4:1.0")
            topo = ct.usb_topology(root, "/dev/video0")
            self.assertTrue(topo["parent_is_root_hub"])
            self.assertEqual(len(topo["hid_siblings"]), 1)
            self.assertFalse(topo["shared_hub_with_hid"])

    def test_not_usb(self):
        with tempfile.TemporaryDirectory() as t:
            node = Path(t) / "class/video4linux/video0"
            node.mkdir(parents=True)
            (Path(t) / "devices/platform/x").mkdir(parents=True)
            (node / "device").symlink_to(Path(t) / "devices/platform/x")
            self.assertEqual(ct.usb_topology(Path(t), "/dev/video0"), {"usb": None})
            self.assertEqual(ct.usb_topology(Path(t), "/dev/video9"), {"usb": None})


class Bus:
    """A fake sysfs bus (usb3) that mirrors the real layout, for the shared-hub rule."""

    def __init__(self, root: Path):
        self.root = root
        self.bus = root / "devices/pci0/usb3"
        self.bus.mkdir(parents=True)

    def device(self, path: str, classes, product=None, vid="1234"):
        """A USB device at `path` below the bus ('3-7/3-7.2'), interfaces of `classes`."""
        d = self.bus / path
        d.mkdir(parents=True, exist_ok=True)
        name = d.name
        (d / "idVendor").write_text(vid + "\n")
        (d / "idProduct").write_text("0001\n")
        if product:
            (d / "product").write_text(product + "\n")
        for n, cls in enumerate(classes):
            i = d / f"{name}:1.{n}"
            i.mkdir(exist_ok=True)
            (i / "bInterfaceClass").write_text(cls + "\n")
        return d

    def camera(self, path: str, classes=("0e", "0e")):
        d = self.device(path, classes, product="Cam")
        node = self.root / "class/video4linux/video0"
        node.mkdir(parents=True, exist_ok=True)
        link = node / "device"
        if link.is_symlink():
            link.unlink()
        link.symlink_to(d / f"{d.name}:1.0")
        return d

    def topo(self):
        return ct.usb_topology(self.root, "/dev/video0")


class SharedHub(unittest.TestCase):
    """The daemon's rule (crates/obc-pipeline/src/usb_topology.rs), mirrored."""

    def test_keyboard_on_an_upper_hub_counts(self):
        with tempfile.TemporaryDirectory() as t:
            b = Bus(Path(t))
            b.device("3-7", ["09"])
            b.device("3-7/3-7.2", ["09"])           # the camera's own hub has no HID
            b.device("3-7/3-7.4", ["03"], product="USB Keyboard")
            b.camera("3-7/3-7.2/3-7.2.1")
            topo = b.topo()
            self.assertTrue(topo["shared_hub_with_hid"])
            self.assertEqual(topo["shared_hub"], "3-7")
            self.assertEqual([h["path"] for h in topo["hid_siblings"]], ["3-7.4"])

    def test_the_nearest_hub_with_hid_wins(self):
        with tempfile.TemporaryDirectory() as t:
            b = Bus(Path(t))
            b.device("3-7", ["09"])
            b.device("3-7/3-7.4", ["03"])
            b.device("3-7/3-7.2", ["09"])
            b.device("3-7/3-7.2/3-7.2.5", ["03"])
            b.camera("3-7/3-7.2/3-7.2.1")
            topo = b.topo()
            self.assertEqual(topo["shared_hub"], "3-7.2")
            self.assertEqual([h["path"] for h in topo["hid_siblings"]], ["3-7.2.5"])

    def test_a_hid_device_below_a_sibling_hub_counts(self):
        with tempfile.TemporaryDirectory() as t:
            b = Bus(Path(t))
            b.device("3-7", ["09"])
            b.device("3-7/3-7.3", ["09"])
            b.device("3-7/3-7.3/3-7.3.1", ["03"])
            b.camera("3-7/3-7.1")
            self.assertEqual(b.topo()["shared_hub"], "3-7")

    def test_a_webcam_with_a_button_is_no_hid_device(self):
        with tempfile.TemporaryDirectory() as t:
            b = Bus(Path(t))
            b.device("3-2", ["09"])
            b.device("3-2/3-2.2", ["0e", "0e", "03"], product="Other webcam with a button")
            b.camera("3-2/3-2.1", ["0e", "0e", "03"])  # and the camera's own button
            topo = b.topo()
            self.assertFalse(topo["shared_hub_with_hid"])
            self.assertEqual(topo["hid_siblings"], [])
            b.device("3-2/3-2.3", ["03"], product="USB Keyboard")
            self.assertTrue(b.topo()["shared_hub_with_hid"])

    def test_root_hub_is_never_the_shared_hub(self):
        with tempfile.TemporaryDirectory() as t:
            b = Bus(Path(t))
            b.device("3-1", ["03"])
            b.camera("3-4")
            topo = b.topo()
            self.assertFalse(topo["shared_hub_with_hid"])
            self.assertTrue(topo["parent_is_root_hub"])
            self.assertEqual(len(topo["hid_siblings"]), 1)  # reported, not gated

    def test_limit_is_inclusive(self):
        captured = []

        class Cam:
            def capture(self, mode, *a, **k):
                captured.append(mode.label)
                return {"fmt": mode.fmt, "width": mode.width, "height": mode.height, "error": "x"}

        exactly = ct.Mode("YUYV", 400, 300, 50.0)   # 12 000 000 B/s
        under = ct.Mode("YUYV", 400, 300, 49.0)
        self.assertEqual(ct.mode_bandwidth(exactly), 12_000_000)
        args = argparse.Namespace(allow_shared_hub=False, seconds=1.0)
        res = ct.measure_modes(Cam(), [exactly, under], {"shared_hub_with_hid": True}, args, log=lambda *_: None)
        self.assertIn("skipped", res[0])
        self.assertNotIn("skipped", res[1])
        self.assertEqual(captured, [under.label])


class FakeChild:
    """A child process that ends on terminate (polite) or only on kill (stubborn)."""

    def __init__(self, stubborn=False):
        self.stubborn = stubborn
        self.calls = []
        self.alive = True
        self.stdout = self.stderr = None
        self.returncode = None

    def poll(self):
        return None if self.alive else self.returncode

    def terminate(self):
        self.calls.append("terminate")
        if not self.stubborn:
            self.alive, self.returncode = False, -15

    def kill(self):
        self.calls.append("kill")
        self.alive, self.returncode = False, -9

    def wait(self, timeout=None):
        self.calls.append(f"wait({timeout})")
        if self.alive:
            raise subprocess.TimeoutExpired("x", timeout)
        return self.returncode


class StopChild(unittest.TestCase):
    def test_terminate_then_wait_is_enough_for_a_polite_child(self):
        r = ct.Runner()
        r.child = FakeChild()
        r.stop_child()
        self.assertEqual(r.child.calls, ["terminate", "wait(5.0)"])

    def test_a_stubborn_child_is_killed_after_the_wait(self):
        r = ct.Runner()
        r.child = FakeChild(stubborn=True)
        r.stop_child()
        self.assertEqual(r.child.calls, ["terminate", "wait(5.0)", "kill", "wait(5.0)"])
        self.assertFalse(r.child.alive)

    def test_an_ended_child_is_left_alone(self):
        r = ct.Runner()
        c = FakeChild()
        c.alive, c.returncode = False, 0
        r.child = c
        r.stop_child()
        self.assertEqual(c.calls, [])

    def test_a_signal_during_popen_stops_the_child_and_raises(self):
        r = ct.Runner()
        child = FakeChild()

        def popen(*_a, **_k):
            # The handler runs here, before Popen returns and self.child is set.
            self.assertTrue(r.defer(signal.SIGTERM))
            self.assertIsNone(r.child)
            return child

        with mock.patch.object(ct.subprocess, "Popen", popen):
            with self.assertRaises(ct.Interrupted):
                r.run(["v4l2-ctl"], 5)
        self.assertIn("terminate", child.calls)
        self.assertIsNone(r.child)
        self.assertFalse(r.spawning)

    def test_a_signal_outside_popen_is_not_deferred(self):
        r = ct.Runner()
        self.assertFalse(r.defer(signal.SIGINT))

    def test_a_failed_popen_with_a_pending_signal_still_raises_interrupted(self):
        r = ct.Runner()

        def popen(*_a, **_k):
            r.defer(signal.SIGINT)
            raise FileNotFoundError("v4l2-ctl")

        with mock.patch.object(ct.subprocess, "Popen", popen):
            with self.assertRaises(ct.Interrupted):
                r.run(["v4l2-ctl"], 5)

    def test_the_temporary_directory_does_not_outlive_an_interrupted_capture(self):
        with tempfile.TemporaryDirectory() as t:
            runner = ct.Runner()

            def interrupted(*_a, **_k):
                raise ct.Interrupted(signal.SIGINT)
            runner.run = interrupted
            cam = ct.Camera("/dev/video99", runner, tmpdir=t)
            cam._fmt_cache[("YUYV", 640, 480)] = {}
            with self.assertRaises(ct.Interrupted):
                cam.capture(ct.Mode("YUYV", 640, 480, 30.0), 30, 15, 24)
            self.assertEqual(os.listdir(t), [])
            self.assertFalse(runner.streaming)


class FakeCam:
    """A stand-in for Camera that keeps control values in a dict and records writes."""

    def __init__(self, controls, fail=(), flaky=None):
        self.c = {n: ct.Control(**{**vars(x)}) for n, x in controls.items()}
        self.writes = []
        self.written = {}
        self.fail = set(fail)
        self.flaky = dict(flaky or {})  # name -> writes silently ignored before one takes

    def controls(self):
        for n, c in self.c.items():
            for master, (manual, deps) in ct.MASTERS.items():
                if n in deps and master in self.c:
                    flags = [f for f in c.flags if f != "inactive"]
                    if self.c[master].value != manual:
                        flags.append("inactive")
                    c.flags = tuple(flags)
        return {n: ct.Control(**vars(c)) for n, c in self.c.items()}

    def read_value(self, name):
        return self.c[name].value

    def set_control(self, name, value):
        self.writes.append((name, value))
        self.written[name] = True
        if (name, value) in self.fail:
            return False, "failed: Protocol error"
        c = self.c[name]
        if "inactive" in self.controls()[name].flags:
            return False, "failed: Permission denied"
        if self.flaky.get(name, 0) > 0:
            self.flaky[name] -= 1
            return True, None
        c.value = value
        return True, None

    def tool_wrote(self, *names):
        """The tool wrote these (and the camera holds a value that differs from the start)."""
        for n in names:
            self.written[n] = True


LENS = """\
                  auto_exposure 0x009a0901 (menu)   : min=0 max=3 default=3 value=3 (Aperture Priority Mode)
\t\t\t\t1: Manual Mode
\t\t\t\t3: Aperture Priority Mode
          exposure_time_absolute 0x009a0902 (int)    : min=3 max=2047 step=1 default=250 value=250 flags=inactive
                  focus_absolute 0x009a090a (int)    : min=0 max=255 step=1 default=0 value=40 flags=inactive
      focus_automatic_continuous 0x009a090c (bool)   : default=1 value=1
"""


class Restore(unittest.TestCase):
    def setUp(self):
        self.orig = ct.parse_controls(CONTROLS + "\n"
                                      "                           gain 0x00980913 (int)    : min=0 max=255 step=1 default=0 value=172 flags=inactive, has-min-max\n")
        # make auto_exposure drive gain, white balance drive its temperature
        self.orig["gain"].flags = ("inactive", "has-min-max")

    def quiet(self, cam, orig=None):
        return ct.restore_controls(cam, orig or self.orig, log=lambda *_: None)

    def test_restores_values_masters_last_and_inactive_dependents(self):
        cam = FakeCam(self.orig)
        cam.c["brightness"].value = -10
        cam.c["white_balance_automatic"].value = 0   # manual: temperature becomes writable
        cam.c["white_balance_temperature"].value = 2800
        cam.c["auto_exposure"].value = 1
        cam.c["gain"].value = 99
        cam.tool_wrote("brightness", "white_balance_automatic", "white_balance_temperature",
                       "auto_exposure", "gain")
        left = self.quiet(cam)
        self.assertEqual(left, [])
        for n, c in self.orig.items():
            self.assertEqual(cam.c[n].value, c.value, n)

    def test_unchanged_controls_are_not_written(self):
        cam = FakeCam(self.orig)
        cam.tool_wrote("brightness", "auto_exposure")
        self.quiet(cam)
        self.assertEqual([w for w in cam.writes if w[0] in ("brightness", "auto_exposure")], [])

    def test_a_run_that_wrote_nothing_restores_nothing(self):
        cam = FakeCam(self.orig)
        cam.c["brightness"].value = 9  # drift, not the tool's doing
        self.assertEqual(self.quiet(cam), [])
        self.assertEqual(cam.writes, [])

    def test_never_writes_readonly(self):
        cam = FakeCam(self.orig)
        cam.c["privacy"].value = 1
        cam.tool_wrote("privacy")
        self.quiet(cam)
        self.assertNotIn("privacy", [w[0] for w in cam.writes])

    def test_reports_what_could_not_be_restored(self):
        cam = FakeCam(self.orig, fail=[("brightness", 2)])
        cam.c["brightness"].value = 9
        cam.tool_wrote("brightness")
        msgs = []
        left = ct.restore_controls(cam, self.orig, log=msgs.append)
        self.assertEqual(left, ["brightness"])
        self.assertTrue(any("brightness" in m for m in msgs))

    def lens_orig(self):
        text = CONTROLS.split("Camera Controls")[0] + "\nCamera Controls\n\n" + LENS
        return ct.parse_controls(text)

    def test_controls_the_tool_never_wrote_are_left_alone(self):
        # Autofocus and auto exposure drive focus, exposure time and gain on their own.
        orig = self.lens_orig()
        cam = FakeCam(orig)
        cam.c["focus_absolute"].value = 200      # the lens moved by itself
        cam.c["exposure_time_absolute"].value = 80
        cam.c["brightness"].value = -10           # the tool's sweep left this one
        cam.tool_wrote("brightness")
        left = self.quiet(cam, orig)
        self.assertEqual(left, [])
        self.assertEqual(cam.writes, [("brightness", 2)])
        self.assertEqual(cam.c["focus_absolute"].value, 200)  # untouched

    def test_quick_run_writes_nothing_despite_drift(self):
        orig = self.lens_orig()
        cam = FakeCam(orig)
        cam.c["focus_absolute"].value = 7
        cam.c["auto_exposure"].value = 1  # even a master flipped by something else
        self.assertEqual(self.quiet(cam, orig), [])
        self.assertEqual(cam.writes, [])

    def test_deny_listed_and_not_measurable_controls_are_never_restored(self):
        orig = self.lens_orig()
        cam = FakeCam(orig)
        cam.c["focus_absolute"].value = 200
        cam.c["exposure_time_absolute"].value = 80
        cam.c["power_line_frequency"] = ct.Control("power_line_frequency", 0x980918, "menu", 0, 2, 1, 2, 1)
        orig["power_line_frequency"] = ct.Control("power_line_frequency", 0x980918, "menu", 0, 2, 1, 2, 2)
        cam.tool_wrote("focus_absolute", "exposure_time_absolute", "focus_automatic_continuous",
                       "power_line_frequency")
        self.assertEqual(self.quiet(cam, orig), [])
        self.assertEqual(cam.writes, [])

    def test_inactive_at_start_is_skipped_once_its_master_is_back(self):
        cam = FakeCam(self.orig)  # auto_exposure at its start value 3: gain inactive
        cam.c["gain"].value = 99  # the camera drives it
        cam.tool_wrote("gain", "auto_exposure")
        left = self.quiet(cam)
        self.assertEqual(left, [])
        self.assertNotIn("gain", [w[0] for w in cam.writes])

    def test_masters_that_started_manual_first_and_auto_last(self):
        text = (CONTROLS.replace("white_balance_automatic 0x0098090c (bool)   : default=1 value=1",
                                 "white_balance_automatic 0x0098090c (bool)   : default=1 value=0")
                .replace("value=6500 flags=inactive, has-min-max", "value=4000 flags=has-min-max"))
        orig = ct.parse_controls(text)
        cam = FakeCam(orig)
        cam.c["white_balance_automatic"].value = 1   # the sweep left it on auto: temperature inactive
        cam.c["white_balance_temperature"].value = 6500
        cam.c["brightness"].value = 9
        cam.c["auto_exposure"].value = 1             # started on auto (3)
        cam.tool_wrote("auto_exposure", "brightness", "white_balance_temperature", "white_balance_automatic")
        left = self.quiet(cam, orig)
        self.assertEqual(left, [])
        order = [w[0] for w in cam.writes]
        self.assertEqual(order[0], "white_balance_automatic")           # manual master first
        self.assertLess(order.index("white_balance_temperature"), order.index("auto_exposure"))
        self.assertLess(order.index("brightness"), order.index("auto_exposure"))
        self.assertEqual(order[-1], "auto_exposure")                    # auto master last
        self.assertEqual(cam.c["white_balance_temperature"].value, 4000)

    def test_a_written_control_that_still_differs_is_retried(self):
        cam = FakeCam(self.orig, flaky={"brightness": 1})  # the first write does not take
        cam.c["brightness"].value = 9
        cam.tool_wrote("brightness")
        self.assertEqual(self.quiet(cam), [])
        self.assertEqual(cam.c["brightness"].value, 2)
        self.assertEqual([w for w in cam.writes if w[0] == "brightness"], [("brightness", 2)] * 2)


class Safety(unittest.TestCase):
    def test_second_stream_is_refused(self):
        runner = ct.Runner()
        cam = ct.Camera("/dev/video99", runner)
        runner.streaming = True
        with self.assertRaises(RuntimeError):
            cam.capture(ct.Mode("YUYV", 640, 480, 30.0), 30, 15, 24)

    def test_streaming_flag_is_cleared_after_a_failed_capture(self):
        runner = ct.Runner()

        def boom(*_a, **_k):
            raise OSError("no v4l2-ctl")
        runner.run = boom
        cam = ct.Camera("/dev/video99", runner)
        cam._fmt_cache[("YUYV", 640, 480)] = {}
        with self.assertRaises(OSError):
            cam.capture(ct.Mode("YUYV", 640, 480, 30.0), 30, 15, 24)
        self.assertFalse(runner.streaming)

    def test_busy_node_is_refused_unless_forced(self):
        orig = ct.node_holders
        ct.node_holders = lambda dev: [4242]
        try:
            with tempfile.NamedTemporaryFile() as f:
                args = ct.build_parser().parse_args([f.name])
                with self.assertRaises(ct.ToolError) as cm:
                    ct.run(args, log=lambda *_: None)
                self.assertIn("4242", str(cm.exception))
                self.assertIn("--force", str(cm.exception))
        finally:
            ct.node_holders = orig

    def test_shared_hub_skips_big_raw_modes_but_not_mjpeg_or_small(self):
        captured = []

        class Cam:
            def capture(self, mode, *a, **k):
                captured.append(mode.label)
                return {"fmt": mode.fmt, "width": mode.width, "height": mode.height, "error": "x"}

        modes = [ct.Mode("YUYV", 1280, 720, 30.0), ct.Mode("YUYV", 320, 240, 30.0),
                 ct.Mode("MJPG", 1920, 1080, 30.0)]
        args = argparse.Namespace(allow_shared_hub=False, seconds=1.0)
        res = ct.measure_modes(Cam(), modes, {"shared_hub_with_hid": True}, args, log=lambda *_: None)
        self.assertIn("shared with HID", res[0]["skipped"])
        self.assertEqual(captured, ["YUYV 320x240@30", "MJPG 1920x1080@30"])
        captured.clear()
        args.allow_shared_hub = True
        ct.measure_modes(Cam(), modes, {"shared_hub_with_hid": True}, args, log=lambda *_: None)
        self.assertEqual(len(captured), 3)
        captured.clear()
        ct.measure_modes(Cam(), modes, {"shared_hub_with_hid": False}, argparse.Namespace(allow_shared_hub=False, seconds=1.0), log=lambda *_: None)
        self.assertEqual(len(captured), 3)

    def test_choose_primary(self):
        def m(fmt, w, h, ok=True, skipped=None):
            d = {"fmt": fmt, "width": w, "height": h}
            if ok:
                d["analysis"] = {"x": 1}
            if skipped:
                d["skipped"] = skipped
            return d
        p = ct.choose_primary([m("MJPG", 640, 480), m("YUYV", 1280, 720), m("YUYV", 640, 480),
                               m("YUYV", 320, 240), m("YUYV", 1920, 1080, skipped="hub")])
        self.assertEqual((p["fmt"], p["width"]), ("YUYV", 640))
        self.assertEqual(ct.choose_primary([m("MJPG", 1280, 720), m("MJPG", 640, 480)])["width"], 640)
        self.assertIsNone(ct.choose_primary([m("YUYV", 640, 480, ok=False)]))

    def test_script_is_executable_and_help_explains_safety(self):
        import os
        self.assertTrue(os.access(SCRIPT, os.X_OK))
        text = ct.HELP_EPILOG
        for word in ("fuser", "RESTORES", "SIGINT/SIGTERM", "privacy", "--allow-shared-hub", "never two"):
            self.assertIn(word, text)


class Sweep(unittest.TestCase):
    def test_sweep_restores_each_control_and_classifies(self):
        orig = ct.parse_controls(CONTROLS)
        cam = FakeCam(orig)
        # brightness has an effect, auto white balance fails to switch to manual (EPROTO).
        cam.fail = {("white_balance_automatic", 0)}

        def capture(mode, frames, warmup, analysis_frames, with_info=True):
            bright = cam.c["brightness"].value
            y = 100 + bright * 5
            u = -30 if cam.writes and cam.writes[-1] == ("white_balance_automatic", 0) else 0
            return {"analysis": {"luma": {"mean": y}, "chroma": {"U": u, "V": 0.0}, "y_std": 40.0,
                                 "sharpness_mean": 50.0, "temporal_noise": {"midtone": 0.5, "p25": 0.4}}}
        cam.capture = capture
        args = argparse.Namespace(settle=0.0)
        import time as _t
        orig_sleep = _t.sleep
        _t.sleep = lambda *_: None
        try:
            out = ct.sweep_controls(cam, ct.Mode("YUYV", 640, 480, 30.0), orig, args, log=lambda *_: None)
        finally:
            _t.sleep = orig_sleep
        verdicts = {c["name"]: c["verdict"] for c in out["controls"]}
        self.assertEqual(verdicts["brightness"], "effect")
        self.assertEqual(verdicts["white_balance_automatic"], "broken")
        self.assertNotIn("privacy", verdicts)
        self.assertNotIn("power_line_frequency", verdicts)
        self.assertEqual(cam.c["brightness"].value, orig["brightness"].value)
        self.assertEqual(cam.c["white_balance_automatic"].value, orig["white_balance_automatic"].value)
        self.assertEqual(cam.c["auto_exposure"].value, orig["auto_exposure"].value)


if __name__ == "__main__":
    unittest.main()
