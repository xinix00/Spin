//! Hostdialer voor dezelfde providerpool als op HopOS.
use crate::{client_net::Transport, executor};
use std::{
    net::{TcpStream, ToSocketAddrs},
    time::Duration,
};
pub(crate) struct Dial;
impl leanhttp::Dial for Dial {
    type Conn = Transport;
    fn is_encrypted(&self) -> bool {
        true
    }
    async fn dial(&mut self, target: leanhttp::Target<'_>) -> leanhttp::Result<Transport> {
        let addresses = (target.host, target.port)
            .to_socket_addrs()
            .map_err(|_| leanhttp::Error::Connect)?;
        for address in addresses.take(16) {
            if let Ok(socket) = TcpStream::connect_timeout(&address, Duration::from_millis(500)) {
                return Transport::connect(socket, target.host, target.port, target.https)
                    .await
                    .map_err(|_| leanhttp::Error::Connect);
            }
            executor::next_round().await;
        }
        Err(leanhttp::Error::Connect)
    }
}
