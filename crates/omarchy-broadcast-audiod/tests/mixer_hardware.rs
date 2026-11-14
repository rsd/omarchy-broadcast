//! The hardware capture gain on a real USB microphone: the ALSA mixer of
//! the card named by `OBC_TEST_MIC_CARD` (an ALSA card index or id, such as
//! `Brio`), through the worker's own [`Alsa`] mixer.
//!
//! Run with
//! `OBC_TEST_MIC_CARD=Brio cargo test -p omarchy-broadcast-audiod --test mixer_hardware -- --ignored --nocapture`.
//!
//! It changes the card's capture gain (the minimum, the maximum, a value
//! between, one off a step), checks that a second handle gets a control
//! event for each change, and puts the raw value of every channel it found
//! back at the end, also when it fails (a guard). PipeWire shows the same control as the
//! source's volume: that volume moves while the test runs, and comes back
//! with the gain. No audio is recorded.

use std::time::{Duration, Instant};

use omarchy_broadcast_audiod::mixer::{Alsa, Mixer, choose_control, quantise};

const IGNORE: &str = "needs OBC_TEST_MIC_CARD (the ALSA card index or id of a USB microphone \
     with one capture control, e.g. Brio); it changes that card's capture gain and restores it";

fn required(var: &str, what: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| panic!("this test needs {var}: {what}. {IGNORE}"))
}

/// Every capture channel of `control` with its raw value.
type Channels = Vec<(alsa::mixer::SelemChannelId, i64)>;

/// The capture element `control` of card `card`, opened anew.
fn with_selem<T>(
    card: &str,
    control: &str,
    f: impl FnOnce(&alsa::mixer::Selem<'_>) -> Result<T, String>,
) -> Result<T, String> {
    let m = alsa::mixer::Mixer::new(&format!("hw:{card}"), false).map_err(|e| e.to_string())?;
    let s = m
        .find_selem(&alsa::mixer::SelemId::new(control, 0))
        .ok_or_else(|| format!("no control {control:?}"))?;
    f(&s)
}

/// The raw value of every capture channel of `control`.
fn raw_channels(card: &str, control: &str) -> Result<Channels, String> {
    with_selem(card, control, |s| {
        Ok(alsa::mixer::SelemChannelId::all()
            .iter()
            .filter(|c| s.has_capture_channel(**c))
            .filter_map(|c| s.get_capture_volume(*c).ok().map(|v| (*c, v)))
            .collect())
    })
}

/// Puts the raw value of every channel found at the start back, also on
/// a panic.
struct Restore {
    card: String,
    control: String,
    channels: Channels,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let res = with_selem(&self.card, &self.control, |s| {
            for (c, v) in &self.channels {
                s.set_capture_volume(*c, *v)
                    .map_err(|e| format!("{c:?}: {e}"))?;
            }
            Ok(())
        });
        match res {
            Ok(()) => eprintln!(
                "restored {} of card {} to {:?}",
                self.control, self.card, self.channels
            ),
            Err(e) => eprintln!(
                "COULD NOT RESTORE {} of card {} to {:?}: {e}",
                self.control, self.card, self.channels
            ),
        }
    }
}

/// Waits (at most two seconds) for a control event on `watch`; whether
/// one came.
fn event_on(watch: &mut Alsa) -> bool {
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        let mut fds = watch.poll_fds();
        assert!(!fds.is_empty(), "no control event descriptors");
        let left = end.saturating_duration_since(Instant::now());
        let ready = obc_audio::sys::poll(&mut fds, Some(left)).unwrap();
        if ready > 0 && watch.events(&fds) {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
    }
}

#[test]
#[ignore = "needs OBC_TEST_MIC_CARD (the ALSA card index or id of a USB microphone with one capture control, e.g. Brio); it changes that card's capture gain and restores it"]
fn the_capture_gain_is_set_read_back_and_restored() {
    let card = required(
        "OBC_TEST_MIC_CARD",
        "the ALSA card index or id of a USB microphone",
    );
    let mut m = Alsa::default();
    let controls = m
        .capture_controls(&card)
        .unwrap_or_else(|e| panic!("OBC_TEST_MIC_CARD={card}: {e}"));
    let control = choose_control(&controls)
        .unwrap_or_else(|| {
            panic!("OBC_TEST_MIC_CARD={card}: no single capture control among {controls:?}")
        })
        .to_string();
    let original = m.read(&card, &control).unwrap();
    eprintln!("card {card}, control {control}: {original:?}");
    assert!(original.min_db < original.max_db, "{original:?}");
    assert!(original.step_db > 0.0, "{original:?}");
    let channels = raw_channels(&card, &control).unwrap();
    assert!(!channels.is_empty(), "no capture channel");
    eprintln!("raw values: {channels:?}");
    let restore = Restore {
        card: card.clone(),
        control: control.clone(),
        channels: channels.clone(),
    };

    // A second handle, watching the card's events as the gain thread does.
    let mut watch = Alsa::default();
    watch.read(&card, &control).unwrap();

    let mid = quantise((original.min_db + original.max_db) / 2.0, &original).unwrap();
    let off_step = mid + original.step_db * 0.4;
    let mut before = original.db;
    for (asked, want) in [
        (original.min_db, original.min_db),
        (original.max_db, original.max_db),
        (mid, mid),
        (off_step, quantise(off_step, &original).unwrap()),
        (original.max_db + 30.0, original.max_db),
    ] {
        let db = quantise(asked, &original).unwrap();
        m.set_db(&card, &control, db).unwrap();
        let back = m.read(&card, &control).unwrap();
        eprintln!("asked {asked} dB: set {db} dB, read back {} dB", back.db);
        assert!(
            (back.db - want).abs() < 0.01,
            "asked {asked} dB, want {want} dB, read {back:?}"
        );
        assert_eq!(
            (back.min_db, back.max_db, back.step_db),
            (original.min_db, original.max_db, original.step_db)
        );
        // The other handle sees a change as a control event, and reads it.
        if (want - before).abs() >= 0.01 {
            assert!(event_on(&mut watch), "no control event for {asked} dB");
        }
        before = want;
        let seen = watch.read(&card, &control).unwrap();
        assert!((seen.db - want).abs() < 0.01, "the watcher read {seen:?}");
    }
    drop(restore);
    let after = raw_channels(&card, &control).unwrap();
    eprintln!("after the restore: {after:?}");
    assert_eq!(after, channels);
    let read = Alsa::default().read(&card, &control).unwrap();
    assert!((read.db - original.db).abs() < 0.01, "{read:?}");
}
