// Ops
mod cfg;
mod log;

// Underlying tech modules.
mod netting;
mod simulation;
// mod render;
mod input;
mod bus;

// Actual game modules.
mod world;

use std::{error::Error, fmt::{Display, Pointer}, net::IpAddr, sync::{atomic::AtomicBool, Arc}};

use chrono::{Duration, DateTime, Utc};

use tokio::sync::mpsc;

use crate::{netting::{NettingMessageKind, Netting, NettingApi}, simulation::Simulation, bus::Bus, world::World};

const TARGET_FPS: u16 = 120;

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

const DEFAULT_FILE: &str = "cfg.toml";

#[tokio::main]
async fn main() {
    match main_with_error_handler().await {
        Ok(()) => {
            /* do nothing */
        },
        Err(err) => {
            todo!("handle error report -- {err:?}");
        },
    }
}

#[derive(Debug)]
pub struct ReportableError {
    pub message: String,
}

impl Display for ReportableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Error for ReportableError {
}

async fn main_with_error_handler() -> Result<(), ReportableError> {
    let cfg = Box::leak(cfg::read()?);
    log::setup(cfg)?;

    // Networking channels
    let (osynt_tx, osynt_rx) = mpsc::channel(1024);
    let (onm_tx, onm_rx) = mpsc::channel(1024);
    let (inm_tx, inm_rx) = mpsc::channel(1024);
    // Simulation/Bus channels
    let (run_state_tx, run_state_rx) = mpsc::channel(10);
    let (force_jump_tx, force_jump_rx) = mpsc::channel(10);
    let (framesync_recv_tx, framesync_recv_rx) = mpsc::channel(1);
    let (request_snapshot_tx, request_snapshot_rx) = mpsc::channel(1);
    let (force_world_reset_tx, force_world_reset_rx) = mpsc::channel(10);
    let sim_running = Arc::new(AtomicBool::new(false));

    let net_api = NettingApi {
        osynt_tx: osynt_tx.clone(),
        onm_tx,
    };

    // Systems setup
    let net = Netting::<World>::init(netting::Inputs {
        onm_rx,
        osynt_rx,
    }, netting::Outputs {
        inm_tx,
        osynt_tx: osynt_tx.clone(),
    });
    let bus = Bus::init(bus::Inputs {
        sim_running: Arc::clone(&sim_running),
        inm_rx,
    }, bus::Outputs {
        request_snapshot_tx,
        force_world_reset_tx,
        force_jump_tx,
        run_state_tx: run_state_tx.clone(),
        framesync_recv_tx,
        net_api: net_api.clone(),
    });
    let sim = Simulation::<_, UserAction>::init(simulation::Inputs {
        run_state_rx,
        force_jump_rx,
        request_snapshot_rx,
        force_world_reset_rx,
        framesync_recv_rx,
        net_api: net_api.clone(),
    }, simulation::Outputs {
        running: Arc::clone(&sim_running),
    });

    let net_handle = net.start();
    let bus_handle = bus.start();
    let sim_handle = sim.start();

    // Input buffer
    let mut input = String::new();

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
        run_state_tx.send(true).await.expect("no problems kicking off game loop");
        net_api.create_peer_connection(addr.parse().unwrap()).await;
    }
    input.truncate(0);

    loop {
        println!("Enter string to send");
        std::io::stdin().read_line(&mut input).unwrap();
        let msg = input.trim();
        net_api.broadcast(NettingMessageKind::NakedLogString(msg.to_owned()).to_msg()).await;
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
