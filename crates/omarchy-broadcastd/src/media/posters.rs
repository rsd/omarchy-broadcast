//! The posters a listing found missing, made off the listing (:
//! listing the gallery never decodes).
//!
//! [`PosterMaker::request`] queues videos and returns at once; one worker
//! thread, started when there is work and ending when there is none, makes
//! their posters one at a time ([`super::video_poster`]: the check, the
//! hash, a bundled video's shipped poster or ffmpeg). A video already
//! queued, or being made, is not queued again, so listings made while the
//! worker runs add nothing. Each poster made calls the maker's `made`
//! callback (the D-Bus object raises `MediaSerial`, so clients list again
//! and find it). A video whose poster could not be made is not tried again
//! for the same file content, unless ffmpeg was missing then and is found
//! now.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use super::MediaStore;
use super::ffmpeg::Ffmpeg;
use super::hash::FileKey;
use crate::{debug, warning};

/// The callback of a poster made.
type Made = dyn Fn(&Path) + Send + Sync;

/// The poster worker of a store (see the module documentation).
pub struct PosterMaker {
    inner: Arc<Inner>,
}

struct Inner {
    store: MediaStore,
    made: Box<Made>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    queue: VecDeque<PathBuf>,
    /// What is queued or being made.
    pending: HashSet<PathBuf>,
    /// Whether the worker thread runs.
    running: bool,
    /// The videos whose poster could not be made: the file content it was
    /// tried for, and whether ffmpeg was found then.
    failed: HashMap<PathBuf, (FileKey, bool)>,
}

impl PosterMaker {
    /// The maker of `store`'s posters; `made` is called with the video's
    /// path on the worker thread after each poster made.
    pub fn new(store: MediaStore, made: impl Fn(&Path) + Send + Sync + 'static) -> PosterMaker {
        PosterMaker {
            inner: Arc::new(Inner {
                store,
                made: Box::new(made),
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Queues the posters of `videos` (paths the listing gave), skipping
    /// what is queued or being made and what failed for the same content
    /// (see the module documentation); starts the worker when it is not
    /// running. Returns at once (a `stat` per video).
    pub fn request(&self, videos: impl IntoIterator<Item = PathBuf>) {
        if self.inner.store.cache().is_none() {
            return;
        }
        let mut st = self
            .inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut ffmpeg_now: Option<bool> = None;
        for path in videos {
            if st.pending.contains(&path) {
                continue;
            }
            if let Some((key, had_ffmpeg)) = st.failed.get(&path).copied() {
                let same = std::fs::metadata(&path).is_ok_and(|m| FileKey::of(&m) == key);
                let found = *ffmpeg_now.get_or_insert_with(|| Ffmpeg::locate().is_ok());
                if same && (had_ffmpeg || !found) {
                    continue;
                }
                st.failed.remove(&path);
            }
            st.pending.insert(path.clone());
            st.queue.push_back(path);
        }
        if st.queue.is_empty() || st.running {
            return;
        }
        st.running = true;
        drop(st);
        let inner = Arc::clone(&self.inner);
        let started = std::thread::Builder::new()
            .name("obc-posters".into())
            .spawn(move || inner.work());
        if let Err(e) = started {
            warning!("media: the poster thread did not start: {e}");
            let mut st = self
                .inner
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            st.running = false;
            st.queue.clear();
            st.pending.clear();
        }
    }

    /// Whether posters are queued or being made (for tests).
    pub fn busy(&self) -> bool {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .running
    }
}

impl Inner {
    fn work(&self) {
        loop {
            let next = {
                let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                match st.queue.pop_front() {
                    Some(p) => p,
                    None => {
                        st.running = false;
                        return;
                    }
                }
            };
            let had_ffmpeg = Ffmpeg::locate().is_ok();
            let key = std::fs::metadata(&next).ok().map(|m| FileKey::of(&m));
            let made = match super::video_poster(Some(&self.store), &next) {
                Ok((_, Some(_))) => true,
                Ok((_, None)) => false,
                Err(e) => {
                    debug!("media: no poster: {e}");
                    false
                }
            };
            {
                let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                st.pending.remove(&next);
                if !made && let Some(key) = key {
                    st.failed.insert(next.clone(), (key, had_ffmpeg));
                }
            }
            if made {
                (self.made)(&next);
            }
        }
    }
}
