use std::collections::VecDeque;

use bincode::{Decode, Encode};
use crate::{simulation::SynchronizedSimulatable, UserAction, UserActionKind};

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

    // Super temp thing to prove out a concept.
    pub color: [f32; 3],
    pub text: Option<String>,
}

impl SynchronizedSimulatable<UserAction> for World {
    fn execute(&mut self, action: &UserAction) {
        trc::info!("UserAction triggered {action:?}");
        match action.kind {
            UserActionKind::SetColor(rgb_f32) => {
                self.color = rgb_f32;
            },
            UserActionKind::SetLog(ref message) => {
                self.text = Some(message.clone());
            },
        }
    }

    fn advance(self) -> Self {
        Self {
            generation: self.generation + 1,
            ..self
        }
    }

    fn generation(&self) -> u64 {
        self.generation
    }
}
