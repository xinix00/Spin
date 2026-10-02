//! De getallen van het contract met de kern, zoals applib ze gebruikt.
//!
//! De enige waarheid is de `abi`-crate (`abi::layout`, `abi::hopabi`,
//! `abi::systemapi`); dit module geeft ze de korte namen waarmee de rest van
//! applib leest, en voegt niets toe dan twee afgeleide sommen. Zo staat er
//! nergens in applib een getal dat van de kern kan afwijken.

pub(crate) use abi::hopabi::{
    CTRL_CORES, CTRL_ENV_DATA, CTRL_ENV_LEGACY_MAX, CTRL_ENV_LEN, CTRL_ENV_MAX, CTRL_EXIT_CODE,
    CTRL_HEARTBEAT, CTRL_IDLE, CTRL_IDLE_MODE, CTRL_KILL, CTRL_MEM_SYS, CTRL_RAM_SIZE,
    CTRL_RNG_GEN, CTRL_RNG_SEED, CTRL_RNG_SEED_LEN, CTRL_RNG_SOURCE, CTRL_RX_DOOR, CTRL_SHARED,
    CTRL_STATUS, CTRL_TEMP, CTRL_WAKES, CTRL_WALL_OFF, HDR_LEN as HOPABI_HDR_LEN, IDLE_YIELD,
    OP_LIST, OP_READ, OP_REMOVE, OP_STAT, OP_STORE_DROP, OP_STORE_LIST, OP_STORE_PULL,
    OP_STORE_PUSH, OP_SYNC, OP_TRUNCATE, OP_WRITE, RNG_SRC_JITTER, RNG_SRC_RNDR, RNG_SRC_SMCCC,
    RNG_SRC_SOC, RX_DOOR_ARMED as RX_ARMED, STATUS_NO_ENT as STATUS_NOENT, STATUS_OK,
    VERSION as HOPABI_VERSION, rng_source,
};
pub(crate) use abi::layout::{
    LINK_BASE as SLOT_LINK_BASE, NET_MTU, NET_RING_DATA_CAP, RING_DATA_CAP,
};
pub(crate) use abi::systemapi::{
    HEADER_LEN as SYS_HEADER_LEN, MAGIC as SYS_MAGIC, MAX_IO_CHUNK, MAX_PAYLOAD, PORT as SYS_PORT,
    VERSION as SYS_VERSION,
};

/// De ABI-versie die in het image gestempeld wordt. Alleen het target-image
/// stempelt; de host-build leest hem niet.
#[cfg_attr(
    not(all(target_os = "none", target_arch = "aarch64")),
    allow(dead_code)
)]
pub(crate) const ABI_VERSION: u64 = abi::ABI_VERSION as u64;

/// De ABI-staart per slot.
#[cfg_attr(
    not(all(target_os = "none", target_arch = "aarch64")),
    allow(dead_code)
)]
pub(crate) const ABI_TAIL: u64 = abi::layout::ABI_TAIL;

/// Frame: een call (`systemapi::Kind::Call`).
pub(crate) const KIND_CALL: u8 = abi::systemapi::Kind::Call as u8;
/// Frame: het resultaat van een call.
pub(crate) const KIND_RESULT: u8 = abi::systemapi::Kind::Result as u8;
/// Frame: een logregel.
pub(crate) const KIND_LOG: u8 = abi::systemapi::Kind::Log as u8;

/// Het interne IPv4 van slot `i` (big-endian): kern .1, slot i .(i+1). Als
/// som over `u64` omdat een app zijn slot als getal kent (0 = onbekend of
/// de kern zelf); `abi::layout::slot_ip4` rekent hetzelfde voor een `Slot`.
pub(crate) const fn slot_ip4(i: u64) -> u32 {
    abi::layout::HOST_IP4 + (i as u32 & 0xff)
}

/// De deterministische MAC van slot `i` (kern = slot 0).
pub(crate) const fn slot_mac(i: u64) -> [u8; 6] {
    let mut m = abi::layout::HOST_MAC;
    m[5] = i as u8;
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use abi::layout::Slot;

    // De getallen die de Go-kant hard noemt, als toets dat applib dezelfde
    // leest als de kern.
    #[test]
    fn derived_geometry_matches_the_go_numbers() {
        assert_eq!(RING_DATA_CAP, 0x7000);
        assert_eq!(NET_RING_DATA_CAP, 0xE_F000);
        // 0xED8 tot 29-09; de temperatuur (CTRL_TEMP) en de timebase
        // (CTRL_TIMEBASE_HZ) namen er elk 8 van, het EL1-fault-rapport
        // (CTRL_APP_FAULT_*) op 30-09 nog eens 32, en het RNG-blok
        // (CTRL_RNG_*) daarna nog eens 48. Een env van een oude kern mag
        // tot de oude grens (Env::read).
        assert_eq!(CTRL_ENV_MAX, 0xE78);
        assert_eq!(CTRL_ENV_LEGACY_MAX, 0xEA8);
        assert_eq!(ABI_TAIL, 0x20_0000);
        assert_eq!((KIND_CALL, KIND_RESULT, KIND_LOG), (1, 2, 3));
    }

    #[test]
    fn net_plan_is_deterministic() {
        assert_eq!(slot_ip4(0), 0x0a64_0001);
        assert_eq!(slot_ip4(3), 0x0a64_0004);
        assert_eq!(slot_mac(3), [2, 0, 0, 0, 0, 3]);
        let s = Slot::new(3).unwrap();
        assert_eq!(slot_ip4(3), abi::layout::slot_ip4(s));
        assert_eq!(slot_mac(3), abi::layout::slot_mac(s));
    }
}
