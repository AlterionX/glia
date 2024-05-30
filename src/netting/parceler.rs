use std::{collections::VecDeque, fmt::Debug, sync::{atomic::AtomicUsize, Arc}};

use tokio::{sync::{mpsc::{Receiver, Sender}, oneshot}, task::JoinHandle};

use crate::{exec, netting::{OutboundSynapseTransmission, OutboundSynapseTransmissionKind}};

use super::{ClientId, NettingMessage};

pub struct Inputs<W> {
    pub kill_rx: oneshot::Receiver<()>,
    pub onm_rx: Receiver<(NettingMessage<W>, Option<ClientId>)>,
    pub death_tally: Arc<AtomicUsize>,
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
        exec::spawn_kill_reporting(self.inputs.death_tally, async move {
            let mut cached_unsent_osynts = VecDeque::with_capacity(5);
            loop {
                if exec::kill_requested(&mut self.inputs.kill_rx) {
                    trc::info!("KILL net parceler");
                    return;
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
                    let osynt = OutboundSynapseTransmission {
                        kind: OutboundSynapseTransmissionKind::KnownPacket,
                        bytes,
                        maybe_target: client_id,
                    };
                    cached_unsent_osynts.push_back(osynt);
                }

                // Can't just block since we're going to check if we need to die soon.
                //
                // It's alright to set a timeout and stash the message for later since we expect to
                // die very soon -- we *probably* won't run out of memory. But in case we do,
                // something else is too inefficient so we probably don't want to function anyways.
                // How to fix this is an unanswered question.
                while let Some(inm) = cached_unsent_osynts.pop_front() {
                    match self.outputs.osynt_tx.try_send(inm) {
                        Ok(()) => {},
                        Err(tokio::sync::mpsc::error::TrySendError::Full(v)) => {
                            cached_unsent_osynts.push_front(v);
                            // We couldn't send it -- check if we need to kill the collater.
                            break;
                        },
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                            // We're good to drop the message -- no one wants it anyways.
                        },
                    }
                }
            }
        })
    }
}
