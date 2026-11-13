//! The one place the daemon runs ffmpeg: the transcode of an animated
//! image at import and the posters of videos; and the command a user runs
//! to convert a video for playback without the NVIDIA GPU
//! ([`cpu_convert_command`]), which the daemon never runs itself.
//!
//! Every input is untrusted, so every run is the same hardened shape
//! ([`Ffmpeg::argv`]):
//!
//! * an argument list, never a shell; `-nostdin`, `-hide_banner`,
//!   `-loglevel error`;
//! * `-protocol_whitelist file` and the demuxer forced with `-f` to the
//!   format the daemon's own check found, so a crafted file cannot make
//!   ffmpeg probe another format, read another path or open the network;
//! * the input is the file the daemon opened and checked, handed to the
//!   child as its descriptor 3 and named `file:/proc/self/fd/3`, so a path
//!   swapped after the check cannot redirect it;
//! * the output is a path the daemon chose, never overwritten (`-n`);
//! * the child runs at `nice 19` (`setpriority` in the child before
//!   `exec`) with `-threads` set to half the logical CPUs (at least one),
//!   because a job may run during a call on the CPU doing the cut-out; it
//!   is killed when its parent thread exits (`PR_SET_PDEATHSIG`), when its
//!   wall time runs out, and when the caller cancels.
//!
//! ffmpeg is looked up on `PATH` ([`Ffmpeg::locate`]): once found it is
//! kept for the process; not found, it is looked up again at the next ask
//! (an install while the daemon runs is then seen). Its absence is
//! [`FfmpegError::Missing`].

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// The longest side box of a CPU-ready video: 1920x1080.
pub const CPU_MAX_WIDTH: u32 = 1920;
/// See [`CPU_MAX_WIDTH`].
pub const CPU_MAX_HEIGHT: u32 = 1080;
/// The highest frame rate of a CPU-ready video.
pub const CPU_MAX_FPS: u32 = 30;

/// The wall time of a poster.
pub const POSTER_TIMEOUT: Duration = Duration::from_secs(30);

/// The wall time of the transcode of an animated image at import.
pub const ANIMATED_TIMEOUT: Duration = Duration::from_secs(120);

/// The poster's bounding box (never upscaled).
const POSTER_MAX: (u32, u32) = (1280, 720);

/// The descriptor the input has in the child.
const CHILD_INPUT_FD: i32 = 3;

/// The most of ffmpeg's error output kept for a reason.
const STDERR_KEEP: usize = 4096;

/// How often a running job is checked for its end, a cancel or its time.
const POLL: Duration = Duration::from_millis(20);

/// Why a run of ffmpeg did not produce its output.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FfmpegError {
    /// ffmpeg is not on `PATH`.
    #[error("ffmpeg is not installed")]
    Missing,
    /// It could not be started.
    #[error("ffmpeg could not be started: {0}")]
    Spawn(String),
    /// It ended with an error; the reason is its last error line.
    #[error("ffmpeg failed: {0}")]
    Failed(String),
    /// It ran out of wall time and was killed.
    #[error("ffmpeg took longer than {} s and was stopped", .0.as_secs())]
    TimedOut(Duration),
    /// The caller cancelled it.
    #[error("cancelled")]
    Cancelled,
}

/// A located ffmpeg binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ffmpeg {
    path: PathBuf,
}

/// One run: the checked input, its forced demuxer, the arguments after the
/// input, the output path and the wall time.
#[derive(Debug)]
pub struct FfmpegJob<'a> {
    /// The input, opened and checked by the daemon.
    pub input: &'a File,
    /// The demuxer ffmpeg must use (`mov`, `matroska`, `gif`, `apng`,
    /// `webp_anim`).
    pub input_format: &'a str,
    /// The arguments between the input and the output (filters, codec,
    /// the output format).
    pub output_args: Vec<OsString>,
    /// Where the output goes; it must not exist.
    pub output: &'a Path,
    /// The wall time.
    pub timeout: Duration,
}

impl Ffmpeg {
    /// The ffmpeg of this process: the first executable `ffmpeg` on
    /// `PATH`. Found once, it is kept; not found, the next call looks
    /// again (a few `stat`s).
    pub fn locate() -> Result<Ffmpeg, FfmpegError> {
        static FOUND: Mutex<Option<Ffmpeg>> = Mutex::new(None);
        let mut found = FOUND
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if found.is_none() {
            *found = Self::locate_in(std::env::var_os("PATH").as_deref());
        }
        found.clone().ok_or(FfmpegError::Missing)
    }

    /// The first executable `ffmpeg` in the directories of `path_var` (a
    /// `PATH` value; relative entries are skipped).
    pub fn locate_in(path_var: Option<&OsStr>) -> Option<Ffmpeg> {
        let dirs = std::env::split_paths(path_var?);
        for dir in dirs.filter(|d| d.is_absolute()) {
            let candidate = dir.join("ffmpeg");
            if is_executable(&candidate) {
                return Some(Ffmpeg { path: candidate });
            }
        }
        None
    }

    /// An ffmpeg at `path` (tests use a fake one).
    pub fn at(path: impl Into<PathBuf>) -> Ffmpeg {
        Ffmpeg { path: path.into() }
    }

    /// The binary's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The arguments of `job` (without the program name).
    pub fn argv(&self, job: &FfmpegJob<'_>) -> Vec<OsString> {
        let threads = job_threads().to_string();
        let mut v: Vec<OsString> = [
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-protocol_whitelist",
            "file",
            "-threads",
            &threads,
            "-f",
            job.input_format,
            "-i",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        v.push(format!("file:/proc/self/fd/{CHILD_INPUT_FD}").into());
        v.extend(job.output_args.iter().cloned());
        v.push("-threads".into());
        v.push(threads.into());
        v.push("-n".into());
        let mut out = OsString::from("file:");
        out.push(job.output);
        v.push(out);
        v
    }

    /// Runs `job` to its end. `cancel` set kills it ([`FfmpegError::Cancelled`]);
    /// so does its wall time ([`FfmpegError::TimedOut`]). A partial output
    /// is the caller's to remove.
    pub fn run(&self, job: &FfmpegJob<'_>, cancel: &AtomicBool) -> Result<(), FfmpegError> {
        let input_fd = job.input.as_raw_fd();
        let mut cmd = Command::new(&self.path);
        cmd.args(self.argv(job))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // SAFETY: the closure runs in the child between fork and exec and
        // calls only async-signal-safe functions (setpriority, prctl,
        // dup2, fcntl), touching no memory of the parent.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setpriority(libc::PRIO_PROCESS, 0, 19) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if input_fd == CHILD_INPUT_FD {
                    // Already in place: clear its close-on-exec flag.
                    if libc::fcntl(input_fd, libc::F_SETFD, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::dup2(input_fd, CHILD_INPUT_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // A binary just written may still be open for writing in a child
        // forked meanwhile (ETXTBSY until that child execs): try again.
        let mut tries = 0;
        let mut child = loop {
            match cmd.spawn() {
                Ok(c) => break c,
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 50 => {
                    tries += 1;
                    std::thread::sleep(POLL);
                }
                Err(e) => return Err(FfmpegError::Spawn(e.to_string())),
            }
        };
        let stderr = child.stderr.take().map(|mut err| {
            std::thread::spawn(move || {
                let mut kept: Vec<u8> = Vec::new();
                let mut buf = [0u8; 1024];
                while let Ok(n) = err.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    kept.extend_from_slice(&buf[..n]);
                    if kept.len() > STDERR_KEEP {
                        kept.drain(..kept.len() - STDERR_KEEP);
                    }
                }
                kept
            })
        });
        let deadline = Instant::now() + job.timeout;
        let ended = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(e) => break Err(FfmpegError::Failed(e.to_string())),
            }
            let stop = if cancel.load(Ordering::Relaxed) {
                Some(FfmpegError::Cancelled)
            } else if Instant::now() >= deadline {
                Some(FfmpegError::TimedOut(job.timeout))
            } else {
                None
            };
            if let Some(why) = stop {
                let _ = child.kill();
                let _ = child.wait();
                break Err(why);
            }
            std::thread::sleep(POLL);
        };
        let err_text = stderr
            .and_then(|h| h.join().ok())
            .map(|b| last_line(&b))
            .unwrap_or_default();
        let status = ended?;
        if status.success() {
            Ok(())
        } else {
            let code = status
                .code()
                .map_or_else(|| "killed by a signal".to_string(), |c| format!("exit {c}"));
            Err(FfmpegError::Failed(if err_text.is_empty() {
                code
            } else {
                format!("{err_text} ({code})")
            }))
        }
    }
}

/// The threads a job uses: half the logical CPUs, at least one.
pub fn job_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(1)
        .max(1)
}

/// The arguments of the CPU-ready VP9 WebM, after the input: the first
/// video stream only, no audio, subtitles, data or metadata; fitted inside
/// 1920x1080 without upscaling, even sizes; at most 30 fps; `yuv420p`, VP9
/// profile 0, realtime preset, constrained quality (CRF 32 under an
/// 8 Mbit/s ceiling); a keyframe at least every 2 s. The animated import
/// runs them, and [`cpu_convert_command`] hands them to the user.
pub fn vp9_args() -> Vec<OsString> {
    let fit = |side: &str| {
        format!("max(2\\,trunc(min(1\\,min({CPU_MAX_WIDTH}/iw\\,{CPU_MAX_HEIGHT}/ih))*{side}/2)*2)")
    };
    let vf = format!("scale=w={}:h={},setsar=1", fit("iw"), fit("ih"));
    [
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-map_metadata",
        "-1",
        "-vf",
        &vf,
        "-fpsmax",
        &CPU_MAX_FPS.to_string(),
        "-pix_fmt",
        "yuv420p",
        "-c:v",
        "libvpx-vp9",
        "-profile:v",
        "0",
        "-deadline",
        "realtime",
        "-cpu-used",
        "8",
        "-row-mt",
        "1",
        "-crf",
        "32",
        "-b:v",
        "8M",
        "-g",
        "60",
        "-force_key_frames",
        "expr:gte(t,n_forced*2)",
        "-f",
        "webm",
    ]
    .iter()
    .map(OsString::from)
    .collect()
}

/// The arguments of a poster, after the input: the first video frame as a
/// JPEG fitted inside 1280x720 without upscaling.
pub fn poster_args() -> Vec<OsString> {
    let (w, h) = POSTER_MAX;
    let vf = format!("scale=w=min({w}\\,iw):h=min({h}\\,ih):force_original_aspect_ratio=decrease");
    [
        "-map",
        "0:v:0",
        "-an",
        "-sn",
        "-dn",
        "-map_metadata",
        "-1",
        "-frames:v",
        "1",
        "-vf",
        &vf,
        "-q:v",
        "3",
        "-c:v",
        "mjpeg",
        "-f",
        "image2",
    ]
    .iter()
    .map(OsString::from)
    .collect()
}

/// The shell command that converts the video `input` into a CPU-ready VP9
/// WebM at `output`, for the user to run: `ffmpeg -nostdin -n
/// -protocol_whitelist file -i <input>` (the input may be an untrusted
/// file: nothing read from the terminal, nothing overwritten, only local
/// files followed) then the very arguments of [`vp9_args`] and
/// `<output>`, each word that needs it
/// single-quoted (POSIX: a `'` is written `'\''`; the paths always), so it
/// can be pasted into a shell as it is. `None` when a path is not UTF-8
/// (the command would not name it).
pub fn cpu_convert_command(input: &Path, output: &Path) -> Option<String> {
    let mut words: Vec<String> = [
        "ffmpeg",
        "-nostdin",
        "-n",
        "-protocol_whitelist",
        "file",
        "-i",
    ]
    .map(String::from)
    .into();
    words.push(quote(input.to_str()?));
    for arg in vp9_args() {
        words.push(quote_if_needed(arg.to_str()?));
    }
    words.push(quote(output.to_str()?));
    Some(words.join(" "))
}

/// `word` in single quotes, a `'` in it as `'\''`: one shell word, read
/// back unchanged (POSIX).
pub fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// `word` as it is when a shell reads it as one word unchanged, else
/// [`quote`]d.
fn quote_if_needed(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:=,+@%".contains(&b));
    if plain { word.to_string() } else { quote(word) }
}

/// The last non-empty line of `bytes`, control characters removed, at
/// most 300 characters.
fn last_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or_default();
    line.chars().filter(|c| !c.is_control()).take(300).collect()
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
