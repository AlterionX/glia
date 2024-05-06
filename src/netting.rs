use std::{alloc::Layout, collections::HashMap, net::SocketAddr, pin::Pin, sync::Arc};

use chacha::{AeadCore, KeyInit, aead::Aead};
use chrono::{DateTime, TimeDelta, Utc};
use derivative::Derivative;
use mio::{net::UdpSocket, Events, Interest, Poll, Token};
use p256::{ecdh::EphemeralSecret, EncodedPoint, PublicKey};
use rand::rngs::OsRng;
use tokio::{sync::{mpsc::{self, error::TryRecvError, Receiver, Sender}, RwLock}, task::JoinHandle};

use crate::{World, UserAction};

const MAX_UDP_DG_SIZE: usize = 65_535;

/// Struct holding network connections
pub struct Netting {
    pub registry: Arc<RwLock<PeerRegistry>>,
    force_conn_tx: Sender<SocketAddr>,
    force_synapse_transmission_tx: Sender<OutboundSynapseTransmission>,
}

impl Netting {
    // TODO This should probably be yet another event.
    pub async fn new() -> (Self, Receiver<UserAction>) {
        // TODO Actually use these
        let (input_tx, input_rx) = mpsc::channel::<UserAction>(512);

        let (force_conn_tx, force_conn_rx) = mpsc::channel::<SocketAddr>(512);
        let (inner_registry, mut new_connection_trigger, force_synapse_transmission_tx) = PeerRegistry::new(force_conn_rx).await;
        let registry = Arc::new(RwLock::new(inner_registry));

        // Now, let's properly handle that new connection call.
        let new_connection_synapse_transmission_tx = force_synapse_transmission_tx.clone();
        let new_connection_registry = Arc::clone(&registry);
        tokio::spawn(async move {
            loop {
                let Some(addr) = new_connection_trigger.recv().await else {
                    // TODO Repair fiber.
                    break;
                };
                trc::info!("Attempting connection to {addr:?}");
                new_connection_synapse_transmission_tx.send(OutboundSynapseTransmission {
                    kind: OutboundSynapseTransmissionKind::HandshakeInitiate,
                    bytes: vec![],
                    maybe_target_address: Some(addr),
                }).await.expect("handle this properly");
            }
        });

        (Self {
            registry,
            force_conn_tx,
            force_synapse_transmission_tx,
        }, input_rx)
    }

    pub async fn create_peer_connection(&self, addr: SocketAddr) {
        self.force_conn_tx.send(addr).await.unwrap()
    }
}

pub enum NettingMessageKind {
    Noop,
    Handshake,
    NewConnection(SocketAddr),
    DroppedConnection {
        client_id: [u8; 9],
    },
    WorldTransfer(Box<World>),
    User(UserAction),
}

impl NettingMessageKind {
    const HANDSHAKE_DISCRIMINANT: u8 = 149;

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

trait NettingMessenger {
    fn net_send(&self, netting: NettingMessage);
    fn net_recv(&self) -> NettingMessage;
}

impl NettingMessenger for UdpSocket {
    fn net_send(&self, _msg: NettingMessage) {
        unimplemented!();
    }
    fn net_recv(&self) -> NettingMessage {
        unimplemented!();
    }
}

pub enum SocketMessage {
    Netting(NettingMessage),
    RecipientRemove(SocketAddr),
    RecipientCreate(SocketAddr),
}

pub struct PeerRegistryEntry {
    pub client_id: [u8; 9],
    pub task_handle: Option<JoinHandle<()>>,
    pub local_addr: SocketAddr,
}

pub struct RecvReconciliationState {
    pub client_id: [u8; 9],
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum SynapseTransmissionKind {
    HandshakeInitiate,
    // Perhaps this can be combined with initiate, but I'll just leave it as is rn
    HandshakeResponse,
    Ack,
    KnownPacket,
    PeerDiscovery,
}

impl SynapseTransmissionKind {
    pub fn from_discriminant(byte: u8) -> Self {
        match byte >> 5 {
            0b000u8 => SynapseTransmissionKind::HandshakeInitiate,
            0b001u8 => SynapseTransmissionKind::HandshakeResponse,
            0b010u8 => SynapseTransmissionKind::Ack,
            0b011u8 => SynapseTransmissionKind::KnownPacket,
            0b100u8 => SynapseTransmissionKind::PeerDiscovery,
            0b101u8 => unimplemented!("Unknown discriminant 5"),
            0b110u8 => unimplemented!("Unknown discriminant 6"),
            0b111u8 => unimplemented!("Unknown discriminant 7"),
            _ => unreachable!("a u8 shifted over 6 only has 2 not-necessarily 0 bytes"),
        }
    }

    pub fn to_discriminant(&self) -> u8 {
        let base = match self {
            Self::HandshakeInitiate => 0b000u8,
            Self::HandshakeResponse => 0b001u8,
            Self::Ack => 0b010u8,
            Self::KnownPacket => 0b011u8,
            Self::PeerDiscovery => 0b100u8,
        };
        base << 5
    }
}

#[derive(Debug)]
pub struct SynapseTransmission {
    pub kind: SynapseTransmissionKind,
    pub transmission_number: usize,
    pub bytes: Vec<u8>,
}

impl SynapseTransmission {
    /// Datagram structure:
    ///
    /// [  3 bits    ][   5 bits   ][  remainder  ]
    /// [discriminant][  reserved  ][   message   ]
    pub fn parse_datagram(datagram: &[u8]) -> Self {
        let stk = SynapseTransmissionKind::from_discriminant(datagram[0]);
        let transmission_number = datagram.get(1).copied().unwrap_or_default() as usize;
        let bytes = match stk {
            // Handshake Initiate
            //
            // [  1 byte  ][     1 byte     ][   x bytes  ]
            // [ metadata ][ transmission # ][ public key ]
            SynapseTransmissionKind::HandshakeInitiate => {
                Vec::from(&datagram[2..])
            },
            // Handshake Response
            //
            // [  1 byte  ][     1 byte     ][   x bytes  ]
            // [ metadata ][ transmission # ][ public key ]
            SynapseTransmissionKind::HandshakeResponse => {
                Vec::from(&datagram[2..])
            },
            SynapseTransmissionKind::Ack => {
                Vec::new()
            },
            SynapseTransmissionKind::KnownPacket => {
                Vec::from(&datagram[1..])
            },
            SynapseTransmissionKind::PeerDiscovery => {
                Vec::from(&datagram[1..])
            },
        };

        Self {
            kind: stk,
            bytes,
            transmission_number,
        }
    }
}

#[derive(Derivative)]
#[derivative(Debug)]
pub enum OutboundSynapseTransmissionKind {
    HandshakeInitiate,
    // Perhaps this can be combined with initiate, but I'll just leave it as is rn
    HandshakeResponse,
    Ack {
        transmission_number: usize,
    },
    KnownPacket,
    PeerDiscovery,
}

impl OutboundSynapseTransmissionKind {
    pub fn to_discriminant(&self) -> u8 {
        let base = match self {
            Self::HandshakeInitiate => 0b000u8,
            Self::HandshakeResponse => 0b001u8,
            Self::Ack { .. } => 0b010u8,
            Self::KnownPacket => 0b011u8,
            Self::PeerDiscovery => 0b100u8,
        };
        base << 5
    }
}

#[derive(Debug)]
pub struct OutboundSynapseTransmission {
    pub kind: OutboundSynapseTransmissionKind,
    pub bytes: Vec<u8>,
    pub maybe_target_address: Option<SocketAddr>,
}

pub enum PeerKeyRef<'a> {
    Inflight(&'a p256::ecdh::EphemeralSecret),
    Exchanged(&'a p256::ecdh::SharedSecret),
}

#[derive(Derivative)]
#[derivative(Debug)]
pub enum PeerKey {
    Inflight(
        #[derivative(Debug="ignore")]
        p256::ecdh::EphemeralSecret
    ),
    Exchanged(
        #[derivative(Debug="ignore")]
        p256::ecdh::SharedSecret
    ),
}

impl PeerKey {
    pub fn as_ref<'a>(&'a self) -> PeerKeyRef<'a> {
        match self {
            Self::Inflight(ref k) => PeerKeyRef::Inflight(&k),
            Self::Exchanged(ref k) => PeerKeyRef::Exchanged(&k),
        }
    }

    pub fn aead_encrypt(&self, plaintext: &[u8], buffer: &mut [u8]) -> usize {
        let Self::Exchanged(ref k) = self else {
            return 0;
        };
        let mut key_buffer = [0u8; 32];
        k.extract::<sha2::Sha256>(None).expand(&[], &mut key_buffer).expect("key expansion to be fine");
        let aead_nonce = chacha::XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let aead_key = chacha::XChaCha20Poly1305::new(&key_buffer.into());

        let nonce_buffer = &mut buffer[0..aead_nonce.len()];
        nonce_buffer.copy_from_slice(aead_nonce.as_slice());

        let ciphertext_buffer = &mut buffer[aead_nonce.len()..(aead_nonce.len() + plaintext.len())];
        let ciphertext = aead_key.encrypt(&aead_nonce, plaintext).expect("no issues");
        // TODO Get rid of this alloc.
        ciphertext_buffer.copy_from_slice(ciphertext.as_slice());

        ciphertext.len()
    }
}

const MAX_ACTIVE_MESSAGES: usize = 256;

pub struct PeerConnectionData {
    pub exchanged_key: Option<PeerKey>,
    pub active_messages: Pin<Box<[(usize, [u8; MAX_UDP_DG_SIZE]); MAX_ACTIVE_MESSAGES]>>,
    pub next_transmission_number: u8,
}

impl Default for PeerConnectionData {
    fn default() -> Self {
        let buf = unsafe {
            let ptr = std::alloc::alloc(Layout::new::<[(usize, [u8; MAX_UDP_DG_SIZE]); MAX_ACTIVE_MESSAGES]>());
            if ptr.is_null() {
                panic!("oom allocating peer connection buffer");
            }
            Box::from_raw(ptr as *mut[(usize, [u8; MAX_UDP_DG_SIZE]); MAX_ACTIVE_MESSAGES])
        }.into();

        Self {
            exchanged_key: None,
            active_messages: buf,
            next_transmission_number: 0,
        }
    }
}

impl PeerConnectionData {
    pub fn alloc_transmission_number(&mut self) -> u8 {
        self.next_transmission_number = if self.next_transmission_number == 255 {
            0
        } else {
            self.next_transmission_number + 1
        };
        self.next_transmission_number
    }

    pub fn write_synapse_transmission(&mut self, encrypted: bool, discriminant: u8, bytes: &[u8]) {
        debug_assert!(bytes.len() < MAX_UDP_DG_SIZE / 2, "synapse transmission sizes should be less than a single udp packet");

        let transmission_number = self.alloc_transmission_number();
        let buffer = &mut self.active_messages[self.next_transmission_number as usize].1[..];

        buffer[0] = discriminant;
        buffer[1] = transmission_number;

        let bytes_written_to_buffer = if encrypted {
            let Some(ref k) = self.exchanged_key else {
                // TODO Handle this properly, but just drop the packet on the ground for now.
                return;
            };
            k.aead_encrypt(bytes, &mut buffer[2..]) + 2
        } else {
            // data range
            let range = 0..(0 + buffer.len() - 2).min(bytes.len());
            let bytes_to_copy = range.len();
            buffer[2..bytes_to_copy + 2].copy_from_slice(&bytes[range]);
            bytes_to_copy + 2
        };

        self.active_messages[self.next_transmission_number as usize].0 = bytes_written_to_buffer;
    }
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

    const OWN_PORT: Token = Token(0);
    /// Starts up the current machine's networking.
    ///
    /// The synapse-level work will handle connection negotiation and
    /// encryption of higher level systems as well as handling packet replay.
    ///
    /// Higher level systems will defrag synapse transmissions into netting
    /// messages that have actual game data.
    pub async fn mine() -> (Self, Receiver<NettingMessage>, Sender<OutboundSynapseTransmission>) {
        let client_id = Self::client_id_gen();
        trc::info!("Client ID: {client_id:?}");

        // Determine which port we're using to look at the world.
        let mut socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        // TODO Run STUN here to figure out true ip/port. This is what will be sent in the future
        let local_addr = socket.local_addr().unwrap();

        let mut poll = Poll::new().unwrap();
        let mut events = Events::with_capacity(1024);

        // Channels for sending netting messages. These are reconstituted known message synapse
        // transmissions.
        let (known_message_intake_tx, known_message_intake_rx) = mpsc::channel::<Vec<u8>>(1024);

        // Channels for sending synapse transmissions. These are lower level protocols.
        let (external_output_message_tx, mut output_message_rx) = mpsc::channel(1024);
        let internal_output_message_tx = external_output_message_tx.clone();
        let (netting_message_tx, netting_message_rx) = mpsc::channel(1024);

        let task_handle = tokio::spawn(async move {
            poll.registry().register(&mut socket, Self::OWN_PORT, Interest::READABLE | Interest::WRITABLE).unwrap();
            let mut dg_buf_mem = [0u8; MAX_UDP_DG_SIZE];
            let dg_buf = dg_buf_mem.as_mut_slice();
            let mut writable = false;
            let mut readable = false;
            // TODO Convert to array?
            let mut connections: HashMap<SocketAddr, PeerConnectionData> = HashMap::new();

            loop {
                poll.poll(&mut events, Some(TimeDelta::milliseconds(10).to_std().unwrap())).expect("no issues polling");
                for event in events.iter() {
                    match event.token() {
                        Self::OWN_PORT => {
                            if event.is_readable() {
                                readable = true;
                            }

                            if event.is_writable() {
                                writable = true;
                            }
                        },
                        _ => unreachable!("unexpected socket Token"),
                    }
                }

                if readable { 'reader_blk: {
                    let (dg_len, sender_addr) = match socket.recv_from(dg_buf) {
                        Ok(pkt) => pkt,
                        Err(e) => match e.kind() {
                            std::io::ErrorKind::WouldBlock => {
                                readable = false;
                                break 'reader_blk;
                            }
                            _ => {
                                panic!("terminating, unknown input error {e:?}");
                            },
                        },
                    };
                    let dg = &dg_buf[..dg_len];
                    trc::trace!("NET-ISYNT packet landed");

                    let isynt = SynapseTransmission::parse_datagram(dg);

                    let ack_fut = internal_output_message_tx.send(OutboundSynapseTransmission {
                        kind: OutboundSynapseTransmissionKind::Ack {
                            transmission_number: isynt.transmission_number
                        },
                        bytes: vec![],
                        maybe_target_address: Some(sender_addr),
                    });

                    match isynt.kind {
                        SynapseTransmissionKind::HandshakeInitiate => 'initiate_end: {
                            let connection_data = connections.entry(sender_addr).or_default();
                            if connection_data.exchanged_key.is_some() {
                                // We already have a peer -- what's this doing?
                                trc::debug!("NET-ISYNT-HANDSHAKE-INIT connection from {sender_addr:?} rejected");
                                break 'initiate_end;
                            }
                            trc::info!("NET-ISYNT-HANDSHAKE-INIT receiver ecdh init");
                            let peer_public = PublicKey::from_sec1_bytes(isynt.bytes.as_slice()).unwrap();
                            let own_secret = EphemeralSecret::random(&mut OsRng);
                            let own_private = EncodedPoint::from(own_secret.public_key());
                            let own_public = PublicKey::from_sec1_bytes(own_private.as_ref()).unwrap();
                            let shared_key = own_secret.diffie_hellman(&peer_public);
                            trc::debug!("NET-ISYNT-HANDSHAKE-INIT key minted");
                            // Now queue up response.
                            let handshake_internal_output_message_tx = internal_output_message_tx.clone();
                            connection_data.exchanged_key = Some(PeerKey::Exchanged(shared_key));

                            ack_fut.await.expect("no issues sending ack");
                            handshake_internal_output_message_tx.send(OutboundSynapseTransmission {
                                kind: OutboundSynapseTransmissionKind::HandshakeResponse,
                                bytes: own_public.to_sec1_bytes().to_vec(),
                                maybe_target_address: Some(sender_addr),
                            }).await.expect("channel to not be closed");
                        },
                        SynapseTransmissionKind::HandshakeResponse => 'response_end: {
                            let peer_public_bytes = isynt.bytes.as_slice();
                            let Some(connection_data) = connections.get_mut(&sender_addr) else {
                                trc::debug!("NET-ISYNT-HANDSHAKE-RESP rejected -- not initiated");
                                // We have not initiated this connection, this
                                // is the wrong request.
                                break 'response_end;
                            };
                            let Some(PeerKey::Inflight(ref own_secret)) = connection_data.exchanged_key else {
                                trc::debug!(
                                    "NET-ISYNT-HANDSHAKE-RESP rejected -- already complete or not yet started {:?}",
                                    connection_data.exchanged_key
                                );
                                // We have either already initiated this connection, or
                                // not created the key. Deny this.
                                break 'response_end;
                            };
                            trc::info!("NET-ISYNT-HANDSHAKE-RESP wrapping up initiator ecdh");
                            let peer_public = PublicKey::from_sec1_bytes(peer_public_bytes).unwrap();
                            let shared_key = own_secret.diffie_hellman(&peer_public);
                            connection_data.exchanged_key = Some(PeerKey::Exchanged(shared_key));
                            trc::debug!("NET-ISYNT-HANDSHAKE-RESP complete");

                            ack_fut.await.expect("no issues sending ack");
                        },
                        SynapseTransmissionKind::PeerDiscovery => unimplemented!("peer connection not yet working"),
                        SynapseTransmissionKind::Ack => 'ack_end: {
                            let Some(connection_data) = connections.get_mut(&sender_addr) else {
                                trc::debug!("NET-ISYNT-ACK ignoring ack");
                                // We have not initiated this connection yet, this is a bad request.
                                break 'ack_end;
                            };
                            trc::info!("NET-ISYNT-ACK tx_no={:?}", isynt.transmission_number);
                            // TODO Reset?
                            connection_data.active_messages[isynt.transmission_number].0 = 0;
                        },
                        SynapseTransmissionKind::KnownPacket => 'known_packet_end: {
                            let Some(connection_data) = connections.get_mut(&sender_addr) else {
                                // We aren't connected, reject their request.
                                break 'known_packet_end;
                            };
                            // TODO Decrypt
                            // TODO reassemble multi-datagram transmissions
                            netting_message_tx.send(unimplemented!("packet parsing not ready yet")).await.expect("channel to not be closed");

                            ack_fut.await.expect("no issues sending ack");
                        },
                    }
                }}

                if writable { 'writer_blk: {
                    for (i, (&addr, (st_len, st_bytes))) in connections.iter_mut().flat_map(|(addr, data)| data.active_messages.iter_mut().map(move |active_message| (addr, active_message)).enumerate()) {
                        if *st_len == 0 {
                            // Skip entry since it's not a message
                            continue;
                        }
                        trc::info!("NET-WRITE writing message... {i:?} <<>> {st_len:?} <<>> {:?}", SynapseTransmissionKind::from_discriminant(st_bytes[0]));
                        // We're handling disconnected sockets by crashing... maybe
                        // I'll fix this in the future, but for now...
                        let sent = match socket.send_to(&st_bytes[..*st_len], addr) {
                            Ok(sent_len) => sent_len,
                            Err(e) => match e.kind() {
                                std::io::ErrorKind::WouldBlock => {
                                    writable = false;
                                    break 'writer_blk;
                                },
                                _ => {
                                    panic!("terminating, unknown input error {e:?}");
                                },
                            },
                        };
                        debug_assert_eq!(sent, *st_len, "sent as much bytes as in the message");
                        // Acks will get zeroed out no matter what since they're one time send.
                        // TODO Support limited resends appropriately.
                        if st_bytes[0] == SynapseTransmissionKind::Ack.to_discriminant() {
                            *st_len = 0;
                        }
                        trc::info!("NET-WRITE message written");
                    }
                }}

                let start_time = Utc::now();
                let mut num_msgs_written = 0;
                while let Some(osynt) = 'msg_rx_read: {
                    // TODO Try to only do this once per frame.
                    let time_limit_reached = (Utc::now() - start_time) > TimeDelta::milliseconds(1000 / 20);
                    if time_limit_reached {
                        break 'msg_rx_read None;
                    }
                    if num_msgs_written > 20 {
                        break 'msg_rx_read None;
                    }
                    match output_message_rx.try_recv() {
                        Ok(msg_and_addr) => Some(msg_and_addr),
                        Err(TryRecvError::Empty) => None,
                        Err(TryRecvError::Disconnected) => None,
                    }
                } {
                    let discriminant = osynt.kind.to_discriminant();
                    let (encrypted, bytes) = match &osynt.kind {
                        OutboundSynapseTransmissionKind::HandshakeInitiate => {
                            let Some(target_addr) = osynt.maybe_target_address else {
                                trc::debug!("NET-OSYNT-HANDSHAKE-INIT handshake synt with unknown addr");
                                continue;
                            };
                            trc::debug!("NET-OSYNT-HANDSHAKE-INIT creating connection data records...");
                            let connection_data = connections.entry(target_addr).or_default();
                            if connection_data.exchanged_key.is_some() {
                                // We already have a peer -- what's this doing?
                                trc::debug!("NET-OSYNT-HANDSHAKE-INIT already connecting or connected to {target_addr:?}");
                                continue;
                            }
                            trc::info!("NET-OSYNT-HANDSHAKE-INIT ecdh init");
                            let own_secret = EphemeralSecret::random(&mut OsRng);
                            let own_private = EncodedPoint::from(own_secret.public_key());
                            let own_public = PublicKey::from_sec1_bytes(own_private.as_ref()).unwrap();
                            connection_data.exchanged_key = Some(PeerKey::Inflight(own_secret));
                            trc::debug!("NET-OSYNT-HANDSHAKE-INIT complete!");
                            (false, own_public.to_sec1_bytes().to_vec())
                        },
                        OutboundSynapseTransmissionKind::HandshakeResponse => {
                            trc::info!("NOT-OSYNT-HANDSHAKE-RESP sending");
                            (false, osynt.bytes)
                        },
                        OutboundSynapseTransmissionKind::Ack { .. } => {
                            (false, osynt.bytes)
                        },
                        OutboundSynapseTransmissionKind::PeerDiscovery => {
                            (true, osynt.bytes)
                        },
                        OutboundSynapseTransmissionKind::KnownPacket => {
                            (true, osynt.bytes)
                        },
                    };

                    if let Some(ref addr) = osynt.maybe_target_address {
                        // Just drop the packet if we're not connected yet.
                        if let Some(connection_data) = connections.get_mut(addr) {
                            connection_data.write_synapse_transmission(encrypted, discriminant, bytes.as_slice());
                        }
                    }
                    for (_, data) in connections.iter_mut() {
                        data.write_synapse_transmission(encrypted, discriminant, bytes.as_slice());
                        num_msgs_written += 1;
                    }
                }
            }
        });

        (Self {
            client_id,
            task_handle: Some(task_handle),
            local_addr,
        }, netting_message_rx, external_output_message_tx)
    }

    pub async fn new(passed_peer_addr: SocketAddr) {
    }
}

pub struct PeerRegistry {
    // Reference to send/recv socket is hidden.
    pub me: PeerRegistryEntry,
    pub siblings: Vec<PeerRegistryEntry>,
}

impl PeerRegistry {
    // Returns a receiver for so that the owner of the registry can trigger a new peer connection.
    pub async fn new(mut local_collation_rx: Receiver<SocketAddr>) -> (Self, Receiver<SocketAddr>, Sender<OutboundSynapseTransmission>) {
        let (me, mut network_collation_rx, synapse_transmission_tx) = PeerRegistryEntry::mine().await;
        trc::info!("Connector port opened {:?}", me.local_addr);

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
        }, collation_rx, synapse_transmission_tx)
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
