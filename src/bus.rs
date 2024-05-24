use std::{collections::{hash_map, HashMap}, fmt::Debug, sync::{atomic::AtomicBool, Arc}};

use chrono::{DateTime, Utc};
use tokio::{task::JoinHandle, sync::{mpsc::{Sender, Receiver}, oneshot}};

use crate::{netting::{ClientId, InboundNettingMessage, NettingApi, NettingMessageKind}, simulation::{SnapshotWorldState, SynchronizedSimulatable}};

pub struct Inputs<W> {
    pub sim_running: Arc<AtomicBool>,
    pub inm_rx: Receiver<InboundNettingMessage<W>>,
}

pub struct Outputs<W> {
    pub request_snapshot_tx: Sender<oneshot::Sender<SnapshotWorldState<W>>>,
    pub force_world_reset_tx: Sender<W>,
    pub force_jump_tx: Sender<Option<(DateTime<Utc>, u64)>>,
    pub run_state_tx: Sender<bool>,
    pub framesync_recv_tx: Sender<(ClientId, u64)>,
    pub net_api: NettingApi<W>,
}

pub struct Bus<W> {
    inputs: Inputs<W>,
    outputs: Outputs<W>,
}

impl <W: bincode::Decode + bincode::Encode + Debug + Sync + Send + 'static + Clone + SynchronizedSimulatable> Bus<W> {
    pub fn init(inputs: Inputs<W>, outputs: Outputs<W>) -> Self {
        Bus {
            inputs,
            outputs,
        }
    }

    pub fn start(mut self) -> JoinHandle<()> {
        tokio::spawn(async move {
            // Local record to avoid lock contention.
            let mut framesync_records = HashMap::new();

            loop {
                let Some(inbound_msg) = self.inputs.inm_rx.recv().await else {
                    continue;
                };
                let msg = inbound_msg.msg;
                let span = trc::span!(trc::Level::TRACE, "NM-MSG", id = msg.message_id);
                let entered = span.enter();
                match msg.kind {
                    NettingMessageKind::Noop => {
                        trc::info!("NM-NOOP [{:?}] nothin' doin'", msg.message_id);
                    },
                    NettingMessageKind::NakedLogString(log) => {
                        trc::info!("NM-LOG [{:?}] {log:?}", msg.message_id);
                        println!("raw log: {:?}", log);
                    },
                    NettingMessageKind::Handshake => {
                        trc::info!("NM-GREET [{:?}] {:?}", msg.message_id, msg.message_id);
                    },
                    NettingMessageKind::NewConnection => {
                        trc::info!("NM-CONN [{:?}] {:?}", msg.message_id, inbound_msg.sender_id);
                        // TODO Avoid this and reconcile multiple WorldTransfers correctly, especially
                        // on startup.
                        if self.inputs.sim_running.load(std::sync::atomic::Ordering::Acquire) {
                            // Only send if running -- this prevents needing reconciliation of game
                            // state when receiving a WorldTransfer ... for now.
                            let (world_tx, world_rx) = oneshot::channel();
                            self.outputs.request_snapshot_tx.send(world_tx).await.expect("sending to be fine");
                            let stashed_net_api = self.outputs.net_api.clone();
                            // Just drop the handle on the ground, it's fine.
                            tokio::spawn(async move {
                                let Ok(snapshot) = world_rx.await else {
                                    return;
                                };
                                let kind = NettingMessageKind::WorldTransfer(Box::new(snapshot.next));
                                stashed_net_api.send_to(kind.to_msg(), inbound_msg.sender_id).await;
                            });
                        }
                    },
                    NettingMessageKind::DroppedConnection { client_id, } => {
                        trc::info!("NM-DROP [{:?}] {:?}", msg.message_id, client_id);
                    },
                    NettingMessageKind::WorldSyncStart => {
                        trc::info!("NM-WSYNC [{:?}] start", msg.message_id);
                    },
                    NettingMessageKind::WorldSyncEnd => {
                        trc::info!("NM-WSYNC [{:?}] end", msg.message_id);
                    },
                    NettingMessageKind::WorldTransfer(w) => {
                        trc::info!("NM-WTX [{:?}] transfering world {:?} ...", msg.message_id, w);
                        let gen = w.generation();
                        self.outputs.force_world_reset_tx.send(*w).await.expect("rx to still exist");
                        self.outputs.force_jump_tx.send(Some((Utc::now(), gen))).await.expect("everything to be linked");
                        // Kickstart! This will get ignored if we're already running, so it's all good.
                        self.outputs.run_state_tx.send(true).await.expect("everything to be linked");
                    },
                    NettingMessageKind::User(_ua) => {
                        unimplemented!("not yet able to handle user actions");
                    },
                    NettingMessageKind::FrameSync(frame) => {
                        let should_loudly_log = match framesync_records.entry(inbound_msg.sender_id) {
                            hash_map::Entry::Occupied(mut b) => {
                                if *b.get() >= frame {
                                    // We should skip this message since we've already moved past it.
                                    continue;
                                }
                                // We're going to try to regulate the log to once per second
                                let is_big_jump = *b.get() > frame + u64::from(super::TARGET_FPS);
                                b.insert(frame);
                                is_big_jump
                            }
                            hash_map::Entry::Vacant(a) => {
                                a.insert(frame);
                                true
                            }
                        };
                        if should_loudly_log {
                            trc::info!("NM-FSYNC [{:?}] syncing to {:?} ...", msg.message_id, frame);
                        } else {
                            trc::debug!("NM-FSYNC [{:?}] syncing to {:?} ...", msg.message_id, frame);
                        }
                        self.outputs.framesync_recv_tx.send((inbound_msg.sender_id, frame)).await.expect("sending to be fine");
                    },
                }
                drop(entered);
            }
        })
    }
}