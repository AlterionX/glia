mod netting;

use std::{net::IpAddr, sync::{atomic::{AtomicBool, Ordering}, mpsc, Mutex}, thread};

use chrono::{Duration, DateTime, Utc};

#[derive(Default)]
pub enum Terrain {
    #[default]
    Grass,
    StoneTile,
    Stone,
    Forest,
}

#[derive(Default)]
pub struct CombatMapCell {
    pub terrain: Terrain,
    pub occupant: Option<usize>,
}

#[derive(Default)]
pub struct CombatMap {
    pub cells: Vec<Vec<CombatMapCell>>,
}

pub enum Action {
}

pub enum GameStage {
    Combat(CombatMap),
}

pub enum EnemyKind {
}

pub enum PlayerKind {
}

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

pub enum RelicKind {
}

pub struct Relic {
    pub kind: RelicKind,
    pub active: bool,
}

#[derive(Default)]
pub struct RelicCollection {
    pub active_relics: Vec<usize>,
    pub relics: Vec<Relic>,
}

pub enum Card {
}

#[derive(Default)]
pub struct Deck {
    pub cards: Vec<Card>,
    pub draw: Vec<usize>,
    pub hand: Vec<usize>,
    pub discard: Vec<usize>,
}

#[derive(Default)]
pub struct World {
    pub generation: u64,
    pub stage: Option<GameStage>,
    pub characters: Vec<Character>,
}

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

#[tokio::main]
async fn main() {
    let (netting, msg_rx) = netting::Netting::new().await;

    // We're running some tests for connecting, need to input thing.
    use std::io::Stdin;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap();
    let input = input.trim();
    println!("Will attempt to connect to {input:?}...");
    netting.create_peer_connection(input.parse().unwrap()).await;

    loop {
        // TODO Make this a "wait for everything to finish" thing instead of just sleeping.
        tokio::time::sleep(Duration::milliseconds(50000).to_std().unwrap()).await;
    }
    // // Systems setup
    // let mut world = World::default();
    // let (input_tx, input_rx) = mpsc::channel::<Input>();
    // let game_complete = AtomicBool::new(false);
    // let network_and_inputs = Netting::new();

    // // Begin running game systems...
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
