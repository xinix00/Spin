//! De store-ops van een app: een kopie op afroep tussen de eigen map van de
//! job in de object-store (`apps/<cluster>/<job>/`) en het eigen zicht op
//! hopfs (Go: `App.Pull`, `Push`, `StoreList`, `StoreDrop`).
//!
//! Persistentie is een daad: pull bij de start wat je vorige leven pushte
//! (de eigen root is bij elke start leeg), push wanneer het bewaard moet
//! zijn. Er is geen synchronisatie op de achtergrond, want die belooft
//! persistentie die er tussen twee uploads niet is.
//!
//! De app kiest alleen een NAAM binnen zijn eigen map; bucket, endpoint en
//! sleutels zijn van de operator, en de prefix bouwt Hop. De kern weigert
//! `..` en een lege naam. Een call duurt zo lang als het object groot is
//! (Hop stroomt hem tussen de bucket en hopfs), dus de timeout is
//! [`sys::STORE_TIMEOUT`], en de verbinding is zolang bezet: één call
//! tegelijk per client.
//!
//! Elke op is idempotent (pull en push vervangen, drop van iets dat er niet
//! is, is geen fout), dus de ene herhaling van de client na een weggevallen
//! verbinding (een kern-flip) is veilig.

use crate::sys::{self, Client, Dial, Req, Result, Timer};
use abi::hopabi::{OP_STORE_DROP, OP_STORE_LIST, OP_STORE_PULL, OP_STORE_PUSH};

/// De grootste lijst namen in één antwoord (8 KiB): een buffer van deze maat
/// past altijd voor [`list`]. Past de lijst niet, dan is dat een fout van de
/// kern ("use a narrower prefix"), nooit een afgekapte lijst.
pub const LIST_MAX: usize = abi::systemapi::store::MAX_STORE_LIST;

/// Eén store-call: de naam in `path`, een ander lokaal pad in `data`.
async fn call<D: Dial, T: Timer>(
    c: &mut Client<D, T>,
    op: u8,
    name: &str,
    local: &str,
    dst: &mut [u8],
) -> Result<(sys::Resp, usize)> {
    let req = Req {
        data: local.as_bytes(),
        ..Req::path(op, name)
    };
    c.call(req, dst, sys::STORE_TIMEOUT).await
}

/// Haalt object `name` uit de eigen map en VERVANGT er het lokale bestand
/// met dezelfde naam mee (maakt bestand en ouder-mappen); geeft de maat.
/// Bestaat het object niet, dan blijft het lokale bestand onaangeraakt en
/// komt er [`sys::Error::NotFound`] terug.
pub async fn pull<D: Dial, T: Timer>(c: &mut Client<D, T>, name: &str) -> Result<u64> {
    pull_to(c, name, "").await
}

/// Als [`pull`], maar naar het lokale pad `local` (leeg: dezelfde naam).
pub async fn pull_to<D: Dial, T: Timer>(
    c: &mut Client<D, T>,
    name: &str,
    local: &str,
) -> Result<u64> {
    let (r, _) = call(c, OP_STORE_PULL, name, local, &mut []).await?;
    Ok(r.size)
}

/// Uploadt het lokale bestand `name` als object `name` (vervangend); geeft
/// de maat. De inhoud is die van het moment van de call: schrijf het
/// bestand af vóór je pusht (Go: een push waar je zelf doorheen schrijft,
/// is een luide fout, nooit stil een corrupt object).
pub async fn push<D: Dial, T: Timer>(c: &mut Client<D, T>, name: &str) -> Result<u64> {
    push_from(c, "", name).await
}

/// Als [`push`], maar van het lokale pad `local` (leeg: dezelfde naam).
pub async fn push_from<D: Dial, T: Timer>(
    c: &mut Client<D, T>,
    local: &str,
    name: &str,
) -> Result<u64> {
    let (r, _) = call(c, OP_STORE_PUSH, name, local, &mut []).await?;
    Ok(r.size)
}

/// De objectnamen in de eigen map onder `prefix` ("" is alles), relatief
/// aan de map: een naam uit de lijst kan zo naar [`pull`]. De match is die
/// van een object-store, geen mappenboom: "db" vindt ook "dbx.json"; sluit
/// af met "/" voor een map. `dst` krijgt de namen; [`sys::names`] splitst.
pub async fn list<'b, D: Dial, T: Timer>(
    c: &mut Client<D, T>,
    prefix: &str,
    dst: &'b mut [u8],
) -> Result<impl Iterator<Item = &'b str> + use<'b, D, T>> {
    let (_, n) = call(c, OP_STORE_LIST, prefix, "", dst).await?;
    let got: &'b [u8] = dst.get(..n).unwrap_or_default();
    Ok(sys::names(got))
}

/// Verwijdert object `name` uit de eigen map; een object dat er niet is,
/// is geen fout.
pub async fn drop<D: Dial, T: Timer>(c: &mut Client<D, T>, name: &str) -> Result {
    call(c, OP_STORE_DROP, name, "", &mut []).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{Conn, ConnError};
    use abi::hopabi::{self, STATUS_NO_ENT, STATUS_OK};
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use core::time::Duration;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::string::String;
    use std::vec::Vec;

    fn block_on<F: Future>(f: F) -> F::Output {
        let mut f = pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..1000 {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
        }
        panic!("future bleef hangen");
    }

    /// Kern en Hop samen, in RAM: lokale bestanden en een bucket.
    #[derive(Default)]
    struct World {
        local: BTreeMap<String, Vec<u8>>,
        bucket: BTreeMap<String, Vec<u8>>,
        ops: Vec<(u8, String, String)>,
    }

    struct FakeConn {
        w: Rc<RefCell<World>>,
        tx: Vec<u8>,
        rx: Vec<u8>,
    }

    impl FakeConn {
        /// Beantwoordt een compleet frame in `tx`.
        fn answer(&mut self) {
            if self.tx.len() < 12 {
                return;
            }
            let n = u32::from_le_bytes(self.tx[8..12].try_into().unwrap()) as usize;
            if self.tx.len() < 12 + n {
                return;
            }
            let frame: Vec<u8> = self.tx.drain(..12 + n).collect();
            let req = hopabi::decode_req(&frame[12..]).unwrap();
            let name = String::from_utf8(req.path.to_vec()).unwrap();
            let local = String::from_utf8(req.data.to_vec()).unwrap();
            let file = if local.is_empty() {
                name.clone()
            } else {
                local.clone()
            };
            let mut w = self.w.borrow_mut();
            w.ops.push((req.op, name.clone(), local));
            let (status, size, data): (u16, u64, Vec<u8>) = match req.op {
                OP_STORE_PUSH => match w.local.get(&file).cloned() {
                    Some(b) => {
                        let len = b.len() as u64;
                        w.bucket.insert(name, b);
                        (STATUS_OK, len, Vec::new())
                    }
                    None => (STATUS_NO_ENT, 0, b"no such file".to_vec()),
                },
                OP_STORE_PULL => match w.bucket.get(&name).cloned() {
                    Some(b) => {
                        let len = b.len() as u64;
                        w.local.insert(file, b);
                        (STATUS_OK, len, Vec::new())
                    }
                    None => (STATUS_NO_ENT, 0, b"no such object".to_vec()),
                },
                OP_STORE_LIST => {
                    let names: Vec<&str> = w
                        .bucket
                        .keys()
                        .filter(|k| k.starts_with(&name))
                        .map(String::as_str)
                        .collect();
                    (STATUS_OK, names.len() as u64, names.join("\n").into_bytes())
                }
                _ => {
                    w.bucket.remove(&name);
                    (STATUS_OK, 0, Vec::new())
                }
            };
            let mut resp = std::vec![0u8; hopabi::HDR_LEN + data.len()];
            let r = hopabi::Resp {
                op: req.op,
                status,
                seq: req.seq,
                size,
                data: &data,
            };
            let len = hopabi::encode_resp(&mut resp, &r).unwrap();
            self.rx
                .extend_from_slice(&sys::frame_header(2, u32::try_from(len).unwrap()));
            self.rx.extend_from_slice(&resp[..len]);
        }
    }

    impl Conn for FakeConn {
        async fn read(&mut self, buf: &mut [u8]) -> core::result::Result<usize, ConnError> {
            self.answer();
            let n = buf.len().min(self.rx.len());
            buf[..n].copy_from_slice(&self.rx[..n]);
            self.rx.drain(..n);
            Ok(n)
        }
        async fn write(&mut self, buf: &[u8]) -> core::result::Result<usize, ConnError> {
            self.tx.extend_from_slice(buf);
            Ok(buf.len())
        }
    }

    struct FakeDial(Rc<RefCell<World>>);

    impl Dial for FakeDial {
        type Conn = FakeConn;
        async fn dial(&mut self) -> core::result::Result<FakeConn, ConnError> {
            Ok(FakeConn {
                w: self.0.clone(),
                tx: Vec::new(),
                rx: Vec::new(),
            })
        }
    }

    struct Never;
    impl Timer for Never {
        fn sleep(&self, _: Duration) -> impl Future<Output = ()> {
            core::future::pending()
        }
    }

    /// De toets van `store_demo.go`: push, list ziet hem, pull naar een
    /// ander pad, drop, list leeg; en een pull van niets is een nette fout.
    #[test]
    fn push_list_pull_to_drop_round_trip() {
        let w = Rc::new(RefCell::new(World::default()));
        w.borrow_mut()
            .local
            .insert(String::from("state.json"), b"leven 1".to_vec());
        let mut c = Client::new(FakeDial(w.clone()), Never);
        assert!(matches!(
            block_on(pull(&mut c, "never.json")),
            Err(sys::Error::NotFound { .. })
        ));
        assert_eq!(block_on(push(&mut c, "state.json")).unwrap(), 7);
        let mut buf = [0u8; LIST_MAX];
        let names: Vec<String> = block_on(list(&mut c, "", &mut buf))
            .unwrap()
            .map(String::from)
            .collect();
        assert_eq!(names, ["state.json"]);
        assert_eq!(
            block_on(pull_to(&mut c, "state.json", "copy.json")).unwrap(),
            7
        );
        assert_eq!(w.borrow().local["copy.json"], b"leven 1");
        block_on(drop(&mut c, "state.json")).unwrap();
        assert_eq!(block_on(list(&mut c, "", &mut buf)).unwrap().count(), 0);
        // De bytes op de draad: de naam in `path`, het lokale pad in `data`.
        let ops = &w.borrow().ops;
        assert_eq!(
            ops[3],
            (
                OP_STORE_PULL,
                String::from("state.json"),
                String::from("copy.json")
            )
        );
        assert_eq!(
            ops[1],
            (OP_STORE_PUSH, String::from("state.json"), String::new())
        );
    }
}
