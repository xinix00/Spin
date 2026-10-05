//! De uitgaande dial van de host: Lean's `TcpDial` (DNS en TCP van std, de
//! socket niet-blokkerend op de host-executor) met Lean's `WebDial` erboven.
use crate::{
    client_net::{self, Transport},
    net::Executor,
};
use leanhttp::host::TcpDial;
use std::time::Duration;

/// Termijn per adres; een naam geeft hoogstens [`ADDRESSES`] adressen.
const CONNECT: Duration = Duration::from_millis(500);
const ADDRESSES: usize = 16;

/// De webdialer van de host, met de reden van de laatste mislukte dial.
pub struct Dial {
    tcp: TcpDial<Executor>,
    tls: Option<leanhttps::Error>,
}
impl Dial {
    pub(crate) fn new() -> Self {
        let mut tcp = TcpDial::new(Executor);
        tcp.connect = CONNECT;
        tcp.addresses = ADDRESSES;
        Self { tcp, tls: None }
    }
    /// De fout van de laatste dial voor een logregel: "tls: …" van `WebDial`,
    /// anders "resolve: …" of "connect ip:port: …" van `TcpDial`.
    pub(crate) fn failure(&self, error: leanhttp::Error) -> std::io::Error {
        match (&self.tls, self.tcp.last_error()) {
            (Some(tls), _) => client_net::reason(format_args!("tls: {tls}")),
            (None, Some(tcp)) => client_net::reason(tcp),
            (None, None) => client_net::reason(error),
        }
    }
}
impl leanhttp::Dial for Dial {
    type Conn = Transport;
    fn is_encrypted(&self) -> bool {
        true
    }
    async fn dial(&mut self, target: leanhttp::Target<'_>) -> leanhttp::Result<Transport> {
        // Per dial een verse WebDial over de geleende TcpDial: zo blijven beide
        // redenen leesbaar, en een oude TLS-reden blijft niet staan.
        let mut web = client_net::web(&mut self.tcp);
        let result = web.dial(target).await;
        self.tls = web.last_error();
        result
    }
}
