//! Begrensde HTTP-overdrachten; het archief en de verbinding hebben één eigenaar.
use crate::{archive::Temporary, client_net, executor, images, runner_socket};
use spin_core::{docker::Docker, validation::text};
use spin_domain::{
    self as d, TryClone, Wire,
    json::{Object, Value},
    protocol as p,
};
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    time::{Duration, Instant},
};
type Result<T> = std::io::Result<T>;
const CHUNK: usize = 1 << 20;
fn io(e: impl std::fmt::Debug) -> std::io::Error {
    crate::client_net::error(e)
}
fn field<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.as_object()
        .and_then(|o| o.get(key))
        .unwrap_or(&Value::Null)
}
fn segment(value: &str) -> Result<String> {
    let mut out = String::new();
    out.try_reserve(value.len().saturating_mul(3)).map_err(io)?;
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~:".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(b"0123456789ABCDEF"[usize::from(b >> 4)]));
            out.push(char::from(b"0123456789ABCDEF"[usize::from(b & 15)]));
        }
    }
    Ok(out)
}
pub(crate) struct Client {
    endpoint: runner_socket::Endpoint,
    base: String,
    authorization: String,
    addresses: Vec<SocketAddr>,
}
struct Reply {
    status: u16,
    header: leanhttp::Header,
    bytes: Vec<u8>,
}
impl Client {
    pub(crate) fn new(server: &str, token: &str) -> Result<Self> {
        let endpoint = runner_socket::endpoint(server)?;
        let prefix = endpoint.path.strip_suffix("/api/runner/ws").unwrap_or("");
        let base = text(format_args!(
            "{}://{}{prefix}",
            if endpoint.encrypted { "https" } else { "http" },
            endpoint.authority
        ))
        .map_err(io)?;
        let authorization = text(format_args!("Bearer {token}")).map_err(io)?;
        let mut addresses = Vec::new();
        for address in (endpoint.host.as_str(), endpoint.port)
            .to_socket_addrs()?
            .take(16)
        {
            d::try_push(&mut addresses, address).map_err(io)?;
        }
        if addresses.is_empty() {
            return Err(io("server has no addresses"));
        }
        Ok(Self {
            endpoint,
            base,
            authorization,
            addresses,
        })
    }
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        offset: Option<u64>,
    ) -> Result<Reply> {
        let mut socket = None;
        for address in &self.addresses {
            match TcpStream::connect_timeout(address, Duration::from_millis(500)) {
                Ok(s) => {
                    socket = Some(s);
                    break;
                }
                Err(_) => executor::next_round().await,
            }
        }
        let socket = socket.ok_or_else(|| io("cannot connect to archive server"))?;
        let connection = client_net::connect(
            socket,
            &self.endpoint.host,
            self.endpoint.port,
            self.endpoint.encrypted,
        )
        .await?;
        let url = text(format_args!("{}{path}", self.base)).map_err(io)?;
        let mut header = leanhttp::Header::new();
        header
            .set("Authorization", &self.authorization)
            .map_err(io)?;
        header
            .set(
                "Content-Type",
                if offset.is_some() {
                    "application/octet-stream"
                } else {
                    "application/json"
                },
            )
            .map_err(io)?;
        if let Some(offset) = offset {
            header
                .set(
                    "X-Spin-Upload-Offset",
                    &text(format_args!("{offset}")).map_err(io)?,
                )
                .map_err(io)?;
        }
        let until = Instant::now() + Duration::from_secs(120);
        let mut operation = std::pin::pin!(async {
            let mut response = leanhttp::send(
                connection,
                leanhttp::Call {
                    method,
                    url: &url,
                    header,
                    body: Some(body),
                    header_timeout: Some(Duration::from_secs(120)),
                    no_follow: true,
                    ..Default::default()
                },
            )
            .await
            .map_err(io)?;
            let bytes = response.read_to_end(CHUNK).await.map_err(io)?;
            Ok(Reply {
                status: response.status,
                header: response.header,
                bytes,
            })
        });
        std::future::poll_fn(|cx| {
            if Instant::now() >= until {
                return std::task::Poll::Ready(Err(std::io::ErrorKind::TimedOut.into()));
            }
            operation.as_mut().poll(cx)
        })
        .await
    }
    // GETs and chunk writes are idempotent. Creation is intentionally not retried.
    async fn retry(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        offset: Option<u64>,
    ) -> Result<Reply> {
        let pause_until = Instant::now() + Duration::from_secs(1800);
        let mut attempt = 0;
        loop {
            let result = self.request(method, path, body, offset).await;
            if let Ok(reply) = &result {
                if reply.status < 500 {
                    return result;
                }
                if reply.status == 503 && Instant::now() < pause_until {
                    wait(Duration::from_secs(10)).await;
                    continue;
                }
            }
            attempt += 1;
            if attempt >= 5 {
                return result;
            }
            wait(Duration::from_millis(500 * (1 << (attempt - 1)))).await;
        }
    }
    pub(crate) async fn upload(
        &self,
        kind: &str,
        name: &str,
        snapshot: Option<&d::CapsuleSnapshot>,
        file: &mut Temporary,
    ) -> Result<p::ArchiveResult> {
        let size = file.file.metadata()?.len();
        if size == 0 || size > crate::archive::FILE_LIMIT {
            return Err(io("invalid archive size"));
        }
        let mut meta = Object::new();
        for (key, value) in [
            ("kind", Value::string(kind).map_err(io)?),
            ("name", Value::string(name).map_err(io)?),
            ("size", Value::uint(size)),
        ] {
            meta.push(key, value).map_err(io)?;
        }
        if let Some(snapshot) = snapshot {
            let mut snapshot = snapshot.try_clone().map_err(io)?;
            snapshot.contents = None;
            meta.push("snapshot", snapshot.to_value().map_err(io)?)
                .map_err(io)?;
        }
        let reply = self
            .request(
                "POST",
                "/api/uploads",
                Value::Object(meta).to_json().map_err(io)?.as_bytes(),
                None,
            )
            .await?;
        if reply.status != 201 {
            return Err(io(("create upload", reply.status)));
        }
        let status = Value::from_json(&reply.bytes).map_err(io)?;
        let id = field(&status, "id")
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| io("upload has no id"))?;
        let path = text(format_args!("/api/uploads/{}", segment(id)?)).map_err(io)?;
        let result = async {
            let chunk_size = field(&status, "chunk_size")
                .as_i64()
                .filter(|n| *n > 0 && *n <= CHUNK as i64)
                .ok_or_else(|| io("invalid upload chunk size"))?
                as usize;
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(chunk_size).map_err(io)?;
            bytes.resize(chunk_size, 0);
            file.rewind()?;
            let mut offset = 0;
            while offset < size {
                let n = (size - offset).min(chunk_size as u64) as usize;
                file.file.read_exact(&mut bytes[..n])?;
                let reply = self.retry("PUT", &path, &bytes[..n], Some(offset)).await?;
                if reply.status != 200 {
                    return Err(io(("upload chunk", reply.status)));
                }
                let status = Value::from_json(&reply.bytes).map_err(io)?;
                if field(&status, "offset").as_i64().unwrap_or(-1) < (offset + n as u64) as i64 {
                    return Err(io("upload did not commit chunk"));
                }
                offset += n as u64;
                executor::next_round().await;
            }
            let finish = text(format_args!("{path}/complete")).map_err(io)?;
            let reply = self.retry("POST", &finish, &[], None).await?;
            if reply.status != 200 {
                return Err(io(("complete upload", reply.status)));
            }
            let result = p::ArchiveResult::from_json(&reply.bytes).map_err(io)?;
            if result.size != size as i64 {
                return Err(io("archive size changed"));
            }
            Ok(result)
        }
        .await;
        if result.is_err() {
            let _ = self.request("DELETE", &path, &[], None).await;
        }
        result
    }
    pub(crate) async fn archive(
        &self,
        docker: &Docker,
        snapshot: &d::CapsuleSnapshot,
    ) -> Result<p::ArchiveResult> {
        let mut file = images::export(docker, snapshot).await?;
        self.upload("snapshot", &snapshot.r#ref, Some(snapshot), &mut file)
            .await
    }
    pub(crate) async fn download(
        &self,
        path: &str,
        expected: i64,
        limit: u64,
    ) -> Result<Temporary> {
        let mut file = Temporary::new()?;
        let mut offset = 0;
        let mut total = None;
        let mut digest = String::new();
        let mut hash = spin_security::Sha256::new();
        loop {
            let url = text(format_args!("{path}?offset={offset}")).map_err(io)?;
            let reply = self.retry("GET", &url, &[], None).await?;
            if reply.status != 200 {
                return Err(io(("download chunk", reply.status)));
            }
            let size = reply
                .header
                .get("X-Spin-Size")
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| *n > 0 && *n <= limit)
                .ok_or_else(|| io("invalid download size"))?;
            let identity = reply.header.get("X-Spin-Digest").unwrap_or("");
            if total.is_none() {
                if expected > 0 && size != expected as u64 {
                    return Err(io("download size differs from snapshot"));
                }
                total = Some(size);
                digest = d::try_string(identity).map_err(io)?;
            }
            if total != Some(size)
                || identity != digest
                || reply.bytes.is_empty()
                || reply.bytes.len() as u64 > size.saturating_sub(offset)
            {
                return Err(io("download changed or is truncated"));
            }
            hash.update(&reply.bytes);
            file.file.write_all(&reply.bytes)?;
            offset += reply.bytes.len() as u64;
            if offset == size {
                break;
            }
            executor::next_round().await;
        }
        if !digest.is_empty() {
            let actual = hash.finish();
            let mut encoded = d::try_string("sha256:").map_err(io)?;
            encoded.try_reserve_exact(64).map_err(io)?;
            for b in actual {
                encoded.push(char::from(b"0123456789abcdef"[usize::from(b >> 4)]));
                encoded.push(char::from(b"0123456789abcdef"[usize::from(b & 15)]));
            }
            if encoded != digest {
                return Err(io("download digest mismatch"));
            }
        }
        file.rewind()?;
        Ok(file)
    }
    pub(crate) async fn pull(
        &self,
        docker: &Docker,
        snapshot: &d::CapsuleSnapshot,
        size: i64,
    ) -> Result<()> {
        if images::has_snapshot(docker, snapshot).await? {
            return Ok(());
        }
        let path = text(format_args!(
            "/api/snapshots/{}",
            segment(&snapshot.digest)?
        ))
        .map_err(io)?;
        let mut archive = self
            .download(&path, size, crate::archive::FILE_LIMIT)
            .await?;
        images::import(docker, snapshot, &mut archive).await
    }
    pub(crate) async fn ensure(
        &self,
        docker: &Docker,
        composition: &d::Composition,
        artifacts: &[d::Artifact],
    ) -> Result<()> {
        let plan = spin_core::layers::plan_layers(composition, artifacts).map_err(io)?;
        for artifact in plan.needed() {
            // Walk missing delta parents first; a bounded path also rejects cycles.
            let mut chain = Vec::new();
            let mut current = &artifact.snapshot;
            loop {
                if images::has_snapshot(docker, current).await? {
                    break;
                }
                if chain.len() >= spin_core::layers::MAX_LAYERS
                    || chain
                        .iter()
                        .any(|s: &&d::CapsuleSnapshot| s.r#ref == current.r#ref)
                {
                    return Err(io("snapshot parent cycle"));
                }
                d::try_push(&mut chain, current).map_err(io)?;
                if !current.delta || current.parent_ref.is_empty() {
                    break;
                }
                let Some(parent) = artifacts
                    .iter()
                    .find(|a| a.snapshot.r#ref == current.parent_ref)
                else {
                    break;
                };
                current = &parent.snapshot;
            }
            for snapshot in chain.into_iter().rev() {
                self.pull(docker, snapshot, 0).await?;
            }
        }
        Ok(())
    }
}
async fn wait(delay: Duration) {
    let until = Instant::now() + delay;
    while Instant::now() < until {
        executor::next_round().await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::net::TcpListener;
    fn receive(listener: &TcpListener) -> (TcpStream, String, Vec<u8>) {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut b = [0];
            socket.read_exact(&mut b).unwrap();
            header.push(b[0]);
        }
        let header = String::from_utf8(header).unwrap();
        assert!(header.contains("Authorization: Bearer secret\r\n"));
        let len = header
            .lines()
            .find_map(|l| {
                l.to_ascii_lowercase()
                    .strip_prefix("content-length: ")
                    .map(|s| s.parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        let mut body = vec![0; len];
        socket.read_exact(&mut body).unwrap();
        (socket, header, body)
    }
    fn respond(mut socket: TcpStream, status: u16, extra: &str, body: &[u8]) {
        write!(
            socket,
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
            body.len()
        )
        .unwrap();
        socket.write_all(body).unwrap();
    }
    #[test]
    fn lost_chunk_and_completion_replies_are_retried_and_download_hash_is_checked() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = Client::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            "secret",
        )
        .unwrap();
        std::thread::scope(|scope| {
            let peer = scope.spawn(|| {
                let (socket, header, body) = receive(&listener);
                assert!(header.starts_with("POST /api/uploads "));
                assert_eq!(field(&Value::from_json(&body).unwrap(), "size").as_i64(), Some(1_048_579));
                respond(socket, 201, "", br#"{"id":"one","chunk_size":1048576,"offset":0}"#);
                let (socket, header, body) = receive(&listener);
                assert!(header.contains("X-Spin-Upload-Offset: 0\r\n"));
                assert_eq!(body, vec![b'a'; CHUNK]);
                drop(socket); // Durable write, but the response never arrives.
                let (socket, _, again) = receive(&listener);
                assert_eq!(again, body);
                respond(socket, 200, "", br#"{"offset":1048576}"#);
                let (socket, header, body) = receive(&listener);
                assert!(header.contains("X-Spin-Upload-Offset: 1048576\r\n")); assert_eq!(body, b"end");
                respond(socket, 200, "", br#"{"offset":1048579}"#);
                let (socket, header, _) = receive(&listener);
                assert!(header.starts_with("POST /api/uploads/one/complete ")); drop(socket);
                let (socket, header, _) = receive(&listener);
                assert!(header.starts_with("POST /api/uploads/one/complete "));
                respond(socket, 200, "", br#"{"ref":"bundle:test","digest":"sha256:archive","size":1048579}"#);
                let (socket, header, _) = receive(&listener);
                assert!(header.starts_with("GET /api/blobs/bundle:test?offset=0 "));
                respond(socket, 200, "X-Spin-Size: 3\r\nX-Spin-Digest: sha256:wrong\r\n", b"bad");
                let (socket, _, _) = receive(&listener);
                respond(socket, 200, "X-Spin-Size: 3\r\nX-Spin-Digest: sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad\r\n", b"abc");
            });
            executor::block_on(async {
                let mut file = Temporary::new().unwrap();
                file.file.write_all(&vec![b'a'; CHUNK]).unwrap();
                file.file.write_all(b"end").unwrap();
                let result = client
                    .upload("bundle", "test", None, &mut file)
                    .await
                    .unwrap();
                assert_eq!(result.size, 1_048_579);
                assert!(
                    client
                        .download("/api/blobs/bundle:test", 3, 100)
                        .await
                        .is_err()
                );
                let mut file = client
                    .download("/api/blobs/bundle:test", 3, 100)
                    .await
                    .unwrap();
                let mut bytes = Vec::new();
                file.file.read_to_end(&mut bytes).unwrap();
                assert_eq!(bytes, b"abc");
            });
            peer.join().unwrap();
        });
    }
}
