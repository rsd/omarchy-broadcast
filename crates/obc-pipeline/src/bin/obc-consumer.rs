//! A loopback consumer in a process of its own, for the loopback writer's
//! hardware tests (and for looking at what an application receives):
//! `v4l2src device=<device> io-mode=mmap ! capsfilter video/x-raw ! appsink`,
//! reading the device as a video-call application would (raw frames in
//! system memory, whatever format the device offers), and reporting on
//! stdout, one line each:
//!
//! ```text
//! caps <caps>                      every caps negotiation reaching the appsink
//! frame <n> <mean> <min> <max> <stamp>
//!                                  every frame: luma (Y) mean, min and max,
//!                                  sampled every 4th pixel of every 4th row
//!                                  from row 2 (row 0 may carry a stamp), and
//!                                  the stamp of `obc_pipeline::stamp` at the
//!                                  start of the luma plane
//! error <message>                  a pipeline error; the process exits 1
//! eos                              end of stream; the process exits 0
//! ```
//!
//! A frame whose format has no luma plane first (not YUV) is reported with
//! `-` for the three figures, and a frame without a stamp with `-` for it. The process also exits (0) when its standard
//! input closes, so it never outlives a parent that holds the other end.
//!
//! Usage: `obc-consumer <device>`.

use std::io::Read;
use std::process::ExitCode;

use gst::prelude::*;

/// Luma statistics of `buffer` (`info` its layout): mean, min, max over a
/// 4x4 subsample of plane 0. `None` unless the format is YUV with Y in
/// plane 0, one byte per sample.
fn luma(
    info: &gst_video::VideoInfo,
    buffer: &gst::BufferRef,
) -> Option<(f64, u8, u8, Option<u64>)> {
    let finfo = info.format_info();
    if !finfo.is_yuv() || finfo.plane()[0] != 0 || finfo.poffset()[0] != 0 {
        return None;
    }
    let pstride = usize::try_from(finfo.pixel_stride()[0]).ok()?;
    if pstride == 0 || finfo.depth()[0] != 8 {
        return None;
    }
    let map = buffer.map_readable().ok()?;
    let (offset, stride) = (info.offset()[0], usize::try_from(info.stride()[0]).ok()?);
    let (mut sum, mut n, mut min, mut max) = (0u64, 0u64, u8::MAX, u8::MIN);
    for row in (2..info.height() as usize).step_by(4) {
        for col in (0..info.width() as usize).step_by(4) {
            let v = *map.get(offset + row * stride + col * pstride)?;
            sum += u64::from(v);
            n += 1;
            min = min.min(v);
            max = max.max(v);
        }
    }
    let stamp = (pstride == 1)
        .then(|| map.get(offset..).and_then(obc_pipeline::stamp::read_stamp))
        .flatten();
    (n > 0).then(|| (sum as f64 / n as f64, min, max, stamp))
}

fn main() -> ExitCode {
    let Some(device) = std::env::args().nth(1) else {
        eprintln!("usage: obc-consumer <device>");
        return ExitCode::from(2);
    };
    if let Err(e) = gst::init() {
        println!("error gst::init: {e}");
        return ExitCode::from(1);
    }
    // The parent closing its end of stdin ends this process.
    std::thread::spawn(|| {
        let mut sink = [0u8; 64];
        let mut stdin = std::io::stdin();
        while matches!(stdin.read(&mut sink), Ok(n) if n > 0) {}
        std::process::exit(0);
    });

    let elements = gst::ElementFactory::make("v4l2src")
        .property("device", &device)
        .property_from_str("io-mode", "mmap")
        .build()
        .and_then(|src| {
            // System memory: no DMABuf caps an application would not take.
            let raw = gst::ElementFactory::make("capsfilter")
                .property("caps", gst::Caps::builder("video/x-raw").build())
                .build()?;
            Ok((src, raw))
        });
    let (src, raw) = match elements {
        Ok(e) => e,
        Err(e) => {
            println!("error v4l2src: {e}");
            return ExitCode::from(1);
        }
    };
    let appsink = gst_app::AppSink::builder()
        .sync(false)
        .enable_last_sample(false)
        .build();
    appsink
        .static_pad("sink")
        .expect("an appsink has a sink pad")
        .add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, |_, info| {
            if let Some(gst::PadProbeData::Event(e)) = &info.data
                && let gst::EventView::Caps(c) = e.view()
            {
                println!("caps {}", c.caps());
            }
            gst::PadProbeReturn::Ok
        });
    let mut index = 0u64;
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Flushing)?;
                let info = sample
                    .caps()
                    .and_then(|c| gst_video::VideoInfo::from_caps(c).ok());
                let stats = match (info, sample.buffer()) {
                    (Some(info), Some(buffer)) => luma(&info, buffer),
                    _ => None,
                };
                match stats {
                    Some((mean, min, max, stamp)) => println!(
                        "frame {index} {mean:.2} {min} {max} {}",
                        stamp.map_or_else(|| "-".to_string(), |s| s.to_string())
                    ),
                    None => println!("frame {index} - - - -"),
                }
                index += 1;
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    let pipeline = gst::Pipeline::with_name("obc-consumer");
    if let Err(e) = pipeline
        .add_many([&src, &raw, appsink.upcast_ref()])
        .and_then(|()| gst::Element::link_many([&src, &raw, appsink.upcast_ref()]))
    {
        println!("error linking: {e}");
        return ExitCode::from(1);
    }
    let bus = pipeline.bus().expect("a pipeline has a bus");
    let code = match pipeline.set_state(gst::State::Playing) {
        Err(e) => {
            // The details are on the bus.
            let detail =
                bus.pop_filtered(&[gst::MessageType::Error])
                    .and_then(|m| match m.view() {
                        gst::MessageView::Error(e) => Some(e.error().to_string()),
                        _ => None,
                    });
            println!("error {e}: {}", detail.unwrap_or_default());
            ExitCode::from(1)
        }
        Ok(_) => loop {
            let Some(msg) = bus.timed_pop_filtered(
                gst::ClockTime::NONE,
                &[gst::MessageType::Error, gst::MessageType::Eos],
            ) else {
                continue;
            };
            match msg.view() {
                gst::MessageView::Error(e) => {
                    println!("error {} ({:?})", e.error(), e.debug());
                    break ExitCode::from(1);
                }
                _ => {
                    println!("eos");
                    break ExitCode::SUCCESS;
                }
            }
        },
    };
    let _ = pipeline.set_state(gst::State::Null);
    code
}
