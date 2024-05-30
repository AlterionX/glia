use std::{collections::{HashMap, VecDeque}, fmt::Debug, sync::{atomic::{AtomicBool, AtomicUsize}, Arc}};

use chrono::{DateTime, Utc};
use tokio::{sync::{mpsc::Receiver, oneshot, RwLock}, task::JoinHandle};

use crate::{exec, netting::{ClientId, NettingApi, NettingMessageKind}};

// TODO use a triple buffer with some network-related niceties
#[derive(Default)]
pub struct WorldStash<W, A> {
    historical_actions: VecDeque<A>,
    historical_worlds: VecDeque<Arc<W>>,
}

impl <W: Clone, A> WorldStash<W, A> {
    // Keep 1 second of world data.
    const MAX_HISTORY: usize = 100;

    fn push_world(&mut self, world: W) {
        // 3's kind of arbitrary marker for "much more than one so that a while loop is too
        // slow"
        if self.historical_worlds.len() >= Self::MAX_HISTORY + 3 {
            // Directly turncate -- it's usually just one but sometimes there's more.
            drop(self.historical_worlds.drain(..self.historical_worlds.len() - Self::MAX_HISTORY))
        }
        while self.historical_worlds.len() >= Self::MAX_HISTORY {
            self.historical_worlds.pop_front();
        }
        // TODO Drop old user actions that occurred prior to end of world.

        // finally stash the new world.
        self.historical_worlds.push_back(world.into());
    }

    fn new(world: W) -> Self {
        Self {
            historical_worlds: [world.into()].into(),
            historical_actions: VecDeque::with_capacity(Self::MAX_HISTORY),
        }
    }

    fn force_reset(&mut self, world: W) {
        self.historical_worlds.clear();
        self.historical_actions.clear();
        self.historical_worlds.push_back(world.into());
    }

    fn insert_recent_action(&mut self, _action: A) {
        unimplemented!("booo");
    }

    fn peek_most_recent(&self) -> Option<Arc<W>> {
        self.historical_worlds.back().cloned()
    }

    fn snapshot(&self) -> SnapshotWorldState<W> {
        let (prev_idx_offset, next_idx_offset) = if self.historical_worlds.len() == 1 {
            (1, 1)
        } else {
            (2, 1)
        };
        SnapshotWorldState {
            prev: (*self.historical_worlds[self.historical_worlds.len() - prev_idx_offset]).clone(),
            next: (*self.historical_worlds[self.historical_worlds.len() - next_idx_offset]).clone(),
            // TODO Actually select interpolation factor.
            interpolation: 0.
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotWorldState<W> {
    pub prev: W,
    pub next: W,
    pub interpolation: f64,
}

pub struct Inputs<W> {
    /// This informs if the simulation should kill itself.
    pub kill_rx: oneshot::Receiver<()>,
    /// This informs the simulation to begin execution or to pause.
    pub run_state_rx: Receiver<bool>,
    /// This informs us that we need to do a full reset of the world.
    ///
    /// If this is prior to the initial state, we'll need to do a full state resync of the
    /// requested world.
    ///
    /// TODO actually do the full resync, for now it'll just explode and assume that players won't
    /// notice
    pub force_jump_rx: Receiver<Option<(DateTime<Utc>, u64)>>,
    /// When a request is sent, this will send back information about the current world state, the
    /// projected future state, as well as an interpolation percentage on [0., 1.]
    pub request_snapshot_rx: Receiver<oneshot::Sender<SnapshotWorldState<W>>>,
    pub force_world_reset_rx: Receiver<W>,
    pub net_api: NettingApi<W>,
    pub framesync_recv_rx: Receiver<(ClientId, u64)>,
    pub death_tally: Arc<AtomicUsize>,
}

#[derive(Clone)]
pub struct Outputs {
    /// This represents if the simulation is currently running.
    pub running: Arc<AtomicBool>,
}

pub struct Simulation<W, A> {
    kill_rx: oneshot::Receiver<()>,
    running: Arc<AtomicBool>,
    world_stash: WorldStash<W, A>,
    framesync: FrameSync,
    run_state_rx: Receiver<bool>,
    force_jump_rx: Receiver<Option<(chrono::DateTime<chrono::Utc>, u64)>>,
    force_world_reset_rx: Receiver<W>,
    request_snapshot_rx: Receiver<oneshot::Sender<SnapshotWorldState<W>>>,
    framesync_recv_rx: Receiver<(ClientId, u64)>,
    death_tally: Arc<AtomicUsize>,

    net_api: NettingApi<W>,
}

impl <W: Default + Clone, A> Simulation<W, A> {
    pub fn init(inputs: Inputs<W>, outputs: Outputs) -> Simulation<W, A> {
        Simulation {
            kill_rx: inputs.kill_rx,
            running: Arc::clone(&outputs.running),
            world_stash: WorldStash::new(W::default()),
            run_state_rx: inputs.run_state_rx,
            force_jump_rx: inputs.force_jump_rx,
            force_world_reset_rx: inputs.force_world_reset_rx,
            request_snapshot_rx: inputs.request_snapshot_rx,
            framesync_recv_rx: inputs.framesync_recv_rx,
            framesync: FrameSync::new(),
            death_tally: inputs.death_tally,
            net_api: inputs.net_api,
        }
    }
}

pub trait SynchronizedSimulatable {
    fn generation(&self) -> u64;
    fn advance(self) -> Self;
}

pub struct SimulationHandle {
    sim: JoinHandle<()>,
}

impl<
    W: SynchronizedSimulatable + Clone + Send + Default + Sync + 'static + Debug + bincode::Encode + bincode::Decode,
    A: Send + 'static
> Simulation<W, A> {
    pub fn start(mut self) -> SimulationHandle {
        // let actions_since_sync: Vec<UserAction> = vec![];
        // let worlds_since_sync: Vec<(DateTime<Utc>, World)> = vec![];

        // let network_input_thread = s.spawn(|| loop {
        //     if game_complete.load(Ordering::Relaxed) {
        //         break;
        //     }
        //     // Read user input, network events and rearrange them. Also manage periodic sync.
        //     // Also ruthlessly kill connections if they haven't been around for a minute. Not
        //     // sure how the game world will react to this, though.
        // });

        let sim_handle = exec::spawn_kill_reporting(self.death_tally, async move {
            loop {
                if exec::kill_requested(&mut self.kill_rx) {
                    trc::info!("KILL sim");
                    return;
                }

                self.running.store(false, std::sync::atomic::Ordering::Relaxed);
                while let Some(run_state) = self.run_state_rx.recv().await {
                    if run_state {
                        break;
                    }
                }
                self.running.store(true, std::sync::atomic::Ordering::Relaxed);

                // The world loops repeatedly.
                let mut target_sim_time = Utc::now();
                loop {
                    let now = Utc::now();
                    if now < target_sim_time {
                        tokio::select! {
                            opt_msg = self.force_jump_rx.recv() => match opt_msg {
                                Some(Some((target_arrival_time, _target_generation))) => {
                                    // This means we're hard resetting and need to reset counters.
                                    trc::info!("SIM-RESET resetting catchup target to {target_arrival_time:?}");
                                    target_sim_time = target_arrival_time + (now - target_arrival_time);
                                },
                                None | Some(None) => {
                                    // This means we're still aiming for the same target generation --
                                    // no changes other than cancelling the sleep is needed.
                                },
                            },
                            _ = tokio::time::sleep((target_sim_time - now).to_std().unwrap()) => {},
                        }
                    }
                    // It's okay to drop errors. If it's disconnected, something's probably
                    // catastrophically wrong anyways.
                    if self.run_state_rx.try_recv() == Ok(false) {
                        break;
                    }

                    while let Ok(world) = self.force_world_reset_rx.try_recv() {
                        self.world_stash.force_reset(world);
                    }
                    while let Ok((client_id, frame)) = self.framesync_recv_rx.try_recv() {
                        self.framesync.record_framesync(client_id, frame);
                    }

                    let working_world = self.world_stash.peek_most_recent().expect("at least one world to be present").clone();
                    let working_world_generation = working_world.generation();
                    let next_working_world = (*working_world).clone().advance();
                    trc::trace!("SIM-LOOP generation={:?}", working_world_generation);
                    // As the last step of each step, stash the world and move on.
                    // TODO force fixed wake up
                    self.world_stash.push_world(next_working_world);

                    let Ok(()) = self.net_api.broadcast(NettingMessageKind::FrameSync(working_world_generation).to_msg()).await else {
                        break;
                    };
                    if let Some(extra_required_delay) = self.framesync.calculate_delay(working_world_generation).await {
                        target_sim_time -= extra_required_delay;
                    }

                    let mut snapshot = None;
                    while let Ok(snapshot_request) = self.request_snapshot_rx.try_recv() {
                        // TODO Determine if we're inbetween frames in the future and properly
                        // calculate frame time offset.
                        let Ok(_) = snapshot_request.send(snapshot.get_or_insert_with(|| self.world_stash.snapshot()).clone()) else {
                            break;
                        };
                    }

                    target_sim_time += chrono::TimeDelta::milliseconds(1000 / i64::from(super::TARGET_FPS));
                }
            }
        });

        SimulationHandle {
            sim: sim_handle,
        }
    }
}

/// Stashes away the "more/less" bounds on the timeline.
///
/// Due to wraparound, this has a single discontinuous point. But the center is also a
/// discontinuous point, so we need 4 potential ranges to fully represent the thing.
#[derive(Debug, Clone)]
pub struct FrameWindowingRanges {
    pub center: u64,
}

impl FrameWindowingRanges {
    fn new(center: u64) -> Self {
        Self {
            center,
        }
    }

    fn signed_distance(&self, input: u64) -> i64 {
        let data = input.wrapping_sub(self.center);
        if data > u64::MAX / 2 {
            // This is the realm of negative distances.
            -((u64::MAX - data) as i64)
        } else {
            data as i64
        }
    }

    fn is_wraparound(&self, input: u64) -> bool {
        input.wrapping_sub(self.center) > input
    }
}

pub struct FrameSync {
    historical_framesyncs: HashMap<ClientId, (chrono::DateTime<chrono::Utc>, u64)>,
    range_cache: RwLock<Option<FrameWindowingRanges>>,
}

impl FrameSync {
    const FRAME_MILLISECONDS: u16 = 1000 / super::TARGET_FPS;
    /// We'll tolerate ~ 2 seconds of being ahead -- that's (currently) around how long it
    /// takes to make two round trips around the globe.
    const PEER_LAG_TOLERANCE: u16 = 2000 / Self::FRAME_MILLISECONDS;
    /// This is the bottom window of frames we're expecting from across the network. We won't have
    /// an upper limit, since the upper limit doesn't really exist.
    const VALIDITY_WINDOW_LOWER_DISTANCE: i64 = Self::PEER_LAG_TOLERANCE as i64 * 2;
    const VALIDITY_WINDOW_UPPER_DISTANCE: i64 = 9999;
    /// We'll allow old data up to two times the lookahead tolerance. This should let us get away
    /// with using u16s as the frame integer type, but I'm lazy.
    const DATA_AGE_LIMIT: chrono::TimeDelta = chrono::TimeDelta::milliseconds(Self::PEER_LAG_TOLERANCE as i64 * 5);

    pub fn new() -> Self {
        Self {
            historical_framesyncs: HashMap::new(),
            range_cache: RwLock::new(None),
        }
    }

    /// To bust cache, set range_cache to None.
    async fn get_ranges(&self, center: u64) -> FrameWindowingRanges {
        let mut cache = self.range_cache.write().await;
        match *cache {
            Some(ref r) if r.center == center => r.clone(),
            None | Some(_) => {
                let r = FrameWindowingRanges::new(center);
                *cache = Some(r.clone());
                r
            },
        }
    }

    pub fn record_framesync(&mut self, peer: ClientId, frame: u64) {
        let entry = self.historical_framesyncs.entry(peer).or_insert_with(|| (Utc::now(), frame));
        // Only update if frame advances the value.
        if entry.1 < frame {
            *entry = (Utc::now(), frame);
        }
    }

    pub async fn calculate_delay(&self, position: u64) -> Option<chrono::TimeDelta> {
        let earliest_allowed_timestamp = Utc::now() - Self::DATA_AGE_LIMIT;
        // First find the "earliest" frame synchronized. We'll use this as the limiting factor.
        let mut limiter = None;
        for (&peer, &(update_timestamp, frame)) in self.historical_framesyncs.iter() {
            if update_timestamp < earliest_allowed_timestamp {
                continue;
            }
            let Some((_wraparound, signed_distance)) = self.signed_distance(position, frame).await else {
                // Since this is "too far" aka the peer is problematic, we'll just ignore them.
                trc::warn!("FRAMESYNC-PEER-IGNORE peer={peer:?} frame={frame:?}");
                continue;
            };
            trc::trace!("FRAMESYNC-DIST peer={peer:?} frame={frame:?} distance={signed_distance:?}");
            let Some(prev) = limiter else {
                trc::trace!("FRAMESYNC-LIMITER-UPDATE peer={peer:?} frame={frame:?} distance={signed_distance:?}");
                limiter = Some((peer, frame, signed_distance));
                continue;
            };
            if signed_distance < prev.2 {
                trc::trace!("FRAMESYNC-LIMITER-UPDATE peer={peer:?} frame={frame:?} distance={signed_distance:?}");
                limiter = Some((peer, frame, signed_distance));
            } else {
                limiter = Some(prev);
            }
        }
        trc::info!("FRAMESYNC-LIMITER-RECORD center={position:?} limiter={limiter:?}");
        // If we don't have an earliest frame, assume we're alone and proceed as if we can
        // continue.
        let (_, limiting_frame, _) = limiter?;

        self.required_delay(position, limiting_frame).await
    }

    /// Returns Some((is_wraparound, signed_distance)).
    /// Returns None if this value should be ignored.
    async fn signed_distance(&self, position: u64, input: u64) -> Option<(bool, i64)> {
        let r = self.get_ranges(position).await;
        let distance = r.signed_distance(input);
        trc::info!("FRAMESYNC-SIGNED-DISTANCE position={position:?} input={input:?}");
        if !(-Self::VALIDITY_WINDOW_LOWER_DISTANCE..=Self::VALIDITY_WINDOW_UPPER_DISTANCE).contains(&distance) {
            return None;
        }
        Some((r.is_wraparound(input), distance))
    }

    async fn required_delay(&self, position: u64, input: u64) -> Option<chrono::TimeDelta> {
        let r = self.get_ranges(position).await;
        let distance = r.signed_distance(input);
        if distance > -(Self::PEER_LAG_TOLERANCE as i64) {
            return None;
        }

        // We don't want to wait too long in case the network catchs up to us -- hence the min(20).
        let frames_to_wait = (distance + (Self::PEER_LAG_TOLERANCE as i64)).min(20);
        Some(chrono::TimeDelta::milliseconds(Self::FRAME_MILLISECONDS as i64 * frames_to_wait as i64))
    }
}
