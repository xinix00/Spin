//! Eén eigenaar bepaalt de volgorde van browser- en runneruploads.
//!
//! Een I/O-taak krijgt een ticket. Alleen zijn bevestiging schuift de duurzame
//! prefix op; een timeout of korte write geeft het ticket als fout terug.
use alloc::vec::Vec;

/// De bestaande maximale afstand vóór de aaneengesloten prefix.
pub const REORDER_WINDOW: u64 = 64 << 20;
/// Een eigenaar accepteert nooit een onbegrensd aantal gelijktijdige writes.
pub const MAX_PENDING: usize = 64;
/// Afgeronde, nog niet aaneengesloten spans blijven eveneens begrensd.
pub const MAX_AHEAD: usize = 128;
/// De fout bevat bij verkeerd geplaatste chunks het hervatpunt.
#[derive(Debug, PartialEq)]
pub enum Error {
    /// De levensduur is afgesloten.
    Closed,
    /// Lengte nul, overflow of bytes buiten de gedeclareerde omvang.
    Size,
    /// Een chunk kruist de prefix of landt buiten het reorder-window.
    Offset {
        /// De bevestigde prefix.
        committed: u64,
    },
    /// Het budget voor werk of afgeronde spans is vol.
    Full,
    /// Het ticket hoort niet bij een nog open write.
    Ticket,
    /// Nog onvoltooid werk of een ontbrekende byte.
    Incomplete {
        /// Het eerst ontbrekende byte.
        committed: u64,
    },
    /// Een buffer kon niet faalbaar worden gereserveerd.
    Memory,
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Closed => f.write_str("upload is closed"),
            Self::Size => f.write_str("chunk exceeds upload or is empty"),
            Self::Offset { committed } => {
                write!(f, "upload offset mismatch: committed={committed}")
            }
            Self::Full => f.write_str("upload queue is full"),
            Self::Ticket => f.write_str("stale upload ticket"),
            Self::Incomplete { committed } => write!(f, "upload incomplete: committed={committed}"),
            Self::Memory => f.write_str("upload allocation failed"),
        }
    }
}
impl core::error::Error for Error {}
#[derive(Clone, Copy, Debug, PartialEq)]
struct Span {
    start: u64,
    end: u64,
}
/// De I/O-taak bezit dit bewijs; het kan niet worden gekloond of zelf gemaakt.
#[derive(Debug, PartialEq)]
pub struct Ticket {
    id: u64,
    span: Span,
}
impl Ticket {
    /// Het bestandsoffset dat de taak moet schrijven.
    pub fn offset(&self) -> u64 {
        self.span.start
    }
    /// Exact dit aantal bytes moet succesvol zijn geschreven.
    pub fn length(&self) -> u64 {
        self.span.end - self.span.start
    }
}
/// Een volledig bevestigde retry doet geen tweede write.
#[derive(Debug, PartialEq)]
pub enum Prepared {
    /// De client kan direct vanaf deze prefix hervatten.
    Committed(u64),
    /// Eerst I/O uitvoeren en dan het ticket teruggeven aan de eigenaar.
    Write(Ticket),
}
/// De begrensde assembler deelt geen mutex of buffer met zijn I/O-taken.
pub struct Assembler {
    size: u64,
    offset: u64,
    sequence: u64,
    ahead: Vec<Span>,
    pending: Vec<(u64, Span)>,
    closed: bool,
}
impl Assembler {
    /// Maakt een lege upload met een vaste bovengrens.
    pub const fn new(size: u64) -> Self {
        Self {
            size,
            offset: 0,
            sequence: 0,
            ahead: Vec::new(),
            pending: Vec::new(),
            closed: false,
        }
    }
    /// Het eerste byte dat nog niet aaneengesloten is bevestigd.
    pub const fn offset(&self) -> u64 {
        self.offset
    }
    /// Controleert een write en reserveert boekhoudruimte vóór externe I/O.
    pub fn prepare(&mut self, offset: u64, length: u64) -> core::result::Result<Prepared, Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= self.size)
            .ok_or(Error::Size)?;
        if length == 0 {
            return Err(Error::Size);
        }
        if end <= self.offset {
            return Ok(Prepared::Committed(self.offset));
        }
        if offset < self.offset || offset - self.offset > REORDER_WINDOW {
            return Err(Error::Offset {
                committed: self.offset,
            });
        }
        if self.pending.len() >= MAX_PENDING || self.ahead.len() + self.pending.len() >= MAX_AHEAD {
            return Err(Error::Full);
        }
        let sequence = self.sequence.checked_add(1).ok_or(Error::Full)?;
        self.ahead
            .try_reserve(self.pending.len() + 1)
            .map_err(|_| Error::Memory)?;
        self.pending.try_reserve(1).map_err(|_| Error::Memory)?;
        let span = Span { start: offset, end };
        self.pending.push((sequence, span));
        self.sequence = sequence;
        Ok(Prepared::Write(Ticket { id: sequence, span }))
    }
    /// Verwerkt een I/O-antwoord; alleen een volledige write telt mee.
    pub fn complete(
        &mut self,
        ticket: Ticket,
        written: u64,
        success: bool,
    ) -> core::result::Result<u64, Error> {
        let index = self
            .pending
            .iter()
            .position(|(id, span)| *id == ticket.id && *span == ticket.span)
            .ok_or(Error::Ticket)?;
        self.pending.swap_remove(index);
        if self.closed {
            return Err(Error::Closed);
        }
        if !success || written != ticket.length() {
            return Ok(self.offset);
        }
        // prepare reserveerde één plaats voor ieder uitstaand ticket.
        self.ahead.push(ticket.span);
        self.ahead.sort_unstable_by_key(|s| s.start);
        let mut consumed = 0;
        for span in &self.ahead {
            if span.start > self.offset {
                break;
            }
            self.offset = self.offset.max(span.end);
            consumed += 1;
        }
        self.ahead.drain(..consumed);
        Ok(self.offset)
    }
    /// Publicatie kan alleen na alle writes en na iedere gedeclareerde byte.
    pub fn finish(&mut self) -> core::result::Result<(), Error> {
        if self.closed {
            return Err(Error::Closed);
        }
        if !self.pending.is_empty() || self.offset != self.size {
            return Err(Error::Incomplete {
                committed: self.offset,
            });
        }
        self.closed = true;
        Ok(())
    }
    /// Sluit de levensduur; reeds onderweg zijnde antwoorden kunnen niets publiceren.
    pub fn close(&mut self) -> bool {
        let was_open = !self.closed;
        self.closed = true;
        was_open
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ticket(a: &mut Assembler, start: u64, n: u64) -> Ticket {
        match a.prepare(start, n).unwrap() {
            Prepared::Write(t) => t,
            _ => panic!("expected write"),
        }
    }
    #[test]
    fn reordering_retry_and_failed_io_never_skip_a_hole() {
        let mut a = Assembler::new(12);
        let second = ticket(&mut a, 4, 4);
        let third = ticket(&mut a, 8, 4);
        let first = ticket(&mut a, 0, 4);
        assert_eq!(a.complete(third, 4, true).unwrap(), 0);
        assert_eq!(a.complete(second, 4, true).unwrap(), 0);
        assert_eq!(a.complete(first, 3, true).unwrap(), 0);
        assert_eq!(a.finish(), Err(Error::Incomplete { committed: 0 }));
        let first = ticket(&mut a, 0, 4);
        assert_eq!(a.complete(first, 4, true).unwrap(), 12);
        assert_eq!(a.prepare(0, 4).unwrap(), Prepared::Committed(12));
        a.finish().unwrap();
        assert_eq!(a.prepare(0, 4), Err(Error::Closed));
    }
    #[test]
    fn close_fences_pending_work_and_finish_waits_for_duplicate_writes() {
        let mut a = Assembler::new(4);
        let one = ticket(&mut a, 0, 4);
        let two = ticket(&mut a, 0, 4);
        a.complete(one, 4, true).unwrap();
        assert_eq!(a.finish(), Err(Error::Incomplete { committed: 4 }));
        assert!(a.close());
        assert!(!a.close());
        assert_eq!(a.complete(two, 4, true), Err(Error::Closed));
    }
    #[test]
    fn size_window_and_queue_limits_are_checked_before_io() {
        let mut a = Assembler::new(u64::MAX);
        assert_eq!(a.prepare(u64::MAX, 1), Err(Error::Size));
        assert_eq!(
            a.prepare(REORDER_WINDOW + 1, 1),
            Err(Error::Offset { committed: 0 })
        );
        let first = ticket(&mut a, 0, 4);
        a.complete(first, 4, true).unwrap();
        assert_eq!(a.prepare(3, 4), Err(Error::Offset { committed: 4 }));
        for _ in 0..MAX_PENDING {
            let _ = ticket(&mut a, 4, 1);
        }
        assert_eq!(a.prepare(4, 1), Err(Error::Full));
    }
}
