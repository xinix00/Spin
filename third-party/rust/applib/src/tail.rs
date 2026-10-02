//! De staart-rekenkunde: uit RamStart en RamSize volgt elk adres dat een
//! app met de kern deelt.
//!
//! Een app hoeft geen enkel absoluut adres te kennen. RamStart en RamSize
//! staan al in zijn image (de kern patcht ze bij plaatsing), en de staart
//! ligt direct daarboven: control-page, outbox-ring, frame-ringen. De som is
//! [`abi::layout::Tail`], dezelfde die de kern met de fysieke partitiebasis
//! maakt; hier staat alleen wat de app-kant er extra van eist.
//!
//! Dit module bezit niets en raakt geen geheugen aan.

use core::fmt;

pub use abi::layout::Tail;

/// Waarom een RAM-declaratie geen geldige staart geeft.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TailError {
    /// RamSize is nul: de patch van de kern kwam niet aan.
    Empty,
    /// Start of maat is niet paginagealigneerd, of de staart loopt over het
    /// einde van de adresruimte.
    Invalid {
        /// RamStart.
        start: u64,
        /// RamSize.
        size: u64,
    },
}

impl fmt::Display for TailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("RAM declaration is empty (RamSize 0)"),
            Self::Invalid { start, size } => {
                write!(
                    f,
                    "RAM declaration {start:#x}+{size:#x} gives no valid tail"
                )
            }
        }
    }
}

/// Rekent de staart uit de RAM-declaratie `ram_start`, `ram_size`.
///
/// Strenger dan `Tail::new`: ook de start moet op een pagina staan, want
/// het linkadres doet dat altijd, en een lege declaratie is een patch die
/// niet aankwam.
pub fn tail_of(ram_start: u64, ram_size: u64) -> Result<Tail, TailError> {
    if ram_size == 0 {
        return Err(TailError::Empty);
    }
    let invalid = TailError::Invalid {
        start: ram_start,
        size: ram_size,
    };
    if !ram_start.is_multiple_of(0x1000) {
        return Err(invalid);
    }
    Tail::new(ram_start, ram_size).ok_or(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ABI_TAIL;
    use dev::Pa;

    #[test]
    fn offsets_follow_from_the_ram_declaration() {
        // Het canonieke linkadres met een partitie van 64 MB min de staart,
        // zoals de kern hem patcht (RamSize = partitie - AbiTail).
        let t = tail_of(0x5000_0000, 0x400_0000 - ABI_TAIL).unwrap();
        assert_eq!(t.base(), Pa(0x53E0_0000));
        assert_eq!(t.ctrl_page(), Pa(0x53E0_0000));
        assert_eq!(t.outbox(), Pa(0x53E0_1000));
        assert_eq!(t.net_tx(), Pa(0x53E2_0000));
        assert_eq!(t.net_rx(), Pa(0x53F1_0000));
        // De staart eindigt precies op het einde van de partitie.
        assert_eq!(t.base().0 + ABI_TAIL, 0x5400_0000);
    }

    #[test]
    fn same_sum_on_both_sides() {
        // De kern rekent met de fysieke basis, de app met het linkadres: het
        // verschil tussen beide moet voor elk adres gelijk zijn.
        let app = tail_of(0x5000_0000, 0x0100_0000).unwrap();
        let hop = tail_of(0x8_1234_5000, 0x0100_0000).unwrap();
        let d = hop.base().0 - app.base().0;
        assert_eq!(hop.ctrl_page().0 - app.ctrl_page().0, d);
        assert_eq!(hop.outbox().0 - app.outbox().0, d);
        assert_eq!(hop.net_rx().0 - app.net_rx().0, d);
    }

    #[test]
    fn refuses_what_the_kernel_would_never_patch() {
        assert_eq!(tail_of(0x5000_0000, 0), Err(TailError::Empty));
        assert!(matches!(
            tail_of(0x5000_0010, 0x1000),
            Err(TailError::Invalid { .. })
        ));
        assert!(matches!(
            tail_of(0x5000_0000, 0x1010),
            Err(TailError::Invalid { .. })
        ));
        assert!(matches!(
            tail_of(0xFFFF_FFFF_FFFF_F000, 0x1000),
            Err(TailError::Invalid { .. })
        ));
        assert!(matches!(
            tail_of(0xFFFF_FFFF_FFE0_0000, 0x1000),
            Err(TailError::Invalid { .. })
        ));
    }
}
