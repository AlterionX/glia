use std::{collections::HashSet, net::SocketAddr, sync::Arc};

use chrono::{DateTime, Utc};
use base64::prelude::*;
use mio::net::UdpSocket;
use tokio::{sync::{mpsc::{self, Sender, Receiver}, RwLock}, task::JoinHandle};

use crate::{World, UserAction};

/// Struct holding network connections
pub struct Netting {
    pub registry: Arc<RwLock<PeerRegistry>>,
    force_conn_tx: Sender<SocketAddr>,
}

impl Netting {
    // TODO This should probably be yet another event.
    pub async fn new() -> (Self, Receiver<UserAction>) {
        // TODO Actually use these
        let (input_tx, input_rx) = mpsc::channel::<UserAction>(512);

        let (force_conn_tx, force_conn_rx) = mpsc::channel::<SocketAddr>(512);
        let (inner_registry, mut new_connection_trigger) = PeerRegistry::new(force_conn_rx).await;
        let registry = Arc::new(RwLock::new(inner_registry));

        // Now, let's properly handle that new connection call.
        let new_connection_registry = Arc::clone(&registry);
        tokio::spawn(async move {
            loop {
                let Some(addr) = new_connection_trigger.recv().await else {
                    // TODO Repair fiber.
                    break;
                };
                println!("Attempting connection to {addr:?}");
                // TODO try it
                PeerRegistryEntry::new(addr);
            }
        });

        (Self {
            registry,
            force_conn_tx,
        }, input_rx)
    }

    pub async fn create_peer_connection(&self, addr: SocketAddr) {
        self.force_conn_tx.send(addr).await.unwrap()
    }
}

pub enum NettingMessageKind {
    Noop,
    NewConnection(SocketAddr),
    DroppedConnection {
        client_id: [u8; 9],
    },
    WorldTransfer(Box<World>),
    User(UserAction),
}

impl NettingMessageKind {
    pub fn to_msg(self) -> NettingMessage {
        NettingMessage {
            packet_id: Utc::now().timestamp_millis().to_string(),
            time: Utc::now(),
            kind: self,
        }
    }
}

pub struct NettingMessage {
    packet_id: String,
    time: DateTime<Utc>,
    kind: NettingMessageKind,
}

pub struct PeerRegistryEntry {
    pub client_id: [u8; 9],
    pub rx_task_handle: JoinHandle<()>,
    pub tx_task_handle: JoinHandle<()>,
    pub local_addr: SocketAddr,
}

impl PeerRegistryEntry {
    fn client_id_gen() -> [u8; 9] {
        // While this doesn't guarantee name uniqueness (we'd need a PhD for that shit) this is
        // probably good enough for any reasonable person. It's highly unlikely for two clients to
        // be created at the same millisecond (we're talking O(5) here) and even then, the chaos
        // determinant is for 256.
        //
        // I really don't want to re-negotiate names.
        //
        // Also, this particular random number isn't persisted and we shouldn't rely on this being
        // fixed between saves/sessions.
        let time_component = Utc::now().timestamp_millis().to_be_bytes();
        let chaos_determinant = [rand::random::<u8>()];
        // Network order (big endian) bytes -- 9 bytes!
        debug_assert_eq!(time_component.len() + chaos_determinant.len(), 9);
        [
            time_component[0],
            time_component[1],
            time_component[2],
            time_component[3],
            time_component[4],
            time_component[5],
            time_component[6],
            time_component[7],
            chaos_determinant[0]
        ]
    }

    async fn read_netting_messages(socket: UdpSocket, tx: Sender<NettingMessage>) {
        loop {
            // TODO actually listen to socket.
            let _s = socket;
            // TODO Remove this.
            tx.send(NettingMessageKind::Noop.to_msg()).await;
            loop {
                // listen, ack, gather into NettingMessages based on embedded requestid
                // TODO make work since currently nothing happens automagically
            }
        }
    }

    // Begins a bound channel accepting connections from everywhere.
    pub async fn mine() -> (Self, Receiver<NettingMessage>) {
        let client_id = Self::client_id_gen();
        println!("Client ID: {client_id:?}");

        // Determine which port we're using.
        let socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let local_addr = socket.local_addr().unwrap();
        // TODO Run STUN here to figure out true ip/port. This is what will be sent in the future

        let (netting_message_tx, netting_message_rx) = mpsc::channel::<NettingMessage>(512);
        let rx_task_handle = tokio::spawn(Self::read_netting_messages(socket, netting_message_tx));

        (Self {
            client_id,
            rx_task_handle,
            // We can't transmit since there's nobody on the other side
            tx_task_handle: tokio::spawn(async move {}),
            local_addr,
        }, netting_message_rx)
    }

    // passed_peer_addr is the negotiating socket of the peer if no receiving socket
    //
    // receiving socket being present means that the peer has contacted us, and we sent
    // the address of the receiving socket back to them. the passed_peer_addr is the peer's
    // receiving socket.
    pub async fn new(
        passed_peer_addr: SocketAddr,
        receiving_socket: Option<UdpSocket>,
    ) {
        let (own_socket, peer_addr) = if let Some(s) = peer_recipient_socket {
            (Arc::new(s), passed_peer_addr)
        } else {
            UdpSocket::bind("0.0.0.0:0");
            socket.send_to(NettingMessage::Handshake, peer_addr);
            // TODO wait for ack
            let receiving_addr = socket.recv_from(peer_addr);
            // TODO send ack
            (Arc::new(socket), receiving_addr)
        };
        own_socket.connect(peer_addr);

        // Takes message and splits it into packets
        let (tx_packet_tx_handle, tx_packet_rx_handle) = mpsc::channel::<NettingMessage>(512);
        let tx_socket_ref = ARc::clone(&own_socket);
        let tx_task_handle = tokio::spawn(async move {
            // This should be periodically emptied... how?
            // let mut remembered_messages = HashSet::new();
            loop {
                tx_socket_ref.send();
            }
        });

        // Receives and collates packets into message
        let (rx_packet_tx_handle, rx_packet_rx_handle) = mpsc::channel::<NettingMessage>(512);
        let rx_socket_ref = ARc::clone(&own_socket);
        let rx_task_handle = tokio::spawn(async move {
            loop {
                tx_socket_ref.recv();
            }
        });
    }
}

pub struct PeerRegistry {
    pub me: PeerRegistryEntry,
    pub siblings: Vec<PeerRegistryEntry>,
}

impl PeerRegistry {
    // Returns a receiver for so that the owner of the registry can trigger a new peer connection.
    pub async fn new(mut local_collation_rx: Receiver<SocketAddr>) -> (Self, Receiver<SocketAddr>) {
        let (me, mut network_collation_rx) = PeerRegistryEntry::mine().await;
        println!("Connector port opened {:?}", me.local_addr);

        // Combine new connection queues into same queue.
        let (collation_tx, collation_rx) = mpsc::channel(512);
        let network_merger_tx = collation_tx.clone();
        tokio::spawn(async move {
            loop {
                let Some(msg) = network_collation_rx.recv().await else {
                    // TODO re-start this fiber
                    break;
                };
                let NettingMessageKind::NewConnection(addr) = msg.kind else {
                    continue;
                };
                let Ok(()) = network_merger_tx.send(addr).await else {
                    // TODO re-start this fiber
                    break;
                };
            }
        });
        let local_collation_merger_tx = collation_tx;
        tokio::spawn(async move {
            loop {
                let Some(addr) = local_collation_rx.recv().await else {
                    // TODO re-start this fiber
                    break;
                };
                let Ok(()) = local_collation_merger_tx.send(addr).await else {
                    // TODO re-start this fiber
                    break;
                };
            }
        });

        (Self {
            me,
            siblings: vec![],
        }, collation_rx)
    }

    pub async fn accept_connections() {
    }

    pub async fn tx(&mut self, msg: NettingMessage) {
    }

    pub async fn rx(&mut self) -> NettingMessage {
        NettingMessageKind::Noop.to_msg()
        // ACK message
    }
}
