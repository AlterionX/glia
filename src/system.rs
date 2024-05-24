use std::sync::{atomic::AtomicBool, Arc};

use tokio::{sync::{mpsc::Sender, oneshot}, task::JoinHandle};
use winit::{application::ApplicationHandler, error::EventLoopError, event::WindowEvent, event_loop::EventLoop, window::Window};

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
}

impl ApplicationHandler for WindowingManager {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        let w = event_loop.create_window(Window::default_attributes()).unwrap();
        self.window = Some(Arc::new(w));
    }

    fn window_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, window_id: winit::window::WindowId, event: winit::event::WindowEvent) {
        match self.kill_rx.try_recv() {
            Ok(_) | Err(oneshot::error::TryRecvError::Closed) => {
                trc::info!("KILL Close requested");
                for (target, kill_tx) in self.kill_txs.drain(..) {
                    trc::info!("KILL-TX {:?}", target);
                    kill_tx.send(()).ok();
                }
                event_loop.exit();
            },
            Err(oneshot::error::TryRecvError::Empty) => {},
        }

        match event {
            WindowEvent::CloseRequested => {
                trc::info!("KILL Close requested on window (likely close button)");
                for (target, kill_tx) in self.kill_txs.drain(..) {
                    trc::info!("KILL-TX {:?}", target);
                    kill_tx.send(()).ok();
                }
                event_loop.exit();
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
        })
    }
}
