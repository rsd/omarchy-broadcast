#!/usr/bin/env python3
"""camera-tune.py: measure ONE camera and propose rows for the camera quirks table.

This is how a new camera is added to crates/obc-pipeline/src/quirks.rs. Run it
on a camera that nothing else is using, pointed at a STATIC, evenly lit scene
that has some highlights and some shadows (a desk with a lamp, not a grey
wall), then paste the printed Rust rows into the table after reading the
evidence next to each. The rows are proposals: a human decides.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import math
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

try:
    import numpy as np
except ImportError:  # pragma: no cover - exercised only without numpy
    print("camera-tune: numpy is required (python3 -m pip install numpy)", file=sys.stderr)
    sys.exit(2)  # the exit status of every missing requirement

HELP_EPILOG = """\
WHAT IT DOES
  1. Identifies the camera: card and driver (v4l2-ctl), USB vendor:product and
     product name and USB path (sysfs), and whether the camera sits behind a
     hub that also carries HID devices (keyboard, mouse).
  2. Lists the modes (v4l2-ctl --list-formats-ext) and measures, for every
     format, its largest sizes up to 1080p (--sizes) at their top frame rate,
     for --seconds each: delivered fps against nominal (kernel buffer
     timestamps, after 15 warm-up frames), interval jitter, first-frame time,
     error frames, luma range and the share outside 16-235, temporal noise
     (mid-tones, static scene), spatial noise (Immerkaer), sharpness
     (Laplacian variance at 640x360).
  3. Unless --quick: sweeps the integer, menu and bool controls one at a time
     at min, default and max (a settle time, then a short capture measuring
     mean Y/U/V and noise). Controls gated by an automatic master (white
     balance, exposure) are swept with the master switched to manual.
  4. Prints a summary and PROPOSED Rust rows: RangeQuirk (raw luma outside the
     limited range, the same rule as the daemon's run-time detector),
     ControlQuirk (NoEffect: the sweep changes nothing measurable; Broken: a
     write fails with EPROTO/EIO or the chroma jumps), CameraProfile (empty
     values unless --recommend NAME=VALUE is given; noisy = mid-tone temporal
     noise above 1.0).

SAFETY
  * Refuses when any other process holds the video node (fuser), unless
    --force. Stop the daemon first; this tool never starts or stops anything.
  * Streams one mode at a time on one node, never two.
  * Reads every control first and RESTORES the ones it wrote, on exit, on an
    exception, and on SIGINT/SIGTERM (a second signal during the restore is
    ignored so it can finish). Controls it never wrote (a focus or an exposure
    the camera drives itself) are left alone. A control that was inactive at
    the start is not written back once its automatic master is back at its
    starting value.
  * Never writes privacy, LED, motion (pan, tilt, zoom, roll, iris, focus)
    controls, the power-line frequency, or anything read-only or inactive
    (a dependent of an automatic master is swept with the master on manual).
  * If the camera sits behind a hub shared with HID devices (a keyboard or a
    mouse on a non-root hub above the camera), a raw mode that needs at least
    ~12 MB/s is skipped with a warning, unless --allow-shared-hub (streaming
    such a mode once reset a keyboard and mouse).
  * Raw captures go to a temporary directory (--tmpdir) and are deleted; large
    modes are cut short at ~300 MB plus the warm-up frames.

LIMITS
  Every sweep point restarts the stream, so the camera's auto exposure
  re-converges: tolerances are widened by the run-to-run difference of two
  baseline captures. A moving scene shows as noise. Writes of the colour
  controls may legitimately shift chroma a lot (white balance extremes): a
  Broken proposal from a chroma jump alone needs a human look.
"""

# --- constants ---------------------------------------------------------------

WARMUP_FRAMES = 15
ANALYSIS_FRAMES = 24  # the last usable frames of a capture that are analysed
SWEEP_WARMUP = 20
SWEEP_FRAMES = 12
MAX_RAW_BYTES = 300_000_000
HUB_BANDWIDTH_LIMIT = 12_000_000  # bytes per second
NOISY_THRESHOLD = 1.0  # mid-tone temporal noise, code values
MAX_WIDTH, MAX_HEIGHT = 1920, 1080

# The daemon's run-time detector (crates/obc-pipeline/src/colour_range.rs).
RANGE_MARGIN = 2
SUSPECT_SHARE = 0.01
MIN_SAMPLES = 1000

# Sweep classification.
CHROMA_JUMP = 25.0  # mean U or V moving this far (code values) from the baseline
TOL_Y = 1.0
TOL_C = 1.0
TOL_STD_REL = 0.05
TOL_LAP_REL = 0.20

COMPRESSED = {"MJPG", "JPEG", "H264", "HEVC", "MPEG", "VP80", "VP90"}
BYTES_PER_PIXEL = {
    "YUYV": 2.0, "UYVY": 2.0, "NV12": 1.5, "NV21": 1.5, "YU12": 1.5,
    "YV12": 1.5, "RGB3": 3.0, "BGR3": 3.0, "GREY": 1.0,
}

# Control name tokens never written (privacy, LED, motion, lens motors).
DENY_TOKENS = {
    "privacy", "led", "led1", "pan", "tilt", "zoom", "roll", "motion", "ptz",
    "iris", "focus", "reset", "region",
}
# Controls that do not show in the picture, or whose sweep is too slow.
NOT_MEASURABLE = {
    "power_line_frequency", "exposure_dynamic_framerate",
    "exposure_auto_priority", "exposure_time_absolute",
}
# Automatic masters: name -> (manual value, dependents they make inactive).
MASTERS = {
    "white_balance_automatic": (0, ["white_balance_temperature"]),
    "auto_exposure": (1, ["exposure_time_absolute", "gain"]),
    "focus_automatic_continuous": (0, ["focus_absolute"]),
}

# V4L2 fourcc -> obc_core::PixelFormat variant (formats the quirks table knows).
PIXEL_FORMAT = {
    "YUYV": "Yuy2", "UYVY": "Uyvy", "NV12": "Nv12", "NV21": "Nv21",
    "YU12": "I420", "YV12": "Yv12", "RGB3": "Rgb", "BGR3": "Bgr",
}

# Control name -> (V4L2 id, Rust expression the table uses for it).
KNOWN_CONTROLS = {
    "brightness": (0x00980900, "crate::controls::CID_BRIGHTNESS"),
    "contrast": (0x00980901, "crate::controls::CID_CONTRAST"),
    "saturation": (0x00980902, "crate::controls::CID_SATURATION"),
    "hue": (0x00980903, "CID_HUE"),
    "white_balance_automatic": (0x0098090C, "CID_AUTO_WHITE_BALANCE"),
    "gamma": (0x00980910, None),
    "gain": (0x00980913, None),
    "white_balance_temperature": (0x0098091A, "CID_WHITE_BALANCE_TEMPERATURE"),
    "sharpness": (0x0098091B, "crate::controls::CID_SHARPNESS"),
    "backlight_compensation": (0x0098091C, "crate::controls::CID_BACKLIGHT_COMPENSATION"),
}


class Interrupted(BaseException):
    """Raised by the SIGINT/SIGTERM handler: unwinds through the restore."""


class ToolError(Exception):
    """A failure the user should read, not a traceback."""


# --- v4l2-ctl output parsing ---------------------------------------------------

@dataclasses.dataclass(frozen=True)
class Mode:
    fmt: str
    width: int
    height: int
    fps: float  # top rate at this size
    rates: tuple = ()

    @property
    def label(self) -> str:
        return f"{self.fmt} {self.width}x{self.height}@{self.fps:g}"


def parse_modes(text: str) -> list:
    """Modes of the video-capture type in `v4l2-ctl --list-formats-ext` output."""
    modes = []
    capture = False
    fmt = None
    size = None  # [w, h, rates]
    pending = []

    def flush():
        nonlocal size
        if fmt and size and size[2]:
            rates = tuple(sorted(set(size[2]), reverse=True))
            pending.append(Mode(fmt, size[0], size[1], rates[0], rates))
        size = None

    for line in text.splitlines():
        m = re.match(r"\s*Type:\s*(.*)$", line)
        if m:
            flush()
            capture = m.group(1).strip().lower().startswith("video capture")
            fmt = None
            continue
        if not capture:
            continue
        m = re.match(r"\s*\[\d+\]:\s*'(\w{4})'", line)
        if m:
            flush()
            fmt = m.group(1)
            continue
        m = re.match(r"\s*Size:\s*Discrete\s+(\d+)x(\d+)", line)
        if m:
            flush()
            size = [int(m.group(1)), int(m.group(2)), []]
            continue
        m = re.match(r"\s*Size:\s*Stepwise\s+\d+x\d+\s*-\s*(\d+)x(\d+)", line)
        if m:
            flush()
            size = [int(m.group(1)), int(m.group(2)), []]
            continue
        m = re.search(r"Interval:.*\(([\d.]+)(?:-([\d.]+))?\s*fps\)", line)
        if m and size is not None:
            size[2].append(max(float(m.group(1)), float(m.group(2) or 0)))
    flush()
    modes.extend(pending)
    return modes


def select_modes(modes: list, per_format: int = 2) -> list:
    """Per format, the largest `per_format` sizes up to 1080p at their top rate."""
    out = []
    formats = []
    for m in modes:
        if m.fmt not in formats:
            formats.append(m.fmt)
    for f in formats:
        sized = [m for m in modes if m.fmt == f and m.width <= MAX_WIDTH and m.height <= MAX_HEIGHT]
        sized.sort(key=lambda m: m.width * m.height, reverse=True)
        out.extend(sized[:per_format])
    return out


def mode_bandwidth(mode: Mode) -> float:
    """Bytes per second of the mode on the wire (0 for compressed formats)."""
    if mode.fmt in COMPRESSED:
        return 0.0
    return mode.width * mode.height * BYTES_PER_PIXEL.get(mode.fmt, 2.0) * mode.fps


_BUF_RE = re.compile(r"seq:\s*(\d+)\s+bytesused:\s*(\d+)\s+ts:\s*([\d.]+)(.*)$")


def parse_buffer_line(line: str):
    """One `v4l2-ctl --verbose` dequeued-buffer line -> dict, or None.

    Lines look like `cap dqbuf:  3 seq:      3 bytesused: 614400 ts: 4811.212345
    ... (flags)`. The kernel flags follow the timestamp; `error` among them is
    V4L2_BUF_FLAG_ERROR.
    """
    if "dqbuf" not in line:
        return None
    m = _BUF_RE.search(line)
    if not m:
        return None
    tail = m.group(4)
    flags = set(re.findall(r"[a-z][a-z0-9-]*", tail.lower()))
    return {
        "seq": int(m.group(1)),
        "bytesused": int(m.group(2)),
        "ts": float(m.group(3)),
        "error": "error" in flags,
    }


def parse_buffers(text: str) -> list:
    return [b for b in (parse_buffer_line(l) for l in text.splitlines()) if b]


def parse_fmt_info(text: str) -> dict:
    """`v4l2-ctl --get-fmt-video` output -> width, height, stride, sizeimage, labels."""
    def field(key):
        m = re.search(re.escape(key) + r"\s*:\s*(.*)", text)
        return m.group(1).strip() if m else None

    out = {}
    wh = field("Width/Height")
    if wh and "/" in wh:
        w, h = wh.split("/")
        out["width"], out["height"] = int(w), int(h)
    for key, name in (("Bytes per Line", "stride"), ("Size Image", "sizeimage")):
        v = field(key)
        if v and v.split()[0].isdigit():
            out[name] = int(v.split()[0])
    out["colorspace"] = field("Colorspace")
    out["transfer"] = field("Transfer Function")
    out["encoding"] = field("YCbCr/HSV Encoding")
    q = field("Quantization")
    out["quantization"] = q
    out["label_full"] = bool(q) and "full" in q.lower() and "limited" not in q.lower()
    return out


@dataclasses.dataclass
class Control:
    name: str
    cid: int
    type: str
    min: int = 0
    max: int = 0
    step: int = 1
    default: int = 0
    value: int = 0
    flags: tuple = ()
    menu: tuple = ()

    @property
    def writable(self) -> bool:
        return not ({"read-only", "inactive", "disabled", "write-only", "has-payload"} & set(self.flags))


def _to_int(s: str) -> int:
    return int(s, 0)


def parse_controls(text: str) -> dict:
    """`v4l2-ctl -L` output -> {name: Control}, in listing order."""
    out = {}
    last = None
    for line in text.splitlines():
        m = re.match(r"^\s*(\w+)\s+0x([0-9a-fA-F]+)\s+\((\w+)\)\s*:\s*(.*)$", line)
        if m:
            name, cid, typ, rest = m.group(1), int(m.group(2), 16), m.group(3), m.group(4)
            flags = ()
            fm = re.search(r"flags=(.*)$", rest)
            if fm:
                flags = tuple(f.strip() for f in fm.group(1).split(",") if f.strip())
                rest = rest[: fm.start()]
            kv = dict(re.findall(r"(\w+)=(-?\w+)", rest))
            try:
                c = Control(
                    name, cid, typ,
                    min=_to_int(kv.get("min", "0")), max=_to_int(kv.get("max", "1" if typ == "bool" else "0")),
                    step=_to_int(kv.get("step", "1")), default=_to_int(kv.get("default", "0")),
                    value=_to_int(kv.get("value", "0")), flags=flags,
                )
            except ValueError:
                last = None
                continue
            out[name] = c
            last = c
            continue
        mm = re.match(r"^\s+(-?\d+):\s*(.*)$", line)
        if mm and last is not None and last.type == "menu":
            last.menu = last.menu + (int(mm.group(1)),)
    return out


# --- sysfs: USB identity and the hub walk ---------------------------------------

_USB_DEV = re.compile(r"^\d+-\d+(\.\d+)*$")


def _read(p: Path):
    try:
        return p.read_text().strip()
    except OSError:
        return None


def usb_device_of_node(sysfs: Path, node: str):
    """The sysfs directory of the USB device behind a /dev/videoN node, or None."""
    link = sysfs / "class" / "video4linux" / Path(node).name / "device"
    if not link.exists():
        return None
    p = link.resolve()
    root = sysfs.resolve()
    while p != root and p != p.parent:
        if (p / "idVendor").is_file():
            return p
        p = p.parent
    return None


def describe_usb(d: Path) -> dict:
    return {
        "path": d.name,
        "vendor": _read(d / "idVendor"),
        "product_id": _read(d / "idProduct"),
        "product": _read(d / "product"),
        "manufacturer": _read(d / "manufacturer"),
        "speed_mbps": _read(d / "speed"),
    }


def _interface_classes(d: Path) -> dict:
    """{interface directory name: bInterfaceClass} of one USB device."""
    out = {}
    try:
        children = sorted(d.iterdir())
    except OSError:
        return out
    for c in children:
        if ":" in c.name and c.is_dir():
            cls = _read(c / "bInterfaceClass")
            if cls is not None:
                out[c.name] = cls.lower()
    return out


def _is_usb_device_dir(p: Path) -> bool:
    return ":" not in p.name and (p / "idVendor").is_file()


def _kernel_path_key(name: str) -> list:
    return [int(n) if n.isdigit() else 1 << 32 for n in re.split(r"[-.]", name)]


def _hid_devices_below(hub: Path, camera: Path) -> list:
    """The HID devices anywhere below `hub`, as the daemon counts them.

    A HID device has a HID interface (class 03) and no video one (class 0e): a
    webcam with a button has a HID interface and is no keyboard or mouse. The
    camera (with what is below it) is skipped. Real directories only: sysfs
    links are not followed. Ordered by kernel path.
    """
    found = []

    def walk(d: Path):
        try:
            children = sorted(d.iterdir())
        except OSError:
            return
        for c in children:
            if c.is_symlink() or not c.is_dir() or not _USB_DEV.match(c.name) or c == camera:
                continue
            classes = _interface_classes(c)
            hid = [n for n, cls in classes.items() if cls == "03"]
            if hid and "0e" not in classes.values():
                found.append((c, hid))
            walk(c)

    walk(hub)
    found.sort(key=lambda e: _kernel_path_key(e[0].name))
    out = []
    for c, hid in found:
        e = describe_usb(c)
        e["hid_interfaces"] = hid
        out.append(e)
    return out


def usb_topology(sysfs: Path, node: str) -> dict:
    """USB identity, the USB path, and the HID devices that share the camera's hub.

    The same rule as the daemon (crates/obc-pipeline/src/usb_topology.rs): walk
    every hub above the camera up to, not including, the root hub; the nearest
    one with a HID device anywhere below it is the shared hub.
    """
    d = usb_device_of_node(sysfs, node)
    if d is None:
        return {"usb": None}
    info = {"usb": describe_usb(d)}
    chain = []
    p = d
    while p != p.parent and (p.name.startswith("usb") or _USB_DEV.match(p.name)):
        chain.append(p.name)
        p = p.parent
    info["usb_path"] = " <- ".join(chain)
    parent = d.parent
    info["parent"] = parent.name
    info["parent_is_root_hub"] = parent.name.startswith("usb")
    shared_hub, hid = None, []
    hub = parent
    while _is_usb_device_dir(hub) and not hub.name.startswith("usb"):
        found = _hid_devices_below(hub, d)
        if found:
            shared_hub, hid = hub.name, found
            break
        hub = hub.parent
    if shared_hub is None and info["parent_is_root_hub"]:
        # Reported, not gated: the root hub is the bus itself.
        hid = _hid_devices_below(parent, d)
    info["shared_hub"] = shared_hub
    info["hid_siblings"] = hid
    info["shared_hub_with_hid"] = shared_hub is not None
    return info


# --- measurements on frames ---------------------------------------------------

def timing_stats(buffers: list, nominal_fps: float, t0=None, warmup: int = WARMUP_FRAMES) -> dict:
    """Delivered fps, interval jitter, drops and error frames from kernel buffer data."""
    out = {"frames": len(buffers), "error_frames": sum(1 for b in buffers if b["error"])}
    if not buffers:
        return out
    ts = [b["ts"] for b in buffers]
    seq = [b["seq"] for b in buffers]
    out["dropped"] = int(sum(b - a - 1 for a, b in zip(seq, seq[1:]) if b - a > 1))
    if t0 is not None and 0 <= ts[0] - t0 < 60:
        out["first_frame_s"] = round(ts[0] - t0, 3)
    use = ts[warmup:] if len(ts) - warmup >= 3 else ts
    d = np.diff(np.array(use, dtype=np.float64)) * 1000.0
    d = d[d > 0]
    if d.size == 0:
        return out
    out["fps"] = round(float(1000.0 / d.mean()), 2)
    out["nominal_fps"] = nominal_fps
    out["interval_ms"] = {
        "mean": round(float(d.mean()), 2), "std": round(float(d.std()), 2),
        "min": round(float(d.min()), 2), "max": round(float(d.max()), 2),
        "p99": round(float(np.percentile(d, 99)), 2),
    }
    if nominal_fps > 0:
        out["late_frames"] = int((d > 1.5 * 1000.0 / nominal_fps).sum())
    return out


def luma_hist(y) -> "np.ndarray":
    """256-bin luma histogram of an array (or a list of arrays)."""
    arrs = y if isinstance(y, (list, tuple)) else [y]
    h = np.zeros(256, dtype=np.int64)
    for a in arrs:
        h += np.bincount(np.asarray(a, dtype=np.uint8).ravel(), minlength=256)
    return h


def range_evidence(hist):
    """The daemon's evidence: min/max luma and the samples outside 16-235 beyond the margin."""
    hist = np.asarray(hist)
    nz = np.nonzero(hist)[0]
    if nz.size == 0:
        return None
    low = 16 - RANGE_MARGIN
    high = 235 + RANGE_MARGIN
    outside = int(hist[:low].sum() + hist[high + 1:].sum())
    samples = int(hist.sum())
    return {
        "min": int(nz[0]), "max": int(nz[-1]), "outside": outside, "samples": samples,
        "share": outside / samples if samples else 0.0,
    }


def suggests_full_range(hist):
    """Same rule as colour_range::suggests_full_range: the evidence when suspicious."""
    e = range_evidence(hist)
    if e and e["samples"] >= MIN_SAMPLES and e["share"] >= SUSPECT_SHARE:
        return e
    return None


def immerkaer(img) -> float:
    """Immerkaer's single-frame noise estimate (standard deviation, code values)."""
    img = np.asarray(img, dtype=np.float64)
    if img.shape[0] < 3 or img.shape[1] < 3:
        return 0.0
    k = (img[:-2, :-2] - 2 * img[:-2, 1:-1] + img[:-2, 2:]
         - 2 * img[1:-1, :-2] + 4 * img[1:-1, 1:-1] - 2 * img[1:-1, 2:]
         + img[2:, :-2] - 2 * img[2:, 1:-1] + img[2:, 2:])
    return float(math.sqrt(math.pi / 2) * np.abs(k).sum() / (6 * (img.shape[0] - 2) * (img.shape[1] - 2)))


def laplacian_variance(img) -> float:
    img = np.asarray(img, dtype=np.float64)
    if img.shape[0] < 3 or img.shape[1] < 3:
        return 0.0
    lap = (-4 * img[1:-1, 1:-1] + img[:-2, 1:-1] + img[2:, 1:-1]
           + img[1:-1, :-2] + img[1:-1, 2:])
    return float(lap.var())


def downscale_area(img, width: int, height: int):
    """Area-average downscale to (height, width); never upscales."""
    img = np.asarray(img, dtype=np.float64)
    h, w = img.shape
    width, height = min(width, w), min(height, h)
    if (width, height) == (w, h):
        return img
    ys = (np.arange(height) * h) // height
    xs = (np.arange(width) * w) // width
    rows = np.add.reduceat(img, ys, axis=0) / np.diff(np.append(ys, h))[:, None]
    return np.add.reduceat(rows, xs, axis=1) / np.diff(np.append(xs, w))[None, :]


def sharpness_640(img) -> float:
    """Laplacian variance at 640x360, so modes compare on one scale."""
    return laplacian_variance(downscale_area(img, 640, 360))


def temporal_noise(frames) -> dict:
    """Per-pixel temporal std over a static scene, plus the mean image.

    `frames` is (K, H, W). Returns the p25 of the std, the median std over the
    mid-tones (mean image between 60 and 190, where clipping hides nothing),
    and the mean image.
    """
    k = frames.shape[0]
    mean = frames.mean(axis=0, dtype=np.float64)
    acc = np.zeros(mean.shape, dtype=np.float64)
    for i in range(k):
        acc += (frames[i].astype(np.float64) - mean) ** 2
    std = np.sqrt(acc / k)
    band = (mean > 60) & (mean < 190)
    return {
        "p25": float(np.percentile(std, 25)),
        "midtone": float(np.median(std[band])) if band.any() else None,
        "mean_image": mean,
    }


def analyze_frames(y, u=None, v=None) -> dict:
    """Metrics from K decoded frames: y (K,H,W), u/v (K,h,w) or None."""
    k = y.shape[0]
    hist = luma_hist([y[i] for i in range(k)])
    total = int(hist.sum())
    cum = np.cumsum(hist)
    nz = np.nonzero(hist)[0]

    def pct(p):
        return int(np.searchsorted(cum, p / 100.0 * total))

    res = {
        "frames": k,
        "luma": {
            "mean": round(float((np.arange(256) * hist).sum() / total), 2),
            "min": int(nz[0]), "max": int(nz[-1]),
            "p0.1": pct(0.1), "p99.9": min(pct(99.9), 255),
            "frac_below16": round(float(hist[:16].sum() / total), 4),
            "frac_above235": round(float(hist[236:].sum() / total), 4),
        },
    }
    if u is not None and v is not None:
        res["chroma"] = {"U": round(float(u.mean(dtype=np.float64)), 2), "V": round(float(v.mean(dtype=np.float64)), 2)}
    res["range_evidence"] = range_evidence(hist)
    res["range_suspect"] = suggests_full_range(hist) is not None
    if k >= 4:
        tn = temporal_noise(y)
        mean_img = tn.pop("mean_image")
        res["temporal_noise"] = {kk: (round(vv, 3) if vv is not None else None) for kk, vv in tn.items()}
        res["y_std"] = round(float(mean_img.std()), 3)
        res["sharpness_mean"] = round(sharpness_640(mean_img), 1)
    res["noise_immerkaer"] = round(immerkaer(y[-1]), 3)
    res["sharpness"] = round(sharpness_640(y[-1]), 1)
    return res


def midtone_noise(analysis: dict):
    """The mid-tone temporal noise of an analysis (p25 when no mid-tones exist)."""
    tn = (analysis or {}).get("temporal_noise") or {}
    return tn.get("midtone") if tn.get("midtone") is not None else tn.get("p25")


# --- frame decoding ---------------------------------------------------------------

def split_planes(buf, fmt: str, width: int, height: int, stride: int, framesize: int):
    """Raw frame bytes (K, framesize) uint8 -> (Y, U, V) views, or None for an unknown format."""
    k = buf.shape[0]
    if fmt in ("YUYV", "UYVY"):
        if stride < width * 2 or stride * height > framesize:
            return None
        a = buf[:, : stride * height].reshape(k, height, stride)[:, :, : width * 2]
        if fmt == "YUYV":
            return a[:, :, 0::2], a[:, :, 1::4], a[:, :, 3::4]
        return a[:, :, 1::2], a[:, :, 0::4], a[:, :, 2::4]
    if fmt in ("NV12", "NV21"):
        ch = height // 2
        if stride < width or stride * (height + ch) > framesize:
            return None
        y = buf[:, : stride * height].reshape(k, height, stride)[:, :, :width]
        uv = buf[:, stride * height: stride * (height + ch)].reshape(k, ch, stride)[:, :, :width]
        u, v = uv[:, :, 0::2], uv[:, :, 1::2]
        return (y, u, v) if fmt == "NV12" else (y, v, u)
    if fmt in ("YU12", "YV12"):
        ch, cs = height // 2, stride // 2
        if stride < width or stride * height + 2 * cs * ch > framesize:
            return None
        y = buf[:, : stride * height].reshape(k, height, stride)[:, :, :width]
        o = stride * height
        p1 = buf[:, o: o + cs * ch].reshape(k, ch, cs)[:, :, : width // 2]
        p2 = buf[:, o + cs * ch: o + 2 * cs * ch].reshape(k, ch, cs)[:, :, : width // 2]
        return (y, p1, p2) if fmt == "YU12" else (y, p2, p1)
    if fmt == "GREY":
        if stride < width or stride * height > framesize:
            return None
        return buf[:, : stride * height].reshape(k, height, stride)[:, :, :width], None, None
    return None


# --- sweep classification ----------------------------------------------------------

def metrics_of(analysis: dict):
    """The few numbers a sweep point is compared on, from an analysis."""
    if not analysis or "chroma" not in analysis:
        return None
    return {
        "y": analysis["luma"]["mean"], "u": analysis["chroma"]["U"], "v": analysis["chroma"]["V"],
        "std": analysis.get("y_std"), "lap": analysis.get("sharpness_mean"),
        "noise": midtone_noise(analysis),
    }


def _rel(a, b):
    if a is None or b is None:
        return 0.0
    return abs(a - b) / max(abs(a), abs(b), 1e-9)


def tolerances(base_a: dict, base_b: dict) -> dict:
    """Comparison tolerances: the floors, widened by the difference of two baseline captures."""
    return {
        "y": max(TOL_Y, 2 * abs(base_a["y"] - base_b["y"])),
        "c": max(TOL_C, 2 * abs(base_a["u"] - base_b["u"]), 2 * abs(base_a["v"] - base_b["v"])),
        "std": max(TOL_STD_REL, 2 * _rel(base_a["std"], base_b["std"])),
        "lap": max(TOL_LAP_REL, 2 * _rel(base_a["lap"], base_b["lap"])),
    }


DEFAULT_TOL = {"y": TOL_Y, "c": TOL_C, "std": TOL_STD_REL, "lap": TOL_LAP_REL}
_HARD_ERRORS = ("protocol error", "input/output error", "eproto", "eio")


def is_hard_error(error) -> bool:
    return bool(error) and any(t in error.lower() for t in _HARD_ERRORS)


def classify_control(points: list, baseline: dict, tol: dict = None) -> tuple:
    """(verdict, reason) for one swept control.

    `points` are dicts {value, ok, error, metrics}; `baseline` the metrics of
    the unmodified camera. Verdicts: "broken" (a write failed with EPROTO/EIO,
    or mean U/V moved CHROMA_JUMP or more from the baseline), "noeffect" (at
    least two distinct values written fine and nothing measurable changed),
    "effect", or "unknown" (not enough data).
    """
    tol = tol or DEFAULT_TOL
    for p in points:
        if not p.get("ok") and is_hard_error(p.get("error")):
            return "broken", f"write of {p['value']} failed: {p['error'].strip()}"
    measured = [p for p in points if p.get("ok") and p.get("metrics")]
    for p in measured:
        m = p["metrics"]
        jump = max(abs(m["u"] - baseline["u"]), abs(m["v"] - baseline["v"]))
        if jump >= CHROMA_JUMP:
            return "broken", f"value {p['value']} moves the chroma by {jump:.0f} code values"
    if any(not p.get("ok") for p in points):
        return "unknown", "a write failed (not EPROTO/EIO)"
    if len(measured) < 2 or len({p["value"] for p in measured}) < 2:
        return "unknown", "fewer than two measured values"
    ms = [p["metrics"] for p in measured]

    def spread(key):
        vals = [m[key] for m in ms]
        return max(vals) - min(vals)

    changes = []
    if spread("y") > tol["y"]:
        changes.append(f"mean Y moves {spread('y'):.1f}")
    if spread("u") > tol["c"] or spread("v") > tol["c"]:
        changes.append(f"mean U/V move {max(spread('u'), spread('v')):.1f}")
    stds = [m["std"] for m in ms if m.get("std") is not None]
    if stds and (max(stds) - min(stds)) / max(max(stds), 1e-9) > tol["std"]:
        changes.append("contrast changes")
    laps = [m["lap"] for m in ms if m.get("lap") is not None]
    if laps and (max(laps) - min(laps)) / max(max(laps), 1e-9) > tol["lap"]:
        changes.append("sharpness changes")
    if changes:
        return "effect", "; ".join(changes)
    return "noeffect", (
        f"min/default/max change nothing measurable (mean Y within {spread('y'):.2f}, "
        f"U/V within {max(spread('u'), spread('v')):.2f})")


def sweep_values(c: Control) -> list:
    """The values to write: min, default, max (menus: the valid items; bools: 0 and 1)."""
    if c.type == "bool":
        return [0, 1]
    if c.type == "menu":
        items = list(c.menu) or [c.min, c.default, c.max]
        cand = [items[0], c.default if c.default in items else items[0], items[-1]]
    else:
        cand = [c.min, c.default, c.max]
    out = []
    for v in cand:
        if v not in out:
            out.append(v)
    return out


def is_sweepable(c: Control) -> bool:
    if c.type not in ("int", "bool", "menu") or not c.writable:
        return False
    if c.name in NOT_MEASURABLE:
        return False
    return not (set(c.name.split("_")) & DENY_TOKENS)


# --- Rust row text ----------------------------------------------------------------

def rust_str(s: str) -> str:
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def rust_usb(vendor: str, product: str) -> str:
    return f"usb(0x{int(vendor, 16):04x}, 0x{int(product, 16):04x})"


def rust_cid(cid: int) -> str:
    s = f"{cid:08x}"
    return f"0x{s[:4]}_{s[4:]}"


def rust_control_ref(name: str, cid: int) -> str:
    """The table's constant for a control, or a hex literal with its name."""
    known = KNOWN_CONTROLS.get(name)
    if known and known[0] == cid and known[1]:
        return known[1]
    return f"{rust_cid(cid)} /* {name} */"


def rust_range_row(usb: str, formats: list, model: str, evidence: dict) -> str:
    names = [PIXEL_FORMAT[f] for f in formats if f in PIXEL_FORMAT]
    lines = []
    for f, e in evidence.items():
        if f not in PIXEL_FORMAT:
            continue
        lines.append(
            f"    // {f}: luma {e['min']}-{e['max']}, {e['share'] * 100:.1f} % outside 16-235 "
            f"(margin {RANGE_MARGIN}) under a limited label.")
    lines += [
        "    RangeQuirk {",
        f"        usb: {usb},",
        f"        formats: &[{', '.join('PixelFormat::' + n for n in names)}],",
        f"        model: {rust_str(model)},",
        "    },",
    ]
    return "\n".join(lines)


def rust_control_row(usb: str, model: str, because: str, items: list, reset: list, comment: str) -> str:
    lines = [f"    // {c}" for c in comment.splitlines()]
    lines += [
        "    ControlQuirk {",
        f"        usb: {usb},",
        "        hidden: &[" + ", ".join(rust_control_ref(n, cid) for n, cid in items) + "],",
        f"        because: HiddenBecause::{because},",
        "        reset_at_open: &[" + ", ".join(rust_control_ref(n, cid) for n, cid in reset) + "],",
        f"        model: {rust_str(model)},",
        "    },",
    ]
    return "\n".join(lines)


def rust_profile_row(usb: str, model: str, values: list, noisy, noise_value) -> str:
    """The CameraProfile row; with the noise unmeasured, a note and no row.

    `noisy: false` would be a verdict the tool never reached, so an unmeasured
    noise yields no row at all: the values asked for are listed in the note and
    `noisy` is left to whoever writes the row by hand.
    """
    vals = ", ".join(f"({rust_control_ref(n, cid)}, {v})" for n, cid, v in values)
    if noise_value is None:
        lines = [
            "// noisy: not measured (no mode had a frame analysis), so no CameraProfile row is proposed.",
        ]
        if vals:
            lines.append(f"// values asked for with --recommend: &[{vals}]")
        return "\n".join(lines)
    return "\n".join([
        f"    // mid-tone temporal noise {noise_value:.2f}.",
        "    CameraProfile {",
        f"        usb: {usb},",
        f"        values: &[{vals}],",
        f"        noisy: {'true' if noisy else 'false'},",
        f"        model: {rust_str(model)},",
        "    },",
    ])


def parse_recommend(items: list) -> list:
    """['sharpness=5,hue=0'] -> [(name, cid, value)]; unknown names are an error."""
    out = []
    for item in items or []:
        for pair in item.split(","):
            pair = pair.strip()
            if not pair:
                continue
            name, sep, val = pair.partition("=")
            if not sep or not re.fullmatch(r"-?\d+", val.strip()):
                raise ToolError(f"--recommend wants name=value, got {pair!r}")
            name = name.strip()
            if name not in KNOWN_CONTROLS:
                raise ToolError(f"--recommend: unknown control {name!r} (known: {', '.join(KNOWN_CONTROLS)})")
            out.append((name, KNOWN_CONTROLS[name][0], int(val)))
    return out


def propose_rows(identity: dict, results: dict, recommend: list = None) -> str:
    """The Rust text proposed from a run's results (see main for the result layout)."""
    usb_info = identity.get("usb")
    if not usb_info:
        return "// Not a USB camera: no row can be keyed by a USB id."
    usb = rust_usb(usb_info["vendor"], usb_info["product_id"])
    model = usb_info.get("product") or identity.get("card") or "unknown camera"
    parts = []

    # RangeQuirk: raw formats whose luma is outside the limited range under a limited label.
    fmts, evidence = [], {}
    for m in results.get("modes", []):
        a = m.get("analysis")
        if not a or m["fmt"] in COMPRESSED or m.get("label_full"):
            continue
        if a.get("range_suspect") and m["fmt"] not in fmts:
            fmts.append(m["fmt"])
            evidence[m["fmt"]] = a["range_evidence"]
    mapped = [f for f in fmts if f in PIXEL_FORMAT]
    unmapped = [f for f in fmts if f not in PIXEL_FORMAT]
    if mapped:
        parts.append("// RANGE_QUIRKS\n" + rust_range_row(usb, mapped, model, evidence))
    unmeasured = [
        f"{m['fmt']} {m['width']}x{m['height']}" for m in results.get("modes", [])
        if not m.get("analysis") and m["fmt"] not in COMPRESSED and not m.get("label_full")
        and m["fmt"] not in fmts
    ]
    if unmeasured:
        parts.append(
            "// RANGE_QUIRKS: colour range not measured for " + ", ".join(unmeasured)
            + " (no frame analysis): no verdict either way for those modes.")
    if unmapped:
        parts.append(
            "// RANGE_QUIRKS: no row proposed for " + ", ".join(unmapped)
            + ": the luma looks full range under a limited label, but the format has no"
            " PixelFormat in the quirks table.")

    # ControlQuirk.
    sweep = results.get("sweep") or {}
    broken = [s for s in sweep.get("controls", []) if s["verdict"] == "broken"]
    noeff = [s for s in sweep.get("controls", []) if s["verdict"] == "noeffect"]
    rows = []
    if not sweep or sweep.get("error"):
        why = sweep.get("error") if sweep else "no sweep in this run"
        parts.append(f"// CONTROL_QUIRKS: controls not swept ({why}): no verdict on any control.")
    if broken:
        reset = [
            (s["name"], s["cid"]) for s in broken
            if any(p["value"] == s["default"] and p.get("ok") for p in s["points"])
        ]
        comment = "\n".join(f"{s['name']}: {s['reason']}" for s in broken)
        rows.append(("Broken", broken, reset, comment))
    if noeff:
        comment = "\n".join(f"{s['name']}: {s['reason']}" for s in noeff)
        rows.append(("NoEffect", noeff, [], comment))
    if rows:
        text = "// CONTROL_QUIRKS"
        if len(rows) > 1:
            text += (" (the table allows one row per camera: merge these by hand, or keep the"
                     " Broken row and note the NoEffect controls beside it)")
        for because, items, reset, comment in rows:
            text += "\n" + rust_control_row(usb, model, because, [(s["name"], s["cid"]) for s in items], reset, comment)
        parts.append(text)

    # CameraProfile.
    noise = results.get("noise")
    parts.append("// CAMERA_PROFILES\n" + rust_profile_row(
        usb, model, recommend or [], bool(noise is not None and noise > NOISY_THRESHOLD), noise))
    return "\n\n".join(parts)


def usable_frame_offsets(buffers: list, file_size: int, sizeimage: int) -> list:
    """Byte offsets, in a raw capture file, of the frames worth analysing.

    The driver can hand back short frames and frames flagged V4L2_BUF_FLAG_ERROR,
    and the daemon drops both, so the analysis leaves them out. Each frame's
    record (`bytesused`, the error flag) is matched to its place in the file:

    1. The recorded sizes add up to the file: v4l2-ctl wrote each buffer's
       `bytesused` bytes, so the offsets are the running sum of those sizes.
    2. Otherwise, the file is a whole number of full frames: the records are
       aligned to the frames from the end, the last record to the last frame.
       v4l2-ctl can log one dequeue more than it writes (the first, flagged
       error, on a UVC camera), and the analysis only reads the last frames.
       A frame whose aligned record is flagged error or short is left out; a
       frame with no record at all (fewer records than frames) is kept only
       when no record was given for any frame.
    3. Otherwise the frames cannot be found, and the analysis is refused.
    """
    records = [b for b in buffers if isinstance(b, dict)]
    if records and sum(b["bytesused"] for b in records) == file_size:
        out, pos = [], 0
        for b in records:
            if not b["error"] and b["bytesused"] == sizeimage:
                out.append(pos)
            pos += b["bytesused"]
        return out
    if file_size % sizeimage != 0:
        raise ValueError(
            "the capture file is not a whole number of full frames and its per-frame sizes"
            " do not add up to it, so its frames cannot be found")
    n = file_size // sizeimage
    if not records:
        return [i * sizeimage for i in range(n)]
    aligned = records[-n:]
    first = n - len(aligned)  # frames before this have no record
    return [
        (first + j) * sizeimage for j, b in enumerate(aligned)
        if not b["error"] and b["bytesused"] == sizeimage
    ]


# --- process control -----------------------------------------------------------------

class Runner:
    """Runs v4l2-ctl (and ffmpeg) children; knows the current child to stop it on a signal."""

    STOP_WAIT = 5.0  # seconds a child gets after SIGTERM before SIGKILL

    def __init__(self):
        self.child = None
        self.streaming = False
        self.spawning = False  # inside Popen: self.child is not set yet
        self.pending = None  # a signal that arrived while spawning

    def defer(self, signum) -> bool:
        """For the signal handler: True when the signal arrived while a child is being
        spawned (before self.child is set); `run` then stops the child and raises."""
        if self.spawning:
            self.pending = signum
            return True
        return False

    def _take_pending(self):
        sig, self.pending = self.pending, None
        return sig

    def run(self, args: list, timeout: float, capture_bytes: bool = False):
        self.pending = None
        self.spawning = True
        try:
            child = subprocess.Popen(
                args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=not capture_bytes)
            self.child = child
        except BaseException as e:
            self.spawning = False
            sig = self._take_pending()
            if sig is not None and not isinstance(e, Interrupted):
                raise Interrupted(sig) from None
            raise
        self.spawning = False
        try:
            sig = self._take_pending()
            if sig is not None:
                raise Interrupted(sig)
            try:
                out, err = child.communicate(timeout=timeout)
                rc = child.returncode
            except subprocess.TimeoutExpired:
                child.kill()
                out, err = child.communicate()
                rc = -9
        finally:
            self.child = None
            self._stop(child)
            for f in (child.stdout, child.stderr):
                if f is not None:
                    try:
                        f.close()
                    except OSError:
                        pass
        return rc, out, err

    @classmethod
    def _stop(cls, child):
        """Terminate, wait STOP_WAIT seconds, then kill and wait."""
        if child.poll() is not None:
            return
        try:
            child.terminate()
        except OSError:
            pass
        try:
            child.wait(timeout=cls.STOP_WAIT)
            return
        except subprocess.TimeoutExpired:
            pass
        try:
            child.kill()
        except OSError:
            pass
        try:
            child.wait(timeout=cls.STOP_WAIT)
        except subprocess.TimeoutExpired:
            pass

    def stop_child(self):
        c = self.child
        if c is not None:
            self._stop(c)


class Camera:
    """One video node: reads and writes controls, captures and analyses a mode."""

    def __init__(self, dev: str, runner: Runner, tmpdir: str = None):
        self.dev = dev
        self.runner = runner
        self.tmpdir = tmpdir
        self._fmt_cache = {}
        self.written = {}  # every control the tool ever tried to write (an ordered set)

    def ctl(self, *args, timeout: float = 20):
        return self.runner.run(["v4l2-ctl", "-d", self.dev, *args], timeout)

    def info(self) -> dict:
        _, out, _ = self.ctl("--info")
        d = {}
        for key, name in (("Driver name", "driver"), ("Card type", "card"), ("Bus info", "bus")):
            m = re.search(re.escape(key) + r"\s*:\s*(.*)", out)
            d[name] = m.group(1).strip() if m else None
        return d

    def modes(self) -> list:
        rc, out, err = self.ctl("--list-formats-ext")
        if rc != 0:
            raise ToolError(f"v4l2-ctl --list-formats-ext failed: {err.strip()}")
        return parse_modes(out)

    def controls(self) -> dict:
        rc, out, err = self.ctl("-L")
        if rc != 0:
            raise ToolError(f"v4l2-ctl -L failed: {err.strip()}")
        return parse_controls(out)

    def set_control(self, name: str, value: int) -> tuple:
        self.written[name] = True
        rc, out, err = self.ctl("-c", f"{name}={value}")
        text = (err or "") + (out or "")
        if rc != 0 or "failed" in text.lower():
            return False, text.strip() or f"rc={rc}"
        return True, None

    def read_value(self, name: str):
        rc, out, _ = self.ctl("-C", name)
        m = re.search(r":\s*(-?\d+)", out or "")
        return int(m.group(1)) if rc == 0 and m else None

    def capture(self, mode: Mode, frames: int, warmup: int, analysis_frames: int, with_info: bool = True) -> dict:
        """Stream `frames` buffers of the mode, return timing and the analysis of the last frames."""
        if self.runner.streaming:
            raise RuntimeError("a stream is already running: never stream two modes at once")
        fmt_args = [f"--set-fmt-video=width={mode.width},height={mode.height},pixelformat={mode.fmt}",
                    f"--set-parm={mode.fps:g}"]
        res = {"fmt": mode.fmt, "width": mode.width, "height": mode.height, "requested_fps": mode.fps}
        key = (mode.fmt, mode.width, mode.height)
        if with_info or key not in self._fmt_cache:
            rc, out, _ = self.ctl(*fmt_args, "--get-fmt-video", timeout=30)
            self._fmt_cache[key] = parse_fmt_info(out or "")
        info = self._fmt_cache[key]
        if with_info:
            res["format"] = info
        w = info.get("width", mode.width)
        h = info.get("height", mode.height)
        stride = info.get("stride", w * (2 if mode.fmt in ("YUYV", "UYVY") else 1))
        sizeimage = info.get("sizeimage", int(w * h * BYTES_PER_PIXEL.get(mode.fmt, 2.0)))
        if mode.fmt not in COMPRESSED and sizeimage > 0:
            frames = min(frames, warmup + max(analysis_frames, MAX_RAW_BYTES // sizeimage))
        tmp = tempfile.mkdtemp(prefix="camera-tune-", dir=self.tmpdir)
        try:  # the temporary directory never outlives the capture, whatever interrupts it
            raw = os.path.join(tmp, "stream.raw")
            self.runner.streaming = True
            try:
                t0 = time.clock_gettime(time.CLOCK_MONOTONIC)
                rc, out, err = self.runner.run(
                    ["v4l2-ctl", "-d", self.dev, *fmt_args, "--stream-mmap=4", f"--stream-count={frames}",
                     f"--stream-to={raw}", "--verbose"],
                    timeout=frames / max(mode.fps, 1.0) * 3 + 25)
            finally:
                self.runner.streaming = False
            buffers = parse_buffers((out or "") + "\n" + (err or ""))
            res["rc"] = rc
            res.update(timing_stats(buffers, mode.fps, t0, warmup))
            if not buffers:
                res["error"] = ((err or out or "").strip()[-300:]) or "no buffers dequeued"
                return res
            if rc != 0:
                res["error"] = (err or "").strip()[-300:]
            try:
                res["analysis"] = self._analyse_file(raw, mode, w, h, stride, sizeimage, buffers, analysis_frames)
            except (OSError, ValueError) as e:
                res["analysis_error"] = str(e)
        finally:
            shutil.rmtree(tmp, ignore_errors=True)
        return res

    def _analyse_file(self, raw, mode, w, h, stride, sizeimage, buffers, k):
        if mode.fmt == "MJPG":
            return self._analyse_mjpeg(raw, w, h, len(buffers), k)
        size = os.path.getsize(raw)
        if sizeimage <= 0:
            raise ValueError("the frame size is unknown")
        offsets = usable_frame_offsets(buffers, size, sizeimage)
        if len(offsets) < 2:
            raise ValueError(f"capture file holds {len(offsets)} usable frame(s) of {sizeimage} bytes")
        offsets = offsets[-k:]
        buf = np.stack([np.fromfile(raw, dtype=np.uint8, count=sizeimage, offset=o) for o in offsets])
        if buf.shape[1] != sizeimage:
            raise ValueError("capture file is shorter than its frame records")
        planes = split_planes(buf, mode.fmt, w, h, stride, sizeimage)
        if planes is None:
            raise ValueError(f"format {mode.fmt} is not decoded by this tool (timing only)")
        return analyze_frames(*planes)

    def _analyse_mjpeg(self, raw, w, h, nframes, k):
        if shutil.which("ffmpeg") is None:
            raise ValueError("ffmpeg is needed to decode MJPG (timing only without it)")
        skip = max(0, nframes - k)
        rc, out, err = self.runner.run(
            ["ffmpeg", "-v", "error", "-f", "mjpeg", "-i", raw, "-vf", f"select=gte(n\\,{skip})",
             "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "yuvj444p", "-"],
            timeout=120, capture_bytes=True)
        a = np.frombuffer(out, dtype=np.uint8)
        got = a.size // (3 * w * h)
        if got < 2:
            raise ValueError("ffmpeg decoded fewer than 2 frames")
        got = min(got, k)
        a = a[a.size - got * 3 * w * h:].reshape(got, 3, h, w)
        return analyze_frames(a[:, 0], a[:, 1], a[:, 2])


# --- restoring controls ---------------------------------------------------------------

def never_written(name: str) -> bool:
    """A control the tool never writes, so never restores: deny-listed or not measurable."""
    return name in NOT_MEASURABLE or bool(set(name.split("_")) & DENY_TOKENS)


_NOT_WRITABLE_FLAGS = {"read-only", "has-payload", "write-only"}


def _master_of(name: str):
    return next((m for m, (_, deps) in MASTERS.items() if name in deps), None)


def restore_controls(cam, originals: dict, log=print) -> list:
    """Write back the controls the tool wrote (`cam.written`) and only those.

    A control the tool never touched (a focus or an exposure that the camera
    drives itself, a gain that drifts under automatic exposure) is left alone:
    writing it back would move the lens or toggle an automatic mode. Never a
    deny-listed or not-measurable control. Order: the automatic masters that
    started on MANUAL, then the other controls, then the masters that started
    on AUTO; whatever still differs is retried. A control that was inactive at
    the start is skipped once its master is back at its starting value (it is
    the camera's to drive again). Returns the names still different.
    """
    written = [n for n in list(getattr(cam, "written", {}))
               if n in originals and not never_written(n)
               and not (_NOT_WRITABLE_FLAGS & set(originals[n].flags))]
    if not written:
        return []

    def skipped(n, current) -> bool:
        if "inactive" not in originals[n].flags:
            return False
        master = _master_of(n)
        return (master is not None and master in originals and master in current
                and current[master].value == originals[master].value)

    manual_masters = [n for n in written if n in MASTERS and originals[n].value == MASTERS[n][0]]
    auto_masters = [n for n in written if n in MASTERS and originals[n].value != MASTERS[n][0]]
    others = [n for n in written if n not in MASTERS]

    for names in (manual_masters, others, auto_masters):
        current = cam.controls()
        for n in names:
            c = current.get(n)
            if c is None or c.value == originals[n].value or skipped(n, current):
                continue
            cam.set_control(n, originals[n].value)

    # Retry whatever still differs: an inactive dependent is written with its
    # master switched to manual for a moment, and the master put back.
    current = cam.controls()
    for n in written:
        c = current.get(n)
        if c is None or c.value == originals[n].value or skipped(n, current):
            continue
        ok, _ = cam.set_control(n, originals[n].value)
        master = _master_of(n)
        if not ok and master in originals and n not in MASTERS:
            cam.set_control(master, MASTERS[master][0])
            cam.set_control(n, originals[n].value)
            cam.set_control(master, originals[master].value)
    current = cam.controls()
    left = [n for n in written
            if n in current and current[n].value != originals[n].value and not skipped(n, current)]
    for n in left:
        log(f"camera-tune: WARNING control {n} is {current[n].value}, was {originals[n].value}")
    return left


# --- process holders, console output -----------------------------------------------------

def node_holders(dev: str) -> list:
    """PIDs holding the node open (fuser, else a /proc scan); [] when none."""
    if shutil.which("fuser"):
        r = subprocess.run(["fuser", dev], capture_output=True, text=True)
        pids = re.findall(r"\d+", r.stdout)
        return [int(p) for p in pids if int(p) != os.getpid()]
    real = os.path.realpath(dev)
    out = []
    for p in Path("/proc").glob("[0-9]*/fd/*"):
        try:
            if os.path.realpath(p) == real and int(p.parts[2]) != os.getpid():
                out.append(int(p.parts[2]))
        except (OSError, ValueError):
            pass
    return sorted(set(out))


def choose_primary(mode_results: list):
    """The mode the sweep runs on: a measured raw mode of at least 640 wide, smallest area;
    else a compressed one; else any measured mode."""
    ok = [m for m in mode_results if m.get("analysis") and not m.get("skipped")]

    def area(m):
        return m["width"] * m["height"]

    for pred in (lambda m: m["fmt"] not in COMPRESSED and m["width"] >= 640,
                 lambda m: m["width"] >= 640, lambda m: True):
        c = sorted((m for m in ok if pred(m)), key=area)
        if c:
            return c[0]
    return None


def fmt_mode_line(m: dict) -> str:
    if m.get("skipped"):
        return f"  {m['fmt']} {m['width']}x{m['height']}: SKIPPED ({m['skipped']})"
    if "fps" not in m:
        return f"  {m['fmt']} {m['width']}x{m['height']}: FAILED ({m.get('error', 'no data')})"
    a = m.get("analysis") or {}
    iv = m.get("interval_ms", {})
    s = (f"  {m['fmt']} {m['width']}x{m['height']}: {m['fps']} fps (nominal {m['nominal_fps']:g}), "
         f"jitter std {iv.get('std')} ms max {iv.get('max')} ms, first frame {m.get('first_frame_s')} s, "
         f"errors {m['error_frames']}, dropped {m.get('dropped', 0)}")
    if a:
        lu = a["luma"]
        n = midtone_noise(a)
        s += (f"\n      Y {lu['min']}-{lu['max']} (<16 {lu['frac_below16'] * 100:.1f} %, "
              f">235 {lu['frac_above235'] * 100:.1f} %), noise mid-tone temporal {n}, "
              f"spatial {a['noise_immerkaer']}, sharpness {a['sharpness']}")
        if a.get("range_suspect"):
            s += "\n      RANGE: luma outside 16-235 under a limited label (full range suspected)"
    elif m.get("analysis_error"):
        s += f"\n      (no frame analysis: {m['analysis_error']})"
    return s


# --- the run ---------------------------------------------------------------------------------

def measure_modes(cam: Camera, modes: list, topo: dict, args, log=print) -> list:
    results = []
    shared = topo.get("shared_hub_with_hid", False)
    for mode in modes:
        entry = {"fmt": mode.fmt, "width": mode.width, "height": mode.height, "requested_fps": mode.fps}
        bw = mode_bandwidth(mode)
        entry["bandwidth_bytes_per_s"] = int(bw)
        if shared and bw >= HUB_BANDWIDTH_LIMIT and not args.allow_shared_hub:
            entry["skipped"] = f"{bw / 1e6:.0f} MB/s on a hub shared with HID devices; use --allow-shared-hub"
            log(f"camera-tune: WARNING skipping {mode.label}: needs {bw / 1e6:.0f} MB/s behind a hub "
                "shared with HID devices (--allow-shared-hub to stream it)")
            results.append(entry)
            continue
        log(f"camera-tune: measuring {mode.label} for {args.seconds:g} s ...")
        n = WARMUP_FRAMES + max(ANALYSIS_FRAMES, int(mode.fps * args.seconds))
        entry.update(cam.capture(mode, n, WARMUP_FRAMES, ANALYSIS_FRAMES))
        entry["label_full"] = bool(entry.get("format", {}).get("label_full"))
        results.append(entry)
        log(fmt_mode_line(entry))
    return results


def sweep_controls(cam: Camera, mode: Mode, originals: dict, args, log=print) -> dict:
    def point():
        time.sleep(args.settle)
        r = cam.capture(mode, SWEEP_WARMUP + SWEEP_FRAMES, SWEEP_WARMUP, SWEEP_FRAMES, with_info=False)
        return metrics_of(r.get("analysis")), r

    log(f"camera-tune: sweeping controls on {mode.label} (settle {args.settle:g} s) ...")
    base_a, ra = point()
    base_b, _ = point()
    if not base_a or not base_b:
        return {"error": f"baseline capture failed: {ra.get('error') or ra.get('analysis_error')}", "controls": []}
    tol = tolerances(base_a, base_b)
    out = {"baseline": base_a, "baseline_repeat": base_b, "tolerance": tol, "controls": []}

    def sweep_one(c: Control, current_value: int):
        pts = []
        for v in sweep_values(c):
            ok, err = cam.set_control(c.name, v)
            m, r = point()
            pts.append({"value": v, "ok": ok, "error": err, "readback": cam.read_value(c.name), "metrics": m})
        ok, err = cam.set_control(c.name, current_value)
        verdict, reason = classify_control(pts, base_a, tol)
        entry = {"name": c.name, "cid": c.cid, "default": c.default, "points": pts, "verdict": verdict,
                 "reason": reason, "restored": ok}
        log(f"  {c.name:28s} {verdict:8s} {reason}")
        out["controls"].append(entry)

    for c in [c for c in originals.values() if is_sweepable(c)]:
        sweep_one(c, c.value)

    # Controls an automatic master keeps inactive: swept with the master on manual.
    for master, (manual, deps) in MASTERS.items():
        mc = originals.get(master)
        if mc is None or not mc.writable or mc.value == manual:
            continue
        if never_written(master):
            continue
        # Only dependents the sweep would write once the master lets them: a lens
        # motor or a not-measurable exposure time never causes a master switch.
        pending = [d for d in deps if d in originals and not originals[d].writable
                   and "inactive" in originals[d].flags and not never_written(d)
                   and originals[d].type in ("int", "bool", "menu")
                   and not (_NOT_WRITABLE_FLAGS & set(originals[d].flags))]
        if not pending:
            continue
        ok, err = cam.set_control(master, manual)
        if not ok:
            log(f"  {master}: switch to manual failed ({err}); dependents not swept")
            continue
        try:
            now = cam.controls()
            for d in pending:
                dc = now.get(d)
                if dc is not None and is_sweepable(dc):
                    sweep_one(dc, dc.value)
        finally:
            cam.set_control(master, mc.value)
    return out


def run(args, log=print) -> int:
    dev = args.device
    if not os.path.exists(dev):
        raise ToolError(f"{dev} does not exist")
    if shutil.which("v4l2-ctl") is None:
        raise ToolError("v4l2-ctl not found (install v4l-utils)")
    holders = node_holders(dev)
    if holders and not args.force:
        raise ToolError(f"{dev} is held by process(es) {', '.join(map(str, holders))}: stop them "
                        "(the daemon, a browser tab) or pass --force")
    recommend = parse_recommend(args.recommend)

    runner = Runner()
    cam = Camera(dev, runner, args.tmpdir)
    identity = cam.info()
    topo = usb_topology(Path(args.sysfs), dev)
    identity.update(topo)
    u = identity.get("usb")
    log(f"camera-tune: {identity.get('card')} (driver {identity.get('driver')})")
    if u:
        log(f"  USB {u['vendor']}:{u['product_id']} {u.get('manufacturer') or ''} {u.get('product') or ''}"
            f", path {identity.get('usb_path')}, {u.get('speed_mbps')} Mb/s")
        for h in topo.get("hid_siblings", []):
            log(f"  HID beside it: {h['path']} {h['vendor']}:{h['product_id']} {h.get('product')}")
        if topo.get("shared_hub_with_hid"):
            log("  WARNING: behind a hub shared with HID devices; high-bandwidth raw modes are skipped")
        elif topo.get("hid_siblings"):
            log("  note: HID devices share the root hub (and controller) with the camera")
    else:
        log("  not a USB camera (no USB id): no rows can be proposed")

    modes = cam.modes()
    chosen = select_modes(modes, args.sizes)
    log(f"camera-tune: {len(modes)} mode(s) listed, measuring {len(chosen)}")
    originals = cam.controls()
    results = {"identity": identity, "modes": [], "originals": {n: dataclasses.asdict(c) for n, c in originals.items()}}

    interrupted = {"flag": False}

    def on_signal(signum, _frame):
        interrupted["flag"] = True
        if runner.defer(signum):  # a child is being spawned: run() stops it and raises
            return
        runner.stop_child()
        raise Interrupted(signum)

    old = {s: signal.signal(s, on_signal) for s in (signal.SIGINT, signal.SIGTERM)}
    left = []
    try:
        try:
            results["modes"] = measure_modes(cam, chosen, topo, args, log)
            primary = choose_primary(results["modes"])
            if primary:
                results["noise"] = midtone_noise(primary["analysis"])
            if not args.quick:
                if primary is None:
                    log("camera-tune: no measured mode to sweep on; sweep skipped")
                else:
                    pm = Mode(primary["fmt"], primary["width"], primary["height"], primary["requested_fps"])
                    results["sweep_mode"] = pm.label
                    results["sweep"] = sweep_controls(cam, pm, originals, args, log)
        finally:
            for s in old:
                signal.signal(s, signal.SIG_IGN)
            log("camera-tune: restoring controls ...")
            runner.stop_child()
            try:
                left = restore_controls(cam, originals, log)
            except Exception as e:  # noqa: BLE001 - report, never mask the primary failure
                log(f"camera-tune: WARNING restore failed: {e}")
                left = ["?"]
            results["restore_left"] = left
            if args.json:
                try:
                    Path(args.json).write_text(json.dumps(results, indent=1, default=str))
                except OSError as e:
                    log(f"camera-tune: could not write {args.json}: {e}")
            for s, h in old.items():
                signal.signal(s, h)
    except Interrupted:
        log("camera-tune: interrupted; controls restored" + (" (some differ, see above)" if left else ""))
        return 130

    log("")
    log("== proposed rows (crates/obc-pipeline/src/quirks.rs) ==")
    log(propose_rows(identity, results, recommend))
    log("")
    log("Check the evidence on a static, evenly lit scene with highlights and shadows;"
        " a row is a proposal, not a verdict.")
    return 1 if left else 0


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="camera-tune.py",
        description="Measure ONE camera and propose rows for crates/obc-pipeline/src/quirks.rs.",
        epilog=HELP_EPILOG, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("device", help="the video node, e.g. /dev/video0")
    p.add_argument("--quick", action="store_true", help="modes only, no control sweep")
    p.add_argument("--json", metavar="OUT", help="write all measurements to OUT as JSON")
    p.add_argument("--seconds", type=float, default=4.0, help="seconds per mode (default 4)")
    p.add_argument("--sizes", type=int, default=2, help="largest sizes per format to measure (default 2)")
    p.add_argument("--settle", type=float, default=1.0, help="seconds to wait after a control write (default 1)")
    p.add_argument("--recommend", action="append", metavar="NAME=VALUE",
                   help="control values for the CameraProfile row (repeatable, or comma separated)")
    p.add_argument("--force", action="store_true", help="measure even if another process holds the node")
    p.add_argument("--allow-shared-hub", action="store_true",
                   help="stream high-bandwidth raw modes behind a hub shared with HID devices")
    p.add_argument("--tmpdir", help="directory for the temporary raw captures (default: system temp)")
    p.add_argument("--sysfs", default="/sys", help=argparse.SUPPRESS)
    return p


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return run(args)
    except ToolError as e:
        print(f"camera-tune: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
