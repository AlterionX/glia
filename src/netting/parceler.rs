use std::{collections::VecDeque, fmt::Debug, sync::{atomic::AtomicUsize, Arc}};

use tokio::{sync::{mpsc::{Receiver, Sender}, oneshot}, task::JoinHandle};

use crate::{exec::{self, ReceiverTimeoutExt, ThreadDeathReporter}, netting::{OutboundSynapseTransmission, OutboundSynapseTransmissionKind}};

use super::{ClientId, NettingMessage};

pub struct Inputs<W, A> {
    pub kill_rx: oneshot::Receiver<()>,
    pub onm_rx: Receiver<(NettingMessage<W, A>, Option<ClientId>)>,
    pub death_tally: Arc<AtomicUsize>,
}

pub struct Outputs {
    pub osynt_tx: Sender<OutboundSynapseTransmission>,
}

pub struct Parceler<W, A> {
    inputs: Inputs<W, A>,
    outputs: Outputs,
}

impl <W: bincode::Decode + bincode::Encode + Debug + Send + 'static, A: bincode::Decode + bincode::Encode + Debug + Send + 'static> Parceler<W, A> {
    pub fn init(inputs: Inputs<W, A>, outputs: Outputs) -> Self {
        Self {
            inputs,
            outputs,
        }
    }

    pub fn start(mut self) -> JoinHandle<()> {
        ThreadDeathReporter::new(&self.inputs.death_tally, "net-parceler").spawn(async move {
            let mut cached_unsent_osynts = VecDeque::with_capacity(5);
            loop {
                if exec::kill_requested(&mut self.inputs.kill_rx) { return; }

                let Some((msg, client_id)) = self.inputs.onm_rx.recv_for_ms(100).await.value() else {
                    continue;
                };
                trc::trace!("NET-ONM Sending {msg:?}");
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
