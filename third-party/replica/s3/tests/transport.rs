//! De echte SigV4-client, met begrensde bodies, fouten en onderbroken requests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use leans3::{AsyncRead, IoError, Request, Response, Transport};
use replica_core::object::{Store, StoreError};
use replica_sqlite::asynchronous::{Cancelled, Suspend};
use std::{
    cell::RefCell,
    collections::VecDeque,
    future::Future,
    pin::{Pin, pin},
    rc::Rc,
    task::{Context, Poll, Waker},
};
struct Reply {
    status: u16,
    bytes: Vec<u8>,
    at: usize,
    length: Option<u64>,
}
impl AsyncRead for Reply {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        dst: &mut [u8],
    ) -> Poll<Result<usize, IoError>> {
        let this = self.get_mut();
        let n = dst.len().min(this.bytes.len() - this.at);
        dst[..n].copy_from_slice(&this.bytes[this.at..this.at + n]);
        this.at += n;
        Poll::Ready(Ok(n))
    }
}
impl Response for Reply {
    fn status(&self) -> u16 {
        self.status
    }
    fn reason(&self) -> &str {
        ""
    }
    fn header(&self, _: &str) -> Option<&str> {
        None
    }
    fn content_length(&self) -> Option<u64> {
        self.length
    }
}
#[derive(Default)]
struct World {
    replies: VecDeque<Reply>,
    calls: Vec<String>,
    pause: bool,
}
struct Net(Rc<RefCell<World>>);
impl Transport for Net {
    type Response = Reply;
    async fn send(&mut self, r: Request<'_, '_>) -> Result<Reply, IoError> {
        assert!(
            r.headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("authorization")
                    && h.value.starts_with("AWS4-HMAC-SHA256 "))
        );
        assert_eq!(r.host, "s3.example.test");
        self.0
            .borrow_mut()
            .calls
            .push(format!("{} {}", r.method, r.target));
        let pause = self.0.borrow().pause;
        if pause {
            std::future::pending::<()>().await;
        }
        self.0
            .borrow_mut()
            .replies
            .pop_front()
            .ok_or(IoError::Closed)
    }
}
struct Wait {
    cancel: bool,
}
impl Suspend for Wait {
    fn wait<F: Future>(&self, f: F) -> Result<F::Output, Cancelled> {
        let mut f = pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        match f.as_mut().poll(&mut cx) {
            Poll::Ready(r) => Ok(r),
            Poll::Pending if self.cancel => Err(Cancelled),
            Poll::Pending => panic!("unexpected pending in fixture"),
        }
    }
}
fn client() -> leans3::Client {
    leans3::Client {
        endpoint: "https://s3.example.test".into(),
        bucket: "replica".into(),
        region: "auto".into(),
        access_key_id: "test-access".into(),
        secret_access_key: "test-secret".into(),
        session_token: String::new(),
        path_style: true,
        now: Some(|| 1_790_765_296),
    }
}
fn reply(world: &Rc<RefCell<World>>, status: u16, bytes: Vec<u8>, length: Option<u64>) {
    world.borrow_mut().replies.push_back(Reply {
        status,
        bytes,
        at: 0,
        length,
    });
}
#[test]
fn large_segment_streams_with_budget_and_checks_truncation() {
    let world = Rc::new(RefCell::new(World::default()));
    let mut s = replica_s3::S3::new(client(), Net(world.clone()), Wait { cancel: false }).unwrap();
    let data = vec![37; 5 << 20];
    reply(&world, 200, data.clone(), Some(data.len() as u64));
    assert_eq!(s.get("generation/data/one", data.len()).unwrap(), data);
    reply(&world, 200, vec![1; 8192], None);
    assert!(matches!(
        s.get("generation/data/large", 4096),
        Err(StoreError::Limit)
    ));
    reply(&world, 200, vec![1; 10], Some(20));
    assert!(matches!(
        s.get("generation/data/short", 100),
        Err(StoreError::Transport)
    ));
    reply(&world, 404, Vec::new(), Some(0));
    assert!(matches!(s.get("missing", 100), Err(StoreError::Missing)));
    reply(&world, 403, Vec::new(), Some(0));
    assert!(matches!(s.get("denied", 100), Err(StoreError::Denied)));
}
#[test]
fn complete_list_only_and_delete_absent_is_success() {
    let world = Rc::new(RefCell::new(World::default()));
    let mut s = replica_s3::S3::new(client(), Net(world.clone()), Wait { cancel: false }).unwrap();
    let body=b"<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>next</NextContinuationToken><Contents><Key>gen/L0/one.json</Key></Contents></ListBucketResult>".to_vec();
    reply(&world, 200, body, None);
    assert!(matches!(s.list("gen/L", 1), Err(StoreError::Limit)));
    let body=b"<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>gen/L0/one.json</Key></Contents></ListBucketResult>".to_vec();
    reply(&world, 200, body, None);
    let list = s.list("gen/L", 2).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].key, "gen/L0/one.json");
    reply(&world, 404, Vec::new(), Some(0));
    s.delete("missing").unwrap();
}
#[test]
fn cancellation_after_request_disables_owner_and_invalid_clock_sends_nothing() {
    let world = Rc::new(RefCell::new(World {
        pause: true,
        ..World::default()
    }));
    let mut s = replica_s3::S3::new(client(), Net(world.clone()), Wait { cancel: true }).unwrap();
    assert_eq!(s.put("commit", b"manifest"), Err(StoreError::Cancelled));
    assert!(matches!(s.get("commit", 100), Err(StoreError::Cancelled)));
    assert_eq!(world.borrow().calls.len(), 1);
    let mut c = client();
    c.now = Some(|| 1);
    assert!(matches!(
        replica_s3::S3::new(c, Net(world.clone()), Wait { cancel: false }),
        Err(StoreError::Configuration)
    ));
    assert_eq!(world.borrow().calls.len(), 1);
}

#[test]
fn directory_pages_skip_segments_and_batches_allow_explicit_truncation() {
    let world = Rc::new(RefCell::new(World::default()));
    let mut s = replica_s3::S3::new(client(), Net(world.clone()), Wait { cancel: false }).unwrap();
    reply(&world,200,b"<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>two</NextContinuationToken><CommonPrefixes><Prefix>gen/one/</Prefix></CommonPrefixes></ListBucketResult>".to_vec(),None);
    reply(&world,200,b"<ListBucketResult><IsTruncated>false</IsTruncated><CommonPrefixes><Prefix>gen/two/</Prefix></CommonPrefixes></ListBucketResult>".to_vec(),None);
    assert_eq!(s.directories("gen/", 10).unwrap(), ["gen/one/", "gen/two/"]);
    assert!(world.borrow().calls[0].contains("?delimiter=%2F&list-type=2&prefix=gen%2F"));
    assert!(world.borrow().calls[1].contains("?continuation-token=two&delimiter=%2F&list-type=2"));
    reply(&world,200,b"<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>later</NextContinuationToken><Contents><Key>gen/one/data/1</Key></Contents><Contents><Key>gen/one/data/2</Key></Contents></ListBucketResult>".to_vec(),None);
    let batch = s.list_batch("gen/one/data/", 1).unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].key, "gen/one/data/1");
    for prefix in ["gen/", "gen/one/nested/", "foreign/one/"] {
        reply(&world,200,format!("<ListBucketResult><CommonPrefixes><Prefix>{prefix}</Prefix></CommonPrefixes></ListBucketResult>").into_bytes(),None);
        assert!(matches!(
            s.directories("gen/", 10),
            Err(StoreError::Transport)
        ));
    }
}
