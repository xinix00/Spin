//! Hostdialer voor dezelfde providerpool als op HopOS: DNS en TCP van std,
//! met Lean's `WebDial` erboven.
use crate::{client_net::Web, executor, net::Connection};
use std::{
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};
/// De kale TCP-dial van de host.
pub struct Tcp;
impl leanhttp::Dial for Tcp {
    type Conn = Connection;
    async fn dial(&mut self, target: leanhttp::Target<'_>) -> leanhttp::Result<Connection> {
        let addresses = (target.host, target.port)
            .to_socket_addrs()
            .map_err(|_| leanhttp::Error::Connect)?;
        for address in addresses.take(16) {
            if let Ok(socket) = TcpStream::connect_timeout(&address, Duration::from_millis(500)) {
                return Connection::new(socket).map_err(|_| leanhttp::Error::Connect);
            }
            executor::next_round().await;
        }
        Err(leanhttp::Error::Connect)
    }
}
/// De webdialer van de host.
pub(crate) type Dial = Web<Tcp>;
pub(crate) fn dial() -> Dial {
    crate::client_net::web(Tcp)
}
