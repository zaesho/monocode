//! The OpenCode HTTP and SSE transport: loopback-only requests, the event
//! stream handshake, chunk decoding, and cancellation of replaced streams.

use super::*;

/// Records stream frames, and `end` when a stream ends.
struct Events(Mutex<mpsc::Sender<String>>);

impl HarnessEvents for Events {
    fn stdout(&self, _: &str, _: String, _: u32) {}
    fn stderr(&self, _: &str, _: String, _: u32) {}
    fn exit(&self, _: &str, _: Option<i32>, _: u32) {}
    fn sse(&self, _: &str, data: String) {
        let _ = self.0.lock().unwrap().send(data);
    }
    fn sse_end(&self, _: &str, _: Option<String>) {
        let _ = self.0.lock().unwrap().send("end".into());
    }
}

fn host_with_events() -> (HarnessHost, mpsc::Receiver<String>) {
    let (tx, rx) = mpsc::channel();
    (HarnessHost::new(Arc::new(Events(Mutex::new(tx)))), rx)
}

fn live_sse(socket: Option<TcpStream>) -> Arc<LiveSse> {
    Arc::new(LiveSse {
        stop: Arc::new(AtomicBool::new(false)),
        socket: Mutex::new(socket),
    })
}

/// Read one request head.
fn request(socket: &mut TcpStream) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
}

#[test]
fn rejects_credentials_non_loopback_and_redirects() {
    for url in [
        "http://127.0.0.1:80@example.com/",
        "http://localhost:80@192.0.2.1/",
        "https://localhost/",
        "http://example.com/",
    ] {
        assert!(assert_loopback(url).is_err(), "accepted {url}");
    }
    assert!(assert_loopback("http://localhost:123/event").is_ok());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        request(&mut socket);
        socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://192.0.2.1/destination\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    let reply = harness_http(
        format!("http://{address}/"),
        "GET".into(),
        None,
        None,
        Some(1000),
    )
    .unwrap();
    assert_eq!(reply.status, 302);
    server.join().unwrap();
}

#[test]
fn sse_waits_for_headers_decodes_chunks_and_close_interrupts_the_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let sent = Arc::new(AtomicBool::new(false));
    let sent_server = sent.clone();
    let (closed_tx, closed_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        request(&mut socket);
        thread::sleep(Duration::from_millis(50));
        sent_server.store(true, Ordering::SeqCst);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        for piece in ["data: hel", "lo\n\n"] {
            write!(socket, "{:x}\r\n{piece}\r\n", piece.len()).unwrap();
        }
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut byte = [0];
        closed_tx
            .send(socket.read(&mut byte).unwrap() == 0)
            .unwrap();
    });
    let (host, events) = host_with_events();
    harness_sse_open(
        &host,
        "stream".into(),
        format!("http://{address}/event"),
        None,
    )
    .unwrap();
    // The open returned only after the server answered.
    assert!(sent.load(Ordering::SeqCst));
    assert_eq!(
        events.recv_timeout(Duration::from_secs(1)).unwrap(),
        "hello"
    );
    host.stop_sse("stream");
    assert!(closed_rx.recv_timeout(Duration::from_secs(1)).unwrap());
    assert!(events.recv_timeout(Duration::from_millis(50)).is_err());
    server.join().unwrap();
}

#[test]
fn obsolete_sse_cannot_deliver_data_or_end_to_replacement() {
    let (host, events) = host_with_events();
    let old = live_sse(None);
    let stale = CurrentSseEvents {
        host: Arc::downgrade(&host.0),
        events: host.events.clone(),
        session_id: "same".into(),
        live: old.clone(),
    };
    host.insert_sse("same".into(), old);
    host.stop_sse("same");
    host.insert_sse("same".into(), live_sse(None));
    stale.sse("stale".into());
    stale.end(None);
    assert!(events.try_recv().is_err());
    assert!(host.sse.lock().unwrap().contains_key("same"));
}

#[test]
fn replacing_registered_sse_cancels_its_blocked_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (socket, _) = listener.accept().unwrap();
    let (host, _events) = host_with_events();
    let old = live_sse(Some(socket.try_clone().unwrap()));
    let new = live_sse(None);
    let mut socket = SseSocket::new(BufReader::new(socket), old.stop.clone()).unwrap();
    let (started_tx, started_rx) = mpsc::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        started_tx.send(()).unwrap();
        closed_tx.send(socket.read(&mut [0])).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(1)).unwrap();

    // Two opens can both remove the previous stream before either inserts.
    host.stop_sse("same");
    host.stop_sse("same");
    host.insert_sse("same".into(), old.clone());
    host.insert_sse("same".into(), new.clone());

    assert!(old.stop.load(Ordering::SeqCst));
    assert!(old.socket.lock().unwrap().is_none());
    match closed_rx.recv_timeout(Duration::from_secs(1)).unwrap() {
        Ok(read) => assert_eq!(read, 0),
        Err(error) => assert!(!matches!(
            error.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        )),
    }
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    assert_eq!(peer.read(&mut [0]).unwrap(), 0);
    assert!(!new.stop.load(Ordering::SeqCst));
    assert!(Arc::ptr_eq(
        host.sse.lock().unwrap().get("same").unwrap(),
        &new
    ));
    reader.join().unwrap();
}

#[test]
fn sse_socket_cancel_finishes_without_a_peer_disconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let mut socket = SseSocket::new(BufReader::new(socket), stop.clone()).unwrap();
    socket.reader.get_ref().set_nonblocking(true).unwrap();
    assert_eq!(socket.read(&mut []).unwrap(), 0);
    let (closed_tx, closed_rx) = mpsc::channel();
    let reader = thread::spawn(move || {
        closed_tx.send(socket.read(&mut [0])).unwrap();
        socket
    });
    assert!(closed_rx.recv_timeout(Duration::from_millis(100)).is_err());
    stop.store(true, Ordering::SeqCst);
    assert_eq!(
        closed_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap(),
        0
    );
    let socket = reader.join().unwrap();
    peer.set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    assert!(matches!(
        peer.read(&mut [0]).unwrap_err().kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ));
    drop(socket);
}

#[test]
fn sse_socket_idle_polls_preserve_partial_chunk_headers_and_data() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut peer, _) = listener.accept().unwrap();
    let reader = SseSocket::new(BufReader::new(socket), Arc::new(AtomicBool::new(false))).unwrap();
    reader.reader.get_ref().set_nonblocking(true).unwrap();
    let mut body = SseBody {
        reader: BufReader::new(reader),
        chunked: true,
        remaining: 0,
        chunk_end: false,
        finished: false,
    };
    let sender = thread::spawn(move || {
        peer.write_all(b"7\r").unwrap();
        thread::sleep(Duration::from_millis(100));
        peer.write_all(b"\nhe").unwrap();
        thread::sleep(Duration::from_millis(100));
        peer.write_all(b"llo\n\n\r\n0\r\n\r\n").unwrap();
    });
    let mut text = String::new();
    body.read_to_string(&mut text).unwrap();
    assert_eq!(text, "hello\n\n");
    sender.join().unwrap();
}

#[test]
fn rejects_non_sse_and_redirected_handshakes() {
    for response in [
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n",
        "HTTP/1.1 302 Found\r\nLocation: http://192.0.2.1/\r\n\r\n",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            request(&mut socket);
            socket.write_all(response.as_bytes()).unwrap();
        });
        let (host, _events) = host_with_events();
        assert!(
            harness_sse_open(&host, "stream".into(), format!("http://{address}/"), None).is_err()
        );
        assert!(host.sse.lock().unwrap().is_empty());
        server.join().unwrap();
    }
}

#[test]
fn standard_opencode_npm_wrapper_requires_known_entry() {
    let root =
        std::env::temp_dir().join(format!("monocode-opencode-launch-{}", uuid::Uuid::new_v4()));
    let wrapper = root.join("opencode.cmd");
    assert!(opencode_npm_entry(&wrapper.to_string_lossy(), true).is_err());
    let entry = root.join("node_modules/opencode-ai/bin/opencode");
    std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
    std::fs::write(&entry, "process.exit(0)").unwrap();
    assert_eq!(
        opencode_npm_entry(&wrapper.to_string_lossy(), true).unwrap(),
        Some(entry)
    );
    assert_eq!(
        opencode_npm_entry(&wrapper.to_string_lossy(), false).unwrap(),
        None
    );
    std::fs::remove_dir_all(root).unwrap();
}
