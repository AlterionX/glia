use std::sync::{atomic::AtomicUsize, Arc};

use tokio::{sync::{mpsc::Receiver, oneshot}, task::JoinHandle};
use winit::window::Window;

use crate::exec::{self, ReceiverTimeoutExt};

// use self::renderer::{Renderer, RendererPreferences};

// pub mod task;
// mod renderer;
// mod shaders;

pub enum RenderScene {
    Sim,
    Menu,
    MainMenu,
}

pub struct Render {
    inputs: Inputs,
    outputs: Outputs,
}

pub struct RenderingTaskHandle {
    handle: JoinHandle<()>,
}

pub struct Inputs {
    pub window_rx: Receiver<Arc<Window>>,
    pub trigger_render_rx: Receiver<RenderScene>,
    pub kill_rx: oneshot::Receiver<()>,
}

pub struct Outputs {
    pub death_tally: Arc<AtomicUsize>,
}

impl Render {
    pub fn init(inputs: Inputs, outputs: Outputs) -> Self {
        Self {
            inputs,
            outputs,
        }
    }

    pub fn start(mut self) -> RenderingTaskHandle {
        let handle = exec::spawn_kill_reporting(self.outputs.death_tally, async move {
            // let _preferences = RendererPreferences::default();

            loop {
                if exec::kill_requested(&mut self.inputs.kill_rx) {
                    trc::info!("KILL render");
                    return;
                }

                // let renderer = Renderer::init(Some("totality-render-demo".to_owned()), None, inputs.window_rx, &preferences).expect("renderer to load");

                loop {
                    if exec::kill_requested(&mut self.inputs.kill_rx) {
                        trc::info!("KILL render");
                        return;
                    }

                    // Skip over non-recent render requests to get the most recent.
                    let Some(mut initial) = self.inputs.trigger_render_rx.recv_for_ms(100).await.value() else {
                        continue;
                    };
                    while let Ok(next_initial) = self.inputs.trigger_render_rx.try_recv() {
                        initial = next_initial;
                    }
                    // TODO Actually handle the responses.

                    let draw_call = match initial {
                        RenderScene::Sim => {
                            determine_sim_draw_calls()
                        },
                        RenderScene::Menu => {
                            determine_menu_draw_calls()
                        },
                        RenderScene::MainMenu => {
                            determine_main_menu_draw_calls()
                        },
                    };

                    // renderer.draw(draw_call);
                }
            }
        });

        RenderingTaskHandle { handle }
    }
}

fn determine_sim_draw_calls() -> Vec<()> {
    vec![]
}

fn determine_menu_draw_calls() -> Vec<()> {
    todo!("menu not implemented");
}

fn determine_main_menu_draw_calls() -> Vec<()> {
    todo!("main menu not implemented");
}
