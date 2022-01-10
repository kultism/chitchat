use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rand::prelude::*;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time;

use crate::model::ScuttleButtMessage;
use crate::serialize::Serializable;
use crate::ScuttleButt;

/// Buffer size for UDP messages.
/// This value is set to avoid UDP fragmentation.
/// https://jvns.ca/blog/2017/02/07/mtu/
const BUF_SIZE: usize = 1_472;

/// Interval between random gossip.
const GOSSIP_INTERVAL: Duration = Duration::from_secs(1);

/// Number of nodes picked for random gossip.
const GOSSIP_COUNT: usize = 3;

/// UDP ScuttleButt server.
pub struct ScuttleServer {
    channel: UnboundedSender<ChannelMessage>,
    scuttlebutt: Arc<Mutex<ScuttleButt>>,
    join_handle: JoinHandle<Result<(), anyhow::Error>>,
}

impl ScuttleServer {
    /// Launch a new server.
    ///
    /// This will start the ScuttleButt server as a new Tokio background task.
    pub fn spawn(address: impl Into<String>, seed_nodes: &[String]) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();

        let scuttlebutt = ScuttleButt::with_node_id_and_seeds(address.into(), seed_nodes.to_vec());
        let scuttlebutt_arc = Arc::new(Mutex::new(scuttlebutt));
        let scuttlebutt_server = scuttlebutt_arc.clone();

        let join_handle = tokio::spawn(async move {
            let mut server = UdpServer::new(rx, scuttlebutt_server).await?;
            server.run().await
        });

        let scuttle_server = Self {
            channel: tx,
            scuttlebutt: scuttlebutt_arc,
            join_handle,
        };

        // // Advertise the new node to all seed nodes.
        // for node_id in seed_nodes {
        //     let _ = scuttle_server.gossip(node_id);
        // }

        scuttle_server
    }

    pub fn scuttlebutt(&self) -> Arc<Mutex<ScuttleButt>> {
        self.scuttlebutt.clone()
    }

    /// Call a function with mutable access to the [`ScuttleButt`].
    pub async fn with_scuttlebutt<F, T>(&self, mut fun: F) -> T
    where
        F: FnMut(&mut ScuttleButt) -> T,
    {
        let mut scuttlebutt = self.scuttlebutt.lock().await;
        fun(&mut scuttlebutt)
    }

    /// Shut the server down.
    pub async fn shutdown(self) -> Result<(), anyhow::Error> {
        let _ = self.channel.send(ChannelMessage::Shutdown);
        self.join_handle.await?
    }

    /// Perform a ScuttleButt "handshake" with another UDP server.
    pub fn gossip(&self, addr: impl Into<String>) -> Result<(), anyhow::Error> {
        self.channel.send(ChannelMessage::Gossip(addr.into()))?;
        Ok(())
    }
}

/// UDP server for ScuttleButt communication.
struct UdpServer {
    channel: UnboundedReceiver<ChannelMessage>,
    scuttlebutt: Arc<Mutex<ScuttleButt>>,
    socket: Arc<UdpSocket>,
}

impl UdpServer {
    async fn new(
        channel: UnboundedReceiver<ChannelMessage>,
        scuttlebutt: Arc<Mutex<ScuttleButt>>,
    ) -> anyhow::Result<Self> {
        let socket = {
            let address = &scuttlebutt.lock().await.self_node_id;
            Arc::new(UdpSocket::bind(address).await?)
        };

        Ok(Self {
            scuttlebutt,
            channel,
            socket,
        })
    }

    /// Listen for new ScuttleButt messages.
    async fn run(&mut self) -> anyhow::Result<()> {
        let mut gossip_interval = time::interval(GOSSIP_INTERVAL);
        let mut rng = SmallRng::from_entropy();

        let mut buf = [0; BUF_SIZE];
        loop {
            tokio::select! {
                result = self.socket.recv_from(&mut buf) => match result {
                    Ok((len, addr)) => {
                        let _ = self.process_package(addr, &buf[..len]).await;
                    }
                    Err(err) => return Err(err.into()),
                },
                _ = gossip_interval.tick() => self.gossip_multiple(&mut rng).await,
                message = self.channel.recv() => match message {
                    Some(ChannelMessage::Gossip(addr)) => {
                        let _ = self.gossip(addr).await;
                    },
                    Some(ChannelMessage::Shutdown) | None => break,
                },
            }
        }
        Ok(())
    }

    /// Process a single UDP packet.
    async fn process_package(&self, addr: SocketAddr, mut data: &[u8]) -> anyhow::Result<()> {
        // Handle gossip from other servers.
        let message = ScuttleButtMessage::deserialize(&mut data)?;
        let response = self.scuttlebutt.lock().await.process_message(message);

        // Send reply if necessary.
        if let Some(message) = response {
            let mut buf = Vec::new();
            message.serialize(&mut buf);
            self.socket.send_to(&buf[..], addr).await?;
        }

        Ok(())
    }

    /// Gossip to multiple randomly chosen nodes.
    async fn gossip_multiple(&self, rng: &mut SmallRng) {
        let scuttlebutt = self.scuttlebutt.lock().await;
        let self_node_id = &scuttlebutt.self_node_id;
        const EMPTY_STRING: String = String::new();
        let mut rand_nodes = [EMPTY_STRING; GOSSIP_COUNT];

        // Select up to [`GOSSIP_COUNT`] node IDs at random.
        let nodes = scuttlebutt.cluster_state.nodes();
        let count = nodes
            .filter(|node_id| node_id != &self_node_id)
            .map(ToString::to_string)
            .choose_multiple_fill(rng, &mut rand_nodes);

        // Drop lock to prevent deadlock in [`UdpSocket::gossip`].
        drop(scuttlebutt);
        for node in &rand_nodes[..count] {
            let _ = self.gossip(node).await;
        }
    }

    /// Gossip to one other UDP server.
    async fn gossip(&self, addr: impl Into<String>) -> anyhow::Result<()> {
        let syn = self.scuttlebutt.lock().await.create_syn_message();
        let mut buf = Vec::new();
        syn.serialize(&mut buf);
        let _ = self.socket.send_to(&buf, addr.into()).await?;
        Ok(())
    }
}

#[derive(Debug)]
enum ChannelMessage {
    Gossip(String),
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::future::Future;
    use std::time::Duration;

    use crate::{ScuttleButtMessage, HEARTBEAT_KEY};

    async fn timeout<O>(future: impl Future<Output = O>) -> O {
        tokio::time::timeout(Duration::from_millis(100), future)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn syn() {
        let test_addr = "0.0.0.0:1111";
        let socket = UdpSocket::bind(test_addr).await.unwrap();

        let server = ScuttleServer::spawn("0.0.0.0:1112", &[]);
        server.gossip(test_addr).unwrap();

        let mut buf = [0; BUF_SIZE];
        let (len, _addr) = timeout(socket.recv_from(&mut buf)).await.unwrap();

        match bincode::deserialize(&buf[..len]).unwrap() {
            ScuttleButtMessage::Syn { .. } => (),
            message => panic!("unexpected message: {:?}", message),
        }

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn syn_ack() {
        let server_addr = "0.0.0.0:2221";
        let socket = UdpSocket::bind("0.0.0.0:2222").await.unwrap();
        let mut scuttlebutt = ScuttleButt::with_node_id("offline".into());

        let server = ScuttleServer::spawn(server_addr, &[]);

        let syn = scuttlebutt.create_syn_message();
        let message = bincode::serialize(&syn).unwrap();
        socket.send_to(&message, server_addr).await.unwrap();

        let mut buf = [0; BUF_SIZE];
        let (len, _addr) = timeout(socket.recv_from(&mut buf)).await.unwrap();

        match bincode::deserialize(&buf[..len]).unwrap() {
            ScuttleButtMessage::SynAck { .. } => (),
            message => panic!("unexpected message: {:?}", message),
        }

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ignore_broken_payload() {
        let server_addr = "0.0.0.0:3331";
        let server = ScuttleServer::spawn(server_addr, &[]);
        let socket = UdpSocket::bind("0.0.0.0:3332").await.unwrap();
        let mut scuttlebutt = ScuttleButt::with_node_id("offline".into());

        // Send broken payload.
        socket.send_to(b"broken", server_addr).await.unwrap();

        // Confirm nothing broke using a regular payload.
        let syn = scuttlebutt.create_syn_message();
        let message = bincode::serialize(&syn).unwrap();
        socket.send_to(&message, server_addr).await.unwrap();

        let mut buf = [0; BUF_SIZE];
        let (len, _addr) = timeout(socket.recv_from(&mut buf)).await.unwrap();

        match bincode::deserialize(&buf[..len]).unwrap() {
            ScuttleButtMessage::SynAck { .. } => (),
            message => panic!("unexpected message: {:?}", message),
        }

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ignore_oversized_payload() {
        let server_addr = "0.0.0.0:4441";
        let server = ScuttleServer::spawn(server_addr, &[]);
        let socket = UdpSocket::bind("0.0.0.0:4442").await.unwrap();
        let mut scuttlebutt = ScuttleButt::with_node_id("offline".into());

        // Send broken payload.
        socket
            .send_to(&[0; BUF_SIZE + 1], server_addr)
            .await
            .unwrap();

        // Confirm nothing broke using a regular payload.
        let syn = scuttlebutt.create_syn_message();
        let message = bincode::serialize(&syn).unwrap();
        socket.send_to(&message, server_addr).await.unwrap();

        let mut buf = [0; BUF_SIZE];
        let (len, _addr) = timeout(socket.recv_from(&mut buf)).await.unwrap();

        match bincode::deserialize(&buf[..len]).unwrap() {
            ScuttleButtMessage::SynAck { .. } => (),
            message => panic!("unexpected message: {:?}", message),
        }

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn seeding() {
        let server_addr = "0.0.0.0:5551";
        let socket = UdpSocket::bind(server_addr).await.unwrap();

        let server = ScuttleServer::spawn("0.0.0.0:5552", &[server_addr.into()]);

        let mut buf = [0; BUF_SIZE];
        let (len, _addr) = timeout(socket.recv_from(&mut buf)).await.unwrap();

        match bincode::deserialize(&buf[..len]).unwrap() {
            ScuttleButtMessage::Syn { .. } => (),
            message => panic!("unexpected message: {:?}", message),
        }

        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn heartbeat() {
        let test_addr = "0.0.0.0:6661";
        let mut scuttlebutt = ScuttleButt::with_node_id(test_addr.into());
        let socket = UdpSocket::bind(test_addr).await.unwrap();

        let server_addr = "0.0.0.0:6662";
        let server = ScuttleServer::spawn(server_addr, &[]);

        // Add our test socket to the server's nodes.
        server
            .with_scuttlebutt(|server_scuttlebutt| {
                let syn = server_scuttlebutt.create_syn_message();
                let syn_ack = scuttlebutt.process_message(syn).unwrap();
                server_scuttlebutt.process_message(syn_ack);
            })
            .await;

        // Wait for heartbeat.
        let mut buf = [0; BUF_SIZE];
        let (len, _addr) = timeout(socket.recv_from(&mut buf)).await.unwrap();
        let message = match bincode::deserialize(&buf[..len]).unwrap() {
            message @ ScuttleButtMessage::Syn { .. } => message,
            message => panic!("unexpected message: {:?}", message),
        };

        // Reply.
        let syn_ack = scuttlebutt.process_message(message).unwrap();
        let message = bincode::serialize(&syn_ack).unwrap();
        socket.send_to(&message, server_addr).await.unwrap();

        // Wait for delta to ensure heartbeat key was incremented.
        let (len, _addr) = timeout(socket.recv_from(&mut buf)).await.unwrap();
        let delta = match bincode::deserialize(&buf[..len]).unwrap() {
            ScuttleButtMessage::Ack { delta } => delta,
            message => panic!("unexpected message: {:?}", message),
        };

        let node_delta = &delta.node_deltas.get(server_addr).unwrap().key_values;
        let heartbeat = &node_delta.get(HEARTBEAT_KEY).unwrap().value;
        assert_eq!(heartbeat, "2");

        server.shutdown().await.unwrap();
    }
}