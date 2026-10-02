//! De runner heeft één eigenaar. Een reconnect herhaalt antwoorden, nooit lopend werk.
use crate::validation::invalid;
use alloc::{collections::VecDeque, string::String, vec::Vec};
use spin_domain::{
    self as d, TryClone, Wire,
    protocol::{self as p, WireMessage},
    try_push, try_string,
};
/// Maximaal zoveel processen/operaties zijn tegelijk van de runner.
pub const OPERATIONS: usize = 64;
/// Het bestaande antwoordgeheugen bewaart de laatste 256 voltooide verzoeken.
pub const CACHED_RESPONSES: usize = 256;
/// Ook grote antwoorden en streamfragmenten hebben samen een harde heapgrens.
pub const BYTE_BUDGET: usize = 64 << 20;
const OUTBOX: usize = 2048;
struct Cached {
    id: String,
    bytes: Vec<u8>,
}
struct Outbound {
    ticket: u64,
    bytes: Vec<u8>,
    stream: Option<String>,
}
struct Operation {
    id: String,
    cancelled: bool,
}
/// Beslissing vóór de host een proces mag starten.
#[derive(Debug, PartialEq)]
pub enum Admission {
    /// De aanvraag is geregistreerd en mag eenmaal uitgevoerd worden.
    Start,
    /// Hetzelfde verzoek loopt al; zijn antwoord volgt via de bestaande operatie.
    InFlight,
    /// Het antwoord is opnieuw in de outbox gezet.
    Replayed,
    /// De begrensde eigenaar heeft geen plek; de afzender kan later opnieuw vragen.
    Busy,
}
/// Antwoordbytes blijven bij de eigenaar totdat de socket hun ticket bevestigt.
pub struct Worker {
    operations: Vec<Operation>,
    cache: VecDeque<Cached>,
    outbox: VecDeque<Outbound>,
    bytes: usize,
    sequence: u64,
}
impl Default for Worker {
    fn default() -> Self {
        Self::new()
    }
}
impl Worker {
    /// Lege eigenaar; ieder later groeipad reserveert faalbaar.
    pub fn new() -> Self {
        Self {
            operations: Vec::new(),
            cache: VecDeque::new(),
            outbox: VecDeque::new(),
            bytes: 0,
            sequence: 0,
        }
    }
    /// Registreert vóór uitvoering. Een herhaald ID krijgt nooit een tweede proces.
    pub fn begin(&mut self, request: &WireMessage) -> d::Fallible<Admission> {
        if request.r#type != p::MESSAGE_REQUEST
            || request.id.is_empty()
            || request.id.len() > 1024
            || request.method.len() > 256
        {
            return Err(invalid("request", "invalid runner request"));
        }
        if self
            .operations
            .iter()
            .any(|operation| operation.id == request.id)
        {
            return Ok(Admission::InFlight);
        }
        if let Some(cached) = self.cache.iter().find(|cached| cached.id == request.id) {
            if self
                .outbox
                .iter()
                .any(|queued| queued.bytes == cached.bytes)
            {
                return Ok(Admission::Replayed);
            }
            if self.outbox.len() >= OUTBOX
                || cached.bytes.len() > BYTE_BUDGET.saturating_sub(self.bytes)
            {
                return Ok(Admission::Busy);
            }
            self.enqueue_bytes(cached.bytes.try_clone()?, None)?;
            return Ok(Admission::Replayed);
        }
        if self.operations.len() >= OPERATIONS
            || self.outbox.len() >= OUTBOX
            || self.bytes >= BYTE_BUDGET
        {
            return Ok(Admission::Busy);
        }
        try_push(
            &mut self.operations,
            Operation {
                id: try_string(&request.id)?,
                cancelled: false,
            },
        )?;
        Ok(Admission::Start)
    }
    /// Annuleert de eigenaarstaak; het ID blijft bezet tot die taak gestopt is.
    pub fn cancel(&mut self, id: &str) -> bool {
        match self
            .operations
            .iter_mut()
            .find(|operation| operation.id == id)
        {
            Some(operation) => {
                operation.cancelled = true;
                true
            }
            None => false,
        }
    }
    /// De host laat de bijbehorende future vallen en stopt zo zijn eigen proces.
    pub fn cancelled(&self, id: &str) -> bool {
        self.operations
            .iter()
            .any(|operation| operation.id == id && operation.cancelled)
    }
    /// Alleen vóór de eerste poll: een mislukte hostallocatie heeft geen werk gestart.
    pub fn release_unstarted(&mut self, id: &str) {
        if let Some(index) = self
            .operations
            .iter()
            .position(|operation| operation.id == id)
        {
            self.operations.remove(index);
        }
    }
    /// Publiceert en cachet samen; terugdruk laat de operatie en haar antwoord intact.
    pub fn finish(&mut self, response: &WireMessage) -> d::Fallible<bool> {
        let index = self
            .operations
            .iter()
            .position(|operation| operation.id == response.id)
            .ok_or_else(|| invalid("response", "unknown runner operation"))?;
        if response.r#type != p::MESSAGE_RESPONSE {
            return Err(invalid("response", "invalid runner response"));
        }
        let bytes = response.to_json()?.into_bytes();
        if bytes.len() > p::MAX_MESSAGE_BYTES {
            return Err(invalid("response", "runner response exceeds wire limit"));
        }
        // Cache en socket hebben ieder eigen eigendom; beide kopieën tellen mee.
        if self.outbox.len() >= OUTBOX {
            return Ok(false);
        }
        while self.cache.len() >= CACHED_RESPONSES
            || bytes.len().saturating_mul(2) > BYTE_BUDGET.saturating_sub(self.bytes)
        {
            let Some(old) = self.cache.pop_front() else {
                return Ok(false);
            };
            self.bytes -= old.bytes.len();
        }
        self.cache
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        let cached = Cached {
            id: response.id.try_clone()?,
            bytes: bytes.try_clone()?,
        };
        self.enqueue_bytes(bytes, None)?;
        self.bytes += cached.bytes.len();
        self.cache.push_back(cached);
        self.operations.remove(index);
        Ok(true)
    }
    /// Stream- en lifecycleberichten volgen dezelfde FIFO en hetzelfde bytebudget.
    pub fn enqueue(&mut self, message: &WireMessage) -> d::Fallible<bool> {
        let bytes = message.to_json()?.into_bytes();
        if bytes.len() > p::MAX_MESSAGE_BYTES {
            return Err(invalid("message", "runner message exceeds wire limit"));
        }
        if self.outbox.len() >= OUTBOX {
            return Ok(false);
        }
        while bytes.len() > BYTE_BUDGET.saturating_sub(self.bytes) {
            let Some(old) = self.cache.pop_front() else {
                return Ok(false);
            };
            self.bytes -= old.bytes.len();
        }
        let stream = if matches!(
            message.r#type.as_str(),
            p::MESSAGE_STREAM_DATA | p::MESSAGE_STREAM_EXIT
        ) {
            Some(message.id.try_clone()?)
        } else {
            None
        };
        self.enqueue_bytes(bytes, stream)?;
        Ok(true)
    }
    fn enqueue_bytes(&mut self, bytes: Vec<u8>, stream: Option<String>) -> d::Fallible {
        let ticket = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("ticket", "runner ticket exhausted"))?;
        self.outbox
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        self.sequence = ticket;
        self.bytes += bytes.len();
        self.outbox.push_back(Outbound {
            ticket,
            bytes,
            stream,
        });
        Ok(())
    }
    /// De socket krijgt de oudste bytes te leen; geen remove vóór een geslaagde write.
    pub fn outbound(&self) -> Option<(u64, &[u8])> {
        self.outbox
            .front()
            .map(|message| (message.ticket, message.bytes.as_slice()))
    }
    /// Een oud socketticket kan een nieuwer bericht nooit wissen.
    pub fn acknowledge(&mut self, ticket: u64) -> bool {
        if self
            .outbox
            .front()
            .is_none_or(|message| message.ticket != ticket)
        {
            return false;
        }
        if let Some(message) = self.outbox.pop_front() {
            self.bytes -= message.bytes.len();
        }
        true
    }
    /// Procesvrij is afzonderlijk van nog te versturen antwoorden.
    pub fn active(&self) -> usize {
        self.operations.len()
    }
    /// Ook een afgelopen stream blijft gerapporteerd totdat zijn laatste bytes zijn verstuurd.
    pub fn append_stream_ids(&self, ids: &mut d::List<String>) -> d::Fallible {
        for message in &self.outbox {
            if let Some(id) = &message.stream
                && !ids.contains(id)
            {
                ids.push(id.try_clone()?)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn message(kind: &str, id: &str) -> WireMessage {
        WireMessage {
            r#type: try_string(kind).unwrap(),
            id: try_string(id).unwrap(),
            ..Default::default()
        }
    }
    #[test]
    fn reconnect_replays_completed_response_and_fences_old_ack() {
        let mut owner = Worker::new();
        let request = message(p::MESSAGE_REQUEST, "req-1");
        assert_eq!(owner.begin(&request).unwrap(), Admission::Start);
        assert_eq!(owner.begin(&request).unwrap(), Admission::InFlight);
        assert!(
            owner
                .finish(&message(p::MESSAGE_RESPONSE, "req-1"))
                .unwrap()
        );
        let old = owner.outbound().unwrap().0;
        assert_eq!(owner.begin(&request).unwrap(), Admission::Replayed);
        assert!(owner.acknowledge(old));
        assert_eq!(owner.begin(&request).unwrap(), Admission::Replayed);
        assert!(!owner.acknowledge(old));
        assert!(owner.outbound().is_some());
        assert_eq!(owner.active(), 0);
    }
    #[test]
    fn cancellation_keeps_request_owned_until_completion_and_capacity_is_bounded() {
        let mut owner = Worker::new();
        let request = message(p::MESSAGE_REQUEST, "req-1");
        assert_eq!(owner.begin(&request).unwrap(), Admission::Start);
        assert!(owner.cancel("req-1"));
        assert!(owner.cancelled("req-1"));
        assert_eq!(owner.begin(&request).unwrap(), Admission::InFlight);
        for i in 1..OPERATIONS {
            owner
                .begin(&message(
                    p::MESSAGE_REQUEST,
                    &crate::validation::text(format_args!("job-{i}")).unwrap(),
                ))
                .unwrap();
        }
        assert_eq!(
            owner.begin(&message(p::MESSAGE_REQUEST, "extra")).unwrap(),
            Admission::Busy
        );
        assert!(
            owner
                .finish(&message(p::MESSAGE_RESPONSE, "req-1"))
                .unwrap()
        );
        assert_eq!(
            owner.begin(&message(p::MESSAGE_REQUEST, "extra")).unwrap(),
            Admission::Start
        );
    }
}
