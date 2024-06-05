use std::sync::{atomic::{AtomicBool, AtomicUsize}, Arc};

use tokio::{sync::{mpsc::Sender, oneshot}, task::JoinHandle};
use winit::{application::ApplicationHandler, error::EventLoopError, event::{KeyEvent, WindowEvent}, event_loop::EventLoop, keyboard::{KeyCode, PhysicalKey}, window::Window};

use crate::exec;

pub struct Inputs {
    pub kill_rx: oneshot::Receiver<()>,
    pub death_tally: Arc<AtomicUsize>,
}

pub struct Outputs {
    pub kill_txs: Vec<(&'static str, oneshot::Sender<()>)>,
    pub window_tx: Sender<(usize, Arc<Window>)>,
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
    window_generation: usize,
    window: Option<Arc<Window>>,
    window_tx: Sender<(usize, Arc<Window>)>,
    should_exit: bool,
}

impl ApplicationHandler for WindowingManager {
    fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
        let w = Arc::new(event_loop.create_window(Window::default_attributes()).unwrap());
        let gen = self.window_generation;
        self.window_generation += 1;
        self.window = Some(w.clone());
        let tx = self.window_tx.clone();
        tokio::spawn(async move {
            tx.send((gen, w)).await.expect("render thread to be fine");
        });
    }

    fn window_event(&mut self, event_loop: &winit::event_loop::ActiveEventLoop, window_id: winit::window::WindowId, event: winit::event::WindowEvent) {
        if exec::kill_requested(&mut self.kill_rx) {
            trc::info!("SYS -- Close requested");
            self.should_exit = true;
            return;
        }

        /*
                DeviceEvent::Key(key_in) => match key_in.physical_key {
                    PhysicalKey::Code(keycode) => match keycode {
                        // We'll just ignore modifiers for now.
                        KeyCode::Escape => { elwt.exit(); },
                        KeyCode::KeyW => {
                            tx.send(WorldEvent::SetMoveForward(key_in.state.is_pressed())).unwrap();
                        },
                        KeyCode::KeyA => {
                            tx.send(WorldEvent::SetMoveLeft(key_in.state.is_pressed())).unwrap();
                        },
                        KeyCode::KeyS => {
                            tx.send(WorldEvent::SetMoveBackward(key_in.state.is_pressed())).unwrap();
                        },
                        KeyCode::KeyD => {
                            tx.send(WorldEvent::SetMoveRight(key_in.state.is_pressed())).unwrap();
                        },
                        KeyCode::KeyQ => {
                            tx.send(WorldEvent::SetRollRight(key_in.state.is_pressed())).unwrap();
                        },
                        KeyCode::KeyE => {
                            tx.send(WorldEvent::SetRollLeft(key_in.state.is_pressed())).unwrap();
                        },
                        KeyCode::Space => {
                            tx.send(WorldEvent::SetMoveUp(key_in.state.is_pressed())).unwrap();
                        },
                        KeyCode::ControlLeft => {
                            tx.send(WorldEvent::SetMoveDown(key_in.state.is_pressed())).unwrap();
                        },
                        KeyCode::KeyN => {
                            tx.send(WorldEvent::ShiftBackground(key_in.state.is_pressed())).unwrap();
                        },
                        KeyCode::Tab => {
                            tx.send(WorldEvent::ToggleWireFrame(key_in.state.is_pressed())).unwrap();
                        },
                        _ => {},
                    },
                    PhysicalKey::Unidentified(_native) => {},
                },
                DeviceEvent::MouseMotion { delta: (maybe_xd, maybe_yd) } => {
                    let (xd, yd) = match FORCE_MOUSE_MOTION_MODE {
                        Some(MouseMotionMode::Relative) => (maybe_xd, maybe_yd),
                        Some(MouseMotionMode::Warp) => {
                            (calc_relative_motion(&mut last_mouse_x, maybe_xd), calc_relative_motion(&mut last_mouse_y, maybe_yd))
                        },
                        None => {
                            // We'll kind of guess if this is correct.
                            // Absolute values tend to be large -- break on > 2000.
                            // This can probably be better.
                            let is_probably_absolute = warp_mouse_detected || (maybe_xd * maybe_xd + maybe_yd * maybe_yd) > (2000. * 2000.);
                            if is_probably_absolute {
                                warp_mouse_detected = true;
                            }
                            if is_probably_absolute {
                                (calc_relative_motion(&mut last_mouse_x, maybe_xd), calc_relative_motion(&mut last_mouse_y, maybe_yd))
                            } else {
                                (maybe_xd, maybe_yd)
                            }
                        },
                    };

                    let scaling_factor = std::f64::consts::PI / 500.;
                    let x_scaling_factor = -scaling_factor;
                    let y_scaling_factor = -scaling_factor / 5.;
                    log::info!("MOUSE-MOVED x={xd} y={yd}");

                    tx.send(WorldEvent::Pitch((yd * y_scaling_factor) as f32)).unwrap();
                    tx.send(WorldEvent::Yaw((xd * x_scaling_factor) as f32)).unwrap();
                },
                _ => {},
         */

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
            WindowEvent::KeyboardInput { device_id: _, event, is_synthetic: _ } => match event.physical_key {
                PhysicalKey::Code(code) => match code {
                    KeyCode::Escape => {
                        trc::info!("SYS -- Close requested (keyboard)");
                        self.should_exit = true;
                    },
                    KeyCode::Backquote => {},
                    _ => {},
                },
                PhysicalKey::Unidentified(_) => {},
            },
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
            window_generation: 0,
            window: None,
            window_tx: self.outputs.window_tx,
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

        let tokio_metrics = tokio::runtime::Handle::current().metrics();
        if tokio_metrics.active_tasks_count() != 0 {
            let handle = tokio::spawn(async move {
                for (i, task) in tokio::runtime::Handle::current().dump().await.tasks().iter().enumerate() {
                    trc::info!("GLIA (only one task expected) tokio dump {}", i);
                    trc::info!("{}", task.trace());
                }
            });
            while !handle.is_finished() {}
        }
        trc::info!(
            "GLIA terminated, shutting down tokio --- remaining tasks: {:?} --- remaining injected tasks: {:?} ---- blocking threads: {:?} ---- idle threads: {:?}",
            tokio_metrics.active_tasks_count(),
            tokio_metrics.injection_queue_depth(),
            tokio_metrics.num_blocking_threads(),
            tokio_metrics.num_idle_blocking_threads(),
        );

        Ok(())
    }
}
