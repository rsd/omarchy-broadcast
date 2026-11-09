# ADR-0015: Cadence emission for a sub-rate input

- **Status:** Accepted (2026-11-09). Amends [ADR-0010](0010-phase-locked-output-clock.md): the output clock follows a steady input between 1.1 and 1.9 periods per frame (16 to 28 fps against a 30 fps contract) by emitting each tick at the predicted arrival of the frame it carries. This replaces ADR-0010's decision not to follow such inputs, and relaxes one of its consequences (emission intervals of one period).

## Context
- A camera that lowers its rate in low light (the reference cameras: 20 fps against a 30 fps contract) delivers frames that fall on two phases of the ticks, half a period apart (33.3 ms ticks, 50 ms frames). Free-running, the wait of a frame in the slot is uniform over a period: median added latency 21.8 ms at 20.06 fps, 95th percentile 36.9 ms (live, FHD Camera, with a consumer on the loopback). The consumer sees alternating 33 and 67 ms content intervals (a 2:1 judder) and, over a minute, the sawtooth of ADR-0010 as the ticks drift against the frames.
- The reference cameras deliver their frames on a 4 ms grid of arrival times. At 20 fps each of the two phases stands still and then steps by 4 ms at once, more than the lock's 1.5 ms margin. ADR-0010's steering law (the smallest predicted wait of the last eight frames moved to the margin) cannot anticipate such a step: in simulation, a 19.98 fps camera on this grid gave 58 relocks over 120 s and 329 of some 2200 frames that waited a period longer than they had to.
- A lock with regular emission intervals (one phase for every tick, steering one of the two frame phases to the margin) cannot lower the median: the other phase then waits half a period more, so the waits are 50/50 bimodal and the median stays at the free-running value (simulation: 16 to 18 ms locked against 16 ms free-running). It lowers the mean and the 95th percentile only.

## Decision
**Cadence mode.** For a steady input between 1.1 and 1.9 periods per frame, the clock stops using one phase for all ticks. It fits the arrival times, predicts when each frame will arrive, and emits the tick that carries a frame at that predicted arrival plus a margin.

- **Timestamps and ordering are unchanged.** Tick `i` carries `D(i)`, every index is emitted once and in order, the caps never change, and no emission is after its PTS or more than one period before it. The window of tick `i` is `[D(i) - P, D(i)]`.
- **Per-tick instant.** The frame predicted to arrive inside the window of tick `i` is the tick's fresh frame; the tick is emitted at the predicted arrival plus the margin, plus the envelope below. The windows tile the time axis, so each predicted frame belongs to exactly one tick.
- **Repeats.** A tick whose window holds no predicted frame is a repeat. It is emitted midway between the previous scheduled instant and the next tick's target, not at its own timestamp.
- **Due ticks** at a wake-up are counted by walking the chained instants (each instant is computed from the previous one), so a clock stall cannot release more than `max_burst` ticks back to back.
- **Prediction.** A least-squares fit of arrival time on sequence number over the last 512 fresh frames. The jitter envelope is the 4th-largest residual of the last 256 frames, added to the margin so that frames arriving later than the fit still make their tick.
- **Engagement.** The input interval over the fit is between 1.1 and 1.9 periods, and the cadence is steady: it engages when the median absolute residual is at most `P/16` and releases above `P/12`. An irregular input (40 to 60 ms frames, say) stays free-running. The two thresholds are a hysteresis; without it, a camera with 4 ms of jitter flipped 156 times in 120 s.
- **State.** The fit history is cleared on a rate change (30 to 20 fps within a run), including while the 64-frame gate is full and cadence is not yet engaged; otherwise the old rate's frames stay in the fit and the switch missed 235 of 400 frames. A cadence-mode run has no phase and no wraps; the clock reports the offset of the last scheduled instant as `phase_offset`.
- **Unchanged.** Inputs at the contract rate, a multiple of it, or half of it (ADR-0010) behave exactly as before: in a differential run of 386 simulated cases (29.5 to 31 and 58 to 60 fps into 30 and 60, 15 fps half-rate with grid and drop models, 30 to 15 transitions, switches with holds, one-second gaps, under exact and late wake-up models) every tick (index, PTS, instant, kind, offset, locked) was identical to the previous clock.

| Parameter | Value | Why |
|---|---|---|
| Band | 1.1 to 1.9 periods per frame | below 1.1 (28 fps and up) the frames are too close to the contract rate to tell from jitter and the mode was unreliable at 1.07; at 1.9 and above the half-rate lock of ADR-0010 applies |
| Engage / release | median residual at most `P/16` / above `P/12` | `P/8` (the half-rate rule) engaged an irregular 40 to 60 ms input; a single threshold flipped 156 times in 120 s at 4 ms of jitter, the pair 30 times over 11 steady cases (927 with the single threshold) |
| Fit window | 512 frames (about 25 s at 20 fps) | 64 frames misses every 4 ms grid step at 19.99 fps (1.25 % of frames): the grid's staircase sweeps over 160 frames, longer than the window, and the slope the fit sees (50.000 ms) is not the true one (50.025 ms) |
| Envelope | 4th-largest residual of the last 256 | the maximum is inflated for seconds by a single 19 ms late frame (95th percentile of the delay 18 ms); the 4th-largest ignores up to three outliers |
| Repeat instant | midway between the previous instant and the next target | at its own timestamp the repeat landed up to 1.5 ms before the fresh frame's emission, and a consumer-side rate limiter would drop the fresh frame |

## Alternatives considered

| Alternative | Why rejected |
|---|---|
| Regular-grid lock steering one of the two frame phases | cannot lower the median: waits are 50/50 bimodal and the p50 equals the free-running p50 by construction (mean and p95 improve, to 10 to 13 ms and 20 to 24 ms) |
| ADR-0010's minimum-wait law with a 3/2 gate | the 4 ms arrival grid steps the phases by more than the margin: 10 acquisitions and 8 % late frames when replaying a live trace; 58 relocks and 329 misses in simulation |
| The same regular grid with a 64-frame fit | misses each 4 ms grid step at 19.99 fps (above); a 512-frame fit fixes it but the median stays unchanged |
| Maximum residual as the envelope | one late outlier inflates it for seconds |
| Repeat at its own timestamp | lands just before the fresh frame; a consumer with a 30 fps rate limiter drops the fresh frame |
| A larger margin | adds to every wait without removing the grid steps; the envelope covers the late side adaptively |

## Consequences
- **Simulation** (300 s at 19.95 to 20.05 fps; uniform jitter of 0.5, 1 and 2 ms and a 4 ms floor grid plus 0.2 to 2 ms; 3 seeds; 189 runs): one acquisition per run and no wraps; 5 misses (wait over half a period) in 1.12 million frames, worst run 0.05 %; worst median slot wait 4.9 ms and 95th percentile 7.6 ms, against about 16 and 31 ms free-running; the lock engages within 3.2 s; every 1 s window holds 29 to 31 frames; no ADR-0010 bound is violated. At 16, 18, 22, 24, 25 and 27 fps (120 s): 1 miss in 321 000 frames, worst median 5.5 ms and 95th percentile 7.9 ms.
- **Live, FHD Camera at 20.07 fps** (forced by a 45 ms manual exposure; 120 s, with a consumer): one acquisition, no wraps, slot wait p50 4.0 ms and p95 7.1 ms; added latency p50 9.2 ms, p95 12.6 ms, p99 14.3 ms, against 23.6, 38.3 and 40.0 ms free-running in an earlier run at 19.99 fps; one frame over the nominal interval in 2405; output PTS exact; 1 s windows hold 29 to 31 frames.
- **Live, MX Brio** (NV12 1080p, fixed frame rate, 60 s each): 20.01 fps, added p50 4.9 ms and p95 6.9 ms (14.3 and 22.2 ms free-running); 24.02 fps (1.25 periods), added p50 5.3 ms and p95 7.6 ms. Regressions: at 15 fps (half-rate lock) added p50 4.4 ms and p95 6.8 ms, none of 872 frames over the interval after the lock; at 60 fps added p50 3.8 ms and p95 6.3 ms, unchanged from before.
- **Consumer view, 20 fps.** A GStreamer `v4l2src` consumer of the loopback (90 s) sees 30.01 buffers per second and 20.06 content changes per second. Content intervals are even: median 49.9 ms, 2 of 1385 outside 40 to 60 ms (a frame that missed its tick). Free-running, the same consumer sees 33 and 67 ms alternating (100 % outside 40 to 60 ms) and an added p50 of 21.8 ms. The price is that buffer intervals are no longer regular: 25, 25 and 50 ms at 20 fps (median 25.0 ms). The consumer showed no stall and no rate limiting.
- **Not verified with real applications.** Only a GStreamer consumer was checked; a browser or OBS may treat buffer intervals of 25 and 50 ms differently from the regular 33.3 ms, for example by rate limiting. A real-application check remains open.
- **Invariants of ADR-0010 that hold** in cadence mode: PTS on the grid, each index once and in order, emission within `[-P, 0]` of the PTS, the count over any span within one frame of the exact rate, the burst cap, the held frame at a switch, and the bounded output queue. Burst probes (9600 stall runs at 19.98 and 29.97 fps) never exceeded the cap of three back-to-back ticks. The only relaxed consequence is the emission interval of one period.
- **Limits.** A 24 or 25 fps input with 2 ms of jitter on the 4 ms grid superseded 1 and 7 frames in two 120 s runs. The lock follows an input only while it is steady; a camera whose rate changes continuously stays free-running.
- **Tests** (`crates/obc-pipeline/src/clock.rs`): see [../testing.md](../testing.md).
