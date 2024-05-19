mod netting;
mod framesync;

use std::{cell::RefCell, collections::{hash_map, HashMap, VecDeque}, net::IpAddr, ops::{Bound, Range, RangeInclusive}, sync::{atomic::AtomicBool, Arc}};

use chrono::{Duration, DateTime, Utc};
use bincode::{Encode, Decode};

use tokio::sync::{mpsc::{self, error::TryRecvError}, RwLock};

use crate::{netting::{NettingMessageKind, Netting}, framesync::FrameSync};

const TARGET_FPS: u16 = 120;

#[derive(Debug, Default, Clone, Encode, Decode)]
pub enum Terrain {
    #[default]
    Grass,
    StoneTile,
    Stone,
    Forest,
}

#[derive(Debug, Default, Clone, Encode, Decode)]
pub struct CombatMapCell {
    pub terrain: Terrain,
    pub occupant: Option<usize>,
}

#[derive(Debug, Default, Clone, Encode, Decode)]
pub struct CombatMap {
    pub cells: Vec<Vec<CombatMapCell>>,
}

#[derive(Debug, Clone, Encode, Decode)]
pub enum Action {
}

#[derive(Debug, Clone, Encode, Decode)]
pub enum GameStage {
    Combat(CombatMap),
}

#[derive(Debug, Clone, Encode, Decode)]
pub enum EnemyKind {
}

#[derive(Debug, Clone, Encode, Decode)]
pub enum PlayerKind {
}

#[derive(Debug, Clone, Encode, Decode)]
pub enum Character {
    Player {
        name: String,
        kind: PlayerKind,
        relic_collection: RelicCollection,
        deck: Deck,
    },
    Enemy {
        kind: EnemyKind,
        planned_action: Option<Action>,
    },
}

#[derive(Debug, Clone, Encode, Decode)]
pub enum RelicKind {
}

#[derive(Debug, Clone, Encode, Decode)]
pub struct Relic {
    pub kind: RelicKind,
    pub active: bool,
}

#[derive(Debug, Default, Clone, Encode, Decode)]
pub struct RelicCollection {
    pub active_relics: Vec<usize>,
    pub relics: Vec<Relic>,
}

#[derive(Debug, Clone, Encode, Decode)]
pub enum Card {
}

#[derive(Debug, Default, Clone, Encode, Decode)]
pub struct Deck {
    pub cards: Vec<Card>,
    pub draw: VecDeque<usize>,
    pub hand: VecDeque<usize>,
    pub discard: VecDeque<usize>,
}

#[derive(Debug, Default, Clone, Encode, Decode)]
pub struct World {
    pub generation: u64,
    pub stage: Option<GameStage>,
    pub characters: Vec<Character>,
}

#[derive(Debug)]
pub enum UserAction {
}

impl UserAction {
    fn timestamp(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

pub enum Input {
}

pub enum ControlFlow {
    Continue,
    RoundComplete,
    GameComplete,
}

impl World {
    fn execute(&mut self, action: UserAction) {
    }

    /// Action
    fn tick(&mut self) -> ControlFlow {
        ControlFlow::Continue
    }

    /// Determines if the world requires additional input.
    fn waiting_for_input(&self) -> bool {
        false
    }
}

pub struct Connection {
    pub last_sync_generation: u64,
    pub last_sync: DateTime<Utc>,
    pub socket: (),
}

impl Connection {
    pub fn link_to(target_ip: IpAddr, socket: u16, frozen_world: World) -> Self {
        let a = Self {
            last_sync: Utc::now(),
            last_sync_generation: frozen_world.generation,
            socket: (),
        };
        a
    }
}

pub enum Change {
    Noop,
}

pub struct Transition<'a> {
    /// Status the "old" is transitioning from
    pub w0: &'a World,
    /// Status the "new" is transitioning to
    pub w1: &'a World,
    /// What actually changed between the two states
    pub change: Change,
    /// What point, out of 1000, we're at between worlds. This has
    /// nothing to do with duration. Animations will have a separate mapping from
    /// interpolation to duration.
    pub interpolation_point: usize,
}

pub enum Interpolation<'a> {
    Transition(Transition<'a>),
    // second field is remaining duration -- this will be passed to the next invocation of
    // deduce_transition.
    CheckpointReached(&'a World, Duration),
}

pub enum RenderSource<'a> {
    Transition(Transition<'a>),
    Static(&'a World),
}

impl<'a> Interpolation<'a> {
    // Returns nothing when we've gone past w1. If this returns None, the rendering code should
    // pass along the next world to render -- or simply render the world as is.
    // TODO Make time_since_w0 actually matter -- it's currently a fixed world rendering strategy.
    pub fn deduce_transition(w0: &'a World, w1: &'a World, time_since_w0: Duration) -> Self {
        // Target is 60 fps but each world iteration is potentially long lasting. Aim half second
        // delay for now. The world diff will inform additional information here.
        if time_since_w0 > Duration::milliseconds(1000/60) {
            return Self::CheckpointReached(w1, time_since_w0);
        }
        Self::Transition(Transition {
            w0,
            w1,
            change: Change::Noop,
            // This changes based on the change. We'll leave it at 0 for now to get a nice step
            // function.
            interpolation_point: 0,
        })
    }

    pub fn chase_checkpoint_or_transition(self, next_world: &'a World) -> Self {
        match self {
            same_value @ Self::Transition(_) => same_value,
            Self::CheckpointReached(prev_world, duration) => Self::deduce_transition(prev_world, next_world, duration),
        }
    }

    pub fn into_render_source(worlds: &'a [World], duration_since_initial: Duration) -> Option<RenderSource<'a>> {
        if worlds.len() == 0 {
            return None;
        }
        if worlds.len() == 1 {
            return Some(RenderSource::Static(&worlds[0]));
        }

        let mut world_iter = worlds.iter();
        let mut current_case = Self::deduce_transition(&worlds[0], &worlds[1], duration_since_initial);
        loop {
            match current_case {
                Self::Transition(t) => {
                    return Some(RenderSource::Transition(t));
                },
                Self::CheckpointReached(new_old_world, time_diff) => match world_iter.next() {
                    Some(new_world) => {
                        current_case = Self::CheckpointReached(new_old_world, time_diff).chase_checkpoint_or_transition(new_world);
                    },
                    None => {
                        return Some(RenderSource::Static(new_old_world));
                    },
                }
            }
        }
    }
}

pub struct Renderer {
}

impl Renderer {
    /// This renders the state of the game world transitioning from one to the next.
    /// We do some hacky stuff to get animations to work correctly -- by analyzing
    /// diff between the states, we know what animations to play. A side effect of
    /// this is that "skipping" animations becomes really easy -- we simply render
    /// the next state without the diffing step.
    pub fn render(transition: Transition) {
    }
}

// TODO use a triple buffer with some network-related niceties
#[derive(Default)]
pub struct WorldStash {
    stable_state: (),
    historical_actions: VecDeque<UserAction>,
    historical_worlds: VecDeque<World>,
}

impl WorldStash {
    // Keep 1 second of world data.
    const MAX_HISTORY: usize = 100;

    fn advance(&mut self, world: World) {
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
        self.historical_worlds.push_back(world);
    }

    fn new(world: World) -> Self {
        Self {
            stable_state: (),
            historical_worlds: [world].into(),
            historical_actions: VecDeque::with_capacity(Self::MAX_HISTORY),
        }
    }

    fn force_reset(&mut self, world: World) {
        self.historical_worlds.clear();
        self.historical_actions.clear();
        self.historical_worlds.push_back(world);
    }

    fn insert_recent_action(&mut self) {
        unimplemented!("booo");
    }

    fn peek_most_recent(&self) -> Option<&World> {
        self.historical_worlds.back()
    }
}

#[tokio::main]
async fn main() {
    let subscriber = tracing_subscriber::fmt()
        .compact()
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(true)
        .with_target(false)
        .finish();
    trc::subscriber::set_global_default(subscriber).expect("no issues setting up logging");
    let mut input = String::new();

    // Systems setup
    let world_stash = Arc::new(RwLock::new(WorldStash::new(World::default())));
    let (netting, mut netting_msg_rx) = Netting::new().await;
    let framesync = Arc::new(RwLock::new(FrameSync::new()));
    // let (input_tx, input_rx) = mpsc::channel::<Input>();
    // let game_complete = AtomicBool::new(false);

    let sim_world_stash = Arc::clone(&world_stash);
    let sim_netting = Arc::clone(&netting);
    let (sim_run_state_tx, mut sim_run_state_rx) = mpsc::channel::<bool>(10);
    let (sim_force_jump_tx, mut sim_force_jump_rx) = mpsc::channel::<Option<(chrono::DateTime<chrono::Utc>, u64)>>(10);
    let sim_framesync = Arc::clone(&framesync);
    let sim_running = Arc::new(AtomicBool::new(false));
    let sim_running_internal = Arc::clone(&sim_running);
    let _sim_handle = tokio::spawn(async move {
        loop {
            sim_running_internal.store(false, std::sync::atomic::Ordering::Relaxed);
            while !sim_run_state_rx.recv().await.expect("run state rx to not be broken") { }
            sim_running_internal.store(true, std::sync::atomic::Ordering::Relaxed);

            // The world loops repeatedly.
            let mut target_sim_time = Utc::now();
            loop {
                let now = Utc::now();
                if now < target_sim_time {
                    tokio::select! {
                        Some(msg) = sim_force_jump_rx.recv() => {
                            if let Some((
                                target_arrival_time,
                                _target_generation,
                            )) = msg {
                                // This means we're hard resetting and need to reset counters.
                                trc::info!("SIM-RESET resetting catchup target to {target_arrival_time:?}");
                                target_sim_time = target_arrival_time + (now - target_arrival_time);
                            } else {
                                // This means we're still aiming for the same target generation --
                                // no changes other than cancelling the sleep is needed.
                            }
                        },
                        _ = tokio::time::sleep((target_sim_time - now).to_std().unwrap()) => {},
                    }
                }
                // It's okay to drop errors. If it's disconnected, something's probably
                // catastrophically wrong anyways.
                if sim_run_state_rx.try_recv() == Ok(false) {
                    break;
                }

                let mut working_world = sim_world_stash.read().await.peek_most_recent().expect("at least one world to be present").clone();
                let working_world_generation = working_world.generation;
                working_world.generation += 1;
                trc::trace!("SIM-LOOP generation={:?}", working_world.generation);
                // As the last step of each step, stash the world and move on.
                // TODO force fixed wake up
                sim_world_stash.write().await.historical_worlds.push_back(working_world);

                sim_netting.broadcast(NettingMessageKind::FrameSync(working_world_generation).to_msg()).await;
                let framesync_reader = sim_framesync.read().await;
                if let Some(extra_required_delay) = framesync_reader.calculate_delay(working_world_generation).await {
                    target_sim_time -= extra_required_delay;
                }

                target_sim_time += chrono::TimeDelta::milliseconds(1000 / i64::from(TARGET_FPS));
            }
        }
    });

    let netting_world_stash = Arc::clone(&world_stash);
    let netting_netting = Arc::clone(&netting);
    let sim_running_netting = Arc::clone(&sim_running);
    let netting_sim_force_jump_tx = sim_force_jump_tx.clone();
    let netting_sim_run_state_tx = sim_run_state_tx.clone();
    let netting_framesync = Arc::clone(&framesync);
    tokio::spawn(async move {
        // Local record to avoid lock contention.
        let mut framesync_records = HashMap::new();

        loop {
            let Some(inbound_msg) = netting_msg_rx.recv().await else {
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
                },
                NettingMessageKind::Handshake => {
                    trc::info!("NM-GREET [{:?}] {:?}", msg.message_id, msg.message_id);
                },
                NettingMessageKind::NewConnection => {
                    trc::info!("NM-CONN [{:?}] {:?}", msg.message_id, inbound_msg.sender_id);
                    // TODO Avoid this and reconcile multiple WorldTransfers correctly, especially
                    // on startup.
                    if sim_running_netting.load(std::sync::atomic::Ordering::Acquire) {
                        // Only send if running -- this prevents needing reconciliation of game
                        // state when receiving a WorldTransfer ... for now.
                        let boxed_world = Box::new(netting_world_stash.read().await.peek_most_recent().expect("at least one world").clone());
                        netting_netting.send_to(NettingMessageKind::WorldTransfer(boxed_world).to_msg(), inbound_msg.sender_id).await;
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
                    let gen = w.generation;
                    netting_world_stash.write().await.force_reset(*w);
                    netting_sim_force_jump_tx.send(Some((Utc::now(), gen))).await.expect("everything to be linked");
                    // Kickstart! This will get ignored if we're already running, so it's all good.
                    netting_sim_run_state_tx.send(true).await.expect("everything to be linked");
                },
                NettingMessageKind::User(ua) => {
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
                            let is_big_jump = *b.get() > frame + u64::from(TARGET_FPS);
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
                    netting_framesync.write().await.record_framesync(inbound_msg.sender_id, frame);
                },
            }
            drop(entered);
        }
    });

    // We're running some tests for connecting, need to input thing.
    println!("Connect to? (Do not connect on both clients. Hit enter once connected to proceed to message sending.)");
    std::io::stdin().read_line(&mut input).unwrap();
    let addr = input.trim();
    println!("Will attempt to connect to {addr:?}...");
    if sim_running.load(std::sync::atomic::Ordering::Acquire) {
        trc::info!("Skipping peer -- already connected to mesh");
    } else {
        // Also initiate game state
        // TODO resolve race condition of multiple connections
        sim_run_state_tx.send(true).await.expect("no problems kicking off game loop");
        netting.create_peer_connection(addr.parse().unwrap()).await;
    }
    input.truncate(0);

    loop {
        println!("Enter string to send");
        std::io::stdin().read_line(&mut input).unwrap();
        let msg = input.trim();
        netting.broadcast(NettingMessageKind::NakedLogString(msg.to_owned()).to_msg()).await;
        input.truncate(0);
    }

    // thread::scope(|s| {
    //     let actions_since_sync: Vec<UserAction> = vec![];
    //     let worlds_since_sync: Vec<(DateTime<Utc>, World)> = vec![];

    //     let network_input_thread = s.spawn(|| loop {
    //         if game_complete.load(Ordering::Relaxed) {
    //             break;
    //         }

    //         // Read user input, network events and rearrange them. Also manage periodic sync.
    //         // Also ruthlessly kill connections if they haven't been around for a minute. Not
    //         // sure how the game world will react to this, though.
    //     });

    //     let main_thread = s.spawn(|| 'main_loop: loop {
    //         'round_loop: loop {
    //             match world.tick() {
    //                 ControlFlow::Continue => {
    //                 },
    //                 ControlFlow::RoundComplete => {
    //                     break 'round_loop;
    //                 },
    //                 ControlFlow::GameComplete => {
    //                     break 'main_loop;
    //                 },
    //             }
    //         }
    //     });
    // });
}
