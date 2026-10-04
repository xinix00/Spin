//! De volumes van een start: een lengtegecodeerde blob achter de poorten
//! van [`super::StartReq`].
//!
//! Per volume twee `u16`-lengtes (little-endian), dan het lokale pad (zoals
//! de app het ziet) en het gedeelde pad (in de hopfs-boom):
//!
//! ```text
//! local_len u16 | shared_len u16 | local | shared      (herhaald)
//! ```
//!
//! Nul bytes is het formaat van vóór de volumes (alpha.10): een Hop die
//! niets meestuurt, stuurt precies dezelfde bytes als toen. Deze module
//! toetst alleen de VORM (lengtes, aantal, niet leeg); wat een pad mag
//! (geen `/`, niets onder `/.tasks`, geen `..`) beslist de kern bij het
//! normaliseren (`kern::rpc::mount_table`), want dat is de toegangsgrens.

use crate::{Error, Result};

/// Zoveel volumes draagt één start: de grens die de lifecycle en de flip
/// ook bewaren (`kern::kernflip::MAX_FLIP_MOUNTS`, die deze waarde neemt).
pub const MAX_START_MOUNTS: usize = 32;
/// Het langste lokale of gedeelde pad. Gelijk aan wat het handoff-blob van
/// de flip per pad draagt (`kern::kernflip::MAX_FLIP_PATH`): een volume dat de
/// start aannam maar de flip niet kan overdragen, zou een flip later
/// weigeren om iets dat bij de start al vaststond.
pub const MAX_MOUNT_PATH: usize = 256;
/// De grootste blob: elk volume met twee paden van de grootste lengte.
pub const MAX_MOUNT_BYTES: usize = MAX_START_MOUNTS * (4 + 2 * MAX_MOUNT_PATH);

// Een lengte past in zijn `u16`, en de hele blob in `mounts_len` (`u32`).
const _: () = assert!(MAX_MOUNT_PATH <= u16::MAX as usize);
const _: () = assert!(MAX_MOUNT_BYTES <= u32::MAX as usize);

/// Eén geleend volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MountRef<'a> {
    /// Het pad zoals de app het ziet (`/data`).
    pub local: &'a [u8],
    /// Het gedeelde pad in de hopfs-boom (`/volumes/demo`).
    pub shared: &'a [u8],
}

/// Toetst de vorm van één pad: niet leeg, niet te lang.
fn path(p: &[u8]) -> Result {
    if p.is_empty() {
        return Err(Error::Missing("mount path"));
    }
    if p.len() > MAX_MOUNT_PATH {
        return Err(Error::PayloadTooLarge {
            len: p.len(),
            max: MAX_MOUNT_PATH,
        });
    }
    Ok(())
}

/// Schrijft `mounts` in de draadvorm in `dst`; geeft de lengte. Meer dan
/// [`MAX_START_MOUNTS`], een leeg of te lang pad, of een te kort `dst` is
/// een fout vóór er één byte geschreven is.
pub fn mount_blob(mounts: &[MountRef<'_>], dst: &mut [u8]) -> Result<usize> {
    if mounts.len() > MAX_START_MOUNTS {
        return Err(Error::TooMany {
            what: "start mounts",
            cap: MAX_START_MOUNTS,
        });
    }
    let mut need = 0;
    for m in mounts {
        path(m.local)?;
        path(m.shared)?;
        need += 4 + m.local.len() + m.shared.len();
    }
    let len = dst.len();
    let out = dst.get_mut(..need).ok_or(Error::Short { len, need })?;
    let mut at = 0;
    for m in mounts {
        // Past: `path` begrensde beide op MAX_MOUNT_PATH (assertie hierboven).
        out[at..at + 2].copy_from_slice(&(m.local.len() as u16).to_le_bytes());
        out[at + 2..at + 4].copy_from_slice(&(m.shared.len() as u16).to_le_bytes());
        at += 4;
        out[at..at + m.local.len()].copy_from_slice(m.local);
        at += m.local.len();
        out[at..at + m.shared.len()].copy_from_slice(m.shared);
        at += m.shared.len();
    }
    Ok(need)
}

/// Loopt de volumes van een blob af. Een kapot record (te kort, een leeg of
/// te lang pad, meer dan [`MAX_START_MOUNTS`]) is één `Err`, en daarna is
/// de iterator leeg: niemand leest voorbij een fout.
pub struct Mounts<'a> {
    rest: &'a [u8],
    count: usize,
}

impl<'a> Mounts<'a> {
    /// Begint bij het eerste volume van `bytes`.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            rest: bytes,
            count: 0,
        }
    }
}

impl<'a> Iterator for Mounts<'a> {
    type Item = Result<MountRef<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        // Eruit halen: bij elke fout hieronder blijft `rest` leeg.
        let input = core::mem::take(&mut self.rest);
        if self.count == MAX_START_MOUNTS {
            return Some(Err(Error::TooMany {
                what: "start mounts",
                cap: MAX_START_MOUNTS,
            }));
        }
        let Some(head) = input.get(..4) else {
            return Some(Err(Error::Short {
                len: input.len(),
                need: 4,
            }));
        };
        let local = usize::from(u16::from_le_bytes([head[0], head[1]]));
        let shared = usize::from(u16::from_le_bytes([head[2], head[3]]));
        let need = 4 + local + shared;
        let Some(record) = input.get(4..need) else {
            return Some(Err(Error::Short {
                len: input.len(),
                need,
            }));
        };
        let (local, shared) = record.split_at(local);
        if let Err(e) = path(local).and_then(|()| path(shared)) {
            return Some(Err(e));
        }
        self.rest = input.get(need..).unwrap_or_default();
        self.count += 1;
        Some(Ok(MountRef { local, shared }))
    }
}

/// Toetst een hele blob: hoogstens [`MAX_MOUNT_BYTES`], en elk record heel.
pub(super) fn validate(bytes: &[u8]) -> Result {
    if bytes.len() > MAX_MOUNT_BYTES {
        return Err(Error::PayloadTooLarge {
            len: bytes.len(),
            max: MAX_MOUNT_BYTES,
        });
    }
    for m in Mounts::new(bytes) {
        m?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{hopabi, systemapi::StartReq};
    use std::vec::Vec;

    #[test]
    fn mounts_follow_ports_and_old_starts_remain_byte_compatible() {
        let pairs = [
            MountRef {
                local: b"/media",
                shared: b"/volumes/media",
            },
            MountRef {
                local: b"/db",
                shared: b"/volumes/replica",
            },
        ];
        let mut blob = [0; 128];
        let len = mount_blob(&pairs, &mut blob).unwrap();
        let req = StartReq {
            image_size: 1,
            cores: 1,
            env: b"A=B\n",
            ports: &[80, 0],
            mounts: &blob[..len],
            job: b"lumen",
            ..Default::default()
        };
        let mut bytes = [0; 256];
        let n = req.encode(&mut bytes, 1).unwrap();
        let raw = hopabi::decode_req(&bytes[..n]).unwrap();
        let back = StartReq::decode(&raw).unwrap();
        assert_eq!(back, req);
        assert_eq!(
            Mounts::new(back.mounts)
                .collect::<Result<Vec<_>>>()
                .unwrap(),
            pairs
        );
        for end in n - len..n {
            assert!(StartReq::decode(&hopabi::decode_req(&bytes[..end]).unwrap()).is_err());
        }
        let head = hopabi::HDR_LEN + req.job.len();
        // Dit is exact de lengte die alpha.10 nog eist; die weigert de extra bytes.
        assert!(
            raw.data.len()
                > super::super::START_HEAD_LEN + req.group.len() + req.env.len() + req.ports.len()
        );
        assert_eq!(&bytes[head + 28..head + 32], &(len as u32).to_le_bytes());
        let old = StartReq { mounts: &[], ..req };
        let old_n = old.encode(&mut bytes, 1).unwrap();
        assert_eq!(&bytes[head + 28..head + 32], &[0; 4]);
        assert_eq!(
            StartReq::decode(&hopabi::decode_req(&bytes[..old_n]).unwrap()).unwrap(),
            old
        );
        // Een lengtewoord kan geen extra ongecontroleerde data in de payload verbergen.
        bytes[head + 28..head + 32].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(StartReq::decode(&hopabi::decode_req(&bytes[..old_n]).unwrap()).is_err());
    }

    #[test]
    fn malformed_records_and_excessive_counts_are_refused() {
        for bytes in [
            &[1u8][..],
            &[0, 0, 1, 0, b'/'],
            &[1, 0, 0, 0, b'/'],
            &[255, 255, 1, 0, b'/'],
        ] {
            assert!(validate(bytes).is_err());
        }
        let pair = MountRef {
            local: b"/a",
            shared: b"/b",
        };
        assert!(mount_blob(&[pair; MAX_START_MOUNTS + 1], &mut []).is_err());
        assert!(mount_blob(&[pair], &mut [0; 7]).is_err());
        let mut raw = [0; 8];
        mount_blob(&[pair], &mut raw).unwrap();
        assert!(validate(&raw.repeat(MAX_START_MOUNTS + 1)).is_err());
        let long = [b'a'; MAX_MOUNT_PATH + 1];
        assert!(
            mount_blob(
                &[MountRef {
                    local: &long,
                    shared: b"/a"
                }],
                &mut []
            )
            .is_err()
        );
    }
}
