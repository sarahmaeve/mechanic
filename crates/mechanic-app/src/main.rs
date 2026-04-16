//! Mechanic terminal emulator — application entry point.

mod app;
mod convert;
mod input;

fn main() {
    env_logger::init();

    let config = mechanic_config::Config::default();
    let event_loop = winit::event_loop::EventLoop::new().unwrap();
    let mut app = app::App::new(config);
    event_loop.run_app(&mut app).unwrap();
}
