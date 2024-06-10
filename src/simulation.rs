use std::{collections::{HashMap, VecDeque}, fmt::Debug, sync::{atomic::{AtomicBool, AtomicUsize}, Arc}};

use chrono::{DateTime, Utc};
use tokio::{sync::{mpsc::{Receiver, Sender, error::TryRecvError}, oneshot, RwLock}, task::JoinHandle};

use crate::{exec::{self, ReceiverTimeoutExt, ThreadDeathReporter, TimeoutOutcome}, netting::{ClientId, NettingApi, NettingMessage, NettingMessageKind}, render::RenderScene, UserAction, UserActionKind};

// TODO Fundamentally refactor action flow so that this isn't necessary.
pub enum ActionIntake<LA, GA> {
    Local(LA),
    Global(GA),
}

pub trait GlobalizableAction {
    type Output;
    fn globalize(self, timeframe: u64, sender: ClientId) -> Self::Output;
}

impl GlobalizableAction for UserActionKind {
    type Output = UserAction;
    fn globalize(self, sim_time: u64, sender: ClientId) -> Self::Output {
        UserAction {
            sim_time,
            sender,
            kind: self,
        }
    }
}

pub trait SimulationTimeframeAttached {
    fn rank_key(&self) -> (u64, ClientId);
    fn timeframe(&self) -> u64;
}

impl SimulationTimeframeAttached for UserAction {
    fn rank_key(&self) -> (u64, ClientId) {
        (self.sim_time, self.sender)
    }
    fn timeframe(&self) -> u64 {
        self.sim_time
    }
}

// TODO use a triple buffer with some network-related niceties
#[derive(Default)]
pub struct WorldStash<W, A> {
    historical_actions: VecDeque<A>,
    historical_worlds: VecDeque<Arc<W>>,
    max_generation: u64,
}

impl <W: Clone + SynchronizedSimulatable<A>, A: SimulationTimeframeAttached> WorldStash<W, A> {
    // Keep 3 seconds of world data.
    const MAX_HISTORY: usize = 300;

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
        let earliest_gen = self.historical_worlds.front().unwrap().generation();
        while let Some(a) = self.historical_actions.front() {
            if a.timeframe() >= earliest_gen {
                break;
            }
            self.historical_actions.pop_front();
        }
        // TODO Drop old user actions that occurred prior to end of world.

        self.max_generation = self.max_generation.max(world.generation());
        // finally stash the new world.
        self.historical_worlds.push_back(world.into());
    }

    fn new(world: W) -> Self {
        Self {
            max_generation: world.generation(),
            historical_worlds: [world.into()].into(),
            historical_actions: VecDeque::with_capacity(Self::MAX_HISTORY),
        }
    }

    fn force_reset(&mut self, world: W) {
        self.max_generation = world.generation();
        self.historical_worlds.clear();
        self.historical_actions.clear();
        self.historical_worlds.push_back(world.into());
    }

    fn insert_recent_action(&mut self, action: A) {
        if let Some(w) = self.peek_earliest() {
            // If the action is past the earliest generation, ignore it. We keep a large enough
            // buffer that it should be enough (O(seconds)) so anything before that is probably
            // a bad packet. If it still desyncs, move on.
            if w.generation() > action.timeframe() {
                return;
            }
        }

        let recalculation_start = action.timeframe();
        self.historical_actions.push_back(action);
        self.historical_actions.make_contiguous().sort_by_key(|a| a.rank_key());

        // We will need to recalculate all the worlds prior to this point so, time to drop worlds
        // until we're good.
        while let Some(w) = self.peek_most_recent_not_last() {
            if w.generation() <= recalculation_start {
                break;
            }
            self.historical_worlds.pop_back();
        }
    }

    fn peek_earliest(&self) -> Option<Arc<W>> {
        self.historical_worlds.front().cloned()
    }

    fn peek_most_recent_not_last(&self) -> Option<Arc<W>> {
        if self.historical_worlds.len() == 1 {
            return None;
        }
        self.peek_most_recent()
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

    fn generation(&self) -> u64 {
        self.peek_earliest().map(|w| w.generation()).unwrap_or(0)
    }

    fn actions_for_generation(&self, gen: u64) -> impl Iterator<Item=&A> {
        self.historical_actions.iter().filter(move |a| a.timeframe() == gen)
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotWorldState<W> {
    pub prev: W,
    pub next: W,
    pub interpolation: f64,
}

pub struct Inputs<W, LA, GA> {
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
    pub net_api: NettingApi<W, GA>,
    pub framesync_recv_rx: Receiver<(ClientId, u64)>,
    pub user_action_rx: Receiver<ActionIntake<LA, GA>>,
    pub death_tally: Arc<AtomicUsize>,
    pub own_client_id: ClientId,
}

#[derive(Clone)]
pub struct Outputs {
    /// This represents if the simulation is currently running.
    pub running: Arc<AtomicBool>,
    pub draw_call_tx: Sender<RenderScene>,
}

pub struct Simulation<W, LA, GA> {
    own_client_id: ClientId,
    kill_rx: oneshot::Receiver<()>,
    user_action_rx: Receiver<ActionIntake<LA, GA>>,
    running: Arc<AtomicBool>,
    world_stash: WorldStash<W, GA>,
    framesync: FrameSync,
    run_state_rx: Receiver<bool>,
    force_jump_rx: Receiver<Option<(chrono::DateTime<chrono::Utc>, u64)>>,
    force_world_reset_rx: Receiver<W>,
    request_snapshot_rx: Receiver<oneshot::Sender<SnapshotWorldState<W>>>,
    framesync_recv_rx: Receiver<(ClientId, u64)>,
    death_tally: Arc<AtomicUsize>,
    draw_call_tx: Sender<RenderScene>,

    net_api: NettingApi<W, GA>,
}

impl <W: Default + Clone + SynchronizedSimulatable<GA>, LA, GA: SimulationTimeframeAttached> Simulation<W, LA, GA> {
    pub fn init(inputs: Inputs<W, LA, GA>, outputs: Outputs) -> Simulation<W, LA, GA> {
        Simulation {
            own_client_id: inputs.own_client_id,
            kill_rx: inputs.kill_rx,
            user_action_rx: inputs.user_action_rx,
            running: Arc::clone(&outputs.running),
            world_stash: WorldStash::new(W::default()),
            run_state_rx: inputs.run_state_rx,
            force_jump_rx: inputs.force_jump_rx,
            force_world_reset_rx: inputs.force_world_reset_rx,
            request_snapshot_rx: inputs.request_snapshot_rx,
            framesync_recv_rx: inputs.framesync_recv_rx,
            framesync: FrameSync::new(),
            draw_call_tx: outputs.draw_call_tx,
            death_tally: inputs.death_tally,
            net_api: inputs.net_api,
        }
    }
}

pub trait SynchronizedSimulatable<A> {
    fn execute(&mut self, user_action: &A);
    fn generation(&self) -> u64;
    fn advance(self) -> Self;
}

pub struct SimulationHandle {
    sim: JoinHandle<()>,
}

impl<
    W: SynchronizedSimulatable<GA> + Clone + Send + Default + Sync + 'static + Debug + bincode::Encode + bincode::Decode,
    LA: Debug + GlobalizableAction<Output=GA> + Send + 'static,
    GA: Debug + Send + Clone + bincode::Encode + bincode::Decode + SimulationTimeframeAttached + 'static,
> Simulation<W, LA, GA> {
    pub fn start(mut self) -> SimulationHandle {
        let sim_handle = ThreadDeathReporter::new(&self.death_tally, "sim").spawn(async move {
            'main_loop: loop {
                if exec::kill_requested(&mut self.kill_rx) { return; }

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
                        match self.force_jump_rx.recv_for(target_sim_time - now).await {
                            TimeoutOutcome::Value(Some((target_arrival_time, _target_generation))) => {
                                // This means we're hard resetting and need to reset counters.
                                trc::info!("SIM-RESET resetting catchup target to {target_arrival_time:?}");
                                target_sim_time = target_arrival_time + (now - target_arrival_time);
                            },
                            // This means we're still aiming for the same target generation --
                            // no changes other than cancelling the sleep is needed.
                            TimeoutOutcome::Value(None) | TimeoutOutcome::Closed | TimeoutOutcome::Timeout => {},
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

                    let apparent_generation = self.world_stash.max_generation;
                    while let Some(action_intake) = match self.user_action_rx.try_recv() {
                        Ok(action_intake) => Some(action_intake),
                        Err(TryRecvError::Empty) => None,
                        Err(TryRecvError::Disconnected) => {
                            trc::error!("ACTION-INTAKE-DISCONNECT");
                            continue 'main_loop;
                        },
                    } {
                        let action = match action_intake {
                            ActionIntake::Global(g) => g,
                            ActionIntake::Local(l) => {
                                let a = l.globalize(apparent_generation, self.own_client_id);
                                let secondary_a = a.clone();
                                let net_api = self.net_api.clone();
                                tokio::spawn(async move {
                                    net_api.broadcast(NettingMessageKind::User(secondary_a).into_msg()).await.expect("no problems broadcasting");
                                });
                                a
                            },
                        };
                        self.world_stash.insert_recent_action(action);
                    }

                    let mut working_world = (*self.world_stash.peek_most_recent().expect("at least one world to be present")).clone().advance();
                    let working_world_generation = working_world.generation();
                    for action in self.world_stash.actions_for_generation(working_world_generation - 1) {
                        working_world.execute(action);
                    }

                    trc::trace!("SIM-LOOP generation={:?}", working_world_generation);
                    // As the last step of each step, stash the world and move on.
                    // TODO force fixed wake up
                    self.world_stash.push_world(working_world);

                    let Ok(()) = self.net_api.broadcast(NettingMessageKind::FrameSync(working_world_generation).into_msg()).await else {
                        break;
                    };
                    if let Some(extra_required_delay) = self.framesync.calculate_delay(working_world_generation).await {
                        target_sim_time -= extra_required_delay;
                    }

                    let mut snapshot = None;
                    while let Ok(snapshot_request) = self.request_snapshot_rx.try_recv() {
                        // TODO Determine if we're inbetween frames in the future and properly
                        // calculate frame time offset.

                        // We're kinda fine with this since if the other side closed, it's fine.
                        snapshot_request.send(snapshot.get_or_insert_with(|| self.world_stash.snapshot()).clone()).ok();
                    }

                    target_sim_time += chrono::TimeDelta::milliseconds(1000 / i64::from(super::TARGET_FPS));
                    // This doesn't send the world state in case the render thread is being slow.
                    // The render thread will filter out rapid calls and render at whatever is its
                    // sustainable rate.
                    let temp_channel = self.draw_call_tx.clone();
                    tokio::spawn(async move {
                        // TODO Change this to actual scene render
                        temp_channel.send(RenderScene::Sim).await.ok();
                    });
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
        trc::trace!("FRAMESYNC-LIMITER-RECORD center={position:?} limiter={limiter:?}");
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
        trc::trace!("FRAMESYNC-SIGNED-DISTANCE position={position:?} input={input:?}");
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
