//! The handshake and the rings across a real socket, both halves of the API.

use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;

use obc_audio::client::{self, ConnectError};
use obc_audio::proto::{Hello, MAX_MESSAGE_LEN, RefuseReason, Reply};
use obc_audio::server::{Handshake, HandshakeError, Listener, Wait};
use obc_audio::shm::{self, SLOT_FLAG_NEW_STREAM, SLOT_FLAG_RAW};
use obc_audio::sys::{self, SeqPacket};
use obc_audio::{BLOCK_SAMPLES, PROTOCOL_VERSION};

const T: Duration = Duration::from_secs(2);

fn socket_path() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "obc-audio-hs-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("audio.sock")
}

#[test]
fn connect_accept_and_match_sequence_numbers() {
    let path = socket_path();
    let listener = Listener::bind(&path).unwrap();
    let worker = thread::spawn(move || {
        let hs = Handshake::read(listener.accept().unwrap(), T).unwrap();
        assert_eq!(hs.hello().ring_capacity, 8);
        let mut ep = hs.accept(2, 96).unwrap();
        let mut block = [0.0f32; BLOCK_SAMPLES];
        let mut answered = 0;
        while answered < 20 {
            match ep.wait(Duration::from_millis(500)).unwrap() {
                Wait::HangUp => panic!("client left early"),
                Wait::Input | Wait::Timeout => {}
            }
            ep.beat();
            while let Some(meta) = ep.pop_input(&mut block).unwrap() {
                assert_eq!(meta.n_samples, BLOCK_SAMPLES as u32);
                assert_eq!(meta.flags & SLOT_FLAG_NEW_STREAM != 0, meta.seq == 100);
                assert_eq!(block[0], meta.seq as f32, "the block carries its own seq");
                for s in &mut block {
                    *s *= 0.5;
                }
                let flags = if meta.seq % 5 == 0 { SLOT_FLAG_RAW } else { 0 };
                assert!(ep.push_output(meta.seq, flags, &block));
                answered += 1;
            }
        }
        // Wait for the client to hang up.
        while ep.wait(Duration::from_millis(500)).unwrap() != Wait::HangUp {}
        (ep.client_counters(), ep.transport_delay())
    });

    let (mut rings, control) = client::connect(&path, 8, T).unwrap();
    assert_eq!(control.accept.pipeline_depth, 2);
    assert_eq!(control.accept.latency_samples, 96);
    assert_eq!(rings.latency_samples(), 96);
    assert_eq!(rings.pipeline_depth(), 2);

    let mut out = [0.0f32; BLOCK_SAMPLES];
    let mut next_out = 100u64;
    let mut next_in = 100u64;
    let hb0 = rings.heartbeat();
    // Too large to be a transport delay: ignored by the worker.
    rings.publish_transport_delay(obc_audio::MAX_TRANSPORT_DELAY + 1);
    while next_out < 120 {
        // Keep at most 4 blocks in flight; the ring holds 8.
        while next_in < 120 && next_in - next_out < 4 {
            let flags = if next_in == 100 {
                SLOT_FLAG_NEW_STREAM
            } else {
                0
            };
            assert_eq!(
                rings.push_input(next_in, flags, &[next_in as f32; BLOCK_SAMPLES]),
                Ok(true)
            );
            next_in += 1;
        }
        rings.notify();
        while let Some(meta) = rings.pop_output(&mut out).unwrap() {
            assert_eq!(meta.seq, next_out, "output in input order, same seq");
            assert_eq!(meta.flags & SLOT_FLAG_RAW != 0, meta.seq % 5 == 0);
            assert_eq!(out[7], next_out as f32 * 0.5);
            next_out += 1;
        }
        thread::sleep(Duration::from_millis(1));
    }
    assert!(rings.heartbeat() > hb0);
    rings.publish(&shm::ClientCounters {
        play_seq: 119,
        blocks_sent: 20,
        blocks_dropped: 0,
        blocks_missed: 1,
    });
    rings.publish_transport_delay(obc_audio::transport_delay(1024, 2));
    drop(control);
    let (counters, delay) = worker.join().unwrap();
    assert_eq!(delay, Some(1472), "Q = 1024 at depth 2");
    assert_eq!(counters.blocks_sent, 20);
    assert_eq!(counters.blocks_missed, 1);
}

/// Sends `bytes` with `fds` as a hello and returns the worker's reply.
fn raw_hello(
    bytes: &[u8],
    fds: &[std::os::fd::BorrowedFd<'_>],
) -> (Result<Handshake, HandshakeError>, Reply) {
    let (client, server) = SeqPacket::pair().unwrap();
    client.send(bytes, fds).unwrap();
    let hs = Handshake::read(server, T);
    let mut buf = [0u8; MAX_MESSAGE_LEN];
    client
        .set_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let reply = match &hs {
        Ok(_) => Reply::encode_accept(1, 0),
        Err(_) => {
            let (n, _) = client.recv(&mut buf).unwrap();
            buf[..n].to_vec()
        }
    };
    (hs, Reply::decode(&reply).unwrap())
}

fn refused_with(reply: &Reply) -> RefuseReason {
    match reply {
        Reply::Refuse(r) => r.reason,
        Reply::Accept(_) => panic!("accepted"),
    }
}

#[test]
fn malformed_hello_is_refused() {
    let region = shm::Region::create(8).unwrap();
    let (_wake, efd) = sys::wake_pair().unwrap();
    let fds = [region.fd(), efd.as_fd()];

    let (hs, reply) = raw_hello(b"hello there, worker", &fds);
    assert!(matches!(
        hs,
        Err(HandshakeError::Refused(RefuseReason::Malformed, _))
    ));
    assert_eq!(refused_with(&reply), RefuseReason::Malformed);

    // A good hello without its descriptors.
    let (hs, reply) = raw_hello(&Hello::new(8).encode(), &[]);
    assert!(hs.is_err());
    assert_eq!(refused_with(&reply), RefuseReason::Malformed);

    // An unsupported format.
    let mut h = Hello::new(8);
    h.sample_rate = 44_100;
    let (_, reply) = raw_hello(&h.encode(), &fds);
    assert_eq!(refused_with(&reply), RefuseReason::Format);

    // An eventfd where the wake socket belongs (the version-1 descriptor).
    let efd = sys::eventfd_new().unwrap();
    let (hs, reply) = raw_hello(&Hello::new(8).encode(), &[region.fd(), efd.as_fd()]);
    assert!(matches!(
        hs,
        Err(HandshakeError::Refused(RefuseReason::Malformed, _))
    ));
    assert_eq!(refused_with(&reply), RefuseReason::Malformed);

    // An oversized message.
    let (_, reply) = raw_hello(&[0u8; MAX_MESSAGE_LEN + 10], &fds);
    assert_eq!(refused_with(&reply), RefuseReason::Malformed);
}

#[test]
fn other_version_is_refused_with_the_workers_version() {
    let region = shm::Region::create(8).unwrap();
    let (_wake, efd) = sys::wake_pair().unwrap();
    let mut wire = Hello::new(8).encode();
    wire[4..8].copy_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
    let (_, reply) = raw_hello(&wire, &[region.fd(), efd.as_fd()]);
    let Reply::Refuse(r) = reply else {
        panic!("accepted")
    };
    assert_eq!(r.reason, RefuseReason::Version);
    assert_eq!(r.version, PROTOCOL_VERSION);
}

#[test]
fn short_memfd_is_refused_at_accept() {
    // The hello declares the full size but the memfd is one slot short.
    let short = sys::sealed_memfd(c"short", shm::region_size(8) - shm::SLOT_SIZE).unwrap();
    let (_wake, efd) = sys::wake_pair().unwrap();
    let (client, server) = SeqPacket::pair().unwrap();
    client
        .send(&Hello::new(8).encode(), &[short.as_fd(), efd.as_fd()])
        .unwrap();
    let hs = Handshake::read(server, T).unwrap();
    assert!(matches!(
        hs.accept(1, 0),
        Err(HandshakeError::Refused(RefuseReason::SharedMemory, _))
    ));
    let mut buf = [0u8; MAX_MESSAGE_LEN];
    let (n, _) = client.recv(&mut buf).unwrap();
    assert_eq!(
        refused_with(&Reply::decode(&buf[..n]).unwrap()),
        RefuseReason::SharedMemory
    );
}

#[test]
fn client_sees_refusals_and_bad_accepts() {
    let path = socket_path();
    let listener = Listener::bind(&path).unwrap();
    let worker = thread::spawn(move || {
        // 1: an explicit refusal.
        let hs = Handshake::read(listener.accept().unwrap(), T).unwrap();
        hs.refuse(RefuseReason::Busy, "already serving").unwrap();
        // 2: a reply of another version.
        let conn = listener.accept().unwrap();
        let mut buf = [0u8; MAX_MESSAGE_LEN];
        conn.recv(&mut buf).unwrap();
        let mut wire = Reply::encode_accept(1, 0);
        wire[4..8].copy_from_slice(&9u32.to_le_bytes());
        conn.send(&wire, &[]).unwrap();
        // 3: an accept with an absurd depth.
        let conn = listener.accept().unwrap();
        conn.recv(&mut buf).unwrap();
        conn.send(&Reply::encode_accept(50, 0), &[]).unwrap();
        // 4: silence: the client times out.
        let conn = listener.accept().unwrap();
        thread::sleep(Duration::from_millis(400));
        drop(conn);
    });
    let e = client::connect(&path, 8, T).unwrap_err();
    assert!(
        matches!(&e, ConnectError::Refused(r) if r.reason == RefuseReason::Busy),
        "{e}"
    );
    assert!(e.is_refusal());
    let e = client::connect(&path, 8, T).unwrap_err();
    assert!(matches!(e, ConnectError::Version(9)), "{e}");
    assert!(e.is_refusal());
    let e = client::connect(&path, 8, T).unwrap_err();
    assert!(matches!(e, ConnectError::Protocol(_)), "{e}");
    let e = client::connect(&path, 8, Duration::from_millis(100)).unwrap_err();
    assert!(matches!(e, ConnectError::Io(_)), "{e}");
    assert!(!e.is_refusal());
    worker.join().unwrap();
    // No listener at all.
    std::fs::remove_file(&path).unwrap();
    assert!(matches!(
        client::connect(&path, 8, T),
        Err(ConnectError::Io(_))
    ));
}

#[test]
fn a_worker_cannot_make_the_wake_block() {
    // The worker's end of the wake socket is made blocking and never read:
    // the plugin's notify() must still return at once, every time.
    let path = socket_path();
    let listener = Listener::bind(&path).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let hs = Handshake::read(listener.accept().unwrap(), T).unwrap();
        let ep = hs.accept(1, 0).unwrap();
        // SAFETY: plain fcntl on a descriptor the endpoint owns.
        unsafe {
            use std::os::fd::AsRawFd;
            let fd = ep.input_fd().as_raw_fd();
            let fl = libc::fcntl(fd, libc::F_GETFL);
            assert_eq!(libc::fcntl(fd, libc::F_SETFL, fl & !libc::O_NONBLOCK), 0);
        }
        tx.send(()).unwrap();
        // Keep the connection open until the client is done, and never
        // read the wake socket: its queue fills up.
        loop {
            let mut pfd = [sys::pollfd(ep.socket_fd(), libc::POLLIN)];
            if sys::poll(&mut pfd, Some(Duration::from_millis(500))).unwrap() > 0 {
                break;
            }
        }
    });
    let (rings, control) = client::connect(&path, 8, T).unwrap();
    rx.recv().unwrap();
    let start = std::time::Instant::now();
    for _ in 0..100_000 {
        rings.notify();
    }
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "notify() blocked: {:?}",
        start.elapsed()
    );
    drop(control);
    drop(rings);
    worker.join().unwrap();
}

#[test]
fn the_wake_reaches_the_worker_and_a_closed_wake_is_a_hang_up() {
    let (plugin, worker_end) = sys::wake_pair().unwrap();
    let (_client, server) = SeqPacket::pair().unwrap();
    let region = shm::Region::create(8).unwrap();
    let (c2, s2) = SeqPacket::pair().unwrap();
    c2.send(&Hello::new(8).encode(), &[region.fd(), worker_end.as_fd()])
        .unwrap();
    drop(worker_end);
    drop(server);
    let ep = Handshake::read(s2, T).unwrap().accept(1, 0).unwrap();
    let mut buf = [0u8; MAX_MESSAGE_LEN];
    c2.recv(&mut buf).unwrap(); // the accept
    assert_eq!(ep.wait(Duration::from_millis(10)).unwrap(), Wait::Timeout);
    sys::wake_signal(plugin.as_fd());
    sys::wake_signal(plugin.as_fd());
    assert_eq!(ep.wait(Duration::from_millis(500)).unwrap(), Wait::Input);
    // Both wakes were discarded by the one wait.
    assert_eq!(ep.wait(Duration::from_millis(10)).unwrap(), Wait::Timeout);
    drop(plugin);
    assert_eq!(ep.wait(Duration::from_millis(500)).unwrap(), Wait::HangUp);
    // Signalling a closed peer is ignored (no SIGPIPE, no error).
    let (lone, gone) = sys::wake_pair().unwrap();
    drop(gone);
    sys::wake_signal(lone.as_fd());
}

#[test]
fn connect_is_cancellable_and_never_blocks_on_a_full_backlog() {
    // A listener that never accepts: the handshake hangs.
    let path = socket_path();
    let listener = Listener::bind(&path).unwrap();
    let cancel = sys::eventfd_new().unwrap();
    let cancel2 = cancel.try_clone().unwrap();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        sys::eventfd_signal(cancel2.as_fd());
    });
    let start = std::time::Instant::now();
    let e = client::connect_cancellable(&path, 8, Duration::from_secs(30), Some(cancel.as_fd()))
        .unwrap_err();
    assert!(matches!(e, ConnectError::Cancelled), "{e}");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    canceller.join().unwrap();

    // Fill the backlog, then a connect fails at once instead of waiting.
    let mut queued = Vec::new();
    loop {
        match SeqPacket::connect_nonblocking(&path) {
            Ok((s, _)) => queued.push(s),
            Err(e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock, "{e}");
                break;
            }
        }
        assert!(queued.len() < 1024, "the backlog never filled");
    }
    let start = std::time::Instant::now();
    let e = client::connect(&path, 8, Duration::from_secs(30)).unwrap_err();
    assert!(
        matches!(&e, ConnectError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock),
        "{e}"
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(listener);
}
