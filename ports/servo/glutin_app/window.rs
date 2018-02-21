/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! A windowing implementation using glutin.

use compositing::compositor_thread::{EmbedderMsg, EventLoopWaker};
use compositing::windowing::{AnimationState, MouseWindowEvent, WindowEvent};
use compositing::windowing::{EmbedderCoordinates, WebRenderDebugOption, WindowMethods};
use euclid::{Length, TypedPoint2D, TypedVector2D, TypedScale, TypedSize2D};
#[cfg(target_os = "windows")]
use gdi32;
use gleam::gl;
use glutin;
use glutin::{Api, GlContext, GlRequest};
use msg::constellation_msg::{self, Key, TopLevelBrowsingContextId as BrowserId};
use msg::constellation_msg::{KeyModifiers, KeyState, TraversalDirection};
use net_traits::pub_domains::is_reg_domain;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use osmesa_sys;
use script_traits::{LoadData, TouchEventType};
use servo::ipc_channel::ipc::IpcSender;
use servo_config::opts;
use servo_config::prefs::PREFS;
use servo_geometry::DeviceIndependentPixel;
use servo_url::ServoUrl;
use std::cell::{Cell, RefCell};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::ffi::CString;
use std::mem;
use std::os::raw::c_void;
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;
use std::thread;
use std::time;
use style_traits::DevicePixel;
use style_traits::cursor::CursorKind;
use super::keyutils::{self, CMD_OR_ALT, CMD_OR_CONTROL, GlutinKeyModifiers};
use tinyfiledialogs;
#[cfg(target_os = "windows")]
use user32;
use webrender_api::{DeviceIntPoint, DeviceUintRect, DeviceUintSize, ScrollLocation};
#[cfg(target_os = "windows")]
use winapi;
use winit;
use winit::{ElementState, Event, MouseButton, MouseScrollDelta, TouchPhase, VirtualKeyCode};
#[cfg(target_os = "macos")]
use winit::os::macos::{ActivationPolicy, WindowBuilderExt};


// This should vary by zoom level and maybe actual text size (focused or under cursor)
const LINE_HEIGHT: f32 = 38.0;

const MULTISAMPLES: u16 = 16;

#[cfg(target_os = "macos")]
fn builder_with_platform_options(mut builder: winit::WindowBuilder) -> winit::WindowBuilder {
    if opts::get().headless || opts::get().output_file.is_some() {
        // Prevent the window from showing in Dock.app, stealing focus,
        // or appearing at all when running in headless mode or generating an
        // output file.
        builder = builder.with_activation_policy(ActivationPolicy::Prohibited)
    }
    builder
}

#[cfg(not(target_os = "macos"))]
fn builder_with_platform_options(builder: winit::WindowBuilder) -> winit::WindowBuilder {
    builder
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct HeadlessContext {
    width: u32,
    height: u32,
    _context: osmesa_sys::OSMesaContext,
    _buffer: Vec<u32>,
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct HeadlessContext {
    width: u32,
    height: u32,
}

impl HeadlessContext {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn new(width: u32, height: u32) -> HeadlessContext {
        let mut attribs = Vec::new();

        attribs.push(osmesa_sys::OSMESA_PROFILE);
        attribs.push(osmesa_sys::OSMESA_CORE_PROFILE);
        attribs.push(osmesa_sys::OSMESA_CONTEXT_MAJOR_VERSION);
        attribs.push(3);
        attribs.push(osmesa_sys::OSMESA_CONTEXT_MINOR_VERSION);
        attribs.push(3);
        attribs.push(0);

        let context = unsafe {
            osmesa_sys::OSMesaCreateContextAttribs(attribs.as_ptr(), ptr::null_mut())
        };

        assert!(!context.is_null());

        let mut buffer = vec![0; (width * height) as usize];

        unsafe {
            let ret = osmesa_sys::OSMesaMakeCurrent(context,
                                                    buffer.as_mut_ptr() as *mut _,
                                                    gl::UNSIGNED_BYTE,
                                                    width as i32,
                                                    height as i32);
            assert_ne!(ret, 0);
        };

        HeadlessContext {
            width: width,
            height: height,
            _context: context,
            _buffer: buffer,
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn new(width: u32, height: u32) -> HeadlessContext {
        HeadlessContext {
            width: width,
            height: height,
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn get_proc_address(s: &str) -> *const c_void {
        let c_str = CString::new(s).expect("Unable to create CString");
        unsafe {
            mem::transmute(osmesa_sys::OSMesaGetProcAddress(c_str.as_ptr()))
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn get_proc_address(_: &str) -> *const c_void {
        ptr::null() as *const _
    }
}

enum WindowKind {
    Window(glutin::GlWindow, RefCell<winit::EventsLoop>),
    Headless(HeadlessContext),
}

/// The type of a window.
pub struct Window {
    kind: WindowKind,
    screen_size: TypedSize2D<u32, DeviceIndependentPixel>,
    inner_size: Cell<TypedSize2D<u32, DeviceIndependentPixel>>,

    mouse_down_button: Cell<Option<winit::MouseButton>>,
    mouse_down_point: Cell<TypedPoint2D<i32, DevicePixel>>,
    event_queue: RefCell<Vec<WindowEvent>>,

    /// id of the top level browsing context. It is unique as tabs
    /// are not supported yet. None until created.
    browser_id: Cell<Option<BrowserId>>,

    mouse_pos: Cell<TypedPoint2D<i32, DevicePixel>>,
    key_modifiers: Cell<GlutinKeyModifiers>,
    current_url: RefCell<Option<ServoUrl>>,

    last_pressed_key: Cell<Option<constellation_msg::Key>>,

    animation_state: Cell<AnimationState>,

    fullscreen: Cell<bool>,

    gl: Rc<gl::Gl>,
    suspended: Cell<bool>,
}

#[cfg(not(target_os = "windows"))]
fn window_creation_scale_factor() -> TypedScale<f32, DeviceIndependentPixel, DevicePixel> {
    TypedScale::new(1.0)
}

#[cfg(target_os = "windows")]
fn window_creation_scale_factor() -> TypedScale<f32, DeviceIndependentPixel, DevicePixel> {
        let hdc = unsafe { user32::GetDC(::std::ptr::null_mut()) };
        let ppi = unsafe { gdi32::GetDeviceCaps(hdc, winapi::wingdi::LOGPIXELSY) };
        TypedScale::new(ppi as f32 / 96.0)
}


impl Window {
    pub fn set_browser_id(&self, browser_id: BrowserId) {
        self.browser_id.set(Some(browser_id));
    }

    pub fn new(is_foreground: bool,
               window_size: TypedSize2D<u32, DeviceIndependentPixel>) -> Rc<Window> {
        let win_size: DeviceUintSize = (window_size.to_f32() * window_creation_scale_factor()).to_u32();
        let width = win_size.to_untyped().width;
        let height = win_size.to_untyped().height;

        // If there's no chrome, start off with the window invisible. It will be set to visible in
        // `load_end()`. This avoids an ugly flash of unstyled content (especially important since
        // unstyled content is white and chrome often has a transparent background). See issue
        // #9996.
        let visible = is_foreground && !opts::get().no_native_titlebar;

        let screen_size;
        let inner_size;
        let window_kind = if opts::get().headless {
            screen_size = TypedSize2D::new(width, height);
            inner_size = TypedSize2D::new(width, height);
            WindowKind::Headless(HeadlessContext::new(width, height))
        } else {
            let events_loop = winit::EventsLoop::new();
            let mut window_builder = winit::WindowBuilder::new()
                .with_title("Servo".to_string())
                .with_decorations(!opts::get().no_native_titlebar)
                .with_transparency(opts::get().no_native_titlebar)
                .with_dimensions(width, height)
                .with_visibility(visible)
                .with_multitouch();

            window_builder = builder_with_platform_options(window_builder);

            let mut context_builder = glutin::ContextBuilder::new()
                .with_gl(Window::gl_version())
                .with_vsync(opts::get().enable_vsync);

            if opts::get().use_msaa {
                context_builder = context_builder.with_multisampling(MULTISAMPLES)
            }

            let glutin_window = glutin::GlWindow::new(window_builder, context_builder, &events_loop)
                .expect("Failed to create window.");

            unsafe {
                glutin_window.context().make_current().expect("Couldn't make window current");
            }

            let (screen_width, screen_height) = events_loop.get_primary_monitor().get_dimensions();
            screen_size = TypedSize2D::new(screen_width, screen_height);
            // TODO(ajeffrey): can this fail?
            let (width, height) = glutin_window.get_inner_size().expect("Failed to get window inner size.");
            inner_size = TypedSize2D::new(width, height);

            glutin_window.show();

            WindowKind::Window(glutin_window, RefCell::new(events_loop))
        };

        let gl = match window_kind {
            WindowKind::Window(ref window, ..) => {
                match gl::GlType::default() {
                    gl::GlType::Gl => {
                        unsafe {
                            gl::GlFns::load_with(|s| window.get_proc_address(s) as *const _)
                        }
                    }
                    gl::GlType::Gles => {
                        unsafe {
                            gl::GlesFns::load_with(|s| window.get_proc_address(s) as *const _)
                        }
                    }
                }
            }
            WindowKind::Headless(..) => {
                unsafe {
                    gl::GlFns::load_with(|s| HeadlessContext::get_proc_address(s))
                }
            }
        };

        if opts::get().headless {
            // Print some information about the headless renderer that
            // can be useful in diagnosing CI failures on build machines.
            println!("{}", gl.get_string(gl::VENDOR));
            println!("{}", gl.get_string(gl::RENDERER));
            println!("{}", gl.get_string(gl::VERSION));
        }

        gl.clear_color(0.6, 0.6, 0.6, 1.0);
        gl.clear(gl::COLOR_BUFFER_BIT);
        gl.finish();

        let window = Window {
            kind: window_kind,
            event_queue: RefCell::new(vec!()),
            mouse_down_button: Cell::new(None),
            mouse_down_point: Cell::new(TypedPoint2D::new(0, 0)),

            browser_id: Cell::new(None),

            mouse_pos: Cell::new(TypedPoint2D::new(0, 0)),
            key_modifiers: Cell::new(GlutinKeyModifiers::empty()),
            current_url: RefCell::new(None),

            last_pressed_key: Cell::new(None),
            gl: gl.clone(),
            animation_state: Cell::new(AnimationState::Idle),
            fullscreen: Cell::new(false),
            inner_size: Cell::new(inner_size),
            screen_size,
            suspended: Cell::new(false),
        };

        window.present();

        Rc::new(window)
    }

    #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
    fn gl_version() -> GlRequest {
        return GlRequest::Specific(Api::OpenGl, (3, 2));
    }

    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    fn gl_version() -> GlRequest {
        GlRequest::Specific(Api::OpenGlEs, (3, 0))
    }

    fn handle_received_character(&self, ch: char) {
        let modifiers = keyutils::glutin_mods_to_script_mods(self.key_modifiers.get());
        if keyutils::is_identifier_ignorable(&ch) {
            return
        }
        if let Some(last_pressed_key) = self.last_pressed_key.get() {
            let event = WindowEvent::KeyEvent(Some(ch), last_pressed_key, KeyState::Pressed, modifiers);
            self.event_queue.borrow_mut().push(event);
        } else {
            // Only send the character if we can print it (by ignoring characters like backspace)
            if !ch.is_control() {
                match keyutils::char_to_script_key(ch) {
                    Some(key) => {
                        let event = WindowEvent::KeyEvent(Some(ch),
                                                          key,
                                                          KeyState::Pressed,
                                                          modifiers);
                        self.event_queue.borrow_mut().push(event);
                    }
                    None => {}
                }
            }
        }
        self.last_pressed_key.set(None);
    }

    fn toggle_keyboard_modifiers(&self, virtual_key_code: VirtualKeyCode) {
        match virtual_key_code {
            VirtualKeyCode::LControl => self.toggle_modifier(GlutinKeyModifiers::LEFT_CONTROL),
            VirtualKeyCode::RControl => self.toggle_modifier(GlutinKeyModifiers::RIGHT_CONTROL),
            VirtualKeyCode::LShift => self.toggle_modifier(GlutinKeyModifiers::LEFT_SHIFT),
            VirtualKeyCode::RShift => self.toggle_modifier(GlutinKeyModifiers::RIGHT_SHIFT),
            VirtualKeyCode::LAlt => self.toggle_modifier(GlutinKeyModifiers::LEFT_ALT),
            VirtualKeyCode::RAlt => self.toggle_modifier(GlutinKeyModifiers::RIGHT_ALT),
            VirtualKeyCode::LWin => self.toggle_modifier(GlutinKeyModifiers::LEFT_SUPER),
            VirtualKeyCode::RWin => self.toggle_modifier(GlutinKeyModifiers::RIGHT_SUPER),
            _ => {}
        }
    }

    fn handle_keyboard_input(&self, element_state: ElementState, virtual_key_code: VirtualKeyCode) {
        self.toggle_keyboard_modifiers(virtual_key_code);

        if let Ok(key) = keyutils::glutin_key_to_script_key(virtual_key_code) {
            let state = match element_state {
                ElementState::Pressed => KeyState::Pressed,
                ElementState::Released => KeyState::Released,
            };
            if element_state == ElementState::Pressed {
                if keyutils::is_printable(virtual_key_code) {
                    self.last_pressed_key.set(Some(key));
                }
            }
            let modifiers = keyutils::glutin_mods_to_script_mods(self.key_modifiers.get());
            self.event_queue.borrow_mut().push(WindowEvent::KeyEvent(None, key, state, modifiers));
        }
    }

    fn handle_window_event(&self, event: winit::Event) {
        match event {
            Event::WindowEvent {
                event: winit::WindowEvent::ReceivedCharacter(ch),
                ..
            } => self.handle_received_character(ch),
            Event::WindowEvent {
                event: winit::WindowEvent::KeyboardInput {
                    input: winit::KeyboardInput {
                        state, virtual_keycode: Some(virtual_keycode), ..
                    }, ..
                }, ..
            } => self.handle_keyboard_input(state, virtual_keycode),
            Event::WindowEvent {
                event: winit::WindowEvent::MouseInput {
                    state, button, ..
                }, ..
            } => {
                if button == MouseButton::Left || button == MouseButton::Right {
                    self.handle_mouse(button, state, self.mouse_pos.get());
                }
            },
            Event::WindowEvent {
                event: winit::WindowEvent::CursorMoved {
                    position: (x, y),
                    ..
                },
                ..
            } => {
                self.mouse_pos.set(TypedPoint2D::new(x as i32, y as i32));
                self.event_queue.borrow_mut().push(
                    WindowEvent::MouseWindowMoveEventClass(TypedPoint2D::new(x as f32, y as f32)));
            }
            Event::WindowEvent {
                event: winit::WindowEvent::MouseWheel { delta, phase, .. },
                ..
            } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(dx, dy) => (dx, dy * LINE_HEIGHT),
                    MouseScrollDelta::PixelDelta(dx, dy) => (dx, dy),
                };
                let scroll_location = ScrollLocation::Delta(TypedVector2D::new(dx, dy));
                let phase = glutin_phase_to_touch_event_type(phase);
                self.scroll_window(scroll_location, phase);
            },
            Event::WindowEvent {
                event: winit::WindowEvent::Touch(touch),
                ..
            } => {
                use script_traits::TouchId;

                let phase = glutin_phase_to_touch_event_type(touch.phase);
                let id = TouchId(touch.id as i32);
                let point = TypedPoint2D::new(touch.location.0 as f32, touch.location.1 as f32);
                self.event_queue.borrow_mut().push(WindowEvent::Touch(phase, id, point));
            }
            Event::WindowEvent {
                event: winit::WindowEvent::Refresh,
                ..
            } => self.event_queue.borrow_mut().push(WindowEvent::Refresh),
            Event::WindowEvent {
                event: winit::WindowEvent::Closed,
                ..
            } => {
                self.event_queue.borrow_mut().push(WindowEvent::Quit);
            }
            Event::WindowEvent {
                event: winit::WindowEvent::Resized(width, height),
                ..
            } => {
                // width and height are DevicePixel.
                // window.resize() takes DevicePixel.
                if let WindowKind::Window(ref window, _) = self.kind {
                    window.resize(width, height);
                }
                // window.set_inner_size() takes DeviceIndependentPixel.
                let new_size = TypedSize2D::new(width as f32, height as f32);
                let new_size = (new_size / self.hidpi_factor()).to_u32();
                if self.inner_size.get() != new_size {
                    self.inner_size.set(new_size);
                    self.event_queue.borrow_mut().push(WindowEvent::Resize);
                }
            }
            Event::Suspended(suspended) => {
                self.suspended.set(suspended);
                if !suspended {
                    self.event_queue.borrow_mut().push(WindowEvent::Idle);
                }
            }
            Event::Awakened => {
                self.event_queue.borrow_mut().push(WindowEvent::Idle);
            }
            _ => {}
        }
    }

    fn toggle_modifier(&self, modifier: GlutinKeyModifiers) {
        let mut modifiers = self.key_modifiers.get();
        modifiers.toggle(modifier);
        self.key_modifiers.set(modifiers);
    }

    /// Helper function to send a scroll event.
    fn scroll_window(&self, mut scroll_location: ScrollLocation, phase: TouchEventType) {
        // Scroll events snap to the major axis of movement, with vertical
        // preferred over horizontal.
        if let ScrollLocation::Delta(ref mut delta) = scroll_location {
            if delta.y.abs() >= delta.x.abs() {
                delta.x = 0.0;
            } else {
                delta.y = 0.0;
            }
        }

        let event = WindowEvent::Scroll(scroll_location, self.mouse_pos.get(), phase);
        self.event_queue.borrow_mut().push(event);
    }

    /// Helper function to handle a click
    fn handle_mouse(&self, button: winit::MouseButton,
                    action: winit::ElementState,
                    coords: TypedPoint2D<i32, DevicePixel>) {
        use script_traits::MouseButton;

        let max_pixel_dist = 10.0 * self.hidpi_factor().get();
        let event = match action {
            ElementState::Pressed => {
                self.mouse_down_point.set(coords);
                self.mouse_down_button.set(Some(button));
                MouseWindowEvent::MouseDown(MouseButton::Left, coords.to_f32())
            }
            ElementState::Released => {
                let mouse_up_event = MouseWindowEvent::MouseUp(MouseButton::Left, coords.to_f32());
                match self.mouse_down_button.get() {
                    None => mouse_up_event,
                    Some(but) if button == but => {
                        let pixel_dist = self.mouse_down_point.get() - coords;
                        let pixel_dist = ((pixel_dist.x * pixel_dist.x +
                                           pixel_dist.y * pixel_dist.y) as f32).sqrt();
                        if pixel_dist < max_pixel_dist {
                            self.event_queue.borrow_mut().push(WindowEvent::MouseWindowEventClass(mouse_up_event));
                            MouseWindowEvent::Click(MouseButton::Left, coords.to_f32())
                        } else {
                            mouse_up_event
                        }
                    },
                    Some(_) => mouse_up_event,
                }
            }
        };
        self.event_queue.borrow_mut().push(WindowEvent::MouseWindowEventClass(event));
    }

    pub fn get_events(&self) -> Vec<WindowEvent> {
        mem::replace(&mut *self.event_queue.borrow_mut(), Vec::new())
    }

    pub fn handle_servo_events(&self, events: Vec<EmbedderMsg>) {
        for event in events {
            match event {
                EmbedderMsg::Status(top_level_browsing_context, message) => self.status(top_level_browsing_context, message),
                EmbedderMsg::ChangePageTitle(top_level_browsing_context, title) => self.set_page_title(top_level_browsing_context, title),
                EmbedderMsg::MoveTo(top_level_browsing_context, point) => self.set_position(top_level_browsing_context, point),
                EmbedderMsg::ResizeTo(top_level_browsing_context, size) => self.set_inner_size(top_level_browsing_context, size),
                EmbedderMsg::AllowNavigation(top_level_browsing_context, url, response_chan) => self.allow_navigation(top_level_browsing_context, url, response_chan),
                EmbedderMsg::KeyEvent(top_level_browsing_context, ch, key, state, modified) => self.handle_key(top_level_browsing_context, ch, key, state, modified),
                EmbedderMsg::SetCursor(cursor) => self.set_cursor(cursor),
                EmbedderMsg::NewFavicon(top_level_browsing_context, url) => self.set_favicon(top_level_browsing_context, url),
                EmbedderMsg::HeadParsed(top_level_browsing_context, ) => self.head_parsed(top_level_browsing_context, ),
                EmbedderMsg::HistoryChanged(top_level_browsing_context, entries, current) => self.history_changed(top_level_browsing_context, entries, current),
                EmbedderMsg::SetFullscreenState(top_level_browsing_context, state) => self.set_fullscreen_state(top_level_browsing_context, state),
                EmbedderMsg::LoadStart(top_level_browsing_context) => self.load_start(top_level_browsing_context),
                EmbedderMsg::LoadComplete(top_level_browsing_context) => self.load_end(top_level_browsing_context),
                EmbedderMsg::Panic(top_level_browsing_context, reason, backtrace) => self.handle_panic(top_level_browsing_context, reason, backtrace),
            }
        }
    }

    fn is_animating(&self) -> bool {
        self.animation_state.get() == AnimationState::Animating && !self.suspended.get()
    }

    pub fn run<T>(&self, mut servo_callback: T) where T: FnMut() -> bool {
        match self.kind {
            WindowKind::Window(_, ref events_loop) => {
                let mut stop = false;
                loop {
                    if self.is_animating() {
                        // We block on compositing (servo_callback ends up calling swap_buffers)
                        events_loop.borrow_mut().poll_events(|e| {
                            self.handle_window_event(e);
                        });
                        stop = servo_callback();
                    } else {
                        // We block on glutin's event loop (window events)
                        events_loop.borrow_mut().run_forever(|e| {
                            self.handle_window_event(e);
                            if !self.event_queue.borrow().is_empty() {
                                if !self.suspended.get() {
                                    stop = servo_callback();
                                }
                            }
                            if stop || self.is_animating() {
                                winit::ControlFlow::Break
                            } else {
                                winit::ControlFlow::Continue
                            }
                        });
                    }
                    if stop {
                        break;
                    }
                }
            }
            WindowKind::Headless(..) => {
                loop {
                    // Sleep the main thread to avoid using 100% CPU
                    // This can be done better, see comments in #18777
                    if self.event_queue.borrow().is_empty() {
                        thread::sleep(time::Duration::from_millis(5));
                    }
                    let stop = servo_callback();
                    if stop {
                        break;
                    }
                }
            }
        }
    }

    #[cfg(not(target_os = "win"))]
    fn platform_handle_key(&self, key: Key, mods: constellation_msg::KeyModifiers, browser_id: BrowserId) {
        match (mods, key) {
            (CMD_OR_CONTROL, Key::LeftBracket) => {
                let event = WindowEvent::Navigation(browser_id, TraversalDirection::Back(1));
                self.event_queue.borrow_mut().push(event);
            }
            (CMD_OR_CONTROL, Key::RightBracket) => {
                let event = WindowEvent::Navigation(browser_id, TraversalDirection::Forward(1));
                self.event_queue.borrow_mut().push(event);
            }
            _ => {}
        }
    }

    #[cfg(target_os = "win")]
    fn platform_handle_key(&self, key: Key, mods: constellation_msg::KeyModifiers, browser_id: BrowserId) {
    }

    fn page_height(&self) -> f32 {
        let dpr = self.hidpi_factor();
        match self.kind {
            WindowKind::Window(ref window, _) => {
                let (_, height) = window.get_inner_size().expect("Failed to get window inner size.");
                height as f32 * dpr.get()
            },
            WindowKind::Headless(ref context) => {
                context.height as f32 * dpr.get()
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn hidpi_factor(&self) -> TypedScale<f32, DeviceIndependentPixel, DevicePixel> {
        match self.kind {
            WindowKind::Window(ref window, ..) => {
                TypedScale::new(window.hidpi_factor())
            }
            WindowKind::Headless(..) => {
                TypedScale::new(1.0)
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn hidpi_factor(&self) -> TypedScale<f32, DeviceIndependentPixel, DevicePixel> {
        let hdc = unsafe { user32::GetDC(::std::ptr::null_mut()) };
        let ppi = unsafe { gdi32::GetDeviceCaps(hdc, winapi::wingdi::LOGPIXELSY) };
        TypedScale::new(ppi as f32 / 96.0)
    }

    fn set_inner_size(&self, _: BrowserId, size: DeviceUintSize) {
        match self.kind {
            WindowKind::Window(ref window, ..) => {
                let size = size.to_f32() / self.hidpi_factor();
                window.set_inner_size(size.width as u32, size.height as u32)
            }
            WindowKind::Headless(..) => {}
        }
    }

    fn set_position(&self, _: BrowserId, point: DeviceIntPoint) {
        match self.kind {
            WindowKind::Window(ref window, ..) => {
                let point = point.to_f32() / self.hidpi_factor();
                window.set_position(point.x as i32, point.y as i32)
            }
            WindowKind::Headless(..) => {}
        }
    }

    fn set_fullscreen_state(&self, _: BrowserId, state: bool) {
        match self.kind {
            WindowKind::Window(ref window, ..) => {
                if self.fullscreen.get() != state {
                    window.set_fullscreen(None);
                }
            },
            WindowKind::Headless(..) => {}
        }
        self.fullscreen.set(state);
    }

    fn set_page_title(&self, _: BrowserId, title: Option<String>) {
        match self.kind {
            WindowKind::Window(ref window, ..) => {
                let fallback_title: String = if let Some(ref current_url) = *self.current_url.borrow() {
                    current_url.to_string()
                } else {
                    String::from("Untitled")
                };

                let title = match title {
                    Some(ref title) if title.len() > 0 => &**title,
                    _ => &fallback_title,
                };
                let title = format!("{} - Servo", title);
                window.set_title(&title);
            }
            WindowKind::Headless(..) => {}
        }
    }

    fn status(&self, _: BrowserId, _: Option<String>) {
    }

    fn load_start(&self, _: BrowserId) {
    }

    fn load_end(&self, _: BrowserId) {
        if opts::get().no_native_titlebar {
            match self.kind {
                WindowKind::Window(ref window, ..) => {
                    window.show();
                }
                WindowKind::Headless(..) => {}
            }
        }
    }

    fn history_changed(&self, _: BrowserId, history: Vec<LoadData>, current: usize) {
        *self.current_url.borrow_mut() = Some(history[current].url.clone());
    }

    fn head_parsed(&self, _: BrowserId) {
    }

    /// Has no effect on Android.
    fn set_cursor(&self, cursor: CursorKind) {
        match self.kind {
            WindowKind::Window(ref window, ..) => {
                use winit::MouseCursor;

                let glutin_cursor = match cursor {
                    CursorKind::Auto => MouseCursor::Default,
                    CursorKind::None => MouseCursor::NoneCursor,
                    CursorKind::Default => MouseCursor::Default,
                    CursorKind::Pointer => MouseCursor::Hand,
                    CursorKind::ContextMenu => MouseCursor::ContextMenu,
                    CursorKind::Help => MouseCursor::Help,
                    CursorKind::Progress => MouseCursor::Progress,
                    CursorKind::Wait => MouseCursor::Wait,
                    CursorKind::Cell => MouseCursor::Cell,
                    CursorKind::Crosshair => MouseCursor::Crosshair,
                    CursorKind::Text => MouseCursor::Text,
                    CursorKind::VerticalText => MouseCursor::VerticalText,
                    CursorKind::Alias => MouseCursor::Alias,
                    CursorKind::Copy => MouseCursor::Copy,
                    CursorKind::Move => MouseCursor::Move,
                    CursorKind::NoDrop => MouseCursor::NoDrop,
                    CursorKind::NotAllowed => MouseCursor::NotAllowed,
                    CursorKind::Grab => MouseCursor::Grab,
                    CursorKind::Grabbing => MouseCursor::Grabbing,
                    CursorKind::EResize => MouseCursor::EResize,
                    CursorKind::NResize => MouseCursor::NResize,
                    CursorKind::NeResize => MouseCursor::NeResize,
                    CursorKind::NwResize => MouseCursor::NwResize,
                    CursorKind::SResize => MouseCursor::SResize,
                    CursorKind::SeResize => MouseCursor::SeResize,
                    CursorKind::SwResize => MouseCursor::SwResize,
                    CursorKind::WResize => MouseCursor::WResize,
                    CursorKind::EwResize => MouseCursor::EwResize,
                    CursorKind::NsResize => MouseCursor::NsResize,
                    CursorKind::NeswResize => MouseCursor::NeswResize,
                    CursorKind::NwseResize => MouseCursor::NwseResize,
                    CursorKind::ColResize => MouseCursor::ColResize,
                    CursorKind::RowResize => MouseCursor::RowResize,
                    CursorKind::AllScroll => MouseCursor::AllScroll,
                    CursorKind::ZoomIn => MouseCursor::ZoomIn,
                    CursorKind::ZoomOut => MouseCursor::ZoomOut,
                };
                window.set_cursor(glutin_cursor);
            }
            WindowKind::Headless(..) => {}
        }
    }

    fn set_favicon(&self, _: BrowserId, _: ServoUrl) {
    }

    /// Helper function to handle keyboard events.
    fn handle_key(&self, _: Option<BrowserId>, ch: Option<char>, key: Key, state: KeyState, mods: constellation_msg::KeyModifiers) {
        if state == KeyState::Pressed {
            return;
        }
        let browser_id = match self.browser_id.get() {
            Some(id) => id,
            None => { unreachable!("Can't get keys without a browser"); }
        };
        match (mods, ch, key) {
            (_, Some('+'), _) => {
                if mods & !KeyModifiers::SHIFT == CMD_OR_CONTROL {
                    self.event_queue.borrow_mut().push(WindowEvent::Zoom(1.1));
                } else if mods & !KeyModifiers::SHIFT == CMD_OR_CONTROL | KeyModifiers::ALT {
                    self.event_queue.borrow_mut().push(WindowEvent::PinchZoom(1.1));
                }
            }
            (CMD_OR_CONTROL, Some('-'), _) => {
                self.event_queue.borrow_mut().push(WindowEvent::Zoom(1.0 / 1.1));
            }
            (_, Some('-'), _) if mods == CMD_OR_CONTROL | KeyModifiers::ALT => {
                self.event_queue.borrow_mut().push(WindowEvent::PinchZoom(1.0 / 1.1));
            }
            (CMD_OR_CONTROL, Some('0'), _) => {
                self.event_queue.borrow_mut().push(WindowEvent::ResetZoom);
            }

            (KeyModifiers::NONE, None, Key::NavigateForward) => {
                let event = WindowEvent::Navigation(browser_id, TraversalDirection::Forward(1));
                self.event_queue.borrow_mut().push(event);
            }
            (KeyModifiers::NONE, None, Key::NavigateBackward) => {
                let event = WindowEvent::Navigation(browser_id, TraversalDirection::Back(1));
                self.event_queue.borrow_mut().push(event);
            }

            (KeyModifiers::NONE, None, Key::Escape) => {
                if let Some(true) = PREFS.get("shell.builtin-key-shortcuts.enabled").as_boolean() {
                    self.event_queue.borrow_mut().push(WindowEvent::Quit);
                }
            }

            (CMD_OR_ALT, None, Key::Right) => {
                let event = WindowEvent::Navigation(browser_id, TraversalDirection::Forward(1));
                self.event_queue.borrow_mut().push(event);
            }
            (CMD_OR_ALT, None, Key::Left) => {
                let event = WindowEvent::Navigation(browser_id, TraversalDirection::Back(1));
                self.event_queue.borrow_mut().push(event);
            }

            (KeyModifiers::NONE, None, Key::PageDown) => {
               let scroll_location = ScrollLocation::Delta(TypedVector2D::new(0.0,
                                   -self.page_height() + 2.0 * LINE_HEIGHT));
                self.scroll_window(scroll_location,
                                   TouchEventType::Move);
            }
            (KeyModifiers::NONE, None, Key::PageUp) => {
                let scroll_location = ScrollLocation::Delta(TypedVector2D::new(0.0,
                                   self.page_height() - 2.0 * LINE_HEIGHT));
                self.scroll_window(scroll_location,
                                   TouchEventType::Move);
            }

            (KeyModifiers::NONE, None, Key::Home) => {
                self.scroll_window(ScrollLocation::Start, TouchEventType::Move);
            }

            (KeyModifiers::NONE, None, Key::End) => {
                self.scroll_window(ScrollLocation::End, TouchEventType::Move);
            }

            (KeyModifiers::NONE, None, Key::Up) => {
                self.scroll_window(ScrollLocation::Delta(TypedVector2D::new(0.0, 3.0 * LINE_HEIGHT)),
                                   TouchEventType::Move);
            }
            (KeyModifiers::NONE, None, Key::Down) => {
                self.scroll_window(ScrollLocation::Delta(TypedVector2D::new(0.0, -3.0 * LINE_HEIGHT)),
                                   TouchEventType::Move);
            }
            (KeyModifiers::NONE, None, Key::Left) => {
                self.scroll_window(ScrollLocation::Delta(TypedVector2D::new(LINE_HEIGHT, 0.0)), TouchEventType::Move);
            }
            (KeyModifiers::NONE, None, Key::Right) => {
                self.scroll_window(ScrollLocation::Delta(TypedVector2D::new(-LINE_HEIGHT, 0.0)), TouchEventType::Move);
            }
            (CMD_OR_CONTROL, Some('r'), _) => {
                if let Some(true) = PREFS.get("shell.builtin-key-shortcuts.enabled").as_boolean() {
                    self.event_queue.borrow_mut().push(WindowEvent::Reload(browser_id));
                }
            }
            (CMD_OR_CONTROL, Some('l'), _) => {
                if let Some(true) = PREFS.get("shell.builtin-key-shortcuts.enabled").as_boolean() {
                    let url: String = if let Some(ref url) = *self.current_url.borrow() {
                        url.to_string()
                    } else {
                        String::from("")
                    };
                    let title = "URL or search query";
                    if let Some(input) = tinyfiledialogs::input_box(title, title, &url) {
                        if let Some(url) = sanitize_url(&input) {
                            self.event_queue.borrow_mut().push(WindowEvent::LoadUrl(browser_id, url));
                        }
                    }
                }
            }
            (CMD_OR_CONTROL, Some('q'), _) => {
                if let Some(true) = PREFS.get("shell.builtin-key-shortcuts.enabled").as_boolean() {
                    self.event_queue.borrow_mut().push(WindowEvent::Quit);
                }
            }
            (_, Some('3'), _) => if mods ^ KeyModifiers::CONTROL == KeyModifiers::SHIFT {
                self.event_queue.borrow_mut().push(WindowEvent::CaptureWebRender);
            }
            (KeyModifiers::CONTROL, None, Key::F10) => {
                let event = WindowEvent::ToggleWebRenderDebug(WebRenderDebugOption::RenderTargetDebug);
                self.event_queue.borrow_mut().push(event);
            }
            (KeyModifiers::CONTROL, None, Key::F11) => {
                let event = WindowEvent::ToggleWebRenderDebug(WebRenderDebugOption::TextureCacheDebug);
                self.event_queue.borrow_mut().push(event);
            }
            (KeyModifiers::CONTROL, None, Key::F12) => {
                let event = WindowEvent::ToggleWebRenderDebug(WebRenderDebugOption::Profiler);
                self.event_queue.borrow_mut().push(event);
            }

            _ => {
                self.platform_handle_key(key, mods, browser_id);
            }
        }
    }

    fn allow_navigation(&self, _: BrowserId, _: ServoUrl, response_chan: IpcSender<bool>) {
        if let Err(e) = response_chan.send(true) {
            warn!("Failed to send allow_navigation() response: {}", e);
        };
    }

    fn handle_panic(&self, _: BrowserId, _reason: String, _backtrace: Option<String>) {
        // Nothing to do here yet. The crash has already been reported on the console.
    }
}

impl WindowMethods for Window {
    fn gl(&self) -> Rc<gl::Gl> {
        self.gl.clone()
    }

    fn get_coordinates(&self) -> EmbedderCoordinates {
        let dpr = self.hidpi_factor();
        match self.kind {
            WindowKind::Window(ref window, _) => {
                // TODO(ajeffrey): can this fail?
                let (width, height) = window.get_outer_size().expect("Failed to get window outer size.");
                let (x, y) = window.get_position().unwrap_or((0, 0));
                let win_size = (TypedSize2D::new(width as f32, height as f32) * dpr).to_u32();
                let win_origin = (TypedPoint2D::new(x as f32, y as f32) * dpr).to_i32();
                let screen = (self.screen_size.to_f32() * dpr).to_u32();

                let (width, height) = window.get_inner_size().expect("Failed to get window inner size.");
                let inner_size = (TypedSize2D::new(width as f32, height as f32) * dpr).to_u32();

                let viewport = DeviceUintRect::new(TypedPoint2D::zero(), inner_size);

                EmbedderCoordinates {
                    viewport: viewport,
                    framebuffer: inner_size,
                    window: (win_size, win_origin),
                    screen: screen,
                    // FIXME: Glutin doesn't have API for available size. Fallback to screen size
                    screen_avail: screen,
                    hidpi_factor: dpr,
                }
            },
            WindowKind::Headless(ref context) => {
                let size = (TypedSize2D::new(context.width, context.height).to_f32() * dpr).to_u32();
                EmbedderCoordinates {
                    viewport: DeviceUintRect::new(TypedPoint2D::zero(), size),
                    framebuffer: size,
                    window: (size, TypedPoint2D::zero()),
                    screen: size,
                    screen_avail: size,
                    hidpi_factor: dpr,
                }
            }
        }
    }

    fn present(&self) {
        match self.kind {
            WindowKind::Window(ref window, ..) => {
                if let Err(err) = window.swap_buffers() {
                    warn!("Failed to swap window buffers ({}).", err);
                }
            }
            WindowKind::Headless(..) => {}
        }
    }

    fn create_event_loop_waker(&self) -> Box<EventLoopWaker> {
        struct GlutinEventLoopWaker {
            proxy: Option<Arc<winit::EventsLoopProxy>>,
        }
        impl GlutinEventLoopWaker {
            fn new(window: &Window) -> GlutinEventLoopWaker {
                let proxy = match window.kind {
                    WindowKind::Window(_, ref events_loop) => {
                        Some(Arc::new(events_loop.borrow().create_proxy()))
                    },
                    WindowKind::Headless(..) => {
                        None
                    }
                };
                GlutinEventLoopWaker { proxy }
            }
        }
        impl EventLoopWaker for GlutinEventLoopWaker {
            fn wake(&self) {
                // kick the OS event loop awake.
                if let Some(ref proxy) = self.proxy {
                    if let Err(err) = proxy.wakeup() {
                        warn!("Failed to wake up event loop ({}).", err);
                    }
                }
            }
            fn clone(&self) -> Box<EventLoopWaker + Send> {
                Box::new(GlutinEventLoopWaker {
                    proxy: self.proxy.clone(),
                })
            }
        }

        Box::new(GlutinEventLoopWaker::new(&self))
    }

    fn set_animation_state(&self, state: AnimationState) {
        self.animation_state.set(state);
    }

    fn prepare_for_composite(&self, _width: Length<u32, DevicePixel>, _height: Length<u32, DevicePixel>) -> bool {
        true
    }

    fn supports_clipboard(&self) -> bool {
        true
    }
}

fn glutin_phase_to_touch_event_type(phase: TouchPhase) -> TouchEventType {
    match phase {
        TouchPhase::Started => TouchEventType::Down,
        TouchPhase::Moved => TouchEventType::Move,
        TouchPhase::Ended => TouchEventType::Up,
        TouchPhase::Cancelled => TouchEventType::Cancel,
    }
}

fn sanitize_url(request: &str) -> Option<ServoUrl> {
    let request = request.trim();
    ServoUrl::parse(&request).ok()
        .or_else(|| {
            if request.contains('/') || is_reg_domain(request) {
                ServoUrl::parse(&format!("http://{}", request)).ok()
            } else {
                None
            }
        }).or_else(|| {
            PREFS.get("shell.searchpage").as_string().and_then(|s: &str| {
                let url = s.replace("%s", request);
                ServoUrl::parse(&url).ok()
            })
        })
}
