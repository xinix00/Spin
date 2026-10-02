//! Twee stacks aan één draad: de "kern" op 10.100.0.1 en een app op slot 1,
//! elk met een eigen pomp-taak over een [`Nic`] op gewone buffers, op één
//! executor met een klok die de test zelf verzet. Zo lopen handshake, data,
//! close en de system-client door precies de code die op het slot draait.

use super::*;
use crate::contract::{HOPABI_HDR_LEN, HOPABI_VERSION, KIND_RESULT, OP_STAT, SYS_HEADER_LEN};
use crate::ring::tests::Backing;
use crate::ring::{Peek, Reader, Writer};
use crate::sys::{check_frame_header, frame_header};
use core::cell::Cell;
use std::boxed::Box;

thread_local! {
    static NOW: Cell<u64> = const { Cell::new(1_000_000_000) };
}

/// De klok van de test: staat stil tot de lus hem naar de volgende timer zet.
fn now() -> u64 {
    NOW.with(Cell::get)
}

fn leak<T>(v: T) -> &'static T {
    Box::leak(Box::new(v))
}

const CAP: u64 = NET_RING_DATA_CAP;
const BUDGET: usize = 4 << 20;
/// Een poll-ronde van tien seconden: de tests lopen dan op de bel en op
/// de wekken van de stack, en een gemiste wek is een test die tien
/// (gesimuleerde) seconden duurt in plaats van een die toevallig slaagt.
const POLL: RxPoll = RxPoll {
    lo: Duration::from_secs(10),
    hi: Duration::from_secs(10),
    hold: 0,
};

/// De deurbel zoals de idle van de app-core hem belt: ligt er RX, dan de
/// bel van die pomp.
type Door = (Peek, &'static Signal);

struct Pair {
    exec: &'static Exec,
    kern: &'static Net,
    app: &'static Net,
    doors: [Door; 2],
}

fn nic(tx: &Backing, rx: &Backing, slot: u64) -> Nic {
    Nic::over(
        Writer::open(tx.pa(), CAP).unwrap(),
        Reader::open(rx.pa(), CAP).unwrap(),
        Peek::new(rx.pa(), CAP),
        mac_of(slot),
    )
}

fn spawn_pump(exec: &'static Exec, net: &'static Net, mut nic: Nic, rx: &Backing) -> Door {
    let bell: &'static Signal = leak(Signal::new());
    let mut buf = frame_buf(net.frame_len()).unwrap();
    exec.spawn(async move { net.pump(&mut nic, &mut buf, bell, POLL).await })
        .unwrap();
    (Peek::new(rx.pa(), CAP), bell)
}

/// De kern en slot 1 aan één draad, met hun pompen al gespawnd.
fn pair() -> Pair {
    let exec: &'static Exec = leak(Exec::new());
    exec.set_clock(now);
    let up = leak(Backing::new(CAP));
    let down = leak(Backing::new(CAP));
    let kern = leak(Net::new(slot_config(0, BUDGET), 7, exec, now).unwrap());
    let app = leak(Net::new(slot_config(1, BUDGET), 9, exec, now).unwrap());
    app.seed_neighbor(host_ip(), mac_of(0).0).unwrap();
    let k = spawn_pump(exec, kern, nic(down, up, 0), up);
    let a = spawn_pump(exec, app, nic(up, down, 1), down);
    Pair {
        exec,
        kern,
        app,
        doors: [k, a],
    }
}

impl Pair {
    /// Draait de executor tot `done`. Heeft geen taak iets te doen, dan
    /// eerst de deurbel (zoals de slaper vóór de WFE), en anders de klok
    /// naar de volgende timer.
    fn run_until(&self, done: impl Fn() -> bool) {
        for _ in 0..1_000_000 {
            if done() {
                return;
            }
            if self.exec.step() {
                continue;
            }
            let mut rang = false;
            for (peek, bell) in &self.doors {
                if peek.head_pending().1 {
                    bell.set();
                    rang = true;
                }
            }
            if !rang {
                let next = self
                    .exec
                    .next_deadline()
                    .expect("niets te doen en geen timer");
                NOW.with(|c| c.set(c.get().max(next)));
            }
        }
        panic!("liep niet af");
    }
}

/// Hoeveel gesimuleerde tijd sinds `t0`; ruim onder de poll-ronde bewijst
/// dat bel en stack de pompen wekten, niet de timer.
fn elapsed(t0: u64) -> u64 {
    now() - t0
}

fn slot<T>() -> &'static RefCell<Option<T>> {
    leak(RefCell::new(None))
}

/// Een kern van één call: leest een request-frame, antwoordt op een stat
/// met maat 4096, en leest dan tot EOF.
async fn fake_kern(l: TcpListener, seen: &'static RefCell<Option<(u8, Vec<u8>, usize)>>) {
    let mut c = l.accept().await.unwrap();
    let mut fh = [0u8; SYS_HEADER_LEN];
    read_exact(&mut c, &mut fh).await;
    let (_, n) = check_frame_header(&fh).unwrap();
    let mut req = vec![0u8; n];
    read_exact(&mut c, &mut req).await;
    let op = req[1];
    let seq = u32::from_le_bytes(req[4..8].try_into().unwrap());
    let mut resp = [0u8; HOPABI_HDR_LEN];
    resp[0] = HOPABI_VERSION;
    resp[1] = op;
    resp[4..8].copy_from_slice(&seq.to_le_bytes());
    resp[8..16].copy_from_slice(&4096u64.to_le_bytes());
    c.write_all(&frame_header(KIND_RESULT, HOPABI_HDR_LEN as u32))
        .await
        .unwrap();
    c.write_all(&resp).await.unwrap();
    // Na de call: de client sluit, en dat is EOF, geen reset.
    let mut rest = [0u8; 16];
    let eof = c.read(&mut rest).await.unwrap();
    *seen.borrow_mut() = Some((op, req[HOPABI_HDR_LEN..].to_vec(), eof));
}

async fn read_exact(c: &mut TcpStream, mut buf: &mut [u8]) {
    while !buf.is_empty() {
        let n = c.read(buf).await.unwrap();
        assert!(n > 0, "EOF midden in een frame");
        buf = &mut buf[n..];
    }
}

#[test]
fn system_stat_over_a_real_tcp_connection() {
    let p = pair();
    let l = p.kern.tcp_listen(sys::ADDRESS.1).unwrap();
    let seen = slot();
    let got = slot();
    p.exec.spawn(fake_kern(l, seen)).unwrap();
    let app = p.app;
    let t0 = now();
    p.exec
        .spawn(async move {
            let mut client = app.system_client();
            let r = client.stat("/data/db.bin").await;
            let connected = client.is_connected();
            drop(client); // FIN
            *got.borrow_mut() = Some((r, connected));
        })
        .unwrap();
    p.run_until(|| seen.borrow().is_some() && got.borrow().is_some());
    assert_eq!(got.borrow_mut().take(), Some((Ok(4096), true)));
    assert!(elapsed(t0) < 1_000_000, "stat duurde {} ns", elapsed(t0));
    let (op, path, eof) = seen.borrow_mut().take().unwrap();
    assert_eq!(
        (op, path.as_slice(), eof),
        (OP_STAT, &b"/data/db.bin"[..], 0)
    );
}

#[test]
fn an_explicit_connection_carries_the_first_call() {
    let p = pair();
    let l = p.kern.tcp_listen(sys::ADDRESS.1).unwrap();
    let seen = slot();
    let got = slot();
    p.exec.spawn(fake_kern(l, seen)).unwrap();
    let app = p.app;
    p.exec
        .spawn(async move {
            let c = app.tcp_connect(HOST, sys::ADDRESS.1).await.unwrap();
            let local = c.local().unwrap();
            let mut client = app.system_client_over(c);
            let r = client.stat("/").await;
            drop(client);
            *got.borrow_mut() = Some((r, local.ip));
        })
        .unwrap();
    p.run_until(|| seen.borrow().is_some() && got.borrow().is_some());
    assert_eq!(got.borrow_mut().take(), Some((Ok(4096), slot_ip(1))));
}

/// 40 × 8 KiB heen en weer door een echo, dan close: de client ziet de
/// echo byte voor byte terug en daarna EOF.
#[test]
fn bulk_both_ways_then_close() {
    const CHUNK: usize = 8 << 10;
    const ROUNDS: usize = 40;
    let p = pair();
    let l = p.kern.tcp_listen(7).unwrap();
    let echoed = leak(Cell::new(0usize));
    let server_done = leak(Cell::new(false));
    let result = slot();
    let t0 = now();
    p.exec
        .spawn(async move {
            let mut c = l.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            loop {
                let n = c.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                c.write_all(&buf[..n]).await.unwrap();
                echoed.set(echoed.get() + n);
            }
            c.close().unwrap();
            server_done.set(true);
        })
        .unwrap();
    let app = p.app;
    p.exec
        .spawn(async move {
            let mut c = app.tcp_connect([10, 100, 0, 1], 7).await.unwrap();
            let mut back = vec![0u8; CHUNK];
            let mut ok = true;
            for r in 0..ROUNDS {
                let out: Vec<u8> = (0..CHUNK).map(|i| (i * 7 + r) as u8).collect();
                c.write_all(&out).await.unwrap();
                read_exact(&mut c, &mut back).await;
                ok &= back == out;
            }
            // Half dicht is hier heel dicht: close, en de echo sluit ook.
            let h = c.h;
            let net = c.net;
            c.close().unwrap();
            let after = net.with(|st| st.tcp_state(h)).unwrap();
            *result.borrow_mut() = Some((ok, after.is_err()));
        })
        .unwrap();
    p.run_until(|| result.borrow().is_some());
    assert_eq!(result.borrow_mut().take(), Some((true, true)));
    // De echo zag EOF en sloot.
    p.run_until(|| server_done.get());
    assert_eq!(echoed.get(), CHUNK * ROUNDS);
    assert!(
        elapsed(t0) < 1_000_000_000,
        "bulk duurde {} ns",
        elapsed(t0)
    );
    let st = p.app.stats().unwrap();
    assert_eq!(st.refused_no_budget, 0);
}

#[test]
fn a_closed_port_is_refused() {
    let p = pair();
    let got = slot();
    let app = p.app;
    p.exec
        .spawn(async move {
            let r = app.tcp_connect(HOST, 9).await.map(|_| ());
            *got.borrow_mut() = Some(r);
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    assert_eq!(
        got.borrow_mut().take(),
        Some(Err(NetError::Stack(StackError::Refused {
            ip: HOST,
            port: 9
        })))
    );
    assert_eq!(
        conn_error(NetError::Stack(StackError::Refused { ip: HOST, port: 9 })),
        ConnError::Refused
    );
}

#[test]
fn a_silent_peer_times_out_on_the_executor() {
    let p = pair();
    let got = slot();
    let app = p.app;
    let t0 = now();
    p.exec
        .spawn(async move {
            let r = app
                .tcp_connect_timeout([10, 100, 0, 77], 80, Duration::from_millis(200))
                .await
                .map(|_| ());
            *got.borrow_mut() = Some((r, now()));
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    let (r, t) = got.borrow_mut().take().unwrap();
    assert!(
        matches!(
            r,
            Err(NetError::Timeout | NetError::Stack(StackError::DeadlineExceeded))
        ),
        "{r:?}"
    );
    assert!(t - t0 >= 200_000_000 && t - t0 < 300_000_000, "{}", t - t0);
}

#[test]
fn a_read_deadline_fires_and_the_stream_stays_usable() {
    let p = pair();
    let l = p.kern.tcp_listen(7).unwrap();
    let got = slot();
    p.exec
        .spawn(async move {
            let mut c = l.accept().await.unwrap();
            c.set_timeout(Some(Duration::from_millis(50)));
            let mut b = [0u8; 4];
            let first = c.read(&mut b).await.map(|_| ());
            c.set_timeout(None);
            let second = c.read(&mut b).await;
            *got.borrow_mut() = Some((first, second, b));
        })
        .unwrap();
    let app = p.app;
    p.exec
        .spawn(async move {
            let mut c = app.tcp_connect(HOST, 7).await.unwrap();
            app.exec.after(Duration::from_millis(100)).await;
            c.write_all(b"late").await.unwrap();
            app.exec.after(Duration::from_secs(1)).await;
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    assert_eq!(
        got.borrow_mut().take(),
        Some((Err(NetError::Timeout), Ok(4), *b"late"))
    );
}

#[test]
fn udp_round_trip() {
    let p = pair();
    let kern = p.kern;
    let app = p.app;
    let got = slot();
    let server = kern.udp_bind(53).unwrap();
    p.exec
        .spawn(async move {
            let mut buf = [0u8; 64];
            let (n, from) = server.recv_from(&mut buf).await.unwrap();
            buf[..n].reverse();
            server.send_to(from, &buf[..n]).await.unwrap();
        })
        .unwrap();
    p.exec
        .spawn(async move {
            let s = app.udp_bind(0).unwrap();
            let to = Endpoint { ip: HOST, port: 53 };
            s.send_to(to, b"ping").await.unwrap();
            let mut buf = [0u8; 64];
            let (n, from) = s.recv_from(&mut buf).await.unwrap();
            *got.borrow_mut() = Some((buf[..n].to_vec(), from));
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    assert_eq!(
        got.borrow_mut().take(),
        Some((b"gnip".to_vec(), Endpoint { ip: HOST, port: 53 }))
    );
}

/// Multicast: een socket op de poort van de groep hoort een datagram naar
/// de groep pas na de join; de kern zendt naar de groep zoals de switch
/// het naar elk slot floodt.
#[test]
fn a_group_datagram_arrives_only_after_the_join() {
    const MDNS: Endpoint = Endpoint {
        ip: [224, 0, 0, 251],
        port: 5353,
    };
    let p = pair();
    let kern = p.kern;
    let app = p.app;
    let got = slot();
    let listen = app.udp_bind(MDNS.port).unwrap();
    p.exec
        .spawn(async move {
            let mut buf = [0u8; 64];
            let mut out = Vec::new();
            // Vóór de join: de stack laat het datagram vallen, de wacht
            // loopt af.
            let mut l = listen;
            l.set_timeout(Some(Duration::from_millis(50)));
            out.push(l.recv_from(&mut buf).await.map(|(n, _)| buf[..n].to_vec()));
            app.join_group(MDNS.ip).unwrap();
            // Een tweede join is geen fout en geen tweede groep.
            app.join_group(MDNS.ip).unwrap();
            l.set_timeout(Some(Duration::from_millis(50)));
            out.push(l.recv_from(&mut buf).await.map(|(n, _)| buf[..n].to_vec()));
            *got.borrow_mut() = Some(out);
        })
        .unwrap();
    p.exec
        .spawn(async move {
            let s = kern.udp_bind(5354).unwrap();
            s.send_to(MDNS, b"early").await.unwrap();
            // Na de wacht van de luisteraar: de join is er dan.
            kern.exec.after(Duration::from_millis(60)).await;
            s.send_to(MDNS, b"probe").await.unwrap();
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    let out = got.borrow_mut().take().unwrap();
    assert_eq!(out[0], Err(NetError::Timeout));
    assert_eq!(out[1], Ok(b"probe".to_vec()));
    // Alleen link-local: een groep daarbuiten weigert de stack.
    assert_eq!(
        app.join_group([239, 1, 1, 1]),
        Err(NetError::Stack(StackError::NotLinkLocalMulticast {
            ip: [239, 1, 1, 1]
        }))
    );
}

#[test]
fn budget_env_and_address_parsing() {
    assert_eq!(budget_for(16 << 20, None), 2 << 20);
    assert_eq!(budget_for(1 << 20, None), BUDGET_MIN);
    assert_eq!(budget_for(1 << 30, None), BUDGET_MAX);
    assert_eq!(budget_for(1 << 30, Some("512k")), 512 << 10);
    assert_eq!(budget_for(1 << 30, Some("3M")), 3 << 20);
    assert_eq!(budget_for(16 << 20, Some("123456")), 123_456);
    assert_eq!(budget_for(16 << 20, Some("junk")), 2 << 20);
    assert_eq!(budget_for(16 << 20, Some("0")), 2 << 20);
    assert_eq!(parse_ip4("10.100.0.1"), Some(HOST));
    assert_eq!(parse_ip4("1.1.1.1"), Some([1, 1, 1, 1]));
    assert_eq!(parse_ip4("1.1.1"), None);
    assert_eq!(parse_ip4("1.1.1.1.1"), None);
    assert_eq!(parse_ip4("1.1.1.256"), None);
    let c = slot_config(3, 2 << 20);
    assert_eq!(
        (c.ip, c.mac, c.gw, c.prefix),
        (slot_ip(3), mac_of(3).0, HOST, 24)
    );
    assert_eq!(c.mtu, NET_MTU);
    assert_eq!(sys::ADDRESS, (HOST, abi::systemapi::PORT));
}

#[test]
fn transport_errors_map_to_what_the_client_retries() {
    assert_eq!(
        conn_error(NetError::Stack(StackError::Reset)),
        ConnError::Reset
    );
    assert_eq!(
        conn_error(NetError::Stack(StackError::Closed)),
        ConnError::Closed
    );
    assert_eq!(conn_error(NetError::Timeout), ConnError::Refused);
    assert_eq!(conn_error(NetError::NotUp), ConnError::Refused);
}

/// De log-verbinding: vóór de dial gaat een regel naar de outbox, daarna
/// als `KindLog`-frame over TCP naar de kern. De enige test die de
/// log-static aanraakt.
#[test]
fn log_lines_go_over_the_system_connection_once_it_is_up() {
    use crate::contract::KIND_LOG;
    let p = pair();
    let l = p.kern.tcp_listen(sys::ADDRESS.1).unwrap();
    let seen = slot();
    p.exec
        .spawn(async move {
            let mut c = l.accept().await.unwrap();
            let mut got = Vec::new();
            for _ in 0..2 {
                let mut fh = [0u8; SYS_HEADER_LEN];
                read_exact(&mut c, &mut fh).await;
                let (kind, n) = check_frame_header(&fh).unwrap();
                let mut line = vec![0u8; n];
                read_exact(&mut c, &mut line).await;
                got.push((kind, line));
            }
            *seen.borrow_mut() = Some(got);
        })
        .unwrap();
    assert!(!try_log(b"too early"), "uit is outbox");
    log_via_system(p.app).unwrap();
    assert_eq!(log_via_system(p.app), Err(NetError::AlreadyUp));
    assert!(!try_log(b"still dialing"));
    p.run_until(|| {
        log_cell()
            .try_borrow()
            .is_ok_and(|l| l.as_ref().is_some_and(|l| l.conn.is_some()))
    });
    let written = crate::log::WRITTEN.load(Relaxed);
    crate::log::emit_via_net(None, format_args!("slot {} up", 1));
    assert!(crate::log::WRITTEN.load(Relaxed) > written);
    assert!(try_log(b"second"));
    p.run_until(|| seen.borrow().is_some());
    assert_eq!(
        seen.borrow_mut().take().unwrap(),
        [
            (KIND_LOG, b"slot 1 up".to_vec()),
            (KIND_LOG, b"second".to_vec())
        ]
    );
}

// ---- Flush en het net-afscheid ----

/// Een kern die één verbinding aanneemt en alles leest tot EOF.
async fn sink(l: TcpListener, got: &'static RefCell<Option<Vec<u8>>>) {
    let mut c = l.accept().await.unwrap();
    let mut all = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = c.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        all.extend_from_slice(&buf[..n]);
    }
    *got.borrow_mut() = Some(all);
}

/// Het geval van 29-09: een regel die vlak vóór de exit de zendbuffer in
/// gaat, komt aan, omdat de shutdown pas klaar is als de FIN bevestigd is.
/// Daarna gaat er niets nieuws meer open.
#[test]
fn a_line_written_just_before_shutdown_arrives() {
    let p = pair();
    let l = p.kern.tcp_listen(sys::ADDRESS.1).unwrap();
    let got = slot();
    p.exec.spawn(sink(l, got)).unwrap();
    let (app, kern) = (p.app, p.kern);
    let done = slot();
    let t0 = now();
    p.exec
        .spawn(async move {
            let mut c = app.tcp_connect(HOST, sys::ADDRESS.1).await.unwrap();
            let _udp = app.udp_bind(0).unwrap();
            let _l = app.tcp_listen(8080).unwrap();
            c.write_all(b"HOPOS_APPSPIKE_NETLOG last words")
                .await
                .unwrap();
            // Geen flush, geen pauze: meteen het afscheid.
            let d = app.shutdown(Duration::from_millis(200)).await;
            // Op het moment van terugkeer (hierna zou de core parkeren) heeft
            // de stack van de kern elke byte al.
            let at_exit = kern.stats().unwrap().tcp_bytes_in;
            let again = app.tcp_connect(HOST, sys::ADDRESS.1).await.map(|_| ());
            let bind = app.udp_bind(0).map(|_| ());
            *done.borrow_mut() = Some((d, again, bind, now(), at_exit));
        })
        .unwrap();
    p.run_until(|| done.borrow().is_some());
    let (d, again, bind, t, at_exit) = done.borrow_mut().take().unwrap();
    assert!(d.drained, "{d:?}");
    assert_eq!(at_exit, b"HOPOS_APPSPIKE_NETLOG last words".len());
    assert_eq!((d.closed, d.untracked), (3, 0));
    assert!(t - t0 < 200_000_000, "afscheid duurde {} ns", t - t0);
    assert_eq!(again, Err(NetError::ShuttingDown));
    assert_eq!(bind, Err(NetError::ShuttingDown));
    // Bevestigd is bij de kern: zijn lezer krijgt de regel en dan EOF.
    p.run_until(|| got.borrow().is_some());
    assert_eq!(
        got.borrow_mut().take().unwrap(),
        b"HOPOS_APPSPIKE_NETLOG last words"
    );
}

/// Zonder verbindingen is het afscheid meteen klaar.
#[test]
fn shutdown_without_connections_is_immediate() {
    let p = pair();
    let app = p.app;
    let done = slot();
    p.exec
        .spawn(async move {
            let d = app.shutdown(Duration::from_millis(200)).await;
            *done.borrow_mut() = Some(d);
        })
        .unwrap();
    p.run_until(|| done.borrow().is_some());
    let d = done.borrow_mut().take().unwrap();
    assert!(d.drained && d.closed == 0 && d.waited_us == 0, "{d:?}");
}

/// Een flush is pas klaar als de peer alles heeft: direct daarna ligt elke
/// byte in de ontvangstring van de kern, zonder dat de pomp van de app nog
/// iets hoeft te doen.
#[test]
fn flush_returns_once_the_peer_has_every_byte() {
    const N: usize = 12 << 10;
    let p = pair();
    let l = p.kern.tcp_listen(7).unwrap();
    let peer = slot();
    p.exec
        .spawn(async move {
            // Aannemen en dan niet lezen: wat binnen is, blijft in de ring.
            *peer.borrow_mut() = Some(l.accept().await.unwrap());
        })
        .unwrap();
    let (app, kern) = (p.app, p.kern);
    let flushed = slot();
    p.exec
        .spawn(async move {
            let mut c = app.tcp_connect(HOST, 7).await.unwrap();
            let data: Vec<u8> = (0..N).map(|i| i as u8).collect();
            c.write_all(&data).await.unwrap();
            let r = c.flush().await;
            // Op het moment van terugkeer, niet later.
            let at_flush = kern.stats().unwrap().tcp_bytes_in;
            *flushed.borrow_mut() = Some((r, at_flush));
            // Open houden: een close hoort niet bij deze toets.
            core::future::pending::<()>().await;
        })
        .unwrap();
    p.run_until(|| flushed.borrow().is_some() && peer.borrow().is_some());
    assert_eq!(flushed.borrow_mut().take(), Some((Ok(()), N)));
    let kc = peer.borrow_mut().take().unwrap();
    let mut buf = vec![0u8; 2 * N];
    let mut have = 0;
    while let Ok(Ok(n)) = p.kern.with(|st| st.tcp_read(kc.h, &mut buf[have..], now())) {
        have += n;
    }
    assert_eq!(have, N);
    assert!(buf[..N].iter().enumerate().all(|(i, &b)| b == i as u8));
}

/// Een peer die niet leest, laat het venster dichtlopen: de flush geeft
/// dan op de deadline van de stream een timeout, en liegt geen `Ok`.
#[test]
fn flush_times_out_on_a_peer_that_does_not_read() {
    let p = pair();
    let l = p.kern.tcp_listen(7).unwrap();
    let peer = slot();
    p.exec
        .spawn(async move {
            *peer.borrow_mut() = Some(l.accept().await.unwrap());
        })
        .unwrap();
    let app = p.app;
    let got = slot();
    p.exec
        .spawn(async move {
            let mut c = app.tcp_connect(HOST, 7).await.unwrap();
            // Schrijven tot ook de eigen zendring vol is.
            c.set_timeout(Some(Duration::from_millis(50)));
            let chunk = vec![0x5a; 64 << 10];
            let mut wrote = 0;
            while let Ok(n) = c.write(&chunk).await {
                wrote += n;
            }
            c.set_timeout(Some(Duration::from_millis(100)));
            let t0 = now();
            let r = c.flush().await;
            *got.borrow_mut() = Some((r, wrote, now() - t0));
            core::future::pending::<()>().await;
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    let (r, wrote, dt) = got.borrow_mut().take().unwrap();
    assert_eq!(r, Err(NetError::Timeout));
    assert!(wrote > 64 << 10, "wrote {wrote}");
    assert!((100_000_000..200_000_000).contains(&dt), "{dt}");
}

/// De tabel is vast: wat niet past, werkt wel maar telt als niet gevolgd.
#[test]
fn a_full_open_table_counts_instead_of_growing() {
    assert_eq!(open_slots_for(0), OPEN_MIN);
    assert_eq!(open_slots_for(2 << 20), 128);
    assert_eq!(open_slots_for(BUDGET_MAX), OPEN_MAX);
    assert_eq!(open_slots_for(usize::MAX), OPEN_MAX);
    let p = pair();
    let slots = open_slots_for(BUDGET);
    let ls: Vec<TcpListener> = (0..=slots)
        .map(|i| p.app.tcp_listen(1000 + i as u16).unwrap())
        .collect();
    assert!(ls[..slots].iter().all(|l| l.slot.is_some()));
    assert_eq!(ls[slots].slot, None);
    assert_eq!(p.app.untracked.get(), 1);
    // Eén dicht, en de plek is weer vrij.
    drop(ls);
    let l = p.app.tcp_listen(999).unwrap();
    assert_eq!(l.slot, Some(0));
}

// ---- DNS over het testnet: de kern speelt de server op poort 53 ----

/// Wat de nep-server met de zoveelste vraag doet.
type Answerer = fn(usize, u16, &str) -> Vec<Vec<u8>>;

/// Een DNS-server op de kern die elke vraag aan `answer` voorlegt (het
/// volgnummer, het id en de gevraagde naam) en stuurt wat die teruggeeft,
/// in volgorde; niets is zwijgen. Telt de vragen in `seen`.
async fn fake_dns(server: UdpSocket, answer: Answerer, seen: &'static Cell<usize>) {
    let mut buf = [0u8; 512];
    loop {
        let (n, from) = server.recv_from(&mut buf).await.unwrap();
        let q = &buf[..n];
        let id = u16::from_be_bytes([q[0], q[1]]);
        // De naam uit de vraag: labels vanaf offset 12.
        let mut name = std::string::String::new();
        let mut at = 12;
        while q[at] != 0 {
            let len = usize::from(q[at]);
            if !name.is_empty() {
                name.push('.');
            }
            name.push_str(core::str::from_utf8(&q[at + 1..at + 1 + len]).unwrap());
            at += 1 + len;
        }
        let i = seen.get();
        seen.set(i + 1);
        for reply in answer(i, id, &name) {
            server.send_to(from, &reply).await.unwrap();
        }
    }
}

/// Draait één resolve van `host` in slot 1 tegen een nep-server die met
/// `answer` antwoordt; de uitkomst, het aantal vragen en de duur.
fn resolve_against(answer: Answerer, host: &'static str) -> (Result<[u8; 4]>, usize, u64) {
    let p = pair();
    let seen: &'static Cell<usize> = leak(Cell::new(0));
    let server = p.kern.udp_bind(dns::PORT).unwrap();
    p.exec.spawn(fake_dns(server, answer, seen)).unwrap();
    let got = slot();
    let app = p.app;
    let t0 = now();
    p.exec
        .spawn(async move {
            let r = app.resolve_via(HOST, host).await;
            *got.borrow_mut() = Some((r, now()));
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    let (r, t) = got.borrow_mut().take().unwrap();
    (r, seen.get(), t - t0)
}

const A: &[u8] = &[140, 82, 121, 4];

fn good(_: usize, id: u16, host: &str) -> Vec<Vec<u8>> {
    vec![dns::tests::answer(
        id,
        0,
        host,
        &[(dns::tests::AT_QNAME, 1, A)],
    )]
}

#[test]
fn a_name_resolves_over_udp() {
    let (r, seen, took) = resolve_against(good, "github.com");
    assert_eq!((r, seen), (Ok([140, 82, 121, 4]), 1));
    assert!(took < 100_000_000, "resolve duurde {took} ns");
}

#[test]
fn a_wrong_id_is_ignored_and_the_right_answer_still_counts() {
    fn liar(_: usize, id: u16, host: &str) -> Vec<Vec<u8>> {
        // Eerst een vervalsing met een ander id, dan een met een andere
        // naam (beide met een ander adres), dan het echte antwoord: de
        // eerste twee mogen niet winnen en de wacht niet breken.
        let fake = &[(dns::tests::AT_QNAME, 1, &[6u8; 4][..])];
        let mut out = vec![
            dns::tests::answer(id ^ 1, 0, host, fake),
            dns::tests::answer(id, 0, "evil.example", fake),
        ];
        out.extend(good(0, id, host));
        out
    }
    let (r, seen, _) = resolve_against(liar, "github.com");
    assert_eq!((r, seen), (Ok([140, 82, 121, 4]), 1));
}

#[test]
fn silence_gets_one_retry_with_a_new_id() {
    fn second(i: usize, id: u16, host: &str) -> Vec<Vec<u8>> {
        if i == 1 {
            good(i, id, host)
        } else {
            Vec::new()
        }
    }
    let (r, seen, took) = resolve_against(second, "pool.ntp.org");
    assert_eq!((r, seen), (Ok([140, 82, 121, 4]), 2));
    let t = u64::try_from(DNS_TIMEOUT.as_nanos()).unwrap();
    assert!(took >= t && took < 2 * t, "{took}");

    fn never(_: usize, _: u16, _: &str) -> Vec<Vec<u8>> {
        Vec::new()
    }
    let (r, seen, took) = resolve_against(never, "pool.ntp.org");
    assert_eq!(
        (r, seen),
        (Err(NetError::Dns(DnsError::Timeout { attempts: 2 })), 2)
    );
    assert!(took >= 2 * t && took < 3 * t, "{took}");
}

#[test]
fn crooked_answers_over_the_wire_are_errors() {
    fn truncated(_: usize, id: u16, host: &str) -> Vec<Vec<u8>> {
        let m = dns::tests::answer(id, 0, host, &[(dns::tests::AT_QNAME, 1, A)]);
        vec![m[..m.len() - 2].to_vec()]
    }
    fn only_aaaa(_: usize, id: u16, host: &str) -> Vec<Vec<u8>> {
        vec![dns::tests::answer(
            id,
            0,
            host,
            &[(dns::tests::AT_QNAME, 28, &[0; 16])],
        )]
    }
    fn nxdomain(_: usize, id: u16, host: &str) -> Vec<Vec<u8>> {
        vec![dns::tests::answer(id, 3, host, &[])]
    }
    for (f, want) in [
        (truncated as Answerer, DnsError::Truncated),
        (only_aaaa, DnsError::NoAnswer),
        (nxdomain, DnsError::NxDomain),
    ] {
        let (r, seen, _) = resolve_against(f, "example.com");
        // Een antwoord dat nee zegt, wordt niet nog eens gevraagd.
        assert_eq!((r, seen), (Err(NetError::Dns(want)), 1));
    }
}

#[test]
fn an_address_needs_no_server_and_a_name_without_server_says_so() {
    let p = pair();
    let got = slot();
    let app = p.app;
    p.exec
        .spawn(async move {
            let ip = app.resolve("10.0.2.2").await;
            let name = app.resolve("github.com").await;
            *got.borrow_mut() = Some((ip, name));
        })
        .unwrap();
    p.run_until(|| got.borrow().is_some());
    assert_eq!(
        got.borrow_mut().take(),
        Some((Ok([10, 0, 2, 2]), Err(NetError::Dns(DnsError::NoServer))))
    );
}
