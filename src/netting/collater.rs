use std::{collections::HashMap, fmt::Debug};

use tokio::{sync::{mpsc::{Receiver, Sender}, oneshot}, task::JoinHandle};

use crate::netting::{NettingMessage, NettingMessageParseError};

use super::{AttributedInboundBytes, InboundNettingMessage, NettingMessageBytesWithOrdering};

pub struct Inputs {
    pub kill_rx: oneshot::Receiver<()>,
    pub iknown_rx: Receiver<AttributedInboundBytes>,
}

pub struct Outputs<W> {
    pub inm_tx: Sender<InboundNettingMessage<W>>,
}

pub struct Collater<W> {
    inputs: Inputs,
    outputs: Outputs<W>,
}

impl <W: bincode::Decode + bincode::Encode + Debug + Send + 'static> Collater<W> {
    pub fn init(inputs: Inputs, outputs: Outputs<W>) -> Collater<W> {
        Collater {
            inputs,
            outputs,
        }
    }

    pub fn start(mut self) -> JoinHandle<()> {
        tokio::spawn(async move {
            // TODO Periodically clean up old values.
            let mut cached_partials: HashMap<_, Vec<NettingMessageBytesWithOrdering>> = HashMap::new();
            loop {
                match self.inputs.kill_rx.try_recv() {
                    Ok(_) | Err(oneshot::error::TryRecvError::Closed) => {
                        trc::info!("KILL net collater");
                        return;
                    },
                    Err(oneshot::error::TryRecvError::Empty) => {},
                }

                let known_msg = tokio::select! {
                    m = self.inputs.iknown_rx.recv() => match m {
                        Some(f) => f,
                        None => { continue; },
                    },
                    _ = tokio::time::sleep(chrono::Duration::milliseconds(100).to_std().unwrap()) => { continue; },
                };
                trc::debug!("NET-IKNOWN-PARSE {:?}", known_msg.decrypted_bytes);
                let opt_msg = match NettingMessage::parse(known_msg.decrypted_bytes) {
                    Ok(msg) => Some(msg),
                    Err(NettingMessageParseError::PartialMessage {
                        message_id,
                        expected_len,
                        msg: addressed_message_bytes,
                    }) => {
                        let cache = cached_partials.entry((known_msg.addr, message_id, expected_len)).or_default();
                        if cache.iter().all(|a| a.packet_index != addressed_message_bytes.packet_index) {
                            cache.push(addressed_message_bytes);
                        }
                        let loaded_byte_count = cache.iter()
                            .map(|a| a.bytes.len() as u64)
                            .sum::<u64>();
                        if loaded_byte_count == expected_len {
                            cache.sort_by_key(|a| a.packet_index);
                            let cache = cached_partials.remove(&(known_msg.addr, message_id, expected_len)).expect("value I just had to be here");
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
                    Err(NettingMessageParseError::BadWorld(world)) => {
                        // TODO Hexencode
                        trc::error!("corrupted world sent across the wire {:?}", base64::encode(world));
                        None
                    },
                };
                let Some(msg) = opt_msg else {
                    // If we couldn't load a message, we're done for the loop, carry on!
                    continue;
                };
                self.outputs.inm_tx.send(msg.to_inbound(known_msg.client_id)).await.expect("rx channel to not be dropped");
            }
        })
    }
}
