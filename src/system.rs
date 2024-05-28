use std::sync::{atomic::AtomicBool, Arc};

use tokio::{sync::{mpsc::Sender, oneshot}, task::JoinHandle};
use winit::{application::ApplicationHandler, error::EventLoopError, event::WindowEvent, event_loop::EventLoop, window::Window};

use crate::exec;

pub struct Inputs {
    pub kill_rx: oneshot::Receiver<()>,
}

pub struct Outputs {
    pub kill_txs: Vec<(&'static str, oneshot::Sender<()>)>,
    pub window_tx: Sender<Arc<Window>>,
}

pub struct SystemBus {
    eloop: EventLoop<()>,
    inputs: Inputs,
    outputs: Outputs,
}

pub struct SystemBusHandle {
}

pub struct WindowingManager {
    kill_rx: oneshot::Receiver<()>,
    pub kill_txs: Vec<(&'static str, oneshot::Sender<()>)>,
    window: Option<Arc<Window>>,
    should_exit: bool,
}

impl ApplicationHandler for WindowingManager {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        let w = event_loop.create_window(Window::default_attributes()).unwrap();
        self.window = Some(Arc::new(w));
    }

    fn window_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, window_id: winit::window::WindowId, event: winit::event::WindowEvent) {
        if exec::kill_requested(&mut self.kill_rx) {
            trc::info!("KILL Close requested (likely close button)");
            self.kill(event_loop);
            return;
        } else {
            trc::info!("KILL NOT sys");
        }

        match event {
            WindowEvent::CloseRequested => {
                trc::info!("KILL Close requested (likely close button)");
                self.kill(event_loop);
            },
            WindowEvent::RedrawRequested => {},
            WindowEvent::Ime(_) => {},
            WindowEvent::Destroyed => {},
            WindowEvent::Moved(_) => {},
            WindowEvent::Touch(_) => {},
            _ => {
                trc::trace!("Unknown even {event:?}");
            },
        }
    }

    fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        if self.should_exit {
            self.kill(event_loop);
            event_loop.exit();
        }
    }
}

impl WindowingManager {
    fn kill(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        self.should_exit = true;
        for (target, kill_tx) in self.kill_txs.drain(..) {
            trc::info!("KILL-TX {:?}", target);
            kill_tx.send(()).ok();
        }
        event_loop.exit();
    }
}

impl SystemBus {
    pub fn init(inputs: Inputs, outputs: Outputs) -> Self {
        let eloop = winit::event_loop::EventLoop::new().expect("creating event loop to work");
        Self {
            eloop,
            inputs,
            outputs,
        }
    }

    pub fn takeover_thread(self) -> Result<(), EventLoopError> {
        self.eloop.set_control_flow(winit::event_loop::ControlFlow::Poll);

        self.eloop.run_app(&mut WindowingManager {
            kill_rx: self.inputs.kill_rx,
            kill_txs: self.outputs.kill_txs,
            window: None,
            should_exit: false,
        })
    }
}
