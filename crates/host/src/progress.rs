//! Voortgang van een lange runneropdracht (opslaan, archiveren, uploaden). Per
//! verzoek wacht alleen de laatste stand; de hoofdlus stuurt hem als event.
use spin_domain::{
    self as d, Wire,
    protocol::{self as p, WireMessage},
    try_string,
};
use std::cell::RefCell;

thread_local! {
    static WAITING: RefCell<Vec<WireMessage>> = const { RefCell::new(Vec::new()) };
}
/// Meldt namens één verzoek; `NONE` meldt niets.
#[derive(Clone, Copy)]
pub(crate) struct Progress<'a>(Option<&'a str>);
impl<'a> Progress<'a> {
    pub(crate) const NONE: Progress<'static> = Progress(None);
    pub(crate) fn of(request: &'a str) -> Self {
        Self(Some(request))
    }
    /// Best effort: zonder geheugen valt alleen deze melding weg.
    pub(crate) fn report(self, stage: &str, message: &str, current: u64, total: u64) {
        let Some(id) = self.0 else {
            return;
        };
        let Ok(event) = event(id, stage, message, current, total) else {
            return;
        };
        WAITING.with_borrow_mut(|waiting| {
            if let Some(old) = waiting.iter_mut().find(|m| m.id == id) {
                *old = event;
            } else if waiting.try_reserve(1).is_ok() {
                waiting.push(event);
            }
        });
    }
}
fn event(
    id: &str,
    stage: &str,
    message: &str,
    current: u64,
    total: u64,
) -> d::Fallible<WireMessage> {
    Ok(WireMessage {
        version: p::PROTOCOL_VERSION,
        r#type: try_string(p::MESSAGE_EVENT)?,
        method: try_string(p::METHOD_PROGRESS)?,
        id: try_string(id)?,
        payload: d::RawJson(Some(
            d::SealStatus {
                stage: try_string(stage)?,
                message: try_string(message)?,
                current: i64::try_from(current).unwrap_or(i64::MAX),
                total: i64::try_from(total).unwrap_or(i64::MAX),
                ..Default::default()
            }
            .to_value()?,
        )),
        ..Default::default()
    })
}
/// Geeft de wachtende meldingen op volgorde aan `send`; wat niet weg kan, blijft wachten.
pub(crate) fn flush(mut send: impl FnMut(&WireMessage) -> d::Fallible<bool>) -> d::Fallible {
    WAITING.with_borrow_mut(|waiting| {
        while let Some(first) = waiting.first() {
            if !send(first)? {
                break;
            }
            waiting.remove(0);
        }
        Ok(())
    })
}
