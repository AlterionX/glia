mod connman;
mod collater;
mod parceler;

use std::{fmt::Debug, net::SocketAddr, sync::Arc};

use chrono::Utc;
use derivative::Derivative;
use mio::net::UdpSocket;
use tokio::{sync::mpsc::{self, Receiver, Sender}, task::JoinHandle};

use crate::UserAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
pub struct Inputs<W> {
    pub onm_rx: Receiver<(NettingMessage<W>, Option<ClientId>)>,
    pub osynt_rx: Receiver<OutboundSynapseTransmission>,
}

pub struct Outputs<W> {
    pub osynt_tx: Sender<OutboundSynapseTransmission>,
    pub inm_tx: Sender<InboundNettingMessage<W>>,
}

pub struct Netting<W> {
    connman: connman::ConnectionManager<W>,
    collater: collater::Collater<W>,
    parceler: parceler::Parceler<W>,
}

pub struct NettingJoinHandles {
    connman: JoinHandle<()>,
    collater: JoinHandle<()>,
    parceler: JoinHandle<()>,
}

impl <W: bincode::Decode + bincode::Encode + Debug + Send + 'static> Netting<W> {
    pub fn init(inputs: Inputs<W>, outputs: Outputs<W>) -> Self {
        let (iknown_tx, iknown_rx) = mpsc::channel(1024);

        let collater = collater::Collater::init(collater::Inputs {
            iknown_rx,
        }, collater::Outputs {
            inm_tx: outputs.inm_tx.clone(),
        });
        let parceler = parceler::Parceler::init(parceler::Inputs {
            onm_rx: inputs.onm_rx,
        }, parceler::Outputs {
            osynt_tx: outputs.osynt_tx.clone(),
        });
        let connman = connman::ConnectionManager::init(connman::Inputs {
            osynt_rx: inputs.osynt_rx,
        }, connman::Outputs::init(
            outputs.osynt_tx,
            iknown_tx,
            outputs.inm_tx,
        ));

        Self {
            connman,
            parceler,
            collater,
        }
    }

    pub fn start(self) -> NettingJoinHandles {
        NettingJoinHandles {
            connman: self.connman.start(),
            collater: self.collater.start(),
            parceler: self.parceler.start(),
        }
    }
}

#[derive(Clone)]
pub struct NettingApi<W> {
    pub osynt_tx: Sender<OutboundSynapseTransmission>,
    pub onm_tx: Sender<(NettingMessage<W>, Option<ClientId>)>,
}

impl <W> NettingApi<W> {
    // TODO Make this return the client id of the peer.
    pub async fn create_peer_connection(&self, addr: SocketAddr) {
        self.osynt_tx.send(OutboundSynapseTransmission {
            kind: OutboundSynapseTransmissionKind::HandshakeInitiate(addr),
            bytes: vec![],
            maybe_target: None,
        }).await.expect("no issues sending");
    }

    pub async fn broadcast(&self, msg: NettingMessage<W>) {
        self.onm_tx.send((msg, None)).await.expect("no issues sending");
    }

    pub async fn send_to(&self, msg: NettingMessage<W>, peer: ClientId) {
        self.onm_tx.send((msg, Some(peer))).await.expect("no issues sending");
    }
}

#[derive(Debug)]
pub enum NettingMessageKind<W> {
    Noop,
    NakedLogString(String),
    Handshake,
    NewConnection,
    DroppedConnection {
        client_id: ClientId,
    },
    /// Boxed world to prevent enum size blowup.
    WorldTransfer(Box<W>),
    WorldSyncStart,
    WorldSyncEnd,
    User(UserAction),
    FrameSync(u64),
}

impl <W: bincode::Decode + bincode::Encode + Debug> NettingMessageKind<W> {
    const HANDSHAKE_DISCRIMINANT: u8 = 149;

    pub fn to_msg(self) -> NettingMessage<W> {
        NettingMessage {
            message_id: Utc::now().timestamp_millis() as u64,
            kind: self,
        }
    }

    // DISCRIMINANT IS THE LAST BYTE!
    pub fn into_bytes(self) -> Vec<u8> {
        match self {
            Self::Noop => vec![0],
            Self::Handshake => vec![1],
            Self::NewConnection => vec![2],
            Self::DroppedConnection { client_id: _ } => vec![3],
            Self::WorldTransfer(w) => {
                trc::debug!("NET-NM-ENCODE-WTX world: {:?}", w);
                let mut bytes = bincode::encode_to_vec(
                    w,
                    bincode::config::standard()
                ).expect("no issues encoding");
                trc::debug!("NET-NM-ENCODE-WTX bytes: {:?}", bytes);
                bytes.push(4); // discriminant
                bytes
            },
            Self::User(_u) => vec![5],
            Self::NakedLogString(log_str) => {
                let mut v = log_str.into_bytes();
                v.push(6);
                v
            },
            Self::WorldSyncStart => vec![7],
            Self::WorldSyncEnd => vec![8],
            Self::FrameSync(frame) => {
                let b = frame.to_be_bytes().into_iter().chain(std::iter::once(9u8)).collect();
                trc::debug!("NET-NM-ENCODE-FSYNC bytes {b:?}");
                b
            },
        }
    }

    // TODO make this return Result instead of unwrapping
    pub fn parse(mut bytes: Vec<u8>) -> Result<Self, NettingMessageKindParseError> {
        let last_byte = bytes.pop();
        match last_byte.expect("bytes should not be empty") {
            0 => Ok(Self::Noop),
            1 => Ok(Self::Handshake),
            2 => Ok(Self::NewConnection),
            3 => Ok(Self::DroppedConnection { client_id: ClientId(bytes.try_into().unwrap()) }),
            4 => {
                Ok(Self::WorldTransfer(
                    Box::new(
                        bincode::decode_from_slice(
                            bytes.as_slice(),
                            bincode::config::standard()
                        ).map_err(|_| NettingMessageKindParseError::BadWorld(bytes))?.0
                    )
                ))
            },
            5 => Ok(Self::User(unimplemented!("wat"))),
            6 => {
                bytes.pop();
                Ok(Self::NakedLogString(String::from_utf8(bytes).expect("only string passed")))
            },
            7 => {
                Ok(Self::WorldSyncStart)
            },
            8 => {
                Ok(Self::WorldSyncEnd)
            },
            9 => {
                if bytes.len() != 8 {
                    bytes.push(9);
                    return Err(NettingMessageKindParseError::PartialMessage {
                        expected_len: 9,
                        bytes,
                    });
                }
                let frame_number = u64::from_be_bytes([
                    bytes[0],
                    bytes[1],
                    bytes[2],
                    bytes[3],
                    bytes[4],
                    bytes[5],
                    bytes[6],
                    bytes[7],
                ]);
                Ok(Self::FrameSync(frame_number))
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
pub enum NettingMessageKindParseError {
    PartialMessage {
        expected_len: u64,
        bytes: Vec<u8>,
    },
    BadWorld(Vec<u8>),
}

#[derive(Debug)]
pub enum NettingMessageParseError {
    PartialMessage {
        message_id: u64,
        expected_len: u64,
        msg: NettingMessageBytesWithOrdering
    },
    BadWorld(Vec<u8>),
}

#[derive(Debug)]
pub struct NettingMessage<W> {
    pub message_id: u64,
    pub kind: NettingMessageKind<W>,
}

impl <W: bincode::Decode + bincode::Encode + Debug> NettingMessage<W> {
    pub fn into_known_packet_bytes(self) -> Vec<Vec<u8>> {
        let unfettered_bytes = self.kind.into_bytes();
        let common_header: Vec<_> = (unfettered_bytes.len() as u64).to_be_bytes().into_iter()
            .chain(self.message_id.to_be_bytes())
            .collect();
        // We expect max of u8::MAX packets. Give up otherwise
        assert!(unfettered_bytes.len() / (u8::MAX as usize) < connman::MAX_KNOWN_PACKET_LEN, "netting message too big for protocol");
        let chunk_size = connman::MAX_KNOWN_PACKET_LEN - common_header.len() - 1;
        let mut assembled_packets = Vec::with_capacity((unfettered_bytes.len() + chunk_size - 1) / chunk_size);
        for (i, chunk) in unfettered_bytes.chunks(chunk_size).enumerate() {
            let assembled_packet: Vec<_> = common_header.iter().copied().chain(std::iter::once(i as u8)).chain(chunk.into_iter().copied()).collect();
            assembled_packets.push(assembled_packet);
        }
        assembled_packets
    }

    pub fn parse(mut bytes: Vec<u8>) -> Result<Self, NettingMessageParseError> {
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
            trc::debug!("NET-IKNOWN-PARSE parsing kind {:?}", bytes);
            let kind = match NettingMessageKind::parse(bytes) {
                Ok(k) => k,
                Err(NettingMessageKindParseError::BadWorld(w)) => {
                    return Err(NettingMessageParseError::BadWorld(w));
                },
                Err(NettingMessageKindParseError::PartialMessage { expected_len, bytes }) => {
                    return Err(NettingMessageParseError::PartialMessage {
                        message_id,
                        expected_len,
                        msg: NettingMessageBytesWithOrdering {
                            packet_index: expected_pkt_index,
                            bytes,
                        },
                    });
                },
            };
            Ok(NettingMessage {
                message_id,
                kind,
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

    pub fn to_inbound(self, sender_id: ClientId) -> InboundNettingMessage<W> {
        InboundNettingMessage {
            sender_id,
            msg: self,
        }
    }
}

#[derive(Debug)]
pub struct InboundNettingMessage<W> {
    pub sender_id: ClientId,
    pub msg: NettingMessage<W>,
}

pub enum SocketMessage<W> {
    Netting(NettingMessage<W>),
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
    pub client_id: ClientId,
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
    HandshakeInitiate(SocketAddr),
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
            Self::HandshakeInitiate(_) => 0b000u8,
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
    pub maybe_target: Option<ClientId>,
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
