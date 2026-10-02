//! De logische runnerverbinding bezit aanvragen; sockets zijn vervangbare handvatten.
use alloc::{collections::VecDeque, string::String, vec::Vec};
use d::protocol as p;
use spin_domain::{self as d, Map, Name, protocol::WireMessage, try_push, try_string};

/// De bestaande bovengrens van de controle-outbox.
pub const OUTBOX_LIMIT: usize = 8192;
/// Hoogstens zoveel onbeantwoorde aanvragen per runner.
pub const PENDING_LIMIT: usize = 8192;
/// Hoogstens zoveel processtreams per runner.
pub const STREAM_LIMIT: usize = 1024;
/// Een ander proces mag na negentig seconden zonder teken van leven overnemen.
pub const PONG_WAIT_MS: u64 = 90_000;

/// Fouten aan de eigendoms- en budgetgrens van een runner.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// Een ander levend proces draagt dezelfde runneridentiteit.
    IdentityInUse(Name),
    /// Een caller hergebruikte een nog open aanvraag-ID.
    DuplicateRequest(Name),
    /// Er is geen verbonden socket van deze generatie.
    Offline,
    /// Het vaste budget is bereikt.
    Full(&'static str),
    /// Een teller is uitgeput; IDs worden nooit hergebruikt.
    Exhausted,
    /// Geheugen of invoer is ongeldig.
    Data(d::Error),
}
impl From<d::Error> for Error {
    fn from(e: d::Error) -> Self {
        Self::Data(e)
    }
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::IdentityInUse(p) => write!(
                f,
                "runner identity is already connected from another process ({p})"
            ),
            Self::DuplicateRequest(id) => write!(f, "runner request {id} already pending"),
            Self::Offline => f.write_str("runner is not connected"),
            Self::Full(kind) => write!(f, "runner {kind} is full"),
            Self::Exhausted => f.write_str("runner generation or message sequence exhausted"),
            Self::Data(e) => e.fmt(f),
        }
    }
}
impl core::error::Error for Error {}
/// Het resultaat van een brokerbewerking.
pub type Result<T = ()> = core::result::Result<T, Error>;

/// Een bevestiging hoort bij precies één bericht op één fysieke verbinding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    generation: u64,
    sequence: u64,
}

enum Queued {
    Request(String),
    Transient(Vec<WireMessage>),
}
struct Entry {
    sequence: u64,
    message: Queued,
    bytes: usize,
}
struct Pending {
    message: WireMessage,
    bytes: usize,
}
struct Stream {
    bulk: bool,
}

/// Eén eigenaar beheert deze peer; nergens is gedeeld veranderbaar bezit.
pub struct Peer {
    client: d::Client,
    process: String,
    generation: u64,
    sequence: u64,
    connected: bool,
    last_seen_ms: u64,
    refused_until_ms: u64,
    outbox: VecDeque<Entry>,
    pending: Map<Pending>,
    streams: Map<Stream>,
    bytes: usize,
}
impl Peer {
    /// Neemt de duurzame runneridentiteit over, nog zonder socket.
    pub fn new(client: d::Client) -> Self {
        Self {
            client,
            process: String::new(),
            generation: 0,
            sequence: 0,
            connected: false,
            last_seen_ms: 0,
            refused_until_ms: 0,
            outbox: VecDeque::new(),
            pending: Map::new(),
            streams: Map::new(),
            bytes: 0,
        }
    }
    /// De identiteit blijft gelijk na het vervangen van een socket.
    pub fn client(&self) -> &d::Client {
        &self.client
    }
    /// Controleert de procesclaim vóór de duurzame registratie wordt gewijzigd.
    pub fn can_attach(&self, process: &str, now_ms: u64) -> Result {
        if self.connected
            && !self.process.is_empty()
            && !process.is_empty()
            && self.process != process
            && now_ms.saturating_sub(self.last_seen_ms) <= PONG_WAIT_MS
        {
            return Err(Error::IdentityInUse(Name::new(&self.process)));
        }
        Ok(())
    }
    /// Vernieuwt mogelijkheden en drainstatus na een bevestigde Store-mutatie.
    pub fn update_client(&mut self, client: d::Client) -> Result {
        if self.client.id != client.id {
            return Err(crate::validation::invalid("client", "runner identity changed").into());
        }
        self.client = client;
        Ok(())
    }
    /// Alleen de huidige fysieke verbinding mag lezen, schrijven of de liveness vernieuwen.
    pub fn owns(&self, generation: u64) -> bool {
        self.connected && self.generation == generation
    }
    /// Ook een draining runner kan een bestaande capsule nog bedienen.
    pub fn is_connected(&self) -> bool {
        self.connected
    }
    /// Of de runner voor een nieuwe capsule gekozen mag worden.
    pub fn is_available(&self, now_ms: u64) -> bool {
        self.connected
            && !self.client.draining
            && self.client.capabilities.engine.available
            && now_ms > self.refused_until_ms
    }
    /// Een weigering telt tot de pauze om is of er een capsule stopt.
    pub fn refuse_until(&mut self, until_ms: u64) {
        self.refused_until_ms = until_ms;
    }
    /// Een bevestigde stop maakt opnieuw plaats beschikbaar.
    pub fn freed(&mut self) {
        self.refused_until_ms = 0;
    }
    /// Een ping, pong of bericht houdt deze procesclaim levend.
    pub fn touch(&mut self, now_ms: u64) {
        self.last_seen_ms = now_ms;
    }

    /// Vervangt de socket en meldt welke geadopteerde streams verdwenen zijn.
    pub fn attach(
        &mut self,
        process: &str,
        running: Option<&[String]>,
        now_ms: u64,
    ) -> Result<(u64, Vec<String>)> {
        self.can_attach(process, now_ms)?;
        let generation = self.generation.checked_add(1).ok_or(Error::Exhausted)?;
        let process = try_string(process)?;
        let mut gone = Vec::new();
        if let Some(running) = running {
            for id in self.streams.keys() {
                if !self.pending.contains_key(id) && !running.iter().any(|s| s == id) {
                    try_push(&mut gone, try_string(id)?)?;
                }
            }
        }
        // Eerst alle faalbare replay-allocaties, daarna de nieuwe generatie publiceren.
        self.requeue_pending()?;
        self.generation = generation;
        self.process = process;
        self.connected = true;
        self.last_seen_ms = now_ms;
        for id in &gone {
            self.streams.remove(id);
        }
        Ok((generation, gone))
    }

    /// Een oude socket kan de nieuwere verbinding niet offline zetten.
    pub fn detach(&mut self, generation: u64) -> Result<bool> {
        if generation != self.generation {
            return Ok(false);
        }
        self.connected = false;
        self.requeue_pending()?;
        Ok(true)
    }

    fn requeue_pending(&mut self) -> Result {
        let mut replay = Vec::new();
        for id in self.pending.keys() {
            if !self
                .outbox
                .iter()
                .any(|e| matches!(&e.message, Queued::Request(queued) if queued == id))
            {
                try_push(&mut replay, try_string(id)?)?;
            }
        }
        if self.outbox.len() + replay.len() > OUTBOX_LIMIT {
            return Err(Error::Full("control outbox"));
        }
        let count = u64::try_from(replay.len()).map_err(|_| Error::Exhausted)?;
        self.sequence.checked_add(count).ok_or(Error::Exhausted)?;
        self.outbox
            .try_reserve(replay.len())
            .map_err(|_| d::Error::OutOfMemory)?;
        for id in replay {
            self.sequence += 1;
            self.outbox.push_back(Entry {
                sequence: self.sequence,
                message: Queued::Request(id),
                bytes: 0,
            });
        }
        Ok(())
    }

    fn reserve_message(&mut self) -> Result<u64> {
        if self.outbox.len() >= OUTBOX_LIMIT {
            return Err(Error::Full("control outbox"));
        }
        let sequence = self.sequence.checked_add(1).ok_or(Error::Exhausted)?;
        self.outbox
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        Ok(sequence)
    }
    /// Een aanvraag blijft eigendom van de peer tot antwoord of annulering.
    pub fn request(&mut self, message: WireMessage) -> Result {
        if message.id.is_empty() || message.r#type != p::MESSAGE_REQUEST {
            return Err(
                crate::validation::invalid("request", "request needs a non-empty id").into(),
            );
        }
        if self.pending.contains_key(&message.id) {
            return Err(Error::DuplicateRequest(Name::new(&message.id)));
        }
        if self.pending.len() >= PENDING_LIMIT {
            return Err(Error::Full("pending calls"));
        }
        let bytes = self.reserve_bytes(&message, 0)?;
        let sequence = self.reserve_message()?;
        let id = try_string(&message.id)?;
        self.pending
            .insert(try_string(&id)?, Pending { message, bytes })?;
        self.bytes += bytes;
        self.sequence = sequence;
        self.outbox.push_back(Entry {
            sequence,
            message: Queued::Request(id),
            bytes: 0,
        });
        Ok(())
    }
    /// Een stream- of controlebericht is eenmalig en heeft geen replay-call.
    pub fn enqueue(&mut self, message: WireMessage) -> Result {
        let bytes = self.reserve_bytes(&message, 0)?;
        // Een Vec met één element houdt 8192 outboxplaatsen klein en alloceert faalbaar.
        let mut owned = Vec::new();
        try_push(&mut owned, message)?;
        let sequence = self.reserve_message()?;
        self.sequence = sequence;
        self.bytes += bytes;
        self.outbox.push_back(Entry {
            sequence,
            message: Queued::Transient(owned),
            bytes,
        });
        Ok(())
    }
    /// De hello-bevestiging gaat altijd vóór opnieuw aangeboden aanvragen.
    pub fn prepend(&mut self, message: WireMessage) -> Result {
        let welcome = message.r#type == p::MESSAGE_WELCOME;
        let replacing: usize = if welcome {
            self.outbox.iter().filter(|entry| matches!(&entry.message, Queued::Transient(messages) if messages.first().is_some_and(|m| m.r#type == p::MESSAGE_WELCOME))).map(|entry| entry.bytes).sum()
        } else {
            0
        };
        let bytes = self.reserve_bytes(&message, replacing)?;
        let mut owned = Vec::new();
        try_push(&mut owned, message)?;
        let sequence = self.sequence.checked_add(1).ok_or(Error::Exhausted)?;
        if self.outbox.len() >= OUTBOX_LIMIT && replacing == 0 {
            return Err(Error::Full("control outbox"));
        }
        self.outbox
            .try_reserve(1)
            .map_err(|_| d::Error::OutOfMemory)?;
        // Een vorige socket kan vóór zijn welcome-write verdwenen zijn.
        // Zijn welkomstbericht mag niet ná dat van de nieuwe generatie volgen.
        if welcome {
            self.outbox.retain(|entry| !matches!(&entry.message, Queued::Transient(messages) if messages.first().is_some_and(|m| m.r#type == p::MESSAGE_WELCOME)));
        }
        self.sequence = sequence;
        self.bytes = self.bytes - replacing + bytes;
        self.outbox.push_front(Entry {
            sequence,
            message: Queued::Transient(owned),
            bytes,
        });
        Ok(())
    }
    /// Leent het eerste bericht tot de schrijver zijn ticket bevestigt.
    pub fn next(&self, generation: u64) -> Result<Option<(Ticket, &WireMessage)>> {
        if !self.connected || generation != self.generation {
            return Err(Error::Offline);
        }
        let Some(entry) = self.outbox.front() else {
            return Ok(None);
        };
        let message = match &entry.message {
            Queued::Request(id) => self
                .pending
                .get(id)
                .map(|pending| &pending.message)
                .ok_or_else(|| crate::validation::invalid(id, "queued request is missing"))?,
            Queued::Transient(message) => message
                .first()
                .ok_or_else(|| crate::validation::invalid("outbox", "missing transient message"))?,
        };
        Ok(Some((
            Ticket {
                generation,
                sequence: entry.sequence,
            },
            message,
        )))
    }
    /// Verwijdert alleen het bericht waarvoor de socket een bevestiging geeft.
    pub fn acknowledge(&mut self, ticket: Ticket) -> bool {
        if self.generation != ticket.generation
            || self
                .outbox
                .front()
                .is_none_or(|e| e.sequence != ticket.sequence)
        {
            return false;
        }
        if let Some(entry) = self.outbox.pop_front() {
            self.bytes -= entry.bytes;
        }
        true
    }
    /// Een antwoord draagt de aanvraag terug; de caller bezit de response zelf.
    pub fn response(&mut self, id: &str) -> Option<WireMessage> {
        let request = self.pending.remove(id)?;
        self.outbox
            .retain(|e| !matches!(&e.message, Queued::Request(queued) if queued == id));
        self.bytes -= request.bytes;
        Some(request.message)
    }
    /// Annuleert ook een nog niet verzonden aanvraag, en seint de runner in.
    pub fn cancel(&mut self, id: &str) -> Result<bool> {
        if !self.pending.contains_key(id) {
            return Ok(false);
        }
        let cancel = WireMessage {
            r#type: try_string(p::MESSAGE_CANCEL)?,
            id: try_string(id)?,
            ..Default::default()
        };
        self.enqueue(cancel)?;
        self.response(id);
        Ok(true)
    }
    /// Registreert een processstream, ook vóór de runner opnieuw verbindt.
    pub fn adopt_stream(&mut self, id: &str, bulk: bool) -> Result {
        if id.trim().is_empty() {
            return Err(crate::validation::invalid("stream", "stream id is required").into());
        }
        if self.streams.contains_key(id) {
            return Ok(());
        }
        if self.streams.len() >= STREAM_LIMIT {
            return Err(Error::Full("streams"));
        }
        self.streams.insert(try_string(id)?, Stream { bulk })?;
        Ok(())
    }
    /// Een eindbericht sluit het logische proces precies eenmaal.
    pub fn finish_stream(&mut self, id: &str) -> bool {
        self.streams.remove(id).is_some()
    }
    /// Eenmalige bulktransfers zijn na een linkbreuk niet hervatbaar.
    pub fn fail_bulk_streams(&mut self) -> Result<Vec<String>> {
        let mut gone = Vec::new();
        for (id, stream) in self.streams.iter() {
            if stream.bulk {
                try_push(&mut gone, try_string(id)?)?;
            }
        }
        for id in &gone {
            self.streams.remove(id);
        }
        Ok(gone)
    }
    /// Het aantal calls dat nog een antwoord verwacht.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
    fn reserve_bytes(&self, message: &WireMessage, replacing: usize) -> Result<usize> {
        let bytes = d::Wire::to_json(message)?.len();
        if bytes > p::MAX_MESSAGE_BYTES
            || bytes > (64_usize << 20).saturating_sub(self.bytes - replacing)
        {
            return Err(Error::Full("control byte budget"));
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn peer() -> Peer {
        Peer::new(d::Client::default())
    }
    #[test]
    fn byte_budget_keeps_pending_calls_charged_until_response_and_welcome_replaces_in_full_queue() {
        let mut peer = peer();
        let (generation, _) = peer.attach("process", None, 0).unwrap();
        for index in 0..7 {
            let mut request = call(&crate::validation::text(format_args!("{index}")).unwrap());
            request.error = "x".repeat(8 << 20);
            peer.request(request).unwrap();
            let ticket = peer.next(generation).unwrap().unwrap().0;
            peer.acknowledge(ticket);
        }
        let mut request = call("next");
        request.error = "x".repeat(8 << 20);
        assert!(matches!(
            peer.request(request),
            Err(Error::Full("control byte budget"))
        ));
        peer.response("0").unwrap();
        let mut request = call("next");
        request.error = "x".repeat(8 << 20);
        peer.request(request).unwrap();
        assert!(peer.bytes <= 64 << 20);

        let mut peer = Peer::new(d::Client::default());
        let welcome = || WireMessage {
            r#type: try_string(p::MESSAGE_WELCOME).unwrap(),
            ..Default::default()
        };
        peer.prepend(welcome()).unwrap();
        for _ in 1..OUTBOX_LIMIT {
            peer.enqueue(WireMessage::default()).unwrap();
        }
        let before = peer.bytes;
        peer.prepend(welcome()).unwrap();
        assert_eq!(peer.outbox.len(), OUTBOX_LIMIT);
        assert_eq!(peer.bytes, before);
    }
    fn call(id: &str) -> WireMessage {
        WireMessage {
            id: try_string(id).unwrap(),
            r#type: try_string(p::MESSAGE_REQUEST).unwrap(),
            method: try_string(p::METHOD_STOP).unwrap(),
            ..Default::default()
        }
    }
    #[test]
    fn reconnect_replays_same_id_and_fences_old_socket() {
        let mut peer = peer();
        let (first, _) = peer.attach("a", None, 0).unwrap();
        peer.request(call("server-1-call-1")).unwrap();
        let (ticket, message) = peer.next(first).unwrap().unwrap();
        assert_eq!(message.id, "server-1-call-1");
        assert!(peer.acknowledge(ticket));
        assert!(!peer.acknowledge(ticket));
        let (second, _) = peer.attach("a", None, 1).unwrap();
        assert!(!peer.detach(first).unwrap());
        assert!(peer.is_connected());
        assert_eq!(peer.next(second).unwrap().unwrap().1.id, "server-1-call-1");
        assert!(!peer.acknowledge(ticket));
        assert!(peer.response("server-1-call-1").is_some());
        assert!(peer.next(second).unwrap().is_none());
    }
    #[test]
    fn duplicate_identity_and_expiry() {
        let mut peer = peer();
        peer.attach("a", None, 5).unwrap();
        assert!(matches!(
            peer.attach("b", None, 6),
            Err(Error::IdentityInUse(_))
        ));
        peer.attach("a", None, 6).unwrap();
        assert!(peer.attach("b", None, 6 + PONG_WAIT_MS + 1).is_ok());
    }
    #[test]
    fn missing_streams_finish_but_pending_starts_survive() {
        let mut peer = peer();
        peer.adopt_stream("gone", false).unwrap();
        peer.adopt_stream("starting", false).unwrap();
        peer.adopt_stream("alive", false).unwrap();
        peer.request(call("starting")).unwrap();
        let (_, gone) = peer
            .attach("a", Some(&[try_string("alive").unwrap()]), 0)
            .unwrap();
        assert_eq!(gone, ["gone"]);
        assert!(peer.finish_stream("starting"));
        assert!(peer.finish_stream("alive"));
        assert!(!peer.finish_stream("gone"));
    }
    #[test]
    fn bulk_failure_and_cancel_are_bounded() {
        let mut peer = peer();
        peer.adopt_stream("bulk", true).unwrap();
        peer.adopt_stream("agent", false).unwrap();
        assert_eq!(peer.fail_bulk_streams().unwrap(), ["bulk"]);
        assert!(peer.finish_stream("agent"));
        let (generation, _) = peer.attach("a", None, 0).unwrap();
        peer.request(call("cancel")).unwrap();
        assert!(peer.cancel("cancel").unwrap());
        assert_eq!(peer.pending_count(), 0);
        assert_eq!(
            peer.next(generation).unwrap().unwrap().1.r#type,
            p::MESSAGE_CANCEL
        );
    }
}
