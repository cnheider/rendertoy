#![feature(core_intrinsics)]

mod backend;
mod blob;
mod buffer;
mod camera;
mod consts;
mod keyboard;
mod mesh;
mod package;
mod rgb9e5;
mod shader;
mod texture;
mod viewport;

pub use gl;
pub use nalgebra as na;
pub use snoozy::*;

pub use self::blob::*;
pub use self::buffer::*;
pub use self::camera::*;
pub use self::consts::*;
pub use self::keyboard::*;
pub use self::mesh::*;
pub use self::rgb9e5::*;
pub use self::shader::*;
pub use self::texture::*;
pub use self::viewport::*;

pub type Point2 = na::Point2<f32>;
pub type Vector2 = na::Vector2<f32>;

pub type Point3 = na::Point3<f32>;
pub type Vector3 = na::Vector3<f32>;

pub type Point4 = na::Point4<f32>;
pub type Vector4 = na::Vector4<f32>;

pub type Matrix4 = na::Matrix4<f32>;
pub type Isometry3 = na::Isometry3<f32>;

#[macro_use]
extern crate failure;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate snoozy_macros;
#[macro_use]
extern crate abomonation_derive;

use glutin::dpi::*;
use glutin::GlContext;

use clap::ArgMatches;

use std::str::FromStr;

extern "system" fn gl_debug_message(
    _source: u32,
    type_: u32,
    id: u32,
    severity: u32,
    _len: i32,
    message: *const i8,
    _param: *mut std::ffi::c_void,
) {
    unsafe {
        let s = std::ffi::CStr::from_ptr(message);

        let is_ignored_id = id == 131216; // Program/shader state info: GLSL shader * failed to compile. WAT.

        if gl::DEBUG_TYPE_PERFORMANCE == type_
            || gl::DEBUG_SEVERITY_NOTIFICATION == severity
            || is_ignored_id
        {
            println!("GL debug: {}\n", s.to_string_lossy());
        } else {
            panic!("OpenGL Debug message ({}): {}", id, s.to_string_lossy());
        }
    }
}

pub struct Rendertoy {
    events_loop: glutin::EventsLoop,
    gl_window: glutin::GlWindow,
    mouse_state: MouseState,
    last_timing_query_elapsed: u64,
    cfg: RendertoyConfig,
    keyboard: KeyboardState,
}

#[derive(Clone)]
pub struct MouseState {
    pub pos: Point2,
    pub delta: Vector2,
    pub button_mask: u32,
}

impl Default for MouseState {
    fn default() -> Self {
        Self {
            pos: Point2::origin(),
            delta: Vector2::zeros(),
            button_mask: 0,
        }
    }
}

impl MouseState {
    fn update(&mut self, new_state: &MouseState) {
        self.delta = new_state.pos - self.pos;
        self.pos = new_state.pos;
        self.button_mask = new_state.button_mask;
    }
}

pub struct FrameState<'a> {
    pub mouse: &'a MouseState,
    pub keys: &'a KeyboardState,
    pub gpu_time_ms: f64,
    pub window_size_pixels: (u32, u32),
}

#[derive(Copy, Clone, Debug)]
pub struct RendertoyConfig {
    pub width: u32,
    pub height: u32,
}

fn parse_resolution(s: &str) -> Result<(u32, u32)> {
    match s.find('x') {
        Some(pos) => match (
            FromStr::from_str(&s[..pos]),
            FromStr::from_str(&s[pos + 1..]),
        ) {
            (Ok(a), Ok(b)) => return Ok((a, b)),
            _ => (),
        },
        None => (),
    };

    Err(format_err!("Expected NUMBERxNUMBER, got {}", s))
}

impl RendertoyConfig {
    fn from_args(matches: &ArgMatches) -> RendertoyConfig {
        let (width, height) = matches
            .value_of("resolution")
            .map(|val| parse_resolution(val).unwrap())
            .unwrap_or((1280, 720));

        RendertoyConfig { width, height }
    }
}

impl Rendertoy {
    pub fn new_with_config(cfg: RendertoyConfig) -> Rendertoy {
        let events_loop = glutin::EventsLoop::new();
        let window = glutin::WindowBuilder::new()
            .with_title("Hello, rusty world!")
            .with_dimensions(LogicalSize::new(cfg.width as f64, cfg.height as f64));
        let context = glutin::ContextBuilder::new()
            .with_vsync(true)
            .with_gl_debug_flag(true)
            .with_gl_profile(glutin::GlProfile::Core)
            .with_gl(glutin::GlRequest::Specific(glutin::Api::OpenGl, (4, 3)));
        let gl_window = glutin::GlWindow::new(window, context, &events_loop).unwrap();

        unsafe {
            gl_window.make_current().unwrap();
        }

        gl::load_with(|symbol| gl_window.get_proc_address(symbol) as *const _);

        unsafe {
            gl::DebugMessageCallback(gl_debug_message, std::ptr::null_mut());
            gl::DebugMessageControl(
                gl::DONT_CARE,
                gl::DONT_CARE,
                gl::DONT_CARE,
                0,
                std::ptr::null_mut(),
                1,
            );
            gl::DebugMessageControl(
                gl::DEBUG_SOURCE_SHADER_COMPILER,
                gl::DONT_CARE,
                gl::DONT_CARE,
                0,
                std::ptr::null_mut(),
                0,
            );

            gl::Enable(gl::DEBUG_OUTPUT_SYNCHRONOUS);
            gl::Enable(gl::FRAMEBUFFER_SRGB);

            let mut vao: u32 = 0;
            gl::GenVertexArrays(1, &mut vao);
            gl::BindVertexArray(vao);
        }

        Rendertoy {
            events_loop,
            gl_window,
            mouse_state: MouseState::default(),
            last_timing_query_elapsed: 0,
            cfg,
            keyboard: KeyboardState::new(),
        }
    }

    pub fn new() -> Rendertoy {
        let matches = clap::App::new("Rendertoy")
            .version("1.0")
            .about("Does awesome things")
            .arg(
                clap::Arg::with_name("resolution")
                    .long("resolution")
                    .help("Window resolution")
                    .takes_value(true),
            )
            .get_matches();

        Self::new_with_config(RendertoyConfig::from_args(&matches))
    }

    pub fn width(&self) -> u32 {
        self.cfg.width
    }

    pub fn height(&self) -> u32 {
        self.cfg.height
    }

    fn next_frame(&mut self) -> bool {
        self.gl_window.swap_buffers().unwrap();

        let mut running = true;

        let mut events = Vec::new();
        self.events_loop.poll_events(|event| events.push(event));

        let mut keyboard_events: Vec<KeyboardInput> = Vec::new();
        let mut new_mouse_state = self.mouse_state.clone();

        for event in events.iter() {
            #[allow(clippy::single_match)]
            match event {
                glutin::Event::WindowEvent { event, .. } => match event {
                    glutin::WindowEvent::CloseRequested => running = false,
                    glutin::WindowEvent::Resized(logical_size) => {
                        let dpi_factor = self.gl_window.get_hidpi_factor();
                        let phys_size = logical_size.to_physical(dpi_factor);
                        self.gl_window.resize(phys_size);
                    }
                    glutin::WindowEvent::KeyboardInput { input, .. } => {
                        keyboard_events.push(*input);
                    }
                    glutin::WindowEvent::CursorMoved {
                        position: logical_pos,
                        device_id: _,
                        modifiers: _,
                    } => {
                        let dpi_factor = self.gl_window.get_hidpi_factor();
                        let pos = logical_pos.to_physical(dpi_factor);
                        new_mouse_state.pos = Point2::new(pos.x as f32, pos.y as f32);
                    }
                    glutin::WindowEvent::MouseInput { state, button, .. } => {
                        let button_id = match button {
                            glutin::MouseButton::Left => 0,
                            glutin::MouseButton::Middle => 1,
                            glutin::MouseButton::Right => 2,
                            _ => 0,
                        };

                        if let glutin::ElementState::Pressed = state {
                            new_mouse_state.button_mask |= 1 << button_id;
                        } else {
                            new_mouse_state.button_mask &= !(1 << button_id);
                        }
                    }
                    _ => (),
                },
                _ => (),
            }
        }

        // TODO: proper time
        self.keyboard.update(keyboard_events, 1.0 / 60.0);
        self.mouse_state.update(&new_mouse_state);

        running
    }

    fn draw_with_frame_snapshot<F>(&mut self, callback: &mut F) -> bool
    where
        F: FnMut(&FrameState) -> SnoozyRef<Texture>,
    {
        //unsafe {
        //gl::ClearColor(1.0, 1.0, 1.0, 1.0);
        //gl::Clear(gl::COLOR_BUFFER_BIT);
        //}

        let size = self
            .gl_window
            .get_inner_size()
            .map(|s| s.to_physical(self.gl_window.get_hidpi_factor()))
            .unwrap_or(glutin::dpi::PhysicalSize::new(1.0, 1.0));
        let window_size_pixels = (size.width as u32, size.height as u32);

        let state = FrameState {
            mouse: &self.mouse_state,
            keys: &self.keyboard,
            gpu_time_ms: self.last_timing_query_elapsed as f64 * 1e-6,
            window_size_pixels,
        };

        let tex = callback(&state);

        with_snapshot(|snapshot| {
            draw_fullscreen_texture(&*snapshot.get(tex), state.window_size_pixels);
        });

        self.next_frame()
    }

    pub fn draw_forever<F>(&mut self, mut callback: F)
    where
        F: FnMut(&FrameState) -> SnoozyRef<Texture>,
    {
        let mut timing_query_handle = 0u32;
        let mut timing_query_in_flight = false;

        unsafe {
            gl::GenQueries(1, &mut timing_query_handle);
        }

        let mut running = true;
        while running {
            unsafe {
                if timing_query_in_flight {
                    let mut available: i32 = 0;
                    gl::GetQueryObjectiv(
                        timing_query_handle,
                        gl::QUERY_RESULT_AVAILABLE,
                        &mut available,
                    );

                    if available != 0 {
                        timing_query_in_flight = false;
                        gl::GetQueryObjectui64v(
                            timing_query_handle,
                            gl::QUERY_RESULT,
                            &mut self.last_timing_query_elapsed,
                        );
                    }
                }

                if !timing_query_in_flight {
                    gl::BeginQuery(gl::TIME_ELAPSED, timing_query_handle);
                }
            }

            running = self.draw_with_frame_snapshot(&mut callback);

            unsafe {
                if !timing_query_in_flight {
                    gl::EndQuery(gl::TIME_ELAPSED);
                    timing_query_in_flight = true;
                }
            }
        }
    }
}

pub fn draw_fullscreen_texture(tex: &Texture, framebuffer_size: (u32, u32)) {
    backend::draw::draw_fullscreen_texture(tex.texture_id, framebuffer_size);
}
