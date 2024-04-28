use chrono::{DateTime, Utc};
use base64::prelude::*;
use mio::net::UdpSocket;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::World;

/// Struct holding network connections and managing the UI
pub struct Netting {
}

impl Netting {
    fn new() -> Self {
        Self {}
    }
}

pub enum NettingMessageKind {
    Noop,
    WorldTransfer(Box<World>)
}

pub struct NettingMessage {
    packet_id: String,
    data: DateTime<Utc>,
    kind: NettingMessageKind,
}

pub struct PeerRegistryEntry {
    pub client_id: String,
    pub rx_task_handle: JoinHandle<()>,
    pub tx_task_handle: JoinHandle<()>,
    pub rx_handle: handle,
}

impl PeerRegistryEntry {
    pub async fn new() -> Self {
        // While this doesn't guarantee name uniqueness (we'd need a PhD for that shit) this is
        // probably good enough for any reasonable person.
        //
        // This has a period of 11 days. I really doubt a lobby someone sets up will last for
        // that long. I also really doubt people will join the lobby in the same millisecond.
        // Even if they do, there's an extra 4 random digits.
        //
        // Also, this particular random number isn't persisted and we shouldn't rely on this being
        // fixed between saves/sessions.
        let time_component = Utc::now().timestamp_millis() % 1_000_000_000;
        let chaos_determinant = rand::random::<u8>() % 8;
        let numerical_client_id = (time_component << 3) | i64::from(chaos_determinant);
        // Network order (big endian) bytes.
        let client_id_bytes = numerical_client_id.to_be_bytes();
        let client_id = BASE64_STANDARD_NO_PAD.encode(client_id_bytes);


        // Takes message and splits it into packets
        let (tx_packet_tx_handle, tx_packet_rx_handle) = mpsc::channel::<NettingMessage>(512);
        let tx_task_handle = tokio::spawn(async move { loop {
        }});

        // Receives and collates packets into message
        let (rx_packet_tx_handle, rx_packet_rx_handle) = mpsc::channel::<NettingMessage>(512);
        let rx_task_handle = tokio::spawn(async move { loop {
        }});

        Self {
            client_id,
            rx_task_handle,
            tx_task_handle,
            rx_handle: rx_packet_rx_handle,
        }
    }
}

pub struct PeerRegistry {
    pub me: PeerRegistryEntry,
    pub siblings: Vec<PeerRegistryEntry>,
    pub connector_socket: UdpSocket,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self {
            me: PeerRegistryEntry::new(),
            siblings: vec![],
        }
    }

    pub async fn accept_connections() {
    }

    pub async fn tx(&mut self, msg: NettingMessage) {
    }

    pub async fn rx(&mut self) -> NettingMessage {
        NettingMessage {
            packet_id: "".to_owned(),
            data: Utc::now(),
            kind: NettingMessageKind::Noop,
        }
        // ACK message
    }
}
