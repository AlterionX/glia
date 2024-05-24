use std::sync::Arc;

use tokio::{task::JoinHandle, sync::mpsc::Receiver};
use winit::window::Window;

use self::renderer::{Renderer, RendererPreferences};

pub mod task;
mod renderer;
mod shaders;

pub struct Render {
    renderer: Renderer,
    render_task_rx: Receiver<()>,
}

pub struct RenderingTaskHandle {
    handle: JoinHandle<()>,
}

pub struct Inputs {
    window: Arc<Window>,
    render_task_rx: Receiver<()>,
}

pub struct Outputs {
}

impl Render {
    pub fn init(inputs: Inputs, _outputs: Outputs) -> Self {
        let preferences = RendererPreferences::default();
        let renderer = Renderer::init(Some("totality-render-demo".to_owned()), None, inputs.window, &preferences).expect("renderer to load");

        Self {
            renderer,
            render_task_rx: inputs.render_task_rx,
        }
    }

    pub fn start(mut self) -> RenderingTaskHandle {
        let handle = tokio::spawn(async move { loop {
            let initial = {
                // Skip over non-recent render requests to get the most recent.
                let mut initial = self.render_task_rx.recv().await.expect("render task value to be present");
                while let Some(next_initial) = self.render_task_rx.try_recv().ok() {
                    initial = next_initial;
                }
                initial
            };

            // TODO Actually handle the responses.
        }});

        RenderingTaskHandle { handle }
    }
}
