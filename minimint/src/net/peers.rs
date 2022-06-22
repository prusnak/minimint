use crate::net::connect::{AnyConnector, SharedAnyConnector};
use crate::net::framed::AnyFramedTransport;
use crate::net::queue::{MessageId, MessageQueue, UniqueMessage};
use async_trait::async_trait;
use futures::future::select_all;
use futures::{SinkExt, StreamExt};
use hbbft::Target;
use minimint_api::PeerId;
use rand::{thread_rng, Rng};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::cmp::min;
use std::collections::HashMap;
use std::fmt::Debug;
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, info, instrument, trace, warn};

const MAX_FAIL_RECONNECT_COUNTER: u64 = 300;

pub type AnyPeerConnections<M> = Box<dyn PeerConnections<M> + Send + Unpin + 'static>;
pub type PeerConnector<M> = AnyConnector<PeerMessage<M>>;

#[async_trait]
pub trait PeerConnections<T>
where
    T: Serialize + DeserializeOwned + Unpin + Send,
{
    async fn send(&mut self, target: Target<PeerId>, msg: T);

    async fn receive(&mut self) -> (PeerId, T);

    async fn ban_peer(&mut self, peer: PeerId);

    fn to_any(self) -> AnyPeerConnections<T>
    where
        Self: Sized + Send + Unpin + 'static,
    {
        Box::new(self)
    }
}

// FIXME: make connections dynamically managed
pub struct ReconnectPeerConnections<T> {
    connections: HashMap<PeerId, PeerConnection<T>>,
    _listen_task: JoinHandle<()>,
}

struct PeerConnection<T> {
    outgoing: Sender<T>,
    incoming: Receiver<T>,
    _io_task: JoinHandle<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub identity: PeerId,
    pub bind_addr: String,
    pub peers: HashMap<PeerId, ConnectionConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionConfig {
    pub addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerMessage<M> {
    msg: UniqueMessage<M>,
    ack: Option<MessageId>,
}

impl<T: 'static> ReconnectPeerConnections<T>
where
    T: std::fmt::Debug + Clone + Serialize + DeserializeOwned + Unpin + Send + Sync,
{
    #[instrument(skip_all)]
    pub async fn new(cfg: NetworkConfig, connect: PeerConnector<T>) -> Self {
        info!("Starting mint {}", cfg.identity);

        let shared_connector: SharedAnyConnector<PeerMessage<T>> = connect.into();

        let (connection_senders, connections) = cfg
            .peers
            .iter()
            .filter(|(&peer, _)| peer != cfg.identity)
            .map(|(&peer, cfg)| {
                let (connection_sender, connection_receiver) =
                    tokio::sync::mpsc::channel::<AnyFramedTransport<PeerMessage<T>>>(4);
                (
                    (peer, connection_sender),
                    (
                        peer,
                        PeerConnection::new(
                            peer,
                            cfg.clone(),
                            shared_connector.clone(),
                            connection_receiver,
                        ),
                    ),
                )
            })
            .unzip();

        let listen_task = tokio::spawn(Self::run_listen_task(
            cfg,
            shared_connector,
            connection_senders,
        ));

        ReconnectPeerConnections {
            connections,
            _listen_task: listen_task,
        }
    }

    async fn run_listen_task(
        cfg: NetworkConfig,
        connect: SharedAnyConnector<PeerMessage<T>>,
        mut connection_senders: HashMap<PeerId, Sender<AnyFramedTransport<PeerMessage<T>>>>,
    ) {
        let mut listener = connect
            .listen(cfg.bind_addr.clone())
            .await
            .expect("Could not bind port");

        loop {
            let (peer, connection) = match listener.next().await.expect("Listener closed") {
                Ok(connection) => connection,
                Err(e) => {
                    error!(mint = ?cfg.identity, err = %e, "Error while opening incoming connection");
                    continue;
                }
            };

            let err = connection_senders
                .get_mut(&peer)
                .expect("Authenticating connectors should not return unknown peers")
                .send(connection)
                .await
                .is_err();

            if err {
                warn!(
                    ?peer,
                    "Could not send incoming connection to peer io task (possibly banned)"
                );
            }
        }
    }
}

#[async_trait]
impl<T> PeerConnections<T> for ReconnectPeerConnections<T>
where
    T: std::fmt::Debug + Serialize + DeserializeOwned + Clone + Unpin + Send + Sync + 'static,
{
    async fn send(&mut self, target: Target<PeerId>, msg: T) {
        trace!(?target, "Sending message to");
        match target {
            Target::All => {
                for connection in self.connections.values_mut() {
                    connection.send(msg.clone()).await;
                }
            }
            Target::Node(peer_id) => {
                if let Some(peer) = self.connections.get_mut(&peer_id) {
                    peer.send(msg).await;
                } else {
                    trace!(peer = ?peer_id, "Not sending message to unknown peer (maybe banned)");
                }
            }
        }
    }

    async fn receive(&mut self) -> (PeerId, T) {
        // TODO: optimize, don't throw away remaining futures

        let futures_non_banned = self.connections.iter_mut().map(|(&peer, connection)| {
            let receive_future = async move {
                let msg = connection.receive().await;
                (peer, msg)
            };
            Box::pin(receive_future)
        });

        select_all(futures_non_banned).await.0
    }

    async fn ban_peer(&mut self, peer: PeerId) {
        self.connections.remove(&peer);
        warn!("Peer {} banned.", peer);
    }
}

struct CommonPeerConnectionState<M> {
    resend_queue: MessageQueue<M>,
    incoming: Sender<M>,
    outgoing: Receiver<M>,
    peer: PeerId,
    cfg: ConnectionConfig,
    connect: SharedAnyConnector<PeerMessage<M>>,
    incoming_connections: Receiver<AnyFramedTransport<PeerMessage<M>>>,
    last_received: Option<MessageId>,
}

struct DisconnectedPeerConnectionState<M> {
    common: CommonPeerConnectionState<M>,
    reconnect_at: Instant,
    failed_reconnect_counter: u64,
}

struct ConnectedPeerConnectionState<M> {
    common: CommonPeerConnectionState<M>,
    connection: AnyFramedTransport<PeerMessage<M>>,
}

enum PeerConnectionState<M> {
    Disconnected(DisconnectedPeerConnectionState<M>),
    Connected(ConnectedPeerConnectionState<M>),
}

impl<M> CommonPeerConnectionState<M>
where
    M: Clone,
{
    async fn resend_buffer_contents(
        &self,
        connection: &mut AnyFramedTransport<PeerMessage<M>>,
    ) -> Result<(), anyhow::Error> {
        for msg in self.resend_queue.iter().cloned() {
            connection
                .send(PeerMessage {
                    msg,
                    ack: self.last_received,
                })
                .await?
        }

        Ok(())
    }
}

impl<M> PeerConnectionState<M>
where
    M: Debug + Clone,
{
    async fn state_transition(self) -> Self {
        match self {
            PeerConnectionState::Disconnected(disconnected) => {
                disconnected.state_transition().await
            }
            PeerConnectionState::Connected(connected) => connected.state_transition().await,
        }
    }

    fn disconnect(
        common: CommonPeerConnectionState<M>,
        mut disconnect_count: u64,
    ) -> PeerConnectionState<M> {
        disconnect_count += 1;

        let reconnect_at = {
            let scaling_factor = disconnect_count as f64;
            let delay: f64 = thread_rng().gen_range(1.0 * scaling_factor, 4.0 * scaling_factor);
            debug!(delay, "Scheduling reopening of connection");
            Instant::now() + Duration::from_secs_f64(delay)
        };

        PeerConnectionState::Disconnected(DisconnectedPeerConnectionState {
            common,
            reconnect_at,
            failed_reconnect_counter: min(disconnect_count, MAX_FAIL_RECONNECT_COUNTER),
        })
    }

    fn disconnect_err(
        common: CommonPeerConnectionState<M>,
        err: anyhow::Error,
        disconnect_count: u64,
    ) -> Self {
        warn!(peer = ?common.peer, %err, %disconnect_count, "Some error occurred, disconnecting");
        Self::disconnect(common, disconnect_count)
    }

    async fn connect(
        common: CommonPeerConnectionState<M>,
        mut new_connection: AnyFramedTransport<PeerMessage<M>>,
        disconnect_count: u64,
    ) -> Self {
        debug!(peer = ?common.peer, "Received incoming connection");
        match common.resend_buffer_contents(&mut new_connection).await {
            Ok(()) => PeerConnectionState::Connected(ConnectedPeerConnectionState {
                common,
                connection: new_connection,
            }),
            Err(e) => PeerConnectionState::disconnect_err(common, e, disconnect_count),
        }
    }
}

impl<M> ConnectedPeerConnectionState<M>
where
    M: Debug + Clone,
{
    async fn state_transition(mut self) -> PeerConnectionState<M> {
        tokio::select! {
            maybe_msg = self.common.outgoing.recv() => {
                let msg = maybe_msg.expect("Peer connection was dropped");
                self.send_message(msg).await
            },
            new_connection_res = self.common.incoming_connections.recv() => {
                let new_connection = new_connection_res.expect("Listener task died");
                self.receive_connection(new_connection).await
            },
            Some(msg_res) = self.connection.next() => {
                self.receive_message(msg_res).await
            },
        }
    }

    async fn send_message(mut self, msg: M) -> PeerConnectionState<M> {
        let umsg = self.common.resend_queue.push(msg);
        trace!(?self.common.peer, id = ?umsg.id, "Sending outgoing message");

        match self
            .connection
            .send(PeerMessage {
                msg: umsg,
                ack: self.common.last_received,
            })
            .await
        {
            Ok(()) => PeerConnectionState::Connected(self),
            Err(e) => PeerConnectionState::disconnect_err(self.common, e, 0),
        }
    }

    async fn receive_connection(
        self,
        new_connection: AnyFramedTransport<PeerMessage<M>>,
    ) -> PeerConnectionState<M> {
        warn!("Replacing existing connection");
        PeerConnectionState::connect(self.common, new_connection, 0).await
    }

    async fn receive_message(
        mut self,
        msg_res: Result<PeerMessage<M>, anyhow::Error>,
    ) -> PeerConnectionState<M> {
        match self.receive_message_inner(msg_res).await {
            Ok(()) => PeerConnectionState::Connected(self),
            Err(e) => PeerConnectionState::disconnect_err(self.common, e, 0),
        }
    }

    async fn receive_message_inner(
        &mut self,
        msg_res: Result<PeerMessage<M>, anyhow::Error>,
    ) -> Result<(), anyhow::Error> {
        let PeerMessage { msg, ack } = msg_res?;
        trace!(peer = ?self.common.peer, id = ?msg.id, "Received incoming message");

        let expected = self
            .common
            .last_received
            .map(|last_id| last_id.increment())
            .unwrap_or(MessageId(1));

        if msg.id < expected {
            debug!(?expected, received = ?msg.id, "Received old message");
            return Ok(());
        }

        if msg.id > expected {
            warn!(?expected, received = ?msg.id, "Received message from the future");
            return Err(anyhow::anyhow!("Received message from the future"));
        }

        debug_assert_eq!(expected, msg.id, "someone removed the check above");
        self.common.last_received = Some(expected);
        if let Some(ack) = ack {
            self.common.resend_queue.ack(ack);
        }

        self.common
            .incoming
            .send(msg.msg)
            .await
            .expect("Peer connection went away");

        Ok(())
    }
}

impl<M> DisconnectedPeerConnectionState<M>
where
    M: Debug + Clone,
{
    async fn state_transition(mut self) -> PeerConnectionState<M> {
        tokio::select! {
            maybe_msg = self.common.outgoing.recv() => {
                let msg = maybe_msg.expect("Peer connection was dropped");
                self.send_message(msg).await
            },
            new_connection_res = self.common.incoming_connections.recv() => {
                let new_connection = new_connection_res.expect("Listener task died");
                self.receive_connection(new_connection).await
            },
            () = tokio::time::sleep_until(self.reconnect_at) => {
                self.reconnect().await
            }
        }
    }

    async fn send_message(mut self, msg: M) -> PeerConnectionState<M> {
        let umsg = self.common.resend_queue.push(msg);
        trace!(id = ?umsg.id, "Queueing outgoing message");
        PeerConnectionState::Disconnected(self)
    }

    async fn receive_connection(
        self,
        new_connection: AnyFramedTransport<PeerMessage<M>>,
    ) -> PeerConnectionState<M> {
        PeerConnectionState::connect(self.common, new_connection, self.failed_reconnect_counter)
            .await
    }

    async fn reconnect(self) -> PeerConnectionState<M> {
        match self.try_reconnect().await {
            Ok(conn) => {
                PeerConnectionState::connect(self.common, conn, self.failed_reconnect_counter).await
            }
            Err(e) => {
                PeerConnectionState::disconnect_err(self.common, e, self.failed_reconnect_counter)
            }
        }
    }

    async fn try_reconnect(&self) -> Result<AnyFramedTransport<PeerMessage<M>>, anyhow::Error> {
        debug!("Trying to reconnect");
        let addr = &self.common.cfg.addr;
        let (connected_peer, conn) = self.common.connect.connect_framed(addr.clone()).await?;

        if connected_peer == self.common.peer {
            Ok(conn)
        } else {
            return Err(anyhow::anyhow!(
                "Peer identified itself incorrectly: {:?}",
                connected_peer
            ));
        }
    }
}

impl<M> PeerConnection<M>
where
    M: Debug + Clone + Send + Sync + 'static,
{
    fn new(
        id: PeerId,
        cfg: ConnectionConfig,
        connect: SharedAnyConnector<PeerMessage<M>>,
        incoming_connections: Receiver<AnyFramedTransport<PeerMessage<M>>>,
    ) -> PeerConnection<M> {
        let (outgoing_sender, outgoing_receiver) = tokio::sync::mpsc::channel::<M>(1024);
        let (incoming_sender, incoming_receiver) = tokio::sync::mpsc::channel::<M>(1024);

        let io_thread = tokio::spawn(Self::run_io_thread(
            incoming_sender,
            outgoing_receiver,
            id,
            cfg,
            connect,
            incoming_connections,
        ));

        PeerConnection {
            outgoing: outgoing_sender,
            incoming: incoming_receiver,
            _io_task: io_thread,
        }
    }

    async fn send(&mut self, msg: M) {
        self.outgoing.send(msg).await.expect("io task died");
    }

    async fn receive(&mut self) -> M {
        self.incoming.recv().await.expect("io task died")
    }

    #[instrument(skip_all, fields(peer))]
    async fn run_io_thread(
        incoming: Sender<M>,
        outgoing: Receiver<M>,
        peer: PeerId,
        cfg: ConnectionConfig,
        connect: SharedAnyConnector<PeerMessage<M>>,
        incoming_connections: Receiver<AnyFramedTransport<PeerMessage<M>>>,
    ) {
        let mut state_machine = PeerConnectionState::disconnect(
            CommonPeerConnectionState {
                resend_queue: Default::default(),
                incoming,
                outgoing,
                peer,
                cfg,
                connect,
                incoming_connections,
                last_received: None,
            },
            0,
        );

        loop {
            state_machine = state_machine.state_transition().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::net::connect::mock::MockNetwork;
    use crate::net::connect::Connector;
    use crate::net::peers::{
        ConnectionConfig, NetworkConfig, PeerConnections, ReconnectPeerConnections,
    };
    use futures::Future;
    use hbbft::Target;
    use minimint_api::PeerId;
    use std::collections::HashMap;
    use std::time::Duration;
    use tracing_subscriber::EnvFilter;

    async fn timeout<F, T>(f: F) -> Option<T>
    where
        F: Future<Output = T>,
    {
        tokio::time::timeout(Duration::from_secs(100), f).await.ok()
    }

    #[tokio::test]
    async fn test_connect() {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("info,tide=error,minimint::net=trace")),
            )
            .init();

        let net = MockNetwork::new();

        let peers = ["a", "b", "c"]
            .iter()
            .enumerate()
            .map(|(idx, &peer)| {
                let cfg = ConnectionConfig {
                    addr: peer.to_string(),
                };
                (PeerId::from(idx as u16 + 1), cfg)
            })
            .collect::<HashMap<_, _>>();

        let peers_ref = &peers;
        let net_ref = &net;
        let build_peers = |bind: &'static str, id: u16| async move {
            let cfg = NetworkConfig {
                identity: PeerId::from(id),
                bind_addr: bind.to_string(),
                peers: peers_ref.clone(),
            };
            let connect = net_ref.connector(cfg.identity).to_any();
            ReconnectPeerConnections::<u64>::new(cfg, connect).await
        };

        let mut peers_a = build_peers("a", 1).await;
        let mut peers_b = build_peers("b", 2).await;

        peers_a.send(Target::Node(PeerId::from(2)), 42).await;
        let recv = timeout(peers_b.receive()).await.unwrap();
        assert_eq!(recv.0, PeerId::from(1));
        assert_eq!(recv.1, 42);

        peers_a.send(Target::Node(PeerId::from(3)), 21).await;

        let mut peers_c = build_peers("c", 3).await;
        let recv = timeout(peers_c.receive()).await.unwrap();
        assert_eq!(recv.0, PeerId::from(1));
        assert_eq!(recv.1, 21);
    }
}
