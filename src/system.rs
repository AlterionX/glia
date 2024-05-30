use std::sync::{atomic::{AtomicBool, AtomicUsize}, Arc};

use chrono::TimeDelta;
use tokio::{sync::{mpsc::{Receiver, Sender}, oneshot}, task::JoinHandle};
use winit::{application::ApplicationHandler, error::EventLoopError, event::WindowEvent, event_loop::EventLoop, window::Window};

use crate::exec;

pub struct Inputs {
    pub kill_rx: oneshot::Receiver<()>,
    pub death_tally: Arc<AtomicUsize>,
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
            trc::info!("SYS -- Close requested");
            self.should_exit = true;
            return;
        }

        match event {
            WindowEvent::CloseRequested => {
                trc::info!("SYS -- Close requested (likely close button)");
                self.should_exit = true;
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
            trc::info!("KILL sys");
            for (target, kill_tx) in self.kill_txs.drain(..) {
                trc::info!("KILL-TX {:?}", target);
                kill_tx.send(()).ok();
            }
            event_loop.exit();
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

        let expected_deaths = self.outputs.kill_txs.len();
        let mut wm = WindowingManager {
            kill_rx: self.inputs.kill_rx,
            kill_txs: self.outputs.kill_txs,
            window: None,
            should_exit: false,
        };

        self.eloop.run_app(&mut wm)?;

        self.inputs.death_tally.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Wait for kill count to reach expected values before killing system thread.
        let mut prev_deaths = 0;
        while {
            let current_deaths = self.inputs.death_tally.load(std::sync::atomic::Ordering::Relaxed);
            if current_deaths != prev_deaths {
                trc::info!("KILL death count changed: {:?}", current_deaths);
            }
            prev_deaths = current_deaths;
            current_deaths < expected_deaths
        } {
            std::thread::sleep(chrono::TimeDelta::milliseconds(100).to_std().unwrap());
        }

        let handle = tokio::spawn(async move {
            for (i, task) in tokio::runtime::Handle::current().dump().await.tasks().iter().enumerate() {
                trc::info!("GLIA (only one task expected) tokio dump {}", i);
                trc::info!("{}", task.trace());
            }
        });
        while !handle.is_finished() {}

        let tokio_metrics = tokio::runtime::Handle::current().metrics();
        trc::info!(
            "GLIA terminated, shutting down tokio --- remaining tasks: {:?} --- remaining injected tasks: {:?} ---- blocking threads: {:?} ---- idle threads: {:?}",
            tokio_metrics.active_tasks_count(),
            tokio_metrics.injection_queue_depth(),
            tokio_metrics.num_blocking_threads(),
            tokio_metrics.num_idle_blocking_threads(),
        );
        // tokio::runtime::shutdown_timeout(TimeDelta::milliseconds(500).to_std().unwrap());

        Ok(())
    }
}
