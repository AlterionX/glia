use std::{alloc::Layout, fmt::Debug, net::SocketAddr, pin::Pin, sync::{atomic::AtomicUsize, Arc}};

use derivative::Derivative;

use chrono::{TimeDelta, Utc};
use rand::rngs::OsRng;

use sha2::{Sha256, Digest};
use p256::{ecdh::EphemeralSecret, EncodedPoint, PublicKey};
use chacha::{aead::Aead, AeadCore, KeyInit};

use mio::{net::UdpSocket, Events, Interest, Poll, Token};
use tokio::{sync::{mpsc::{self, error::TryRecvError, Receiver, Sender}, oneshot}, task::JoinHandle};

use crate::{exec::{self, ThreadDeathReporter}, netting::{ClientId, NettingMessageKind, OutboundSynapseTransmissionKind, SynapseTransmission, SynapseTransmissionKind}};

use super::{AttributedInboundBytes, InboundNettingMessage, OutboundSynapseTransmission};

const MAX_UDP_DG_SIZE: usize = 65_535;
pub const MAX_KNOWN_PACKET_LEN: usize = MAX_UDP_DG_SIZE / 2;
const HANDSHAKE_GREETING: &[u8] = b"hello";
const MAX_ACTIVE_MESSAGES: usize = 256;

#[derive(Derivative)]
#[derivative(Debug)]
pub enum PeerKey {
    Inflight(
        #[derivative(Debug="ignore")]
        p256::ecdh::EphemeralSecret
    ),
    Exchanged {
        peer_id: ClientId,
        #[derivative(Debug="ignore")]
        _secret: p256::ecdh::SharedSecret,
        #[derivative(Debug="ignore")]
        key: chacha::XChaCha20Poly1305,
    },
}

impl PeerKey {
    pub fn exchanged_from_shared_secret_and_raw_peer(
        shared_secret: p256::ecdh::SharedSecret,
        peer_id: ClientId,
    ) -> Self {
        let mut key_buffer = [0u8; 32];
        shared_secret.extract::<Sha256>(None).expand(&[], &mut key_buffer).expect("key expansion to be fine");
        let aead_key = chacha::XChaCha20Poly1305::new(&key_buffer.into());

        Self::Exchanged {
            peer_id,
            _secret: shared_secret,
            key: aead_key
        }
    }

    pub fn exchanged_from_shared_secret_and_encrypted_handshake(
        shared_secret: p256::ecdh::SharedSecret,
        encrypted_handshake: &[u8],
    ) -> Option<(ClientId, Self)> {
        let mut key_buffer = [0u8; 32];
        shared_secret.extract::<Sha256>(None).expand(&[], &mut key_buffer).expect("key expansion to be fine");
        let aead_key = chacha::XChaCha20Poly1305::new(&key_buffer.into());

        // Now use the combined key.
        let peer_client_id_and_greeting = Self::fixed_key_aead_decrypt(&aead_key, encrypted_handshake)?;
        if peer_client_id_and_greeting.len() != HANDSHAKE_GREETING.len() + ClientId::LEN {
            return None;
        }
        if &peer_client_id_and_greeting[..HANDSHAKE_GREETING.len()] != HANDSHAKE_GREETING {
            return None;
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


        Some((peer_client_id, Self::Exchanged {
            peer_id: peer_client_id,
            _secret: shared_secret,
            key: aead_key
        }))
    }

    pub fn fixed_nonce_aead_encrypt(&self, aead_nonce: &[u8], cleartext: &[u8], buffer: &mut [u8]) -> usize {
        let Self::Exchanged { key: ref aead_key, .. } = self else {
            trc::debug!("NET-ENCODE encrypt attempted without established key");
            return 0;
        };

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

    fn fixed_key_aead_decrypt(key: &chacha::XChaCha20Poly1305, ciphertext: &[u8]) -> Option<Vec<u8>> {
        // TODO remove this
        let sample_nonce = chacha::XChaCha20Poly1305::generate_nonce(&mut OsRng);

        if ciphertext.len() < sample_nonce.len() {
            trc::debug!("NET-DECRYPT bad message length");
            return None;
        }

        let aead_nonce = &ciphertext[0..sample_nonce.len()];
        let raw_ciphertext_buffer = &ciphertext[aead_nonce.len()..];

        let cleartext = match key.decrypt(aead_nonce.into(), raw_ciphertext_buffer) {
            Ok(a) => a,
            Err(e) => {
                trc::error!("no issues expected for decryption, err: {e:?}");
                return None;
            },
        };

        Some(cleartext)
    }

    pub fn aead_decrypt(&self, ciphertext: &[u8]) -> Option<(ClientId, Vec<u8>)> {
        let Self::Exchanged { key: ref aead_key, peer_id, .. } = self else { return None; };
        Some((*peer_id, Self::fixed_key_aead_decrypt(aead_key, ciphertext)?))
    }
}

pub struct PeerConnectionData {
    pub exchanged_key: Option<PeerKey>,
    pub active_messages: Pin<Box<[(usize, [u8; MAX_UDP_DG_SIZE]); MAX_ACTIVE_MESSAGES]>>,
    pub recent_received_messages: Pin<Box<[(chrono::DateTime<chrono::Utc>, Vec<u8>); MAX_ACTIVE_MESSAGES]>>,
    pub next_transmission_number: u8,
}

type RMRecord = (chrono::DateTime<chrono::Utc>, Vec<u8>);
type RMRecords = [RMRecord; MAX_ACTIVE_MESSAGES];
impl Default for PeerConnectionData {
    fn default() -> Self {
        let buf = unsafe {
            let ptr = std::alloc::alloc(Layout::new::<[(usize, [u8; MAX_UDP_DG_SIZE]); MAX_ACTIVE_MESSAGES]>()) as *mut [(usize, [u8; MAX_UDP_DG_SIZE]); MAX_ACTIVE_MESSAGES];
            if ptr.is_null() {
                panic!("oom allocating peer connection buffer");
            }
            for i in 0..MAX_ACTIVE_MESSAGES {
                unsafe {
                    // Numbers are safe to ref without init
                    (&mut *ptr)[i].0 = 0;
                }
            }
            Pin::new(Box::from_raw(ptr as *mut[(usize, [u8; MAX_UDP_DG_SIZE]); MAX_ACTIVE_MESSAGES]))
        };
        let recent_received_messages: Pin<Box<RMRecords>> = unsafe {
            let ptr: *mut (chrono::DateTime<chrono::Utc>, Vec<u8>) = std::alloc::alloc(Layout::new::<RMRecords>()) as *mut RMRecord;
            if ptr.is_null() {
                panic!("oom allocating peer connection buffer");
            }
            let reference_stamp = Utc::now() - TimeDelta::milliseconds(300);
            for i in 0..MAX_ACTIVE_MESSAGES {
                ptr.offset(i.try_into().unwrap()).write((reference_stamp, vec![]));
            }
            Box::into_pin(Box::from_raw(ptr as *mut RMRecords))
        };

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
        !is_mismatch
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
            let range = 0..(buffer.len() - 2).min(bytes.len());
            let bytes_to_copy = range.len();
            buffer[2..bytes_to_copy + 2].copy_from_slice(&bytes[range]);
            bytes_to_copy + 2
        };

        self.active_messages[self.next_transmission_number as usize].0 = bytes_written_to_buffer;
    }

    pub fn decrypt(&self, bytes: &[u8]) -> Option<(ClientId, Vec<u8>)> {
        let Some(ref k) = self.exchanged_key else {
            // TODO Handle this properly, but just drop the packet on the ground for now.
            return None;
        };
        k.aead_decrypt(bytes)
    }
}

#[derive(Default)]
pub struct PeerConnectionLedger {
    connections: Vec<(SocketAddr, Option<ClientId>, PeerConnectionData)>,
}

impl PeerConnectionLedger {
    pub fn idx_for_addr(&self, addr: SocketAddr) -> Option<usize> {
        self.connections.iter().map(|t| t.0).enumerate().find(|t| t.1 == addr).map(|t| t.0)
    }

    pub fn idx_for_id(&self, id: ClientId) -> Option<usize> {
        self.connections.iter().map(|t| t.1).enumerate().find(|t| t.1 == Some(id)).map(|t| t.0)
    }

    pub fn idx_for_addr_or_init(&mut self, addr: SocketAddr) -> usize {
        self.idx_for_addr(addr).unwrap_or_else(|| {
            self.connections.push((addr, None, Default::default()));
            self.connections.len() - 1
        })
    }

    pub fn addr_get(&self, addr: SocketAddr) -> Option<&PeerConnectionData> {
        Some(&self.connections[self.idx_for_addr(addr)?].2)
    }

    pub fn id_get(&self, id: ClientId) -> Option<&PeerConnectionData> {
        Some(&self.connections[self.idx_for_id(id)?].2)
    }

    pub fn addr_get_mut(&mut self, addr: SocketAddr) -> Option<&mut PeerConnectionData> {
        let idx = self.idx_for_addr(addr)?;
        Some(&mut self.connections[idx].2)
    }

    pub fn id_get_mut(&mut self, id: ClientId) -> Option<&mut PeerConnectionData> {
        let idx = self.idx_for_id(id)?;
        Some(&mut self.connections[idx].2)
    }

    pub fn get_or_init(&mut self, addr: SocketAddr) -> &PeerConnectionData {
        let idx = self.idx_for_addr_or_init(addr);
        &self.connections[idx].2
    }

    pub fn get_or_init_mut(&mut self, addr: SocketAddr) -> &mut PeerConnectionData {
        let idx = self.idx_for_addr_or_init(addr);
        &mut self.connections[idx].2
    }

    pub fn set_key(&mut self, addr: SocketAddr, k: PeerKey) {
        let idx = self.idx_for_addr_or_init(addr);
        let (_, opt_id, pcd) = &mut self.connections[idx];

        match k {
            PeerKey::Inflight(_) => {},
            PeerKey::Exchanged { ref peer_id, .. } => {
                *opt_id = Some(*peer_id);
            },
        }
        pcd.exchanged_key = Some(k);
    }

    fn addr_data_iter_mut(&mut self) -> PeerConnectionMessageIter<'_> {
        PeerConnectionMessageIter {
            backing_container: &mut self.connections,
            primary_idx: 0,
            secondary_idx: 0,
        }
    }
}

pub struct PeerConnectionMessageIter<'a> {
    backing_container: &'a mut Vec<(SocketAddr, Option<ClientId>, PeerConnectionData)>,
    primary_idx: usize,
    secondary_idx: usize,
}

impl PeerConnectionMessageIter<'_> {
    fn next(&mut self) -> Option<(
        SocketAddr,
        &'_ mut (usize, [u8; MAX_UDP_DG_SIZE]),
        usize
    )> {
        if self.primary_idx >= self.backing_container.len() {
            return None;
        }

        // stash this to retrieve value. If primary_idx is valid, then secondary_idx should always
        // be valid.
        let (target_connection, target_message) = (self.primary_idx, self.secondary_idx);

        // Advance to next available slot.
        self.secondary_idx += 1;
        while self.secondary_idx >= self.backing_container[self.primary_idx].2.active_messages.len() {
            self.primary_idx += 1;
            self.secondary_idx = 0;
            // Since we advanced the counter we'd need to check again.
            if self.primary_idx >= self.backing_container.len() {
                break;
            }
        }

        Some((
            self.backing_container[target_connection].0,
            &mut self.backing_container[target_connection].2.active_messages[target_message],
            target_message,
        ))
    }
}


// -------- AUXILIARY END ---------


pub struct Inputs {
    pub kill_rx: oneshot::Receiver<()>,
    pub osynt_rx: Receiver<OutboundSynapseTransmission>,
    pub death_tally: Arc<AtomicUsize>,
}

pub struct Outputs<W, A> {
    socket: UdpSocket,
    pub own_cid: ClientId,
    own_addr: SocketAddr,
    pub osynt_tx: Sender<OutboundSynapseTransmission>,
    pub iknown_tx: Sender<AttributedInboundBytes>,
    pub inm_tx: Sender<InboundNettingMessage<W, A>>,
}

impl <W, A> Outputs<W, A> {
    pub fn init(
        osynt_tx: Sender<OutboundSynapseTransmission>,
        iknown_tx: Sender<AttributedInboundBytes>,
        inm_tx: Sender<InboundNettingMessage<W, A>>,
    ) -> Self {
        let own_cid = ClientId::gen();
        trc::info!("NET-INIT Own Client ID: {own_cid:?}");

        // Determine which port we're using to look at the world.
        let socket = UdpSocket::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        // TODO Run STUN here to figure out true ip/port. This is what will be sent in the future
        let local_addr = socket.local_addr().unwrap();
        trc::info!("NET-INIT Connector port opened {:?}", local_addr);

        Self {
            socket,
            own_cid,
            own_addr: local_addr,
            osynt_tx,
            iknown_tx,
            inm_tx,
        }
    }
}

pub struct ConnectionManager<W, A> {
    poll: Poll,
    events: Events,

    ack_tx: Sender<(usize, SocketAddr)>,
    ack_rx: Receiver<(usize, SocketAddr)>,

    inputs: Inputs,
    outputs: Outputs<W, A>,
}

impl <W: bincode::Decode + bincode::Encode + Debug + Send + 'static, A: bincode::Decode + bincode::Encode + Debug + Send + 'static> ConnectionManager<W, A> {
    const OWN_PORT_TOPIC: Token = Token(0);

    /// This is the root of everything.
    pub fn init(inputs: Inputs, outputs: Outputs<W, A>) -> Self {
        let (ack_tx, ack_rx) = mpsc::channel(512);

        Self {
            events: Events::with_capacity(1024),
            poll: Poll::new().unwrap(),
            ack_rx,
            ack_tx,

            inputs,
            outputs,
        }
    }

    pub fn start(mut self) -> JoinHandle<()> {
        ThreadDeathReporter::new(&self.inputs.death_tally, "net-connman").spawn(async move {
            self.poll.registry().register(&mut self.outputs.socket, Self::OWN_PORT_TOPIC, Interest::READABLE | Interest::WRITABLE).unwrap();
            let mut dg_buf_mem = [0u8; MAX_UDP_DG_SIZE];
            let dg_buf = dg_buf_mem.as_mut_slice();
            let mut writable = false;
            let mut readable = false;
            // TODO Convert to array?
            let mut connections = PeerConnectionLedger::default();


            // TODO Make this await a bit more.
            loop {
                if exec::kill_requested(&mut self.inputs.kill_rx) { return; }

                tokio::time::sleep(TimeDelta::milliseconds(1).to_std().unwrap()).await;
                self.poll.poll(&mut self.events, Some(TimeDelta::milliseconds(10).to_std().unwrap())).expect("no issues polling");
                for event in self.events.iter() {
                    match event.token() {
                        Self::OWN_PORT_TOPIC => {
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
                    let (dg_len, sender_addr) = match self.outputs.socket.recv_from(dg_buf) {
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

                    let oneshot_ack_tx = self.ack_tx.clone();
                    // This is try_send so that it avoids blocking. Theoretically, this is fine
                    // since we need to reject identical messages anyways.
                    let ack_fut = async move {
                        match oneshot_ack_tx.try_send((isynt.transmission_number, sender_addr)) {
                            Ok(_) => Ok(()),
                            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                // We couldn't send it -- we'll just drop it.
                                Ok(())
                            },
                            Err(tokio::sync::mpsc::error::TrySendError::Closed(v)) => {
                                // We're good to drop the message -- no one wants it anyways.
                                Err(tokio::sync::mpsc::error::SendError(v))
                            },
                        }
                    };

                    match isynt.kind {
                        // [5 bytes; hello][9 bytes client id][remainder; peer_secret]
                        SynapseTransmissionKind::HandshakeInitiate => 'initiate_end: {
                            let checksum = {
                                let mut hasher = Sha256::new();
                                hasher.update(dg);
                                hasher.finalize()
                            };
                            let connection_data = connections.get_or_init_mut(sender_addr);
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
                            let handshake_internal_output_message_tx = self.outputs.osynt_tx.clone();
                            let combined_key = PeerKey::exchanged_from_shared_secret_and_raw_peer(shared_key, peer_client_id);
                            { // TODO Implement a permissioning system.
                                // We'll write out the client id so that the exchange will also
                                // record the client id.
                                trc::debug!("NET-ISYNT-HANDSHAKE-INIT key minted");
                                let public_bytes = own_public.to_sec1_bytes();
                                let message = HANDSHAKE_GREETING.iter().copied().chain(self.outputs.own_cid.iter()).collect::<Vec<_>>();
                                let encrypted_bytes = combined_key.aead_encrypt(&message, &mut outbound_buffer[8..]);
                                outbound_buffer[..8].copy_from_slice(&encrypted_bytes.to_be_bytes());
                                outbound_buffer[8..][encrypted_bytes..][..public_bytes.len()].copy_from_slice(&public_bytes);
                                outbound_buffer.truncate(encrypted_bytes + 8 + public_bytes.len());
                            }
                            connection_data.exchanged_key = Some(combined_key);

                            let Ok(_) = ack_fut.await else {
                                trc::warn!("NET-ISYNT-HANDSHAKE-INIT no issues sending ack");
                                break 'initiate_end;
                            };
                            let Ok(_) = handshake_internal_output_message_tx.send(OutboundSynapseTransmission {
                                kind: OutboundSynapseTransmissionKind::HandshakeResponse,
                                bytes: outbound_buffer,
                                maybe_target: Some(peer_client_id),
                            }).await else {
                                trc::warn!("NET-ISYNT-HANDSHAKE-INIT no issues internal propagation");
                                break 'initiate_end;
                            };
                            let Ok(_) = self.outputs.inm_tx.send(NettingMessageKind::NewConnection.into_msg().to_inbound(peer_client_id)).await else {
                                break 'initiate_end;
                            };
                        },
                        // TODO Make the HandshakeResponse send another packet to confirm.
                        // [8 bytes; mlen][mlen bytes; ciphertext][remainder; peer_secret]
                        SynapseTransmissionKind::HandshakeResponse => 'response_end: {
                            let checksum = {
                                let mut hasher = sha2::Sha256::new();
                                hasher.update(dg);
                                hasher.finalize()
                            };
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

                            let Some(connection_data) = connections.addr_get_mut(sender_addr) else {
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
                            trc::debug!("NET-ISYNT-HANDSHAKE-RESP wrapping up initiator ecdh");
                            let peer_public = PublicKey::from_sec1_bytes(peer_public_bytes).unwrap();
                            let shared_key = own_secret.diffie_hellman(&peer_public);
                            let Some((peer_client_id, combined_key)) = PeerKey::exchanged_from_shared_secret_and_encrypted_handshake(shared_key, ciphertext) else {
                                // Something went wrong with the handshake, this is not legitimate.
                                // Deny this.
                                trc::debug!(
                                    "NET-ISYNT-HANDSHAKE-RESP rejected -- encryption bad {:?}",
                                    connection_data.exchanged_key
                                );
                                break 'response_end;
                            };

                            connection_data.exchanged_key = Some(combined_key);
                            trc::trace!("NET-ISYNT-HANDSHAKE-RESP complete");
                            let Ok(_) = self.outputs.inm_tx.send(NettingMessageKind::NewConnection.into_msg().to_inbound(peer_client_id)).await else {
                                trc::warn!("NET-ISYNT-HANDSHAKE-RESP no issues sending");
                                break 'response_end;
                            };

                            let Ok(_) = ack_fut.await else {
                                trc::warn!("NET-ISYNT-HANDSHAKE-RESP no issues sending ack");
                                break 'response_end;
                            };
                        },
                        SynapseTransmissionKind::PeerDiscovery => unimplemented!("peer connection not yet working"),
                        SynapseTransmissionKind::Ack => 'ack_end: {
                            let Some(connection_data) = connections.addr_get_mut(sender_addr) else {
                                trc::debug!("NET-ISYNT-ACK ignoring ack");
                                // We have not initiated this connection yet, this is a bad request.
                                break 'ack_end;
                            };
                            trc::trace!("NET-ISYNT-ACK tx_no={:?}", isynt.transmission_number);
                            connection_data.active_messages[isynt.transmission_number].0 = 0;
                        },
                        SynapseTransmissionKind::KnownPacket => 'known_packet_end: {
                            trc::trace!("NET-ISYNT-KNOWN recv {:?}", isynt);
                            let Some(connection_data) = connections.addr_get_mut(sender_addr) else {
                                // We aren't connected, reject their request.
                                break 'known_packet_end;
                            };
                            let Some((client_id, decrypted_bytes)) = connection_data.decrypt(&isynt.bytes) else {
                                // Crypto thinks they're terrible, ignore their message.
                                break 'known_packet_end;
                            };
                            // This checksum is different since we want to checksum the contents,
                            // not the packet itself, jic the cipher decides to change it up on us.
                            let checksum = {
                                let mut hasher = sha2::Sha256::new();
                                hasher.update(decrypted_bytes.as_slice());
                                hasher.finalize()
                            };
                            if connection_data.recently_received(&isynt, checksum.to_vec()) {
                                break 'known_packet_end;
                            }
                            let Ok(_) = self.outputs.iknown_tx.send(AttributedInboundBytes {
                                decrypted_bytes,
                                addr: sender_addr,
                                client_id,
                            }).await else {
                                trc::warn!("NET-ISYNT-KNOWN no issues sending");
                                break 'known_packet_end;
                            };

                            let Ok(_) = ack_fut.await else {
                                trc::warn!("NET-ISYNT-KNOWN no issues sending ack");
                                break 'known_packet_end;
                            };
                        },
                    }
                }}

                if writable { 'writer_blk: {
                    while let Ok((tx_no, addr)) = self.ack_rx.try_recv() {
                        trc::trace!("NET-WRITE sending ack {tx_no:?} to {addr:?}");
                        self.outputs.socket.send_to(&[
                            OutboundSynapseTransmissionKind::Ack { transmission_number: tx_no }.to_discriminant(),
                            tx_no as u8
                        ], addr).expect("socket to be okay");
                    }
                    // TODO this should back off from time to time
                    let mut data = connections.addr_data_iter_mut();
                    while let Some((addr, (st_len, st_bytes), i)) = data.next() {
                        if *st_len == 0 {
                            // Skip entry since it's not a message
                            continue;
                        }
                        trc::debug!("NET-WRITE writing message... {i:?} <<>> {st_len:?} <<>> {:?}", SynapseTransmissionKind::from_discriminant(st_bytes[0]));
                        // We're handling disconnected sockets by crashing... maybe
                        // I'll fix this in the future, but for now...
                        let sent = match self.outputs.socket.send_to(&st_bytes[..*st_len], addr) {
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
                        trc::trace!("NET-WRITE message written len={st_len:?}");
                        // Acks will get zeroed out no matter what since they're one time send.
                        // TODO Support limited resends appropriately.
                        if st_bytes[0] == SynapseTransmissionKind::Ack.to_discriminant() {
                            *st_len = 0;
                        }
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
                    match self.inputs.osynt_rx.try_recv() {
                        Ok(msg_and_addr) => Some(msg_and_addr),
                        Err(TryRecvError::Empty) => None,
                        Err(TryRecvError::Disconnected) => None,
                    }
                } {
                    trc::debug!("NET-OSYNT queuing message {osynt:?}");
                    let discriminant = osynt.kind.to_discriminant();
                    let (encrypted, bytes, target_addr) = match &osynt.kind {
                        OutboundSynapseTransmissionKind::HandshakeInitiate(target_addr) => {
                            trc::debug!("NET-OSYNT-HANDSHAKE-INIT creating connection data records...");
                            let connection_data = connections.get_or_init_mut(*target_addr);
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
                            trc::trace!("NET-OSYNT-HANDSHAKE-INIT complete!");
                            // TODO sign these bytes
                            let greeting_bytes =
                                HANDSHAKE_GREETING.iter().copied()
                                .chain(self.outputs.own_cid.iter())
                                .chain(own_public.to_sec1_bytes().iter().copied())
                                .collect();
                            (false, greeting_bytes, Some(target_addr))
                        },
                        OutboundSynapseTransmissionKind::HandshakeResponse => {
                            trc::trace!("NET-OSYNT-HANDSHAKE-RESP sending");
                            (false, osynt.bytes, None)
                        },
                        OutboundSynapseTransmissionKind::Ack { .. } => {
                            (false, osynt.bytes, None)
                        },
                        OutboundSynapseTransmissionKind::PeerDiscovery => {
                            (true, osynt.bytes, None)
                        },
                        OutboundSynapseTransmissionKind::KnownPacket => {
                            (true, osynt.bytes, None)
                        },
                    };

                    if let Some(addr) = target_addr {
                        // Just drop the packet if we're not connected yet.
                        if let Some(connection_data) = connections.addr_get_mut(*addr) {
                            connection_data.write_synapse_transmission(encrypted, discriminant, bytes.as_slice());
                        }
                    } else {
                        for (_, _, data) in connections.connections.iter_mut() {
                            data.write_synapse_transmission(encrypted, discriminant, bytes.as_slice());
                            num_msgs_written += 1;
                        }
                    }
                }
            }
        })
    }
}

impl <W, A> ConnectionManager<W, A> {
    pub fn own_client_id(&self) -> ClientId {
        self.outputs.own_cid
    }

    pub fn own_socket_address(&self) -> SocketAddr {
        self.outputs.own_addr
    }
}

#[cfg(test)]
mod test_peer_connection_iter {
    use std::net::SocketAddr;

    use super::{PeerConnectionData, PeerConnectionMessageIter};

    #[test]
    fn iterator_works() {
        let mut backing_vec = vec![("127.0.0.1:5000".parse::<SocketAddr>().unwrap(), None, PeerConnectionData::default())];
        let mut iter = PeerConnectionMessageIter { backing_container: &mut backing_vec, primary_idx: 0, secondary_idx: 0 };

        for _ in 0..iter.backing_container[0].2.active_messages.len() {
            let value = iter.next();
            assert!(value.is_some(), "{:?} -- should be some", value);
        }

        let value = iter.next();
        assert!(value.is_none(), "{:?} -- should be none", value);
    }
}
