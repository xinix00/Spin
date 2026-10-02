//! Begrensde SCSI-payloads, bytegelijk aan Go `hopabi/device.go`.
//! Een transportbevestiging kan een SCSI CHECK CONDITION dragen.

/// Commandokop, gevolgd door uitsluitend data-uit.
pub const COMMAND_LEN: usize = 32;
/// Resultaatkop met 64 sensebytes, gevolgd door uitsluitend data-in.
pub const RESULT_LEN: usize = 72;
/// Vijf minuten, inclusief opspinnen.
pub const MAX_TIMEOUT_MS: u32 = 300_000;

/// Ongeldige of afgebroken draadvorm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Invalid;

/// Eén opdracht. De geleende velden blijven bij de eigenaar van de RPC.
#[derive(Debug, Clone, Copy)]
pub struct Command<'a> {
    /// Eén tot zestien commandobytes.
    pub cdb: &'a [u8],
    /// Deadline voor de gehele uitwisseling.
    pub timeout_ms: u32,
    /// Maximale inkomende data; nul bij schrijven.
    pub in_len: u32,
    /// Uitgaande data; leeg bij lezen.
    pub data_out: &'a [u8],
}
impl<'a> Command<'a> {
    fn valid(&self, limit: usize) -> bool {
        (1..=16).contains(&self.cdb.len())
            && (1..=MAX_TIMEOUT_MS).contains(&self.timeout_ms)
            && (self.in_len == 0 || self.data_out.is_empty())
            && self.data_out.len() <= u32::MAX as usize
            && COMMAND_LEN
                .checked_add(self.data_out.len())
                .is_some_and(|n| n <= limit)
            && RESULT_LEN
                .checked_add(self.in_len as usize)
                .is_some_and(|n| n <= limit)
    }
    /// Schrijft zonder allocatie; `limit` geldt ook voor het antwoord.
    pub fn encode(&self, dst: &mut [u8], limit: usize) -> Result<usize, Invalid> {
        if !self.valid(limit) {
            return Err(Invalid);
        }
        let n = COMMAND_LEN + self.data_out.len();
        let dst = dst.get_mut(..n).ok_or(Invalid)?;
        dst[..COMMAND_LEN].fill(0);
        dst[0] = 1;
        dst[1] = self.cdb.len() as u8;
        dst[4..8].copy_from_slice(&self.timeout_ms.to_le_bytes());
        dst[8..12].copy_from_slice(&self.in_len.to_le_bytes());
        dst[12..16].copy_from_slice(&(self.data_out.len() as u32).to_le_bytes());
        dst[16..16 + self.cdb.len()].copy_from_slice(self.cdb);
        dst[COMMAND_LEN..].copy_from_slice(self.data_out);
        Ok(n)
    }
    /// Toetst beide richtingen vóór USB een opdracht krijgt.
    pub fn decode(b: &'a [u8], limit: usize) -> Result<Self, Invalid> {
        if b.len() < COMMAND_LEN
            || b.len() > limit
            || b[0] != 1
            || !(1..=16).contains(&b[1])
            || b[2..4] != [0, 0]
        {
            return Err(Invalid);
        }
        let c = Self {
            cdb: &b[16..16 + usize::from(b[1])],
            timeout_ms: word(b, 4),
            in_len: word(b, 8),
            data_out: &b[COMMAND_LEN..],
        };
        if !c.valid(limit) || word(b, 12) as usize != c.data_out.len() {
            return Err(Invalid);
        }
        Ok(c)
    }
}
/// Apparaatstatus, sense en teruggelezen bytes.
#[derive(Debug, Clone, Copy)]
pub struct Reply<'a> {
    /// SCSI-status: nul is succes, twee is CHECK CONDITION.
    pub status: u8,
    /// Daadwerkelijk overgedragen databytes in de gevraagde richting.
    pub transferred: u32,
    /// Maximaal 64 ruwe sensebytes.
    pub sense: &'a [u8],
    /// Alleen aanwezig bij een leesopdracht.
    pub data: &'a [u8],
}
impl<'a> Reply<'a> {
    /// Leest strikt binnen de oorspronkelijke opdrachtgrenzen.
    pub fn decode(b: &'a [u8], in_len: usize, out_len: usize) -> Result<Self, Invalid> {
        if b.len() < RESULT_LEN || b[1] > 64 || b[2..4] != [0, 0] || (in_len != 0 && out_len != 0) {
            return Err(Invalid);
        }
        let transferred = word(b, 4);
        let data = &b[RESULT_LEN..];
        if transferred as usize > in_len.max(out_len)
            || (in_len == 0 && !data.is_empty())
            || (in_len != 0 && data.len() != transferred as usize)
        {
            return Err(Invalid);
        }
        Ok(Self {
            status: b[0],
            transferred,
            sense: &b[8..8 + usize::from(b[1])],
            data,
        })
    }
    /// Schrijft het antwoord zonder ongebruikte sensebytes prijs te geven.
    pub fn encode(&self, dst: &mut [u8]) -> Result<usize, Invalid> {
        if self.sense.len() > 64
            || (!self.data.is_empty() && self.data.len() != self.transferred as usize)
        {
            return Err(Invalid);
        }
        let n = RESULT_LEN.checked_add(self.data.len()).ok_or(Invalid)?;
        let dst = dst.get_mut(..n).ok_or(Invalid)?;
        dst[..RESULT_LEN].fill(0);
        dst[0] = self.status;
        dst[1] = self.sense.len() as u8;
        dst[4..8].copy_from_slice(&self.transferred.to_le_bytes());
        dst[8..8 + self.sense.len()].copy_from_slice(self.sense);
        dst[RESULT_LEN..].copy_from_slice(self.data);
        Ok(n)
    }
}
fn word(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
