//! De symbolen `memcpy`, `memcmp` en `bcmp` voor een app op arm64; de
//! lussen en de symbolen staan in [`dev::mem`] ([`dev::mem_symbols`]), met
//! het waarom.
//!
//! Sinds 30-09 draait elke app met zijn stage-1 aan ([`crate::mmu`]): al
//! zijn RAM en zijn ringen zijn Normal. Het snelle pad alleen dan
//! ([`crate::mmu::normal`]): de bouwer in `_start` draait met de MMU uit
//! (hij doet geen `memcpy`, maar een onbedoelde kopie valt dan op het trage
//! pad), en een app waarvan de kern de stage-1 weigerde
//! (`HOPOS_APP_NO_MMU`) ziet alles als Device.

dev::mem_symbols!(crate::mmu::normal);
