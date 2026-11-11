//! A minimal worker on the server API: halves the level of every block.
//! For trying the mic host by hand; the real worker is
//! `omarchy-broadcast-audiod`.
//!
//! ```text
//! cargo run -p obc-audio --example gain_worker -- /path/to/audio.sock [gain] [depth]
//! ```

use std::path::PathBuf;
use std::time::Duration;

use obc_audio::BLOCK_SAMPLES;
use obc_audio::server::{Handshake, Listener, Wait};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .map(PathBuf::from)
        .or_else(obc_audio::default_socket_path)
        .ok_or("no socket path")?;
    let gain: f32 = args.next().map_or(Ok(0.5), |g| g.parse())?;
    let depth: u32 = args.next().map_or(Ok(1), |d| d.parse())?;
    let _ = std::fs::remove_file(&path);
    let listener = Listener::bind(&path)?;
    eprintln!("gain_worker: listening on {}", path.display());
    loop {
        let conn = listener.accept()?;
        let mut ep =
            match Handshake::read(conn, Duration::from_secs(2)).and_then(|h| h.accept(depth, 0)) {
                Ok(ep) => ep,
                Err(e) => {
                    eprintln!("gain_worker: {e}");
                    continue;
                }
            };
        eprintln!("gain_worker: client connected");
        let mut block = [0.0f32; BLOCK_SAMPLES];
        let mut blocks = 0u64;
        loop {
            if ep.wait(Duration::from_millis(50))? == Wait::HangUp {
                break;
            }
            ep.beat();
            while let Some(meta) = ep.pop_input(&mut block)? {
                for s in &mut block {
                    *s *= gain;
                }
                ep.push_output(meta.seq, 0, &block);
                blocks += 1;
            }
        }
        let c = ep.client_counters();
        eprintln!(
            "gain_worker: client left after {blocks} blocks (sent {}, dropped {}, missed {})",
            c.blocks_sent, c.blocks_dropped, c.blocks_missed
        );
    }
}
