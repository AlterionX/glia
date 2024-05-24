use std::fmt::Debug;

use tokio::{sync::{mpsc::{Receiver, Sender}, oneshot}, task::JoinHandle};

use crate::netting::{OutboundSynapseTransmission, OutboundSynapseTransmissionKind};

use super::{ClientId, NettingMessage};

pub struct Inputs<W> {
    pub kill_rx: oneshot::Receiver<()>,
    pub onm_rx: Receiver<(NettingMessage<W>, Option<ClientId>)>,
}

pub struct Outputs {
    pub osynt_tx: Sender<OutboundSynapseTransmission>,
}

pub struct Parceler<W> {
    inputs: Inputs<W>,
    outputs: Outputs,
}

impl <W: bincode::Decode + bincode::Encode + Debug + Send + 'static> Parceler<W> {
    pub fn init(inputs: Inputs<W>, outputs: Outputs) -> Self {
        Self {
            inputs,
            outputs,
        }
    }

    pub fn start(mut self) -> JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.inputs.kill_rx.try_recv() {
                    Ok(_) | Err(oneshot::error::TryRecvError::Closed) => {
                        trc::info!("KILL net parceler");
                        return;
                    },
                    Err(oneshot::error::TryRecvError::Empty) => {},
                }

                let (msg, client_id) = tokio::select! {
                    m = self.inputs.onm_rx.recv() => match m {
                        Some(f) => f,
                        None => { continue; },
                    },
                    _ = tokio::time::sleep(chrono::Duration::milliseconds(100).to_std().unwrap()) => { continue; },
                };
                trc::info!("NET-ONM Sending {msg:?}");
                // TODO Break apart into multiple osynts and shove through the network. Drop for
                // now.
                for bytes in msg.into_known_packet_bytes() {
                    self.outputs.osynt_tx.send(OutboundSynapseTransmission {
                        kind: OutboundSynapseTransmissionKind::KnownPacket,
                        bytes,
                        maybe_target: client_id,
                    }).await.expect("rx channel to not be dropped");
                }
            }
        })
    }
}
