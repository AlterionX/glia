use std::{collections::{HashMap, VecDeque}, fmt::Debug, sync::{atomic::AtomicUsize, Arc}};

use tokio::{sync::{mpsc::{Receiver, Sender}, oneshot}, task::JoinHandle};

use crate::{exec::{self, ReceiverTimeoutExt, ThreadDeathReporter}, netting::{NettingMessage, NettingMessageParseError}};

use super::{AttributedInboundBytes, InboundNettingMessage, NettingMessageBytesWithOrdering};

pub struct Inputs {
    pub kill_rx: oneshot::Receiver<()>,
    pub iknown_rx: Receiver<AttributedInboundBytes>,
    pub death_tally: Arc<AtomicUsize>,
}

pub struct Outputs<W, A> {
    pub inm_tx: Sender<InboundNettingMessage<W, A>>,
}

pub struct Collater<W, A> {
    inputs: Inputs,
    outputs: Outputs<W, A>,
}

impl <W: bincode::Decode + bincode::Encode + Debug + Send + 'static, A: bincode::Decode + bincode::Encode + Debug + Send + 'static> Collater<W, A> {
    pub fn init(inputs: Inputs, outputs: Outputs<W, A>) -> Collater<W, A> {
        Collater {
            inputs,
            outputs,
        }
    }

    pub fn start(mut self) -> JoinHandle<()> {
        ThreadDeathReporter::new(&self.inputs.death_tally, "net-collater").spawn(async move {
            let mut cached_unsent_netting_messages = VecDeque::with_capacity(5);
            // TODO Periodically clean up old values.
            let mut cached_partials: HashMap<_, Vec<NettingMessageBytesWithOrdering>> = HashMap::new();
            loop {
                if exec::kill_requested(&mut self.inputs.kill_rx) { return; }

                let Some(known_msg) = self.inputs.iknown_rx.recv_for_ms(100).await.value() else {
                    continue;
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
                if let Some(msg) = opt_msg {
                    // If we couldn't load a message, we're done with the current message. Pop it
                    // in the cache otherwise and move on!
                    cached_unsent_netting_messages.push_back(msg.to_inbound(known_msg.client_id));
                }

                // Can't just block since we're going to check if we need to die soon.
                //
                // It's alright to set a timeout and stash the message for later since we expect to
                // die very soon -- we *probably* won't run out of memory. But in case we do,
                // something else is too inefficient so we probably don't want to function anyways.
                // How to fix this is an unanswered question.
                while let Some(inm) = cached_unsent_netting_messages.pop_front() {
                    match self.outputs.inm_tx.try_send(inm) {
                        Ok(()) => {},
                        Err(tokio::sync::mpsc::error::TrySendError::Full(v)) => {
                            cached_unsent_netting_messages.push_front(v);
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
