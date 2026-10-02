//! De control-page van een slot en de payload van een system-call.
//!
//! **De control-page** ([`CtrlPage`]) is de eerste pagina van de staart
//! ([`crate::layout::Tail::ctrl_page`]): 64-bit woorden in de kop, een
//! env-blob in het midden, en woorden die later kwamen bovenaan de page,
//! naar beneden groeiend. Elk veld heeft één schrijver; die staat bij het
//! veld. De offsets zijn byte voor byte die van Go-ABI 10 (Go telde door tot
//! 10; deze crate begint opnieuw bij 1, zie [`crate::ABI_VERSION`]).
//!
//! **De call-payload** ([`Req`], [`Resp`]) is wat een `Call`- of
//! `Result`-frame van [`crate::systemapi`] draagt: een kop van 24 bytes
//! (little-endian) plus pad en data.
//!
//! ```text
//! req:  ver u8 | op u8 | path_len u16 | seq u32 | off u64 | n u64 | path | data
//! resp: ver u8 | op u8 | status u16   | seq u32 | size u64 | _ u64 | data
//! ```
//!
//! Stateless (paden, geen fd's): een app-crash laat bij de kern niets
//! achter.
//!
//! De codec-payloads (Go `codec.go`) staan in [`codec`]: aanwijzingen naar
//! bytes die al in de partitie van de app liggen, geen bytes. De
//! device-payloads (`device.go`) horen bij de optische drive en komen met
//! haar driver; hun opcode staat er wel, zodat het nummer bezet blijft.

use crate::{Error, Result};
use core::mem::{offset_of, size_of};

pub mod device;

// ---------------------------------------------------------------------------
// De control-page.
// ---------------------------------------------------------------------------

/// App: de status ([`AppStatus`]).
pub const CTRL_STATUS: u64 = 0x00;
/// App: de exitcode, gezet bij exit.
pub const CTRL_EXIT_CODE: u64 = 0x08;
/// Kern naar app: 1 is "stop jezelf" (coöperatief).
pub const CTRL_KILL: u64 = 0x10;
/// App: een oplopende teller, voor hang-detectie.
pub const CTRL_HEARTBEAT: u64 = 0x18;
/// App: de eigen RAM-maat, als bewijs dat de patch aankwam.
pub const CTRL_RAM_SIZE: u64 = 0x20;
/// Kern naar app: de lengte van de env-blob in bytes.
pub const CTRL_ENV_LEN: u64 = 0x28;
/// Kern naar trampoline: de app-entry (EL1) voor de ERET.
pub const CTRL_ENTRY: u64 = 0x30;
/// Kern naar trampoline: het fysieke adres van de stage-2-L1-tabel.
pub const CTRL_S2_TABLE: u64 = 0x38;
/// Kern naar app: de klok-offset (wall-ns bij tellerstand 0, `i64` als
/// bits; 0 = geen klok). De teller is gedeeld over alle cores, dus de
/// offset van de kern geldt exact voor elke app.
pub const CTRL_WALL_OFF: u64 = 0x40;
/// App naar kern: geaccumuleerde idle-TIJD in timer-tikken. Sinds 18-07
/// tijd in plaats van rondes: rondes bleken op ijzer door SEV-ruis
/// opgeblazen. Stond op 0xD8 en botste daar met [`CTRL_SMP_TCR`]; de
/// uniekheidstoets bewaakt dat voortaan.
pub const CTRL_IDLE: u64 = 0x48;
/// Kern naar trampoline: de fysieke basis van de EL2-vectoren.
pub const CTRL_VEC_PA: u64 = 0x50;
/// Vector: ESR_EL2 van de fault die het slot deed vallen.
pub const CTRL_FAULT_ESR: u64 = 0x58;
/// Vector: FAR_EL2, het faultadres.
pub const CTRL_FAULT_FAR: u64 = 0x60;
/// Vector: vectorindex plus 1 (0 = geen fault gezien); zie [`FAULT_SYNC`].
pub const CTRL_FAULT_VEC: u64 = 0x68;
/// Kern naar app: het aantal cores (1 = geen SMP).
pub const CTRL_CORES: u64 = 0x70;
/// De actieve VBAR_EL1 van de dispatchende primaire, voor een secundaire.
pub const CTRL_SMP_VBAR: u64 = 0x78;
/// Kern naar app: het fysieke adres van de EL2-SMP-trampoline.
pub const CTRL_SMP_TRAMP: u64 = 0x80;
/// App naar secundaire: de stacktop (IPA).
pub const CTRL_SMP_SP: u64 = 0x88;
/// App naar secundaire: eerste runtime-argument (Go: `*m`).
pub const CTRL_SMP_MP: u64 = 0x90;
/// App naar secundaire: tweede runtime-argument (Go: `g0`).
pub const CTRL_SMP_G0: u64 = 0x98;
/// App naar secundaire: de entry (IPA).
pub const CTRL_SMP_FN: u64 = 0xA0;
/// App naar trampoline: de EL1-stub waar de secundaire heen ERET't.
pub const CTRL_SMP_STUB: u64 = 0xA8;
/// App naar secundaire: de stage-1-L1-tabel, zodat de stub geen geheugen
/// leest vóór zijn MMU aan staat (een pre-MMU-lees kan stale zijn).
pub const CTRL_SMP_TTBR0: u64 = 0xB0;
/// Kern naar trampoline: het slotnummer (= VMID).
pub const CTRL_SLOT: u64 = 0xB8;
/// App naar kern: de core-index die de runtime als extra SMP-core wil
/// (0 = geen verzoek). De kern valideert tegen [`CTRL_CORES`].
pub const CTRL_SMP_REQ: u64 = 0xC0;
/// Kern naar trampoline: de park-mailbox van déze core.
pub const CTRL_MBOX_PA: u64 = 0xC8;
/// Kern naar trampoline: de park-mailbox van de secundaire.
pub const CTRL_SMP_MBOX: u64 = 0xD0;
/// De actieve TCR_EL1 van de primaire, voor een secundaire (gelezen van de
/// levende registers, geen afgeleide kopie: de 39-bit-standaard kon de
/// Altra-UART op 16 TB niet vertalen, gemeten 17-07).
pub const CTRL_SMP_TCR: u64 = 0xD8;
/// Apploader naar kern: de maat van het gestagede image.
pub const CTRL_STAGED_SIZE: u64 = 0xE0;
/// App naar kern: de werkelijke geheugen-draw van de runtime (0 = nog niet
/// gerapporteerd).
pub const CTRL_MEM_SYS: u64 = 0xE8;
/// Apploader naar kern: de IPA van het zelfplaatsings-stubje (0 = geen).
/// Niet vertrouwd voor isolatie: het draait ín de kooi.
pub const CTRL_PLACE_ENTRY: u64 = 0xF0;
/// De actieve MAIR_EL1 van de primaire, voor een secundaire.
pub const CTRL_SMP_MAIR: u64 = 0xF8;
/// Kern naar app: 1 als dit slot zijn core deelt; de idle-governor yieldt
/// dan naar de switcher in plaats van WFE te slapen.
pub const CTRL_SHARED: u64 = 0x100;
/// App naar kern: het aantal idle-rondes. Bewust ongelezen (besluit Derek
/// 06-08) tot een onverklaarbaar hoog cpu-percentage erom vraagt.
pub const CTRL_WAKES: u64 = 0x108;
/// App naar switcher: de wek-drempel van de doorbell (head | bit 63). Alleen
/// wie de ring draint mag hem wapenen: anders maakt elke ARP-flood een app
/// zonder netstack permanent "due".
pub const CTRL_RX_DOOR: u64 = 0x110;
/// App naar kern: 1 is "maak van mijn doorbell een vFIQ". Alleen voor een
/// app met één core: HCR_EL2 is per core.
pub const CTRL_DOOR_IRQ: u64 = 0x118;
/// De env-blob: `key=val\n`-bytes die de kern schrijft en de app bij start
/// inleest.
pub const CTRL_ENV_DATA: u64 = 0x120;
/// Kern naar app: hoe de cores van dit slot idlen ([`IDLE_YIELD`]; 0 = wat
/// de architectuur zelf doet). Bovenaan de page: woorden die ná de env
/// kwamen, groeien naar beneden, zodat de env niet meer verschuift.
pub const CTRL_IDLE_MODE: u64 = 0xFF8;
/// Kern naar app: de heetste die-temperatuur van de node in milligraden
/// (`i64` als bits; 0 = geen meting), elke seconde gezet door de
/// telemetrie van de kern. Voor de heartbeat van Hop (`hop agents`, zoals
/// `Temp: board.TempMilliC` in de Go-agent). Onder [`CTRL_IDLE_MODE`],
/// naar beneden groeiend (29-09).
pub const CTRL_TEMP: u64 = 0xFF0;
/// Kern naar app: de timebase van de teller van de app in tikken per
/// seconde, gezet bij de bouw van de kooi (0 = de app neemt wat de
/// architectuur zelf zegt). Op arm64 zegt CNTFRQ_EL0 het al; RISC-V heeft
/// geen register waaruit hij volgt (de TIME-CSR telt 10 MHz op QEMU virt en
/// 25 MHz op de LicheeRV), dus daar is dit woord de enige bron. Onder
/// [`CTRL_TEMP`], naar beneden groeiend (29-09).
pub const CTRL_TIMEBASE_HZ: u64 = 0xFE8;
/// App naar kern: de vectorindex plus 1 van een exception die de app op
/// EL1 zelf ving (0 = geen), gezet door de vectortabel van applib vlak
/// vóór hij de app met [`EXIT_APP_FAULT`] laat eindigen. Onder
/// [`CTRL_TIMEBASE_HZ`], naar beneden groeiend, met de drie woorden eronder
/// (30-09).
///
/// Waarom een eigen blok naast [`CTRL_FAULT_VEC`]: dat rapport is van EL2
/// en ziet alleen wat naar EL2 trapt. Een fault die op EL1 blijft (een
/// alignment-fault, een ongedefinieerde instructie) sprong tot 30-09 naar
/// een lege VBAR_EL1, en EL2 zag dan alleen de tweede fault: de
/// instructie-abort op `VBAR + 0x200` (de eerste Pi 5-boot: `esr=0x82000005
/// far=0x200`). Hier staat de échte.
pub const CTRL_APP_FAULT_VEC: u64 = 0xFE0;
/// App naar kern: ESR_EL1 van die exception.
pub const CTRL_APP_FAULT_ESR: u64 = 0xFD8;
/// App naar kern: ELR_EL1, de PC waar hij viel.
pub const CTRL_APP_FAULT_ELR: u64 = 0xFD0;
/// App naar kern: FAR_EL1, het adres dat hij raakte (alleen zinvol bij een
/// abort).
pub const CTRL_APP_FAULT_FAR: u64 = 0xFC8;
/// Kern naar app: 32 bytes zaad uit de DRBG van de kern (`cpu::drbg`),
/// vier woorden van 0xFA8 tot 0xFC8, onder [`CTRL_APP_FAULT_FAR`], naar
/// beneden groeiend (30-09). De kern zet het bij de bouw van de kooi, na
/// een flip-adoptie en daarna elke seconde vers vanuit de telemetrie-tik;
/// elk slot krijgt eigen bytes. Geldig alleen onder het protocol van
/// [`CTRL_RNG_GEN`]; de bron staat in [`CTRL_RNG_SOURCE`].
///
/// Zaad, geen sleutel: de app haalt het door zijn eigen DRBG
/// (`applib::rand`), samen met eigen jitter, en gebruikt het nooit rauw.
/// Waarom op de page en niet als system-op: een woord op de page kost geen
/// verbinding (de TLS van Hop heeft zijn zaad nodig vóór er een
/// system-verbinding is), het is de vorm van [`CTRL_TEMP`] en
/// [`CTRL_WALL_OFF`], en een oude app leest het simpelweg niet.
pub const CTRL_RNG_SEED: u64 = 0xFA8;
/// De lengte van [`CTRL_RNG_SEED`] in bytes.
pub const CTRL_RNG_SEED_LEN: usize = 32;
/// Kern naar app: de generatie van [`CTRL_RNG_SEED`], een seqlock met één
/// schrijver (de kern op zijn OS-core). 0 = geen zaad (een oude kern, of
/// een kern zonder geseede DRBG); oneven = de kern schrijft net; even en
/// niet 0 = geldig. De kern schrijft oneven, dan het zaad en de bron, dan
/// de volgende even waarde, met een barrière en een cache-clean na elke
/// stap. De lezer leest de generatie, het zaad en de generatie opnieuw, en
/// neemt het zaad alleen als beide gelijk, even en niet 0 zijn. Een nieuwe
/// generatie is vers zaad: de app mengt het bij (herzaaien). Per page
/// monotoon, ook over een flip: de kern telt door vanaf wat er staat.
pub const CTRL_RNG_GEN: u64 = 0xFA0;
/// Kern naar app: de bron van het zaad in [`CTRL_RNG_SEED`]:
/// [`RNG_MAGIC`] in de bovenste 32 bits en een `RNG_SRC_*` in de onderste
/// byte ([`rng_source`] leest hem). Een woord zonder de magic is geen bron:
/// op een oude kern kan hier een lange env-blob staan (tot 0xEA8 bytes, zie
/// [`CTRL_ENV_LEGACY_MAX`]), en tekst vormt nooit de magic (zijn bytes
/// liggen boven 0x7F).
pub const CTRL_RNG_SOURCE: u64 = 0xF98;
/// De ruimte voor de env-blob. 0xEA8 tot het RNG-blok (30-09) er 40 van
/// nam.
pub const CTRL_ENV_MAX: u64 = CTRL_RNG_SOURCE - CTRL_ENV_DATA;
/// De grootste env die een kern ooit schreef: 0xEA8 bytes, vóór het
/// RNG-blok. Een lezer die ook op een oude kern moet draaien, accepteert
/// een [`CTRL_ENV_LEN`] tot hier zolang [`CTRL_RNG_SOURCE`] geen
/// [`RNG_MAGIC`] draagt (een oude kern); een nieuwe kern schrijft nooit
/// meer dan [`CTRL_ENV_MAX`].
pub const CTRL_ENV_LEGACY_MAX: u64 = CTRL_APP_FAULT_FAR - CTRL_ENV_DATA;

/// De bovenste 32 bits van [`CTRL_RNG_SOURCE`]. Elke byte ligt boven 0x7F,
/// dus een env van tekst die op een oude kern over het woord loopt, is
/// nooit een bron.
pub const RNG_MAGIC: u32 = 0xC0DE_5EED;
/// [`CTRL_RNG_SOURCE`]: de DRBG van de kern is uit timing-jitter gezaaid,
/// niet uit hardware (QEMU virt, een board zonder TRNG).
pub const RNG_SRC_JITTER: u8 = 1;
/// [`CTRL_RNG_SOURCE`]: hardware, RNDR (FEAT_RNG; de O6N).
pub const RNG_SRC_RNDR: u8 = 2;
/// [`CTRL_RNG_SOURCE`]: hardware, de SMCCC TRNG van de firmware (DEN 0098;
/// de Altra).
pub const RNG_SRC_SMCCC: u8 = 3;
/// [`CTRL_RNG_SOURCE`]: hardware, een TRNG-blok van de SoC (de RNG200 van
/// de Pi's, de RKRNG van de Radxa).
pub const RNG_SRC_SOC: u8 = 4;

/// Het woord voor [`CTRL_RNG_SOURCE`] bij bron `src` (een `RNG_SRC_*`).
#[must_use]
pub const fn rng_source_word(src: u8) -> u64 {
    ((RNG_MAGIC as u64) << 32) | src as u64
}

/// De `RNG_SRC_*` uit een [`CTRL_RNG_SOURCE`]-woord, of `None` als de magic
/// ontbreekt of de bron onbekend is (een oude kern, een env eroverheen).
#[must_use]
pub const fn rng_source(word: u64) -> Option<u8> {
    if (word >> 32) as u32 != RNG_MAGIC || word & 0xFFFF_FF00 != 0 {
        return None;
    }
    match word as u8 {
        s @ (RNG_SRC_JITTER | RNG_SRC_RNDR | RNG_SRC_SMCCC | RNG_SRC_SOC) => Some(s),
        _ => None,
    }
}

/// [`CTRL_EXIT_CODE`] van een app die op EL1 een exception ving: het
/// rapport staat in [`CTRL_APP_FAULT_VEC`] en de drie woorden eronder.
/// Geen gewone exitcode (0 klaar, 1 een spawn, 2 een paniek), en leesbaar
/// in een hexdump.
pub const EXIT_APP_FAULT: u64 = 0xFA17;

/// De bit in [`CTRL_RX_DOOR`] die de drempel wapent; een byte-index haalt
/// dat bit nooit.
pub const RX_DOOR_ARMED: u64 = 1 << 63;

/// [`CTRL_IDLE_MODE`]: idle is een yield naar EL2 (HVC #1), ook met een
/// eigen core. Zo idlet Apple silicon: op de M4 slaapt een app-core op EL1
/// niet (gemeten 02-09). De kern zet dit niet op een SMP-slot.
pub const IDLE_YIELD: u64 = 1;

/// [`CTRL_FAULT_VEC`]: geen fault gezien sinds de laatste start.
pub const FAULT_NONE: u64 = 0;
/// [`CTRL_FAULT_VEC`]: synchroon vanuit EL1 (index 8), een stage-2-fault;
/// ESR en FAR zijn geldig. Een kooi-overtreding en een hard-kill landen
/// allebei hier.
pub const FAULT_SYNC: u64 = 9;

/// [`CTRL_KILL`]: stop jezelf.
pub const KILL_STOP: u64 = 1;

/// De status van een app op de control-page ([`CTRL_STATUS`]).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum AppStatus {
    /// De kern heeft de page geveegd.
    Empty = 0,
    /// De kern heeft de core gestart; de runtime is nog niet klaar.
    Booting = 1,
    /// De runtime draait (gezet door de app).
    Ready = 2,
    /// De app is gestopt; de exitcode staat in [`CTRL_EXIT_CODE`].
    Exited = 3,
    /// De apploader heeft het echte image gestaged en geparkeerd.
    Staged = 4,
}

impl AppStatus {
    /// De status van een rauw woord, of `None` voor een onbekende waarde.
    #[must_use]
    pub const fn from_raw(v: u64) -> Option<AppStatus> {
        Some(match v {
            0 => Self::Empty,
            1 => Self::Booting,
            2 => Self::Ready,
            3 => Self::Exited,
            4 => Self::Staged,
            _ => return None,
        })
    }

    /// Het rauwe woord.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self as u64
    }
}

/// De indeling van de control-page, als type: de offsets hierboven zijn de
/// velden van deze struct, en de asserties eronder houden ze byte voor
/// byte gelijk. Een botsing zoals `CtrlSMPTcr` en `CtrlIdle` op 0xD8
/// (18-07) is zo een compilefout.
#[repr(C)]
#[derive(Debug)]
pub struct CtrlPage {
    /// [`CTRL_STATUS`].
    pub status: u64,
    /// [`CTRL_EXIT_CODE`].
    pub exit_code: u64,
    /// [`CTRL_KILL`].
    pub kill: u64,
    /// [`CTRL_HEARTBEAT`].
    pub heartbeat: u64,
    /// [`CTRL_RAM_SIZE`].
    pub ram_size: u64,
    /// [`CTRL_ENV_LEN`].
    pub env_len: u64,
    /// [`CTRL_ENTRY`].
    pub entry: u64,
    /// [`CTRL_S2_TABLE`].
    pub s2_table: u64,
    /// [`CTRL_WALL_OFF`].
    pub wall_off: u64,
    /// [`CTRL_IDLE`].
    pub idle: u64,
    /// [`CTRL_VEC_PA`].
    pub vec_pa: u64,
    /// [`CTRL_FAULT_ESR`].
    pub fault_esr: u64,
    /// [`CTRL_FAULT_FAR`].
    pub fault_far: u64,
    /// [`CTRL_FAULT_VEC`].
    pub fault_vec: u64,
    /// [`CTRL_CORES`].
    pub cores: u64,
    /// [`CTRL_SMP_VBAR`].
    pub smp_vbar: u64,
    /// [`CTRL_SMP_TRAMP`].
    pub smp_tramp: u64,
    /// [`CTRL_SMP_SP`].
    pub smp_sp: u64,
    /// [`CTRL_SMP_MP`].
    pub smp_mp: u64,
    /// [`CTRL_SMP_G0`].
    pub smp_g0: u64,
    /// [`CTRL_SMP_FN`].
    pub smp_fn: u64,
    /// [`CTRL_SMP_STUB`].
    pub smp_stub: u64,
    /// [`CTRL_SMP_TTBR0`].
    pub smp_ttbr0: u64,
    /// [`CTRL_SLOT`].
    pub slot: u64,
    /// [`CTRL_SMP_REQ`].
    pub smp_req: u64,
    /// [`CTRL_MBOX_PA`].
    pub mbox_pa: u64,
    /// [`CTRL_SMP_MBOX`].
    pub smp_mbox: u64,
    /// [`CTRL_SMP_TCR`].
    pub smp_tcr: u64,
    /// [`CTRL_STAGED_SIZE`].
    pub staged_size: u64,
    /// [`CTRL_MEM_SYS`].
    pub mem_sys: u64,
    /// [`CTRL_PLACE_ENTRY`].
    pub place_entry: u64,
    /// [`CTRL_SMP_MAIR`].
    pub smp_mair: u64,
    /// [`CTRL_SHARED`].
    pub shared: u64,
    /// [`CTRL_WAKES`].
    pub wakes: u64,
    /// [`CTRL_RX_DOOR`].
    pub rx_door: u64,
    /// [`CTRL_DOOR_IRQ`].
    pub door_irq: u64,
    /// [`CTRL_ENV_DATA`].
    pub env: [u8; CTRL_ENV_MAX as usize],
    /// [`CTRL_RNG_SOURCE`].
    pub rng_source: u64,
    /// [`CTRL_RNG_GEN`].
    pub rng_gen: u64,
    /// [`CTRL_RNG_SEED`].
    pub rng_seed: [u64; CTRL_RNG_SEED_LEN / 8],
    /// [`CTRL_APP_FAULT_FAR`].
    pub app_fault_far: u64,
    /// [`CTRL_APP_FAULT_ELR`].
    pub app_fault_elr: u64,
    /// [`CTRL_APP_FAULT_ESR`].
    pub app_fault_esr: u64,
    /// [`CTRL_APP_FAULT_VEC`].
    pub app_fault_vec: u64,
    /// [`CTRL_TIMEBASE_HZ`].
    pub timebase_hz: u64,
    /// [`CTRL_TEMP`].
    pub temp: u64,
    /// [`CTRL_IDLE_MODE`].
    pub idle_mode: u64,
}

/// Alle woord-offsets van de page, voor de uniekheidstoets.
pub const CTRL_WORDS: [u64; 49] = [
    CTRL_STATUS,
    CTRL_EXIT_CODE,
    CTRL_KILL,
    CTRL_HEARTBEAT,
    CTRL_RAM_SIZE,
    CTRL_ENV_LEN,
    CTRL_ENTRY,
    CTRL_S2_TABLE,
    CTRL_WALL_OFF,
    CTRL_IDLE,
    CTRL_VEC_PA,
    CTRL_FAULT_ESR,
    CTRL_FAULT_FAR,
    CTRL_FAULT_VEC,
    CTRL_CORES,
    CTRL_SMP_VBAR,
    CTRL_SMP_TRAMP,
    CTRL_SMP_SP,
    CTRL_SMP_MP,
    CTRL_SMP_G0,
    CTRL_SMP_FN,
    CTRL_SMP_STUB,
    CTRL_SMP_TTBR0,
    CTRL_SLOT,
    CTRL_SMP_REQ,
    CTRL_MBOX_PA,
    CTRL_SMP_MBOX,
    CTRL_SMP_TCR,
    CTRL_STAGED_SIZE,
    CTRL_MEM_SYS,
    CTRL_PLACE_ENTRY,
    CTRL_SMP_MAIR,
    CTRL_SHARED,
    CTRL_WAKES,
    CTRL_RX_DOOR,
    CTRL_DOOR_IRQ,
    CTRL_RNG_SOURCE,
    CTRL_RNG_GEN,
    CTRL_RNG_SEED,
    CTRL_RNG_SEED + 8,
    CTRL_RNG_SEED + 16,
    CTRL_RNG_SEED + 24,
    CTRL_APP_FAULT_FAR,
    CTRL_APP_FAULT_ELR,
    CTRL_APP_FAULT_ESR,
    CTRL_APP_FAULT_VEC,
    CTRL_TIMEBASE_HZ,
    CTRL_TEMP,
    CTRL_IDLE_MODE,
];

macro_rules! at {
    ($field:ident, $off:expr) => {
        const _: () = assert!(offset_of!(CtrlPage, $field) as u64 == $off);
    };
}

const _: () = assert!(size_of::<CtrlPage>() as u64 == crate::layout::CTRL_STRIDE);
at!(status, CTRL_STATUS);
at!(exit_code, CTRL_EXIT_CODE);
at!(kill, CTRL_KILL);
at!(heartbeat, CTRL_HEARTBEAT);
at!(ram_size, CTRL_RAM_SIZE);
at!(env_len, CTRL_ENV_LEN);
at!(entry, CTRL_ENTRY);
at!(s2_table, CTRL_S2_TABLE);
at!(wall_off, CTRL_WALL_OFF);
at!(idle, CTRL_IDLE);
at!(vec_pa, CTRL_VEC_PA);
at!(fault_esr, CTRL_FAULT_ESR);
at!(fault_far, CTRL_FAULT_FAR);
at!(fault_vec, CTRL_FAULT_VEC);
at!(cores, CTRL_CORES);
at!(smp_vbar, CTRL_SMP_VBAR);
at!(smp_tramp, CTRL_SMP_TRAMP);
at!(smp_sp, CTRL_SMP_SP);
at!(smp_mp, CTRL_SMP_MP);
at!(smp_g0, CTRL_SMP_G0);
at!(smp_fn, CTRL_SMP_FN);
at!(smp_stub, CTRL_SMP_STUB);
at!(smp_ttbr0, CTRL_SMP_TTBR0);
at!(slot, CTRL_SLOT);
at!(smp_req, CTRL_SMP_REQ);
at!(mbox_pa, CTRL_MBOX_PA);
at!(smp_mbox, CTRL_SMP_MBOX);
at!(smp_tcr, CTRL_SMP_TCR);
at!(staged_size, CTRL_STAGED_SIZE);
at!(mem_sys, CTRL_MEM_SYS);
at!(place_entry, CTRL_PLACE_ENTRY);
at!(smp_mair, CTRL_SMP_MAIR);
at!(shared, CTRL_SHARED);
at!(wakes, CTRL_WAKES);
at!(rx_door, CTRL_RX_DOOR);
at!(door_irq, CTRL_DOOR_IRQ);
at!(env, CTRL_ENV_DATA);
at!(rng_source, CTRL_RNG_SOURCE);
at!(rng_gen, CTRL_RNG_GEN);
at!(rng_seed, CTRL_RNG_SEED);
// Het zaad sluit precies aan op het fault-rapport erboven.
const _: () = assert!(CTRL_RNG_SEED + CTRL_RNG_SEED_LEN as u64 == CTRL_APP_FAULT_FAR);
at!(app_fault_far, CTRL_APP_FAULT_FAR);
at!(app_fault_elr, CTRL_APP_FAULT_ELR);
at!(app_fault_esr, CTRL_APP_FAULT_ESR);
at!(app_fault_vec, CTRL_APP_FAULT_VEC);
at!(timebase_hz, CTRL_TIMEBASE_HZ);
at!(temp, CTRL_TEMP);
at!(idle_mode, CTRL_IDLE_MODE);
// De SMP-handoff in het ctx-blok draagt de control-velden onder 256 bytes.
const _: () = assert!(CTRL_SMP_MAIR + 8 <= crate::layout::CTX_LEN - crate::layout::CTX_SMP);

// ---------------------------------------------------------------------------
// De call-payload.
// ---------------------------------------------------------------------------

/// De versie van het payload-formaat.
pub const VERSION: u8 = 1;
/// De lengte van de kop van een request of response.
pub const HDR_LEN: usize = 24;
/// De historische mailboxgrens; nieuwe verbindingen begrenzen met
/// [`crate::systemapi::MAX_IO_CHUNK`].
pub const MAX_CHUNK: usize = 8 << 10;

/// `stat(path)`: de maat (een map: 0, status OK).
pub const OP_STAT: u8 = 1;
/// `read(path, off, n)`: de data.
pub const OP_READ: u8 = 2;
/// `write(path, off, data)`: maakt bestand en ouder-mappen.
pub const OP_WRITE: u8 = 3;
/// `list(path)`: namen, `\n`-gescheiden (`naam/` is een map).
pub const OP_LIST: u8 = 4;
/// `remove(path)`: een bestand of lege map.
pub const OP_REMOVE: u8 = 5;
// 6 was OpFetch: de kern downloadde een app-opgegeven URL met zijn volle
// rechten, een SSRF-pad naar alles wat de node bereikt. Gesloopt; het nummer
// blijft leeg, zodat een oud image een nette "onbekende op" krijgt.
/// `truncate(path, n)`: maakt bestand en ouder-mappen.
pub const OP_TRUNCATE: u8 = 7;
/// Object naar eigen pad (vervangend); de maat. `path` is de objectnaam
/// binnen de eigen map, `data` leeg (het lokale pad is dezelfde naam, zoals
/// in Go) of een ander lokaal pad (sinds alpha.12, additief).
pub const OP_STORE_PULL: u8 = 8;
/// Eigen pad naar object (vervangend); de maat. `path` en `data` zoals bij
/// [`OP_STORE_PULL`]: de objectnaam, en eventueel een ander lokaal pad.
pub const OP_STORE_PUSH: u8 = 9;
/// Keys onder de eigen map plus pad-prefix, `\n`-gescheiden en relatief
/// aan de eigen map (voer voor [`OP_STORE_PULL`]); `size` het aantal. Een
/// lijst die niet in één antwoord past, is een fout, nooit afgekapt.
pub const OP_STORE_LIST: u8 = 10;
/// Object weg (idempotent).
pub const OP_STORE_DROP: u8 = 11;
// 12 en 13 waren OpSurfGrant/OpSurfRevoke (gesloopt 06-08, dezelfde dag als
// gebouwd); de nummers blijven leeg.
/// Codec openen; het handvat in `size`. Niet idempotent, zoals alle
/// codec-ops: twee keer dezelfde feed is twee happen bitstream.
pub const OP_CODEC_OPEN: u8 = 14;
/// Bitstream erin (een buffer in de eigen partitie: `off`, `n`).
pub const OP_CODEC_FEED: u8 = 15;
/// Een lege beeldbuffer erin.
pub const OP_CODEC_OFFER: u8 = 16;
/// Nul of meer events.
pub const OP_CODEC_POLL: u8 = 17;
/// Codec sluiten.
pub const OP_CODEC_CLOSE: u8 = 18;
/// Eén SCSI-uitwisseling; niet herhaalbaar.
pub const OP_DEVICE_COMMAND: u8 = 19;
/// Bevestigde opslagbarrière voor een zichtbaar bestaand bestand of directory.
/// `off`, `n` en data zijn nul/leeg. Geeft de vastgelegde boomgeneratie in size.
/// Na verwijderen gebruikt de app het ouderpad; een vluchtige FS weigert.
pub const OP_SYNC: u8 = 20;
/// Het hoogste opnummer van deze module; de bevoegde operaties van
/// [`crate::systemapi`] liggen erboven.
pub const OP_MAX: u8 = OP_SYNC;

/// Een call-status: gelukt.
pub const STATUS_OK: u16 = 0;
/// Een call-status: algemene fout (tekst in de data).
pub const STATUS_ERROR: u16 = 1;
/// Een call-status: het pad bestaat niet.
pub const STATUS_NO_ENT: u16 = 2;
/// Een call-status: buiten de mounts of de eigen root.
pub const STATUS_DENIED: u16 = 3;

/// Een request.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Req<'a> {
    /// De operatie.
    pub op: u8,
    /// Het volgnummer, terug in de response.
    pub seq: u32,
    /// De offset.
    pub off: u64,
    /// De lengte of het getal-argument.
    pub n: u64,
    /// Het pad.
    pub path: &'a [u8],
    /// De data.
    pub data: &'a [u8],
}

/// Een response.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Resp<'a> {
    /// De operatie.
    pub op: u8,
    /// De status (`STATUS_*`); bij een fout draagt `data` de tekst.
    pub status: u16,
    /// Het volgnummer van de request.
    pub seq: u32,
    /// De maat of het handvat.
    pub size: u64,
    /// De data.
    pub data: &'a [u8],
}

/// Een little-endian `u16` op `b[i..]`; de aanroeper toetste de lengte.
fn le16(b: &[u8], i: usize) -> u16 {
    let mut w = [0u8; 2];
    w.copy_from_slice(&b[i..i + 2]);
    u16::from_le_bytes(w)
}

/// Een little-endian `u32` op `b[i..]`.
fn le32(b: &[u8], i: usize) -> u32 {
    let mut w = [0u8; 4];
    w.copy_from_slice(&b[i..i + 4]);
    u32::from_le_bytes(w)
}

/// Een little-endian `u64` op `b[i..]`.
fn le64(b: &[u8], i: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[i..i + 8]);
    u64::from_le_bytes(w)
}

/// Schrijft de kop van 24 bytes; `dst` is minstens [`HDR_LEN`] lang.
fn put_head(dst: &mut [u8], op: u8, w16: u16, seq: u32, a: u64, b: u64) {
    dst[0] = VERSION;
    dst[1] = op;
    dst[2..4].copy_from_slice(&w16.to_le_bytes());
    dst[4..8].copy_from_slice(&seq.to_le_bytes());
    dst[8..16].copy_from_slice(&a.to_le_bytes());
    dst[16..24].copy_from_slice(&b.to_le_bytes());
}

/// Toetst lengte en versie van een binnengekomen payload.
fn check_head(b: &[u8]) -> Result {
    if b.len() < HDR_LEN {
        return Err(Error::Short {
            len: b.len(),
            need: HDR_LEN,
        });
    }
    if b[0] != VERSION {
        return Err(Error::BadVersion {
            got: b[0],
            want: VERSION,
        });
    }
    Ok(())
}

/// Serialiseert een request in `dst`; geeft de lengte.
pub fn encode_req(dst: &mut [u8], r: &Req<'_>) -> Result<usize> {
    let plen = u16::try_from(r.path.len()).map_err(|_| Error::PayloadTooLarge {
        len: r.path.len(),
        max: u16::MAX as usize,
    })?;
    let total = HDR_LEN + r.path.len() + r.data.len();
    if dst.len() < total {
        return Err(Error::Short {
            len: dst.len(),
            need: total,
        });
    }
    put_head(dst, r.op, plen, r.seq, r.off, r.n);
    let (path, data) = dst[HDR_LEN..total].split_at_mut(r.path.len());
    path.copy_from_slice(r.path);
    data.copy_from_slice(r.data);
    Ok(total)
}

/// Parseert een request; pad en data lenen uit `b`.
pub fn decode_req(b: &[u8]) -> Result<Req<'_>> {
    check_head(b)?;
    let plen = usize::from(le16(b, 2));
    let (path, data) = b[HDR_LEN..].split_at_checked(plen).ok_or(Error::Short {
        len: b.len(),
        need: HDR_LEN + plen,
    })?;
    Ok(Req {
        op: b[1],
        seq: le32(b, 4),
        off: le64(b, 8),
        n: le64(b, 16),
        path,
        data,
    })
}

/// Serialiseert een response in `dst`; geeft de lengte.
pub fn encode_resp(dst: &mut [u8], r: &Resp<'_>) -> Result<usize> {
    let total = HDR_LEN + r.data.len();
    if dst.len() < total {
        return Err(Error::Short {
            len: dst.len(),
            need: total,
        });
    }
    dst[HDR_LEN..total].copy_from_slice(r.data);
    encode_resp_head(dst, r, r.data.len())
}

/// Schrijft alleen de kop van `r` in `dst[..HDR_LEN]`; de `n` databytes
/// staan er al op `dst[HDR_LEN..]` (de aanroeper liet de opslag er direct in
/// lezen). `r.data` wordt genegeerd. Geeft de lengte: zelfde draadvorm als
/// [`encode_resp`], zonder de kopie.
pub fn encode_resp_head(dst: &mut [u8], r: &Resp<'_>, n: usize) -> Result<usize> {
    let total = HDR_LEN.saturating_add(n);
    if dst.len() < total {
        return Err(Error::Short {
            len: dst.len(),
            need: total,
        });
    }
    put_head(dst, r.op, r.status, r.seq, r.size, 0);
    Ok(total)
}

/// Parseert een response; de data leent uit `b`.
pub fn decode_resp(b: &[u8]) -> Result<Resp<'_>> {
    check_head(b)?;
    Ok(Resp {
        op: b[1],
        status: le16(b, 2),
        seq: le32(b, 4),
        size: le64(b, 8),
        data: &b[HDR_LEN..],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Geport uit `TestCtrlOffsetsUniek`: élke offset is uniek,
    /// 8-gealigneerd, binnen de page en buiten de env-regio. Aanleiding
    /// (18-07): `CtrlSMPTcr` en `CtrlIdle` stonden allebei op 0xD8.
    #[test]
    fn ctrl_offsets_uniek() {
        let page = crate::layout::CTRL_STRIDE;
        for (i, &v) in CTRL_WORDS.iter().enumerate() {
            assert!(v.is_multiple_of(8), "{v:#x} niet gealigneerd");
            assert!(v + 8 <= page, "{v:#x} buiten de page");
            assert!(
                v + 8 <= CTRL_ENV_DATA || v >= CTRL_ENV_DATA + CTRL_ENV_MAX,
                "{v:#x} overlapt de env-regio"
            );
            assert!(
                !CTRL_WORDS[i + 1..].contains(&v),
                "OFFSET-COLLISIE op {v:#x}"
            );
        }
        const { assert!(CTRL_WORDS.len() >= 20) };
    }

    /// Het bronwoord van het RNG-blok: alleen met de magic, alleen een
    /// bekende bron, en tekst (een env op een oude kern) is nooit een bron.
    #[test]
    fn rng_source_needs_the_magic() {
        for s in [RNG_SRC_JITTER, RNG_SRC_RNDR, RNG_SRC_SMCCC, RNG_SRC_SOC] {
            assert_eq!(rng_source(rng_source_word(s)), Some(s));
        }
        assert_eq!(rng_source(0), None);
        assert_eq!(rng_source(rng_source_word(0)), None);
        assert_eq!(rng_source(rng_source_word(9)), None);
        assert_eq!(rng_source(rng_source_word(RNG_SRC_SOC) | 0x100), None);
        assert_eq!(rng_source(u64::from_le_bytes(*b"HOP_X=1\n")), None);
        assert_eq!(CTRL_ENV_LEGACY_MAX, 0xEA8);
        assert_eq!(CTRL_ENV_MAX, 0xE78);
    }

    #[test]
    fn req_roundtrip() {
        let r = Req {
            op: OP_WRITE,
            seq: 7,
            off: 1 << 40,
            n: 3,
            path: b"/data/x",
            data: b"abc",
        };
        let mut buf = [0u8; 64];
        let n = encode_req(&mut buf, &r).unwrap();
        assert_eq!(n, HDR_LEN + 7 + 3);
        assert_eq!(decode_req(&buf[..n]).unwrap(), r);
        assert!(matches!(
            decode_req(&buf[..HDR_LEN + 3]),
            Err(Error::Short { .. })
        ));
        buf[0] = 2;
        assert!(matches!(
            decode_req(&buf[..n]),
            Err(Error::BadVersion { got: 2, want: 1 })
        ));
    }

    #[test]
    fn resp_roundtrip_en_kop_alleen() {
        let r = Resp {
            op: OP_READ,
            status: STATUS_NO_ENT,
            seq: 9,
            size: 42,
            data: b"weg",
        };
        let mut buf = [0u8; 64];
        let n = encode_resp(&mut buf, &r).unwrap();
        assert_eq!(decode_resp(&buf[..n]).unwrap(), r);
        let mut other = [0u8; 64];
        other[HDR_LEN..HDR_LEN + 3].copy_from_slice(b"weg");
        let m = encode_resp_head(&mut other, &Resp { data: &[], ..r }, 3).unwrap();
        assert_eq!(&other[..m], &buf[..n]);
        assert!(decode_resp(&buf[..HDR_LEN - 1]).is_err());
    }

    #[test]
    fn app_status_roundtrip() {
        for s in [
            AppStatus::Empty,
            AppStatus::Booting,
            AppStatus::Ready,
            AppStatus::Exited,
            AppStatus::Staged,
        ] {
            assert_eq!(AppStatus::from_raw(s.raw()), Some(s));
        }
        assert_eq!(AppStatus::from_raw(5), None);
    }
}

// ---------------------------------------------------------------------------
// De codec-payloads.
// ---------------------------------------------------------------------------

/// De payloads van de codec-ops (Go: `OLD/metal/abi/hopabi/codec.go`).
///
/// Ze doen iets anders dan de rest van de ABI: de opslag-ops dragen bytes,
/// deze dragen AANWIJZINGEN naar bytes die al op hun plek liggen. Een beeld
/// van 4K in P010 is 24 MB, bij 24 fps 597 MB/s, en het slot-LAN piekt op
/// 550 MB/s: die beelden kunnen niet over de system-calls, en ze horen er
/// ook niet (handboek §6: een beeld is een grant). De buffer zelf staat in
/// `Req::off`/`Req::n`: een afstand vanaf het begin van de eigen partitie.
///
/// Elk record heeft een vaste lengte, little-endian, zonder varint of
/// optionele velden: de kern leest ze op de rand van zijn vertrouwensgrens.
pub mod codec {
    use crate::{Error, Result};

    /// De lengte van [`OpenArgs`] op de draad.
    pub const OPEN_ARGS_LEN: usize = 8;
    /// De lengte van [`FeedArgs`].
    pub const FEED_ARGS_LEN: usize = 24;
    /// De lengte van [`BufArgs`].
    pub const BUF_ARGS_LEN: usize = 4;
    /// De lengte van één [`Event`].
    pub const EVENT_LEN: usize = 64;

    /// Event: niets.
    pub const EVENT_NONE: u8 = 0;
    /// De stream is herkend; maten en vlakken staan erin.
    pub const EVENT_FORMAT: u8 = 1;
    /// Een invoerbuffer is weer van de app.
    pub const EVENT_CONSUMED: u8 = 2;
    /// Een resultaat staat in de buffer.
    pub const EVENT_PRODUCED: u8 = 3;
    /// Einde van de stream.
    pub const EVENT_DONE: u8 = 4;
    /// De sessie is stuk.
    pub const EVENT_FAULT: u8 = 5;

    fn short(b: &[u8], need: usize) -> Result {
        if b.len() < need {
            return Err(Error::Short { len: b.len(), need });
        }
        Ok(())
    }

    fn le16(b: &[u8], i: usize) -> u16 {
        super::le16(b, i)
    }

    /// Opent een sessie. Codec, richting en pixel zijn de nummering van
    /// `driver-codec`; de app kent die namen via applib.
    ///
    /// ```text
    /// 0 codec u8 | dir u8 | pixel u8 | _ u8 | width u16 | height u16
    /// ```
    #[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
    pub struct OpenArgs {
        /// De codec.
        pub codec: u8,
        /// 0 decode, 1 encode.
        pub dir: u8,
        /// Het pixelformaat.
        pub pixel: u8,
        /// Breedte (hint bij decode).
        pub width: u16,
        /// Hoogte (hint bij decode).
        pub height: u16,
    }

    impl OpenArgs {
        /// De draadvorm.
        #[must_use]
        pub fn encode(&self) -> [u8; OPEN_ARGS_LEN] {
            let mut b = [0u8; OPEN_ARGS_LEN];
            b[0] = self.codec;
            b[1] = self.dir;
            b[2] = self.pixel;
            b[4..6].copy_from_slice(&self.width.to_le_bytes());
            b[6..8].copy_from_slice(&self.height.to_le_bytes());
            b
        }

        /// Leest de draadvorm.
        pub fn decode(b: &[u8]) -> Result<OpenArgs> {
            short(b, OPEN_ARGS_LEN)?;
            Ok(OpenArgs {
                codec: b[0],
                dir: b[1],
                pixel: b[2],
                width: le16(b, 4),
                height: le16(b, 6),
            })
        }
    }

    /// Hoort bij `OP_CODEC_FEED`; de buffer staat in `off`/`n` van de call.
    ///
    /// ```text
    /// 0 handle u32 | flags u32 | filled u64 | tag u64
    /// ```
    #[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
    pub struct FeedArgs {
        /// Het sessiehandvat uit open.
        pub handle: u32,
        /// EOS 1, headers 2, keyframe 4.
        pub flags: u32,
        /// Hoeveel bytes er werkelijk in staan.
        pub filled: u64,
        /// Komt ongewijzigd terug op het resultaat.
        pub tag: u64,
    }

    impl FeedArgs {
        /// De draadvorm.
        #[must_use]
        pub fn encode(&self) -> [u8; FEED_ARGS_LEN] {
            let mut b = [0u8; FEED_ARGS_LEN];
            b[0..4].copy_from_slice(&self.handle.to_le_bytes());
            b[4..8].copy_from_slice(&self.flags.to_le_bytes());
            b[8..16].copy_from_slice(&self.filled.to_le_bytes());
            b[16..24].copy_from_slice(&self.tag.to_le_bytes());
            b
        }

        /// Leest de draadvorm.
        pub fn decode(b: &[u8]) -> Result<FeedArgs> {
            short(b, FEED_ARGS_LEN)?;
            Ok(FeedArgs {
                handle: super::le32(b, 0),
                flags: super::le32(b, 4),
                filled: super::le64(b, 8),
                tag: super::le64(b, 16),
            })
        }
    }

    /// De kale sessieverwijzing van offer, poll en close.
    #[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
    pub struct BufArgs {
        /// Het sessiehandvat.
        pub handle: u32,
    }

    impl BufArgs {
        /// De draadvorm.
        #[must_use]
        pub fn encode(&self) -> [u8; BUF_ARGS_LEN] {
            self.handle.to_le_bytes()
        }

        /// Leest de draadvorm.
        pub fn decode(b: &[u8]) -> Result<BufArgs> {
            short(b, BUF_ARGS_LEN)?;
            Ok(BufArgs {
                handle: super::le32(b, 0),
            })
        }
    }

    /// Eén event uit een poll, met de buffer waar het over gaat. `off` is
    /// weer de afstand vanaf het begin van de partitie, zodat de app hem
    /// herkent zonder iets van fysiek geheugen te weten.
    ///
    /// Een Format-event gaat over geen buffer en hergebruikt twee velden:
    /// `size` is de minimale buffermaat, `bytes` het aantal buffers dat het
    /// ijzer tegelijk wil vasthouden.
    ///
    /// ```text
    /// 0  kind u8 | key u8 | pixel u8 | _ u8
    /// 4  width u16 | height u16
    /// 8  off u64 | size u64 | bytes u64 | tag u64
    /// 40 stride[3] u16 | _ u16
    /// 48 plane[3] u32 | _ u32      (= 64)
    /// ```
    ///
    /// De soorten zijn bewust NIET de nummering van `driver-codec`: dit is
    /// een draadformaat, en dat verschuift niet als er intern een soort
    /// bijkomt. De kern vertaalt expliciet.
    #[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
    pub struct Event {
        /// `EVENT_*`.
        pub kind: u8,
        /// Bij een bitstream-resultaat: een keyframe.
        pub key: bool,
        /// Bij Format: het pixelformaat.
        pub pixel: u8,
        /// Zichtbaar beeld.
        pub width: u16,
        /// Zichtbaar beeld.
        pub height: u16,
        /// De buffer, vanaf het begin van de partitie.
        pub off: u64,
        /// De maat van de buffer; bij Format de minimale buffermaat.
        pub size: u64,
        /// Bruikbare bytes (0 = niets); bij Format het aantal buffers.
        pub bytes: u64,
        /// De tag van de invoer.
        pub tag: u64,
        /// Bytes per regel per vlak; 0 = het vlak bestaat niet.
        pub stride: [u16; 3],
        /// Begin van elk vlak, vanaf het begin van de buffer.
        pub plane: [u32; 3],
    }

    impl Event {
        /// Schrijft het event op `b[..EVENT_LEN]`; te kort schrijft niets.
        pub fn encode(&self, b: &mut [u8]) -> Result {
            let n = b.len();
            let b = b.get_mut(..EVENT_LEN).ok_or(Error::Short {
                len: n,
                need: EVENT_LEN,
            })?;
            b.fill(0);
            b[0] = self.kind;
            b[1] = u8::from(self.key);
            b[2] = self.pixel;
            b[4..6].copy_from_slice(&self.width.to_le_bytes());
            b[6..8].copy_from_slice(&self.height.to_le_bytes());
            b[8..16].copy_from_slice(&self.off.to_le_bytes());
            b[16..24].copy_from_slice(&self.size.to_le_bytes());
            b[24..32].copy_from_slice(&self.bytes.to_le_bytes());
            b[32..40].copy_from_slice(&self.tag.to_le_bytes());
            for (i, s) in self.stride.iter().enumerate() {
                b[40 + 2 * i..42 + 2 * i].copy_from_slice(&s.to_le_bytes());
            }
            for (i, p) in self.plane.iter().enumerate() {
                b[48 + 4 * i..52 + 4 * i].copy_from_slice(&p.to_le_bytes());
            }
            Ok(())
        }

        /// Leest één event.
        pub fn decode(b: &[u8]) -> Result<Event> {
            short(b, EVENT_LEN)?;
            let mut e = Event {
                kind: b[0],
                key: b[1] != 0,
                pixel: b[2],
                width: le16(b, 4),
                height: le16(b, 6),
                off: super::le64(b, 8),
                size: super::le64(b, 16),
                bytes: super::le64(b, 24),
                tag: super::le64(b, 32),
                ..Event::default()
            };
            for i in 0..3 {
                e.stride[i] = le16(b, 40 + 2 * i);
                e.plane[i] = super::le32(b, 48 + 4 * i);
            }
            Ok(e)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// De offsets van Go's `EncodeEvent`, byte voor byte.
        #[test]
        fn event_bytes_are_those_of_go() {
            let e = Event {
                kind: EVENT_PRODUCED,
                key: true,
                pixel: 4,
                width: 3840,
                height: 2160,
                off: 0x1000,
                size: 0x2000,
                bytes: 12,
                tag: 0xcafe,
                stride: [7680, 7680, 0],
                plane: [0, 0x7e_9000, 0],
            };
            let mut b = [0xffu8; EVENT_LEN];
            e.encode(&mut b).unwrap();
            assert_eq!(&b[..4], &[3, 1, 4, 0]);
            assert_eq!(&b[4..8], &[0x00, 0x0f, 0x70, 0x08]);
            assert_eq!(&b[32..40], &0xcafeu64.to_le_bytes());
            assert_eq!(&b[40..42], &7680u16.to_le_bytes());
            assert_eq!(&b[52..56], &0x7e_9000u32.to_le_bytes());
            assert_eq!(&b[60..64], &[0; 4]);
            assert_eq!(Event::decode(&b).unwrap(), e);
            assert!(Event::decode(&b[..63]).is_err());
            assert!(e.encode(&mut [0u8; 10]).is_err());
        }

        #[test]
        fn args_roundtrip_and_refuse_short() {
            let o = OpenArgs {
                codec: 2,
                dir: 0,
                pixel: 4,
                width: 3840,
                height: 2160,
            };
            assert_eq!(OpenArgs::decode(&o.encode()).unwrap(), o);
            assert!(OpenArgs::decode(&[0; 7]).is_err());
            let f = FeedArgs {
                handle: 3,
                flags: 1,
                filled: 1 << 40,
                tag: 9,
            };
            assert_eq!(FeedArgs::decode(&f.encode()).unwrap(), f);
            assert!(FeedArgs::decode(&[0; 23]).is_err());
            let b = BufArgs { handle: 7 };
            assert_eq!(BufArgs::decode(&b.encode()).unwrap(), b);
            assert!(BufArgs::decode(&[0; 3]).is_err());
        }
    }
}
