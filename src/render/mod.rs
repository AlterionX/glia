use std::sync::{atomic::AtomicUsize, Arc};

use chrono::TimeDelta;
use renderer::{RenderError, RendererBuilder, RendererPreferences};
use task::{DrawTask, FontAtlas, LightCollection, RenderTask};
use tokio::{sync::{mpsc::{Sender, Receiver, error::TryRecvError}, oneshot}, task::JoinHandle};
use vulkano::format::ClearColorValue;
use winit::window::Window;

use crate::{exec::{self, ReceiverTimeoutExt, ThreadDeathReporter}, model::{camera::{Camera, OrthoCamera}, geom::MeshAlloc, AffineTransform}, simulation::{SnapshotWorldState, SynchronizedSimulatable}, world::World};

// use self::renderer::{Renderer, RendererPreferences};

pub mod task;
mod renderer;
mod shaders;

#[derive(Debug)]
pub enum RenderScene {
    Sim,
    Menu,
    MainMenu,
}

pub struct Render<W> {
    inputs: Inputs,
    outputs: Outputs<W>,
}

pub struct RenderingTaskHandle {
    handle: JoinHandle<()>,
}

pub struct Inputs {
    pub window_rx: Receiver<(usize, Arc<Window>)>,
    pub trigger_render_rx: Receiver<RenderScene>,
    pub kill_rx: oneshot::Receiver<()>,
}

pub struct Outputs<W> {
    pub death_tally: Arc<AtomicUsize>,
    pub request_snapshot_tx: Sender<oneshot::Sender<SnapshotWorldState<W>>>,
}

pub trait Renderable {
    type Cache<'a>;

    fn into_render_task_with_cache(self, cache: Self::Cache<'_>) -> RenderTask<'_>;
}

pub trait RenderableWithoutCache {
    fn into_render_task(self) -> RenderTask<'static>;
}

impl <T: Renderable<Cache<'static>=()>> RenderableWithoutCache for T {
    fn into_render_task(self) -> RenderTask<'static> {
        self.into_render_task_with_cache(())
    }
}

impl Renderable for World {
    type Cache<'a> = (&'a FontAtlas, &'a mut MeshAlloc);

    fn into_render_task_with_cache(self, (font_atlas, mesh_alloc): Self::Cache<'_>) -> RenderTask<'_> {
        let mut draws = vec![];
        { // Generation print
            let gen = self.generation();
            let gen_num_chars = {
                let mut g = gen;
                let mut n = 1;
                while { n += 1; g != 0 } { g /= 10; }
                n
            };
            let mut base = AffineTransform::identity();
            base.scaling[0] = 0.1;
            base.scaling[1] = 0.25;
            base.scaling[2] = 0.25;
            base.pos[0] = 0.5 + 0.025 - 0.05 * (gen_num_chars + 4) as f32;
            base.pos[1] = 0.5 - 0.0625;

            let tasks = DrawTask::texts_as_draw_tasks(
                font_atlas,
                vec![(base, format!("Gen {:?}", self.generation()))],
                mesh_alloc,
            );
            draws.extend(tasks);
        };
        if let Some(t) = self.text {
            let mut base = AffineTransform::identity();
            base.scaling[0] = 0.1;
            base.scaling[1] = 0.25;
            base.scaling[2] = 0.25;
            base.pos[0] = -0.5 + 0.025;
            base.pos[1] = 0.5 - 0.0625;
            // shift lower on the screen to avoid overlap with generation
            base.pos[1] -= 0.13;

            // TODO better scaling
            // TODO proper text wrapping based on window size, text size, etc
            let mut line_length = 20;
            let mut adjusted_scaling = 1f32;
            // Arbitarily chosen point to start scaling down.
            // TODO use character count instead of this
            if t.len() > 40 {
                adjusted_scaling = 0.25;
                line_length = 40;
                base.scaling[0] *= adjusted_scaling;
                base.scaling[1] *= adjusted_scaling;
                base.scaling[2] *= adjusted_scaling;
                // shift a bit to the left for decreased width.
                base.pos[0] -= 0.0125;
            }
            let text_chunks = { 
                let mut cc = t.chars().peekable();
                let mut chunk_slices = vec![];
                while cc.peek().is_some() {
                    let mut buffer = String::new();
                    for _i in 0..line_length {
                        if let Some(c) = cc.next() {
                            buffer.push(c);
                        }
                    }
                    if !buffer.is_empty() {
                        chunk_slices.push(buffer);
                    }
                }
                chunk_slices
            };

            let tasks = DrawTask::texts_as_draw_tasks(
                font_atlas,
                text_chunks.into_iter().enumerate().map(|(i, c)| {
                    let mut base_line = base.clone();
                    base_line.pos[1] -= 0.13 * (i as f32) * adjusted_scaling;

                    (base_line, c)
                }).collect(),
                mesh_alloc,
            );
            draws.extend(tasks);
        };
        RenderTask {
            draw_wireframe: false,
            clear_color: ClearColorValue::Float([self.color[0], self.color[1], self.color[2], 1.]),
            draws,
            lights: LightCollection(vec![]),
            cam: Camera::Orthographic(OrthoCamera::default()),
        }
    }
}

impl <W: Sync + Send + for <'a> Renderable<Cache<'a>=(&'a FontAtlas, &'a mut MeshAlloc)> + 'static> Render<W> {
    pub fn init(inputs: Inputs, outputs: Outputs<W>) -> Self {
        Self {
            inputs,
            outputs,
        }
    }

    pub fn start(mut self) -> RenderingTaskHandle {
        let handle = ThreadDeathReporter::new(&self.outputs.death_tally, "render").spawn(async move {
            let preferences = RendererPreferences {
                application_name: Some("Glia Demo".to_owned()),
                application_version: None,
                preferred_physical_device: None,
                preferred_physical_device_type: None,
            };
            let mut renderer_cache = None;
            let mut window_cache = None;
            let mut font_atlas = FontAtlas::new();
            let mut mesh_alloc = MeshAlloc::new();

            'main_loop: loop {
                trc::trace!("RENDER trigger");

                if exec::kill_requested(&mut self.inputs.kill_rx) { return; }

                let (stashed_gen, window) = if let Some((stashed_generation, w)) = window_cache {
                    if let Ok((new_gen, new_window)) = self.inputs.window_rx.try_recv() {
                        if new_gen > stashed_generation {
                            window_cache = Some((new_gen, new_window));
                            renderer_cache = None;
                            continue;
                        }
                    }
                    (stashed_generation, w)
                } else {
                    let Some((gen, window)) = self.inputs.window_rx.recv_for_ms(1000).await.value() else {
                        window_cache = None;
                        continue;
                    };
                    window_cache = Some((gen, window));
                    renderer_cache = None;
                    continue;
                };
                let Some(mut renderer) = renderer_cache else {
                    let renderer = RendererBuilder {
                        windowing: window.clone(),
                        prefs: preferences.clone(),
                    }.init().expect("renderer to load");
                    window_cache = Some((stashed_gen, window));
                    renderer_cache = Some(renderer);

                    continue;
                };

                // Skip over non-recent render requests to get the most recent.
                let Some(mut initial) = self.inputs.trigger_render_rx.recv_for_ms(100).await.value() else {
                    window_cache = Some((stashed_gen, window));
                    renderer_cache = Some(renderer);
                    continue;
                };
                while let Some(render_scene) = match self.inputs.trigger_render_rx.try_recv() {
                    Ok(v) => Some(v),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => {
                        window_cache = Some((stashed_gen, window));
                        renderer_cache = Some(renderer);
                        continue 'main_loop;
                    },
                } {
                    initial = render_scene;
                }
                // TODO Actually handle the responses.

                let render_task = match initial {
                    RenderScene::Sim => {
                        let (world_tx, world_rx) = oneshot::channel();
                        let Ok(_) = self.outputs.request_snapshot_tx.send_timeout(world_tx, TimeDelta::milliseconds(100).to_std().unwrap()).await else {
                            window_cache = Some((stashed_gen, window));
                            renderer_cache = Some(renderer);
                            continue 'main_loop;
                        };
                        let snapshot = match world_rx.await {
                            Ok(s) => s,
                            Err(_e) => {
                                window_cache = Some((stashed_gen, window));
                                renderer_cache = Some(renderer);
                                continue 'main_loop;
                            },
                        };
                        snapshot.prev.into_render_task_with_cache((&font_atlas, &mut mesh_alloc))
                    },
                    RenderScene::Menu => {
                        determine_menu_draw_calls()
                    },
                    RenderScene::MainMenu => {
                        determine_main_menu_draw_calls()
                    },
                };

                match renderer.render_to(window.clone(), dbg!(render_task)) {
                    Ok(()) => {},
                    Err(RenderError::BadSwapchain) => {
                        // We'll just ignore this, we can do it later.
                        trc::debug!("RENDER-ERR swapchain requires recreation, ignoring draw");
                    },
                    Err(RenderError::Vulkan(e)) => {
                        panic!("unexpected error during rendering {e:?}");
                    },
                }

                window_cache = Some((stashed_gen, window));
                renderer_cache = Some(renderer);
            }
        });

        RenderingTaskHandle { handle }
    }
}

fn determine_menu_draw_calls<'a>() -> RenderTask<'a> {
    todo!("menu not implemented");
}

fn determine_main_menu_draw_calls<'a>() -> RenderTask<'a> {
    todo!("main menu not implemented");
}
