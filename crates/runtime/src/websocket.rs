//! Browserframes worden door de sockettaak verstuurd; de actor levert snapshots.
use crate::{Clock, Frame, Mail, response_head};
use alloc::string::String;
use core::{task::Poll, time::Duration};
use leanhttp::{AsyncRead, AsyncWrite};
use spin_core::websocket::{self as ws, Decoder, Event, Role};
fn error(_: ws::Error) -> leanhttp::Error {
    leanhttp::Error::Io(leanhttp::IoError::Other)
}
pub(crate) fn handshake(request: &leanhttp::Request) -> leanhttp::Result<String> {
    let token = |name: &str, wanted: &str| {
        request.header.get(name).is_some_and(|value| {
            value
                .split(',')
                .any(|v| v.trim().eq_ignore_ascii_case(wanted))
        })
    };
    if request.method != "GET"
        || !token("Connection", "Upgrade")
        || !token("Upgrade", "websocket")
        || request.header.get("Sec-WebSocket-Version") != Some("13")
    {
        return Err(leanhttp::Error::Io(leanhttp::IoError::Other));
    }
    ws::accept(request.header.get("Sec-WebSocket-Key").unwrap_or("")).map_err(error)
}
enum Wake {
    Read(usize),
    Frame(Frame),
    Ping,
    Close(u16),
}
pub(crate) async fn serve<C: leanhttp::Conn>(
    exchange: &mut leanhttp::Exchange<'_, C>,
    mail: &Mail,
    index: usize,
    accept: &str,
    runner: bool,
    browser: bool,
    clock: impl Clock,
) -> leanhttp::Result {
    let mut raw = exchange.hijack()?;
    raw.set_read_timeout(None)?;
    raw.set_write_timeout(Some(Duration::from_secs(20)))?;
    let mut response =
        spin_server::Response::empty(101).map_err(|_| leanhttp::Error::Alloc { bytes: 1 })?;
    response
        .header("Upgrade", "websocket")
        .map_err(|_| leanhttp::Error::Alloc { bytes: 1 })?;
    response
        .header("Sec-WebSocket-Accept", accept)
        .map_err(|_| leanhttp::Error::Alloc { bytes: 1 })?;
    leanhttp::write_all(&mut raw, response_head(&response)?.as_bytes()).await?;
    leanhttp::flush(&mut raw).await?;
    let mut decoder = Decoder::new(
        Role::Server,
        if runner {
            spin_domain::protocol::MAX_MESSAGE_BYTES
        } else {
            1 << 20
        },
    );
    let mut read = [0; 8192];
    let mut ping_at = clock.millis();
    let mut pong_at = clock.millis();
    loop {
        let wake = core::future::poll_fn(|cx| {
            if let Some(code) = mail.slots.0.borrow_mut()[index].close.take() {
                return Poll::Ready(Ok(Wake::Close(code)));
            }
            if clock.millis().saturating_sub(pong_at) > 90_000 {
                return Poll::Ready(Err(leanhttp::IoError::TimedOut));
            }
            if clock.millis().saturating_sub(ping_at) > 30_000 {
                return Poll::Ready(Ok(Wake::Ping));
            }
            if let Some(frame) = mail.slots.0.borrow_mut()[index].frame.take() {
                return Poll::Ready(Ok(Wake::Frame(frame)));
            }
            match raw.poll_read(cx, &mut read) {
                Poll::Ready(Ok(n)) => Poll::Ready(Ok(Wake::Read(n))),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Pending => Poll::Pending,
            }
        })
        .await?;
        // Een ingestelde timeout is een absolute deadline voor de schrijffase.
        // Vernieuw ook vóór een antwoord op een late ping of close van de peer.
        raw.set_write_timeout(Some(Duration::from_secs(20)))?;
        match wake {
            Wake::Read(0) => return Ok(()),
            Wake::Read(n) => {
                if runner {
                    pong_at = clock.millis();
                }
                let mut offset = 0;
                while offset < n {
                    let count = decoder
                        .push(read.get(offset..n).ok_or(leanhttp::Error::UnexpectedEof)?)
                        .map_err(error)?;
                    offset += count;
                    let mut progress = false;
                    while let Some(event) = decoder.next_event().map_err(error)? {
                        progress = true;
                        match event {
                            Event::Pong(_) => {
                                pong_at = clock.millis();
                                mail.slots.0.borrow_mut()[index].touched = true;
                            }
                            Event::Ping(payload) => {
                                mail.slots.0.borrow_mut()[index].touched = true;
                                if runner {
                                    pong_at = clock.millis();
                                }
                                leanhttp::write_all(
                                    &mut raw,
                                    &ws::encode(10, &payload, None).map_err(error)?,
                                )
                                .await?;
                                leanhttp::flush(&mut raw).await?;
                            }
                            Event::Close(payload) => {
                                leanhttp::write_all(
                                    &mut raw,
                                    &ws::encode(8, &payload, None).map_err(error)?,
                                )
                                .await?;
                                leanhttp::flush(&mut raw).await?;
                                return Ok(());
                            }
                            Event::Text(text) if runner => {
                                deliver(mail, index, text.as_bytes()).await?
                            }
                            Event::Binary(bytes) if runner => deliver(mail, index, &bytes).await?,
                            Event::Text(text) if browser => {
                                deliver_browser(mail, index, text.as_bytes()).await?
                            }
                            Event::Binary(bytes) if browser => {
                                deliver_browser(mail, index, &bytes).await?
                            }
                            Event::Text(_) | Event::Binary(_) => {}
                        }
                    }
                    if count == 0 && !progress {
                        return Err(leanhttp::Error::Io(leanhttp::IoError::Other));
                    }
                }
            }
            Wake::Frame(frame) => {
                raw.set_write_timeout(Some(Duration::from_secs(20)))?;
                leanhttp::write_all(&mut raw, &frame.bytes).await?;
                leanhttp::flush(&mut raw).await?;
                let mut slots = mail.slots.0.borrow_mut();
                slots[index].frame_bytes = 0;
                slots[index].acknowledged = frame.ticket;
            }
            Wake::Ping => {
                raw.set_write_timeout(Some(Duration::from_secs(20)))?;
                leanhttp::write_all(&mut raw, &ws::encode(9, &[], None).map_err(error)?).await?;
                leanhttp::flush(&mut raw).await?;
                ping_at = clock.millis();
            }
            Wake::Close(code) => {
                leanhttp::write_all(
                    &mut raw,
                    &ws::encode(8, &code.to_be_bytes(), None).map_err(error)?,
                )
                .await?;
                leanhttp::flush(&mut raw).await?;
                return Ok(());
            }
        }
    }
}

async fn deliver_browser(mail: &Mail, index: usize, data: &[u8]) -> leanhttp::Result {
    let message = spin_domain::json::parse(data)
        .map_err(|_| leanhttp::Error::Io(leanhttp::IoError::Other))?;
    core::future::poll_fn(|_| {
        let slots = mail.slots.0.borrow();
        if slots[index].close.is_some() {
            return Poll::Ready(Err(leanhttp::Error::Io(leanhttp::IoError::Closed)));
        }
        if slots[index].browser.is_none() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    })
    .await?;
    mail.slots.0.borrow_mut()[index].browser = Some(message);
    Ok(())
}

async fn deliver(mail: &Mail, index: usize, data: &[u8]) -> leanhttp::Result {
    let message = spin_domain::protocol::WireMessage::decode(data)
        .map_err(|_| leanhttp::Error::Io(leanhttp::IoError::Other))?;
    // Hoogstens één bericht wacht op de eigenaar. Terugdruk stopt verdere reads
    // zonder een RefCell-lening over await; intrekking verbreekt ook deze wacht.
    core::future::poll_fn(|_| {
        let slots = mail.slots.0.borrow();
        if slots[index].close.is_some() {
            return Poll::Ready(Err(leanhttp::Error::Io(leanhttp::IoError::Closed)));
        }
        if slots[index].incoming.is_none() {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    })
    .await?;
    mail.slots.0.borrow_mut()[index].incoming = Some(message);
    Ok(())
}
