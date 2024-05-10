use std::{alloc::Layout, collections::HashMap, net::SocketAddr, pin::Pin, sync::Arc};

use chacha::{aead::Aead, AeadCore, KeyInit};
use chrono::{TimeDelta, Utc};
use derivative::Derivative;
use mio::{net::UdpSocket, Events, Interest, Poll, Token};
use p256::{ecdh::EphemeralSecret, EncodedPoint, PublicKey};
use rand::rngs::OsRng;
use sha2::Digest;
use tokio::{sync::{mpsc::{self, error::TryRecvError, Receiver, Sender}, RwLock}, task::JoinHandle};

use crate::{World, UserAction};

const MAX_UDP_DG_SIZE: usize = 65_535;
const MAX_KNOWN_PACKET_LEN: usize = MAX_UDP_DG_SIZE / 2;
const HANDSHAKE_GREETING: &[u8] = b"hello";

#[derive(Debug, Clone, Copy)]
pub struct ClientId([u8; ClientId::LEN]);

impl ClientId {
    const LEN: usize = 9;

    fn gen() -> Self {
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
        Self([
            time_component[0],
            time_component[1],
            time_component[2],
            time_component[3],
            time_component[4],
            time_component[5],
            time_component[6],
            time_component[7],
            chaos_determinant[0]
        ])
    }

    fn iter<'a>(&'a self) -> impl Iterator<Item=u8> + 'a {
        self.0.iter().copied()
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != Self::LEN {
            return None;
        }

        Some(Self([
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
            bytes[4],
            bytes[5],
            bytes[6],
            bytes[7],
            bytes[8],
        ]))
    }
}

/// Struct holding network connections
pub struct Netting {
    pub registry: PeerRegistry,
    force_osynt_tx: Sender<OutboundSynapseTransmission>,
    netting_tx: Sender<(NettingMessage, Option<SocketAddr>)>,
}

impl Netting {
    // TODO This should probably be yet another event.
    pub async fn new() -> (Self, Receiver<NettingMessage>) {
        // TODO Actually use these
        let (registry, netting_rx, force_osynt_tx, netting_tx) = PeerRegistry::new().await;

        (Self {
            registry,
            force_osynt_tx,
            netting_tx,
        }, netting_rx)
    }

    // TODO Make this return the client id of the peer.
    pub async fn create_peer_connection(&self, addr: SocketAddr) {
        self.force_osynt_tx.send(OutboundSynapseTransmission {
            kind: OutboundSynapseTransmissionKind::HandshakeInitiate,
            bytes: vec![],
            maybe_target_address: Some(addr),
        }).await.expect("no issues sending");
    }

    pub async fn broadcast(&self, msg: &str) {
        self.netting_tx
            .send((NettingMessageKind::NakedLogString(msg.to_owned()).to_msg(), None))
            .await
            .expect("no issues sending");
    }

    pub async fn send_to(&self, msg: &str, peer: ClientId) {
        self.netting_tx
            .send((NettingMessageKind::NakedLogString(msg.to_owned()).to_msg(), None))
            .await
            .expect("no issues sending");
    }
}

#[derive(Debug)]
pub enum NettingMessageKind {
    Noop,
    NakedLogString(String),
    Handshake,
    NewConnection(SocketAddr),
    DroppedConnection {
        client_id: ClientId,
    },
    WorldTransfer(Box<World>),
    User(UserAction),
}

impl NettingMessageKind {
    const HANDSHAKE_DISCRIMINANT: u8 = 149;

    pub fn to_msg(self) -> NettingMessage {
        NettingMessage {
            message_id: Utc::now().timestamp_millis() as u64,
            kind: self,
        }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Noop => vec![0],
            Self::Handshake => vec![1],
            Self::NewConnection(_addr) => vec![2],
            Self::DroppedConnection { client_id: _ } => vec![3],
            Self::WorldTransfer(_w) => vec![4],
            Self::User(_u) => vec![5],
            Self::NakedLogString(log_str) => {
                let mut v = log_str.into_bytes();
                v.push(6);
                v
            },
        }
    }

    // TODO make this return Result instead of unwrapping
    pub fn parse(mut bytes: Vec<u8>) -> Self {
        match bytes.last().expect("bytes should not be empty") {
            0 => Self::Noop,
            1 => Self::Handshake,
            2 => Self::NewConnection(unimplemented!("wat")),
            3 => Self::DroppedConnection { client_id: ClientId(bytes.try_into().unwrap()) },
            4 => Self::WorldTransfer(unimplemented!("wat")),
            5 => Self::User(unimplemented!("wat")),
            6 => {
                bytes.pop();
                Self::NakedLogString(String::from_utf8(bytes).expect("only string passed"))
            },
            e => unimplemented!("unknown netting message kind discriminant {e:?}"),
        }
    }
}

#[derive(Debug)]
pub struct NettingMessageBytesWithOrdering {
    pub packet_index: u8,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub enum NettingMessageParseError {
    PartialMessage {
        message_id: u64,
        expected_len: u64,
        msg: NettingMessageBytesWithOrdering
    },
}

#[derive(Debug)]
pub struct NettingMessage {
    pub message_id: u64,
    pub kind: NettingMessageKind,
}

impl NettingMessage {
    pub fn into_known_packet_bytes(self) -> Vec<Vec<u8>> {
        let unfettered_bytes = self.kind.into_bytes();
        let common_header: Vec<_> = (unfettered_bytes.len() as u64).to_be_bytes().into_iter().chain(self.message_id.to_be_bytes().into_iter()).collect();
        // We expect max of u8::MAX packets. Give up otherwise
        assert!(unfettered_bytes.len() / (u8::MAX as usize) < MAX_KNOWN_PACKET_LEN, "netting message too big for protocol");
        let chunk_size = MAX_KNOWN_PACKET_LEN - common_header.len() - 1;
        let mut assembled_packets = Vec::with_capacity((unfettered_bytes.len() + chunk_size - 1) / chunk_size);
        for (i, chunk) in unfettered_bytes.chunks(chunk_size).enumerate() {
            let assembled_packet: Vec<_> = common_header.iter().copied().chain(std::iter::once(i as u8)).chain(chunk.into_iter().copied()).collect();
            assembled_packets.push(assembled_packet);
        }
        assembled_packets
    }

    pub fn parse(mut bytes: Vec<u8>) -> Result<NettingMessage, NettingMessageParseError> {
        let expected_msg_len = u64::from_be_bytes([
            bytes[0],
            bytes[1],
            bytes[2],
            bytes[3],
            bytes[4],
            bytes[5],
            bytes[6],
            bytes[7],
        ]);
        let message_id = u64::from_be_bytes([
            bytes[8 + 0],
            bytes[8 + 1],
            bytes[8 + 2],
            bytes[8 + 3],
            bytes[8 + 4],
            bytes[8 + 5],
            bytes[8 + 6],
            bytes[8 + 7],
        ]);
        let expected_pkt_index = bytes[2 * 8];
        drop(bytes.drain(..2 * 8 + 1));

        if bytes.len() as u64 == expected_msg_len {
            Ok(NettingMessage {
                message_id,
                kind: NettingMessageKind::parse(bytes),
            })
        } else {
            Err(NettingMessageParseError::PartialMessage {
                message_id,
                expected_len: expected_msg_len,
                msg: NettingMessageBytesWithOrdering {
                    packet_index: expected_pkt_index,
                    bytes,
                },
            })
        }
    }
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
    pub client_id: ClientId,
    pub local_addr: SocketAddr,
}

pub struct RecvReconciliationState {
    pub client_id: [u8; 9],
}

pub struct AttributedInboundBytes {
    pub decrypted_bytes: Vec<u8>,
    pub addr: SocketAddr,
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
            // Known Packet
            //
            // [  remainder  ]
            // [encrypted msg]
            SynapseTransmissionKind::KnownPacket => {
                Vec::from(&datagram[2..])
            },
            SynapseTransmissionKind::PeerDiscovery => {
                Vec::from(&datagram[2..])
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

#[derive(Derivative)]
#[derivative(Debug)]
pub enum PeerKey {
    Inflight(
        #[derivative(Debug="ignore")]
        p256::ecdh::EphemeralSecret
    ),
    Exchanged {
        #[derivative(Debug="ignore")]
        secret: p256::ecdh::SharedSecret,
        #[derivative(Debug="ignore")]
        key: chacha::XChaCha20Poly1305,
    },
}

impl PeerKey {
    pub fn exchanged_from_shared_secret(shared_secret: p256::ecdh::SharedSecret) -> Self {
        let mut key_buffer = [0u8; 32];
        shared_secret.extract::<sha2::Sha256>(None).expand(&[], &mut key_buffer).expect("key expansion to be fine");
        let aead_key = chacha::XChaCha20Poly1305::new(&key_buffer.into());
        Self::Exchanged {
            secret: shared_secret,
            key: aead_key
        }
    }

    pub fn fixed_nonce_aead_encrypt(&self, aead_nonce: &[u8], cleartext: &[u8], buffer: &mut [u8]) -> usize {
        let Self::Exchanged { key: ref aead_key, .. } = self else { return 0; };

        let nonce_buffer = &mut buffer[0..aead_nonce.len()];
        nonce_buffer.copy_from_slice(aead_nonce);

        let ciphertext = aead_key.encrypt(aead_nonce.into(), cleartext).expect("no issues");
        let ciphertext_buffer = &mut buffer[aead_nonce.len()..(aead_nonce.len() + ciphertext.len())];
        // TODO Get rid of this alloc.
        ciphertext_buffer[..ciphertext.len()].copy_from_slice(ciphertext.as_slice());

        ciphertext.len() + aead_nonce.len()
    }

    pub fn aead_encrypt(&self, cleartext: &[u8], buffer: &mut [u8]) -> usize {
        let aead_nonce = chacha::XChaCha20Poly1305::generate_nonce(&mut OsRng);

        self.fixed_nonce_aead_encrypt(aead_nonce.as_slice(), cleartext, buffer)
    }

    pub fn aead_decrypt(&self, ciphertext: &[u8]) -> Option<Vec<u8>> {
        let Self::Exchanged { key: ref aead_key, .. } = self else { return None; };
        // TODO remove this
        let sample_nonce = chacha::XChaCha20Poly1305::generate_nonce(&mut OsRng);

        let aead_nonce = &ciphertext[0..sample_nonce.len()];
        let raw_ciphertext_buffer = &ciphertext[aead_nonce.len()..];

        let cleartext = match aead_key.decrypt(aead_nonce.into(), raw_ciphertext_buffer) {
            Ok(a) => a,
            Err(e) => {
                trc::error!("no issues expected for decryption, err: {e:?}");
                return None;
            },
        };

        Some(cleartext)
    }
}

const MAX_ACTIVE_MESSAGES: usize = 256;

pub struct PeerConnectionData {
    pub exchanged_key: Option<PeerKey>,
    pub active_messages: Pin<Box<[(usize, [u8; MAX_UDP_DG_SIZE]); MAX_ACTIVE_MESSAGES]>>,
    pub recent_received_messages: Pin<Box<[(chrono::DateTime<chrono::Utc>, Vec<u8>); MAX_ACTIVE_MESSAGES]>>,
    pub next_transmission_number: u8,
}

impl Default for PeerConnectionData {
    fn default() -> Self {
        let buf = unsafe {
            let ptr = std::alloc::alloc(Layout::new::<[(usize, [u8; MAX_UDP_DG_SIZE]); MAX_ACTIVE_MESSAGES]>());
            if ptr.is_null() {
                panic!("oom allocating peer connection buffer");
            }
            Pin::new(Box::from_raw(ptr as *mut[(usize, [u8; MAX_UDP_DG_SIZE]); MAX_ACTIVE_MESSAGES]))
        }.into();
        let mut recent_received_messages: Pin<Box<[(chrono::DateTime<chrono::Utc>, Vec<u8>); MAX_ACTIVE_MESSAGES]>> = unsafe {
            let ptr = std::alloc::alloc(Layout::new::<[(chrono::DateTime<chrono::Utc>, Vec<u8>); MAX_ACTIVE_MESSAGES]>());
            if ptr.is_null() {
                panic!("oom allocating peer connection buffer");
            }
            Pin::new(Box::from_raw(ptr as *mut[(chrono::DateTime<chrono::Utc>, Vec<u8>); MAX_ACTIVE_MESSAGES]))
        }.into();
        for (a, b) in recent_received_messages.iter_mut() {
            // Pretend this is just prior to "debounce" period.
            *a = Utc::now() - TimeDelta::milliseconds(300);
            *b = vec![];
        }

        Self {
            exchanged_key: None,
            active_messages: buf,
            recent_received_messages,
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

    pub fn recently_received(&mut self, isynt: &SynapseTransmission, checksum: Vec<u8>) -> bool {
        let now = Utc::now();
        let elapsed = now - self.recent_received_messages[isynt.transmission_number].0;
        let stashed_checksum = &self.recent_received_messages[isynt.transmission_number].1;
        // TODO Improve this?
        let is_mismatch = elapsed > TimeDelta::milliseconds(100) || *stashed_checksum != checksum;
        if is_mismatch {
            self.recent_received_messages[isynt.transmission_number] = (now, checksum);
        }
        is_mismatch
    }

    pub fn write_synapse_transmission(&mut self, encrypted: bool, discriminant: u8, bytes: &[u8]) {
        debug_assert!(bytes.len() < MAX_KNOWN_PACKET_LEN, "synapse transmission sizes should be less than a single udp packet");

        let transmission_number = self.alloc_transmission_number();
        trc::trace!("NET-OSYNT Allocating transmission number {transmission_number:?} for {:?}", discriminant >> 5);
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

    pub fn decrypt(&self, bytes: &[u8]) -> Option<Vec<u8>> {
        let Some(ref k) = self.exchanged_key else {
            // TODO Handle this properly, but just drop the packet on the ground for now.
            return None;
        };
        k.aead_decrypt(bytes)
    }
}

impl PeerRegistryEntry {
    /// Starts up the current machine's networking.
    ///
    /// The synapse-level work will handle connection negotiation and
    /// encryption of higher level systems as well as handling packet replay.
    ///
    /// Higher level systems will defrag synapse transmissions into netting
    /// messages that have actual game data.
    pub async fn mine() -> (Self, UdpSocket) {
        let client_id = ClientId::gen();
        trc::info!("NET-INIT Own Client ID: {client_id:?}");

        // Determine which port we're using to look at the world.
        let socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        // TODO Run STUN here to figure out true ip/port. This is what will be sent in the future
        let local_addr = socket.local_addr().unwrap();

        (Self {
            client_id,
            local_addr,
        }, socket)
    }
}

pub struct PeerRegistry {
    // Reference to send/recv socket is hidden.
    pub me: PeerRegistryEntry,

    // Registry processes.
    pub udp_synt_interop_task_handle: JoinHandle<()>,
    pub netting_to_known_task_handle: JoinHandle<()>,
    pub known_to_netting_task_handle: JoinHandle<()>,
    pub peer_connected_task_handle: JoinHandle<()>,

    pub siblings: Arc<RwLock<Vec<PeerRegistryEntry>>>,
}

impl PeerRegistry {
    const OWN_PORT: Token = Token(0);
    // Returns a receiver for so that the owner of the registry can trigger a new peer connection.
    pub async fn new() -> (
        Self,
        Receiver<NettingMessage>,
        Sender<OutboundSynapseTransmission>,
        Sender<(NettingMessage, Option<SocketAddr>)>,
    ) {
        let (me, mut socket) = PeerRegistryEntry::mine().await;
        let siblings = Arc::new(RwLock::new(Vec::new()));
        trc::info!("NET-INIT Connector port opened {:?}", me.local_addr);

        let mut poll = Poll::new().unwrap();
        let mut events = Events::with_capacity(1024);

        // Channels for sending netting messages. These are reconstituted known message synapse
        // transmissions.
        let (inbound_known_tx, mut inbound_known_rx) = mpsc::channel(1024);
        let (outbound_netting_tx, mut outbound_netting_rx) = mpsc::channel::<(NettingMessage, Option<SocketAddr>)>(1024);
        let (inbound_netting_tx, inbound_netting_rx) = mpsc::channel(1024);

        // Channels for sending synapse transmissions. These are lower level protocols.
        let (external_osynt_tx, mut osynt_rx) = mpsc::channel(1024);
        let internal_osynt_tx = external_osynt_tx.clone();

        let (connection_notification_tx, mut connection_notification_rx) = mpsc::channel(10);

        let own_client_id = me.client_id;
        let udp_synt_interop_task_handle = tokio::spawn(async move {
            poll.registry().register(&mut socket, Self::OWN_PORT, Interest::READABLE | Interest::WRITABLE).unwrap();
            let mut dg_buf_mem = [0u8; MAX_UDP_DG_SIZE];
            let dg_buf = dg_buf_mem.as_mut_slice();
            let mut writable = false;
            let mut readable = false;
            // TODO Convert to array?
            let mut connections: HashMap<SocketAddr, PeerConnectionData> = HashMap::new();
            let (ack_tx, mut ack_rx) = mpsc::channel(512);

            // TODO Make this await a bit more.
            loop {
                tokio::time::sleep(TimeDelta::milliseconds(1).to_std().unwrap()).await;
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

                    let oneshot_ack_tx = ack_tx.clone();
                    let ack_fut = async move { oneshot_ack_tx.send((isynt.transmission_number, sender_addr)).await };
                    let checksum = {
                        let mut hasher = sha2::Sha256::new();
                        hasher.update(dg);
                        hasher.finalize()
                    };

                    match isynt.kind {
                        // [5 bytes; hello][9 bytes client id][remainder; peer_secret]
                        SynapseTransmissionKind::HandshakeInitiate => 'initiate_end: {
                            let connection_data = connections.entry(sender_addr).or_default();
                            if connection_data.recently_received(&isynt, checksum.to_vec()) {
                                break 'initiate_end;
                            }
                            if connection_data.exchanged_key.is_some() {
                                // We already have a peer -- what's this doing?
                                trc::debug!("NET-ISYNT-HANDSHAKE-INIT connection from {sender_addr:?} rejected");
                                break 'initiate_end;
                            }

                            // TODO eventually sign this? It's not a problem for an attacker to
                            // know our client id, I don't think...
                            if isynt.bytes.len() < HANDSHAKE_GREETING.len() + ClientId::LEN {
                                break 'initiate_end;
                            }
                            let peer_client_id_bytes = &isynt.bytes[HANDSHAKE_GREETING.len()..][..ClientId::LEN];
                            let peer_client_id = ClientId::from_bytes(peer_client_id_bytes).expect("parsing to work");
                            let peer_public_bytes = &isynt.bytes[HANDSHAKE_GREETING.len()..][ClientId::LEN..];

                            let mut outbound_buffer = vec![0u8; MAX_UDP_DG_SIZE];

                            trc::info!("NET-ISYNT-HANDSHAKE-INIT receiver ecdh init");
                            let peer_public = PublicKey::from_sec1_bytes(peer_public_bytes).unwrap();
                            let own_secret = EphemeralSecret::random(&mut OsRng);
                            let own_private = EncodedPoint::from(own_secret.public_key());
                            let own_public = PublicKey::from_sec1_bytes(own_private.as_ref()).unwrap();
                            let shared_key = own_secret.diffie_hellman(&peer_public);
                            trc::debug!("NET-ISYNT-HANDSHAKE-INIT key minted");
                            // Now queue up response.
                            let handshake_internal_output_message_tx = internal_osynt_tx.clone();
                            let combined_key = PeerKey::exchanged_from_shared_secret(shared_key);
                            { // TODO Implement a permissioning system.
                                // We'll write out the client id so that the exchange will also
                                // record the client id.
                                trc::debug!("NET-ISYNT-HANDSHAKE-INIT key minted");
                                let public_bytes = own_public.to_sec1_bytes();
                                let message = HANDSHAKE_GREETING.iter().copied().chain(own_client_id.iter()).collect::<Vec<_>>();
                                let encrypted_bytes = combined_key.aead_encrypt(&message, &mut outbound_buffer[8..]);
                                outbound_buffer[..8].copy_from_slice(&encrypted_bytes.to_be_bytes());
                                outbound_buffer[8..][encrypted_bytes..][..public_bytes.len()].copy_from_slice(&public_bytes);
                                outbound_buffer.truncate(encrypted_bytes + 8 + public_bytes.len());
                            }
                            connection_data.exchanged_key = Some(combined_key);

                            ack_fut.await.expect("no issues sending ack");
                            handshake_internal_output_message_tx.send(OutboundSynapseTransmission {
                                kind: OutboundSynapseTransmissionKind::HandshakeResponse,
                                bytes: outbound_buffer,
                                maybe_target_address: Some(sender_addr),
                            }).await.expect("channel to not be closed");
                            connection_notification_tx.send((peer_client_id, sender_addr)).await.expect("rx to be present");
                        },
                        // TODO Make the HandshakeResponse send another packet to confirm.
                        // [8 bytes; mlen][mlen bytes; ciphertext][remainder; peer_secret]
                        SynapseTransmissionKind::HandshakeResponse => 'response_end: {
                            if isynt.bytes.len() < 8 {
                                break 'response_end;
                            }
                            let mlen = u64::from_be_bytes([
                                isynt.bytes[0],
                                isynt.bytes[1],
                                isynt.bytes[2],
                                isynt.bytes[3],
                                isynt.bytes[4],
                                isynt.bytes[5],
                                isynt.bytes[6],
                                isynt.bytes[7],
                            ]) as usize;
                            if isynt.bytes.len() < 8 + mlen {
                                break 'response_end;
                            }
                            let ciphertext = &isynt.bytes[8..][..mlen];
                            let peer_public_bytes = &isynt.bytes[8..][mlen..];

                            let Some(connection_data) = connections.get_mut(&sender_addr) else {
                                trc::debug!("NET-ISYNT-HANDSHAKE-RESP rejected -- not initiated");
                                // We have not initiated this connection, this
                                // is the wrong request.
                                break 'response_end;
                            };
                            if connection_data.recently_received(&isynt, checksum.to_vec()) {
                                break 'response_end;
                            }
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
                            let combined_key = PeerKey::exchanged_from_shared_secret(shared_key);

                            // Now use the combined key.
                            let Some(peer_client_id_and_greeting) = combined_key.aead_decrypt(ciphertext) else {
                                // Bad packet, deny.
                                break 'response_end;
                            };
                            if peer_client_id_and_greeting.len() != HANDSHAKE_GREETING.len() + ClientId::LEN {
                                // Bad packet, deny.
                                break 'response_end;
                            }
                            if &peer_client_id_and_greeting[..HANDSHAKE_GREETING.len()] != HANDSHAKE_GREETING {
                                // Bad packet, deny.
                                break 'response_end;
                            }
                            let peer_client_id_bytes = &peer_client_id_and_greeting[HANDSHAKE_GREETING.len()..];
                            let peer_client_id = ClientId([
                                peer_client_id_bytes[0],
                                peer_client_id_bytes[1],
                                peer_client_id_bytes[2],
                                peer_client_id_bytes[3],
                                peer_client_id_bytes[4],
                                peer_client_id_bytes[5],
                                peer_client_id_bytes[6],
                                peer_client_id_bytes[7],
                                peer_client_id_bytes[8],
                            ]);

                            connection_data.exchanged_key = Some(combined_key);
                            trc::debug!("NET-ISYNT-HANDSHAKE-RESP complete");
                            connection_notification_tx.send((peer_client_id, sender_addr)).await.expect("rx to be present");

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
                            connection_data.active_messages[isynt.transmission_number].0 = 0;
                        },
                        SynapseTransmissionKind::KnownPacket => 'known_packet_end: {
                            let Some(connection_data) = connections.get_mut(&sender_addr) else {
                                // We aren't connected, reject their request.
                                break 'known_packet_end;
                            };
                            if connection_data.recently_received(&isynt, checksum.to_vec()) {
                                break 'known_packet_end;
                            }
                            let Some(decrypted_bytes) = connection_data.decrypt(&isynt.bytes) else {
                                // Crypto thinks they're terrible, ignore their message.
                                break 'known_packet_end;
                            };
                            inbound_known_tx.send(AttributedInboundBytes {
                                decrypted_bytes,
                                addr: sender_addr,
                            }).await.expect("channel to not be closed");

                            ack_fut.await.expect("no issues sending ack");
                        },
                    }
                }}

                if writable { 'writer_blk: {
                    while let Ok((tx_no, addr)) = ack_rx.try_recv() {
                        trc::trace!("NET-WRITE sending ack {tx_no:?} to {addr:?}");
                        socket.send_to(&[
                            OutboundSynapseTransmissionKind::Ack { transmission_number: tx_no }.to_discriminant(),
                            tx_no as u8
                        ], addr).expect("socket to be okay");
                    }
                    // TODO this should back off from time to time
                    for (i, (&addr, (st_len, st_bytes))) in connections.iter_mut().flat_map(|(addr, data)| data.active_messages.iter_mut().map(move |active_message| (addr, active_message)).enumerate()) {
                        if *st_len == 0 {
                            // Skip entry since it's not a message
                            continue;
                        }
                        trc::trace!("NET-WRITE writing message... {i:?} <<>> {st_len:?} <<>> {:?}", SynapseTransmissionKind::from_discriminant(st_bytes[0]));
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
                        trc::trace!("NET-WRITE message written");
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
                    match osynt_rx.try_recv() {
                        Ok(msg_and_addr) => Some(msg_and_addr),
                        Err(TryRecvError::Empty) => None,
                        Err(TryRecvError::Disconnected) => None,
                    }
                } {
                    trc::info!("NET-OSYNT queuing message {osynt:?}");
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
                            // TODO sign these bytes
                            let greeting_bytes =
                                HANDSHAKE_GREETING.iter().copied()
                                .chain(own_client_id.iter())
                                .chain(own_public.to_sec1_bytes().iter().copied())
                                .collect();
                            (false, greeting_bytes)
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
                    } else {
                        for (_, data) in connections.iter_mut() {
                            data.write_synapse_transmission(encrypted, discriminant, bytes.as_slice());
                            num_msgs_written += 1;
                        }
                    }
                }
            }
        });

        let converted_osynt_tx = external_osynt_tx.clone();
        let netting_to_known_task_handle = tokio::spawn(async move {
            loop {
                let (msg, addr) = outbound_netting_rx.recv().await.expect("tx channel to not be dropped");
                // TODO Break apart into multiple osynts and shove through the network. Drop for
                // now.
                for bytes in msg.into_known_packet_bytes() {
                    converted_osynt_tx.send(OutboundSynapseTransmission {
                        kind: OutboundSynapseTransmissionKind::KnownPacket,
                        bytes,
                        maybe_target_address: addr,
                    }).await.expect("rx channel to not be dropped");
                }
            }
        });

        let known_to_netting_task_handle = tokio::spawn(async move {
            // TODO Periodically clean up old values.
            let mut cached_partials: HashMap<_, Vec<NettingMessageBytesWithOrdering>> = HashMap::new();
            loop {
                let msg = match inbound_known_rx.recv().await {
                    Some(data) => { data },
                    None => {
                        trc::error!("tx channel to not be dropped");
                        return;
                    },
                };
                let opt_msg = match NettingMessage::parse(msg.decrypted_bytes) {
                    Ok(msg) => Some(msg),
                    Err(NettingMessageParseError::PartialMessage {
                        message_id,
                        expected_len,
                        msg: addressed_message_bytes,
                    }) => {
                        let cache = cached_partials.entry((msg.addr, message_id, expected_len)).or_default();
                        if cache.iter().all(|a| a.packet_index != addressed_message_bytes.packet_index) {
                            cache.push(addressed_message_bytes);
                        }
                        let loaded_byte_count = cache.iter()
                            .map(|a| a.bytes.len() as u64)
                            .sum::<u64>();
                        if loaded_byte_count == expected_len {
                            cache.sort_by_key(|a| a.packet_index);
                            let cache = cached_partials.remove(&(msg.addr, message_id, expected_len)).expect("value I just had to be here");
                            let combined_message_bytes = expected_len.to_be_bytes().into_iter()
                                .chain(message_id.to_be_bytes())
                                .chain(cache.into_iter().flat_map(|a| a.bytes.into_iter()))
                                .collect::<Vec<_>>();
                            Some(NettingMessage::parse(combined_message_bytes)
                                .expect("message should have fully loaded at this point"))
                        } else {
                            None
                        }
                    },
                };
                let Some(msg) = opt_msg else {
                    // If we couldn't load a message, we're done for the loop, carry on!
                    continue;
                };
                inbound_netting_tx.send(msg).await.expect("rx channel to not be dropped");
            }
        });

        let siblings_ref = Arc::clone(&siblings);
        let peer_connected_task_handle = tokio::spawn(async move {
            loop {
                let Some((peer_id, addr)) = connection_notification_rx.recv().await else {
                    continue;
                };
                siblings_ref.write().await.push(PeerRegistryEntry {
                    client_id: peer_id,
                    local_addr: addr,
                });
            }
        });

        (Self {
            me,
            udp_synt_interop_task_handle,
            netting_to_known_task_handle,
            known_to_netting_task_handle,
            peer_connected_task_handle,
            siblings,
        }, inbound_netting_rx, external_osynt_tx, outbound_netting_tx)
    }
}
