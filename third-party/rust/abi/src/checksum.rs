//! De content-som die de kern én de apps over hetzelfde bestand rekenen.
//!
//! Eén bron van waarheid voor de algoritmekeuze (FNV-1a, 64 bit): zo kunnen
//! app-kant en kern-kant nooit uit elkaar lopen op een handgetypte variant.
//! Geen cryptografische som: hij vangt een afgekapte of verschoven kopie,
//! geen tegenstander.

/// De beginwaarde van FNV-1a-64.
const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// De priem van FNV-64.
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// Een lopende FNV-1a-64-som, voor een bestand dat in happen binnenkomt.
#[derive(Copy, Clone, Debug)]
pub struct Fnv64(u64);

impl Default for Fnv64 {
    fn default() -> Self {
        Self::new()
    }
}

impl Fnv64 {
    /// Een lege som.
    #[must_use]
    pub const fn new() -> Fnv64 {
        Fnv64(OFFSET)
    }

    /// Telt `b` erbij.
    pub fn write(&mut self, b: &[u8]) {
        for &x in b {
            self.0 = (self.0 ^ u64::from(x)).wrapping_mul(PRIME);
        }
    }

    /// De som tot nu toe.
    #[must_use]
    pub const fn sum(&self) -> u64 {
        self.0
    }
}

/// De FNV-1a-64-som van `b`.
#[must_use]
pub fn fnv64(b: &[u8]) -> u64 {
    let mut h = Fnv64::new();
    h.write(b);
    h.sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// De referentiewaarden van Go's `hash/fnv` (New64a), zodat een image
    /// van de Go-generatie en deze crate dezelfde som geven.
    #[test]
    fn fnv64_gelijk_aan_go() {
        assert_eq!(fnv64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv64(b"foobar"), 0x8594_4171_f739_67e8);
        let mut h = Fnv64::new();
        h.write(b"foo");
        h.write(b"bar");
        assert_eq!(h.sum(), fnv64(b"foobar"));
    }
}
