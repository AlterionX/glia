use std::sync::{atomic::AtomicUsize, Arc};

use chrono::TimeDelta;
use rand::{rngs::OsRng, Rng};
use renderer::{RenderError, RendererBuilder, RendererPreferences};
use task::{DrawTask, FontAtlas, LightCollection, RenderTask};
use tokio::{sync::{mpsc::{Sender, Receiver, error::TryRecvError}, oneshot}, task::JoinHandle};
use vulkano::format::ClearColorValue;
use winit::window::Window;

use crate::{exec::{self, ReceiverTimeoutExt, ThreadDeathReporter}, model::{camera::{Camera, OrthoCamera}, geom::MeshAlloc, AffineTransform}, simulation::SnapshotWorldState, world::World};

// use self::renderer::{Renderer, RendererPreferences};

pub mod task;
mod renderer;
mod shaders;

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
        if let Some(t) = self.text {
            let mut base = AffineTransform::identity();
            base.scaling[0] = 0.25;
            base.scaling[1] = 0.25;
            base.scaling[2] = 0.25;

            let tasks = DrawTask::texts_as_draw_tasks(
                font_atlas,
                vec![(base, t)],
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

                {
                    let mut base = AffineTransform::identity();
                    base.scaling[0] = 0.1;
                    base.scaling[1] = 0.25;
                    base.scaling[2] = 0.25;
                    base.pos[0] = -0.5 + 0.05;
                    base.pos[1] = 0.5 - 0.125;

                    let something = &mut OsRng.gen_range(0..99);
                    let draws = DrawTask::texts_as_draw_tasks(
                        &font_atlas,
                        vec![(base, format!("hello there {:?}", something))],
                        &mut mesh_alloc,
                    );
                    renderer.render_to(window.clone(), RenderTask {
                        draw_wireframe: false,
                        clear_color: ClearColorValue::Float([1., 0., 0., 1.]),
                        draws,
                        lights: LightCollection(vec![]),
                        cam: Camera::Orthographic(OrthoCamera::default()),
                    });
                }

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

                match renderer.render_to(window.clone(), render_task) {
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
