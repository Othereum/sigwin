//! Offscreen test mode: no native window, no real input. The app renders with the same
//! software renderer into a buffer, a script feeds it input, and `shot` saves PNGs.
//!
//! `SIGWIN_HEADLESS="<steps>"` (or `@<file>`), steps separated by `;` or newlines:
//!   size W H         window size in logical px (default 1000 680)
//!   scale S          scale factor (default 1)
//!   wait MS          let timers, animations and the backend run
//!   move X Y         pointer move (hover)
//!   click X Y        left click (also `rclick`, `dblclick`)
//!   drag X1 Y1 X2 Y2 left-button drag
//!   wheel X Y N      N wheel notches, positive = up (older), negative = down
//!   type TEXT        key presses for each character
//!   key NAME         Enter Esc Tab Up Down Left Right Backspace Delete Home End PageUp PageDown,
//!                    optionally with modifiers: ctrl+shift+Enter
//!   shot PATH        save the window as PNG
//! With SIGWIN_FIXTURE (a backend without an account, `backend/fixture.rs`) there are scenario steps:
//!   fx OP            an op for the backend: `seed who=1 name=철수 n=100 unread=30`, `recv who=1 n=2`,
//!                    `phone-read who=1`; returns when it and the flush after it are done
//!   open N           open fixture contact N's thread, as a click on it does
//!   jump             the jump-to-latest button
//!   focus on|off     the window gains or loses the focus
//!   away on|off      the user goes away / comes back
//!   shown on|off     the window can / can't be seen (not covered); headless starts as can't
//!   settle           wait until the UI and the backend have stopped talking
//!   expect backend WHAT [who=N] OP VALUE   (`unread`, `read_upto`, `sent`, `toasts`, `window`)
//!   expect ui NAME OP VALUE                (`open-unread`, `near-bottom`, `bottom-gap`, `divider-top`, ...)
//!                    OP is = != < <= > >=; a failed expect is reported at the end and exits with 1
//! Theme: SIGWIN_HEADLESS_THEME=dark|light, default follows the system.
//! The loop exits after the last step. Requires SIGWIN_DATA (never touch the real account).

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Key, Platform, PointerEventButton, WindowAdapter, WindowEvent};
use slint::{LogicalPosition, PhysicalSize, PlatformError, Rgb8Pixel, SharedString};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(8);

#[derive(Debug)]
enum Step {
    Size(f32, f32),
    Scale(f32),
    Wait(u64),
    Move(f32, f32),
    Click(f32, f32, PointerEventButton, u32),
    Drag(f32, f32, f32, f32),
    Wheel(f32, f32, i32),
    Type(String),
    Key(Vec<String>),
    Shot(String),
    /// save every frame drawn from now on as `<dir>\NNNN.png`; empty: stop
    Record(String),
    /// later steps act on window 1 (main) or 2 (the latest other one, the image viewer)
    Window(u32),
    /// a scenario step, run by the driver (`gui/scenario.rs`)
    Scenario(String),
    /// wait until nothing moves between the UI and the backend
    Settle,
}

/// Scenario steps the driver runs.
const SCENARIO_STEPS: [&str; 9] = ["fx", "open", "jump", "focus", "away", "shown", "expect", "settle", "attach"];

/// What runs scenario steps: it knows the window and the backend.
pub struct Driver {
    pub exec: Box<dyn Fn(&str) -> anyhow::Result<()>>,
    /// a round trip to the backend, so what it queued has been sent
    pub sync: Box<dyn Fn()>,
    /// the UI is still seeking or holding back
    pub busy: Box<dyn Fn() -> bool>,
}

thread_local! {
    /// where the scripted pointer is: a headless window has no cursor of the system's to ask
    static POINTER: std::cell::Cell<Option<(f32, f32)>> = const { std::cell::Cell::new(None) };
}

/// The scripted pointer's place (headless runs only).
pub(crate) fn pointer() -> Option<LogicalPosition> {
    POINTER.get().map(|(x, y)| LogicalPosition::new(x, y))
}

thread_local! {
    static DRIVER: RefCell<Option<Driver>> = const { RefCell::new(None) };
}

pub fn set_driver(d: Driver) {
    DRIVER.with_borrow_mut(|s| *s = Some(d));
}

/// Commands and events crossing between the UI and the backend: `settle` waits for it to stop.
static ACTIVITY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FAILED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn bump() {
    ACTIVITY.fetch_add(1, Ordering::Relaxed);
}

/// A check failed: reported now, and the run exits with 1.
pub fn fail(msg: &str) {
    eprintln!("FAILED: {msg}");
    tracing::error!("scenario check failed: {msg}");
    FAILED.fetch_add(1, Ordering::Relaxed);
}

pub fn failed() -> bool {
    FAILED.load(Ordering::Relaxed) > 0
}

fn parse(script: &str) -> anyhow::Result<Vec<Step>> {
    let mut out = Vec::new();
    // a `#` line is a comment whole, even with a `;` in it
    let parts = script.lines().filter(|l| !l.trim_start().starts_with('#')).flat_map(|l| l.split(';'));
    for raw in parts {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        let nums = || -> anyhow::Result<Vec<f32>> {
            rest.split_whitespace().map(|n| n.parse::<f32>().map_err(|e| anyhow::anyhow!("{line}: {e}"))).collect()
        };
        let need = |n: usize| -> anyhow::Result<Vec<f32>> {
            let v = nums()?;
            anyhow::ensure!(v.len() == n, "{line}: expected {n} numbers");
            Ok(v)
        };
        out.push(match cmd {
            "size" => {
                let v = need(2)?;
                Step::Size(v[0], v[1])
            }
            "scale" => Step::Scale(need(1)?[0]),
            "wait" => Step::Wait(need(1)?[0] as u64),
            "move" => {
                let v = need(2)?;
                Step::Move(v[0], v[1])
            }
            "click" | "rclick" | "dblclick" => {
                let v = need(2)?;
                let b = if cmd == "rclick" { PointerEventButton::Right } else { PointerEventButton::Left };
                Step::Click(v[0], v[1], b, if cmd == "dblclick" { 2 } else { 1 })
            }
            "drag" => {
                let v = need(4)?;
                Step::Drag(v[0], v[1], v[2], v[3])
            }
            "wheel" => {
                let v = need(3)?;
                Step::Wheel(v[0], v[1], v[2] as i32)
            }
            "type" => Step::Type(rest.to_string()),
            "key" => Step::Key(rest.split('+').map(|s| s.trim().to_string()).collect()),
            "shot" => Step::Shot(rest.to_string()),
            "record" => Step::Record(rest.to_string()),
            "window" => Step::Window(rest.trim().parse()?),
            "settle" => Step::Settle,
            c if SCENARIO_STEPS.contains(&c) => Step::Scenario(line.to_string()),
            _ => anyhow::bail!("unknown step: {line}"),
        });
    }
    Ok(out)
}

fn key_text(name: &str) -> anyhow::Result<SharedString> {
    let k = match name.to_ascii_lowercase().as_str() {
        "enter" | "return" => Key::Return,
        "esc" | "escape" => Key::Escape,
        "tab" => Key::Tab,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "backspace" => Key::Backspace,
        "delete" => Key::Delete,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "ctrl" | "control" => Key::Control,
        "shift" => Key::Shift,
        "alt" => Key::Alt,
        _ => {
            let mut c = name.chars();
            return match (c.next(), c.next()) {
                (Some(ch), None) => Ok(ch.to_string().into()),
                _ => Err(anyhow::anyhow!("unknown key: {name}")),
            };
        }
    };
    Ok(k.into())
}

type Job = Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct Queue {
    jobs: Mutex<VecDeque<Job>>,
    quit: AtomicBool,
}

struct Proxy(Arc<Queue>);

impl slint::platform::EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
        self.0.quit.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn invoke_from_event_loop(&self, event: Job) -> Result<(), slint::EventLoopError> {
        self.0.jobs.lock().unwrap().push_back(event);
        Ok(())
    }
}

struct Headless {
    steps: RefCell<VecDeque<Step>>,
    window: RefCell<Option<Rc<MinimalSoftwareWindow>>>,
    queue: Arc<Queue>,
}

thread_local! {
    /// The latest window after the main one.
    static SECOND: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

static ACTIVE: AtomicBool = AtomicBool::new(false);

/// True when running offscreen (no native window to hook).
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Theme for the offscreen window: `SIGWIN_HEADLESS_THEME=dark|light`, else the system's app theme.
pub fn dark() -> bool {
    match std::env::var("SIGWIN_HEADLESS_THEME").as_deref() {
        Ok("dark") => true,
        Ok("light") => false,
        _ => system_dark(),
    }
}

#[cfg(windows)]
fn system_dark() -> bool {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let w = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let key = w(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let name = w("AppsUseLightTheme");
    let mut v: u32 = 1;
    let mut len = 4u32;
    // SAFETY: reads one DWORD into a local of matching size
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), &mut v as *mut u32 as *mut _, &mut len)
    };
    r == 0 && v == 0
}

#[cfg(not(windows))]
fn system_dark() -> bool {
    false
}

/// Installs the offscreen platform if `SIGWIN_HEADLESS` is set. Call before creating the UI.
pub fn install_if_requested() -> anyhow::Result<()> {
    let Ok(spec) = std::env::var("SIGWIN_HEADLESS") else { return Ok(()) };
    anyhow::ensure!(
        std::env::var_os("SIGWIN_DATA").is_some(),
        "SIGWIN_HEADLESS requires SIGWIN_DATA (a scratch dir)"
    );
    let script = match spec.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path)?,
        None => spec,
    };
    let mut steps: VecDeque<Step> = parse(&script)?.into();
    // initial geometry: leading size/scale steps apply before the first frame
    steps.push_front(Step::Scale(1.0));
    steps.push_front(Step::Size(1000.0, 680.0));
    let p = Headless { steps: RefCell::new(steps), window: RefCell::new(None), queue: Arc::default() };
    slint::platform::set_platform(Box::new(p)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    ACTIVE.store(true, Ordering::Relaxed);
    // the real idle and lock state of the PC must not decide a run
    crate::away::force(Some(false));
    Ok(())
}

impl Platform for Headless {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let w = MinimalSoftwareWindow::new(RepaintBufferType::ReusedBuffer);
        let mut main = self.window.borrow_mut();
        if main.is_none() {
            *main = Some(w.clone());
        } else {
            SECOND.with_borrow_mut(|s| *s = Some(w.clone()));
        }
        Ok(w)
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn slint::platform::EventLoopProxy>> {
        Some(Box::new(Proxy(self.queue.clone())))
    }

    fn run_event_loop(&self) -> Result<(), PlatformError> {
        let w = self.window.borrow().clone().ok_or_else(|| PlatformError::Other("no window".into()))?;
        let mut st = State { main: w.clone(), w, buf: Vec::new(), size: PhysicalSize::new(0, 0), scale: 1.0, logical: (1000.0, 680.0), record: None };
        loop {
            let Some(step) = self.steps.borrow_mut().pop_front() else { break };
            tracing::debug!(?step, "headless");
            if let Err(e) = st.run(step, || self.pump()) {
                tracing::error!("headless: {e:#}");
                return Err(PlatformError::Other(e.to_string()));
            }
            if self.queue.quit.load(Ordering::SeqCst) {
                break;
            }
        }
        Ok(())
    }
}

impl Headless {
    /// One event-loop turn: queued jobs from other threads, timers, then a frame.
    fn pump(&self) {
        loop {
            let job = self.queue.jobs.lock().unwrap().pop_front();
            let Some(job) = job else { break };
            job();
        }
        slint::platform::update_timers_and_animations();
    }
}

struct State {
    main: Rc<MinimalSoftwareWindow>,
    /// the window steps act on
    w: Rc<MinimalSoftwareWindow>,
    buf: Vec<Rgb8Pixel>,
    size: PhysicalSize,
    scale: f32,
    logical: (f32, f32),
    /// `record`: target dir and frames saved so far
    record: Option<(String, u32)>,
}

impl State {
    fn frame(&mut self) {
        let drawn = self.draw();
        if let (true, Some((dir, n))) = (drawn, self.record.as_mut()) {
            let path = format!("{dir}\\{n:04}.png");
            *n += 1;
            if let Err(e) = self.save(&path) {
                tracing::error!("headless record: {e:#}");
            }
        }
    }

    /// Draws if something changed; true when it did.
    fn draw(&mut self) -> bool {
        let size = self.w.window().size();
        if size != self.size {
            self.size = size;
            self.buf = vec![Rgb8Pixel::default(); (size.width * size.height) as usize];
            self.w.window().request_redraw();
        }
        let width = size.width as usize;
        let buf = &mut self.buf;
        self.w.draw_if_needed(|r| {
            r.render(buf, width);
        })
    }

    /// Runs the loop for `d` (at least one turn).
    fn idle(&mut self, d: Duration, pump: &impl Fn()) {
        let end = Instant::now() + d;
        loop {
            pump();
            self.frame();
            if Instant::now() >= end {
                break;
            }
            std::thread::sleep(FRAME.min(end.saturating_duration_since(Instant::now())));
        }
    }

    fn send(&mut self, ev: WindowEvent, pump: &impl Fn()) {
        if let WindowEvent::PointerMoved { position } | WindowEvent::PointerPressed { position, .. } | WindowEvent::PointerReleased { position, .. } = &ev {
            POINTER.set(Some((position.x, position.y)));
        }
        self.w.window().dispatch_event(ev);
        self.idle(Duration::ZERO, pump);
    }

    fn resize(&mut self) {
        let (lw, lh) = self.logical;
        self.w.window().dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: self.scale });
        self.w.set_size(PhysicalSize::new((lw * self.scale).round() as u32, (lh * self.scale).round() as u32));
        crate::types::set_ui_scale(self.scale);
    }

    fn run(&mut self, step: Step, pump: impl Fn()) -> anyhow::Result<()> {
        let pump = &pump;
        let pos = |x: f32, y: f32| LogicalPosition::new(x, y);
        match step {
            Step::Size(w, h) => {
                self.logical = (w, h);
                self.resize();
                self.idle(Duration::ZERO, pump);
            }
            Step::Scale(s) => {
                self.scale = s;
                self.resize();
                self.idle(Duration::ZERO, pump);
            }
            Step::Wait(ms) => self.idle(Duration::from_millis(ms), pump),
            Step::Move(x, y) => self.send(WindowEvent::PointerMoved { position: pos(x, y) }, pump),
            Step::Click(x, y, button, n) => {
                self.send(WindowEvent::PointerMoved { position: pos(x, y) }, pump);
                for _ in 0..n {
                    self.send(WindowEvent::PointerPressed { position: pos(x, y), button }, pump);
                    self.send(WindowEvent::PointerReleased { position: pos(x, y), button }, pump);
                }
                self.idle(Duration::from_millis(30), pump);
            }
            Step::Drag(x1, y1, x2, y2) => {
                let b = PointerEventButton::Left;
                self.send(WindowEvent::PointerMoved { position: pos(x1, y1) }, pump);
                self.send(WindowEvent::PointerPressed { position: pos(x1, y1), button: b }, pump);
                for i in 1..=10 {
                    let t = i as f32 / 10.0;
                    let p = pos(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t);
                    self.send(WindowEvent::PointerMoved { position: p }, pump);
                    self.idle(Duration::from_millis(16), pump);
                }
                self.send(WindowEvent::PointerReleased { position: pos(x2, y2), button: b }, pump);
            }
            Step::Wheel(x, y, n) => {
                self.send(WindowEvent::PointerMoved { position: pos(x, y) }, pump);
                for _ in 0..n.unsigned_abs() {
                    // winit maps one notch to 60 logical px
                    let delta_y = if n > 0 { 60.0 } else { -60.0 };
                    self.send(WindowEvent::PointerScrolled { position: pos(x, y), delta_x: 0.0, delta_y }, pump);
                    self.idle(Duration::from_millis(30), pump);
                }
            }
            Step::Type(text) => {
                for ch in text.chars() {
                    let t: SharedString = ch.to_string().into();
                    self.send(WindowEvent::KeyPressed { text: t.clone() }, pump);
                    self.send(WindowEvent::KeyReleased { text: t }, pump);
                }
            }
            Step::Key(parts) => {
                let keys = parts.iter().map(|p| key_text(p)).collect::<anyhow::Result<Vec<_>>>()?;
                for k in &keys {
                    self.send(WindowEvent::KeyPressed { text: k.clone() }, pump);
                }
                for k in keys.iter().rev() {
                    self.send(WindowEvent::KeyReleased { text: k.clone() }, pump);
                }
            }
            Step::Shot(path) => {
                self.w.window().request_redraw();
                self.idle(Duration::ZERO, pump);
                self.save(&path)?;
                tracing::info!(path, "headless shot");
            }
            Step::Window(n) => {
                self.w = match n {
                    1 => self.main.clone(),
                    _ => SECOND.with_borrow(|s| s.clone()).ok_or_else(|| anyhow::anyhow!("no second window"))?,
                };
                self.size = PhysicalSize::new(0, 0);
                self.idle(Duration::ZERO, pump);
            }
            Step::Scenario(line) => {
                DRIVER.with_borrow(|d| match d {
                    Some(d) => (d.exec)(&line),
                    None => Err(anyhow::anyhow!("{line}: scenario steps need SIGWIN_FIXTURE")),
                })?;
                self.idle(Duration::ZERO, pump);
            }
            Step::Settle => {
                let mut quiet = 0;
                let started = Instant::now();
                while quiet < 3 {
                    anyhow::ensure!(started.elapsed() < Duration::from_secs(20), "settle: the UI and the backend never stopped");
                    let before = ACTIVITY.load(Ordering::Relaxed);
                    DRIVER.with_borrow(|d| d.as_ref().map(|d| (d.sync)()));
                    self.idle(Duration::from_millis(100), pump);
                    let busy = DRIVER.with_borrow(|d| d.as_ref().is_some_and(|d| (d.busy)()));
                    quiet = if ACTIVITY.load(Ordering::Relaxed) == before && !busy { quiet + 1 } else { 0 };
                }
            }
            Step::Record(dir) => {
                if !dir.is_empty() {
                    std::fs::create_dir_all(&dir)?;
                }
                self.record = (!dir.is_empty()).then_some((dir, 0));
            }
        }
        Ok(())
    }

    fn save(&self, path: &str) -> anyhow::Result<()> {
        let f = std::io::BufWriter::new(std::fs::File::create(path)?);
        let mut enc = png::Encoder::new(f, self.size.width, self.size.height);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header()?;
        let bytes: Vec<u8> = self.buf.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
        w.write_image_data(&bytes)?;
        Ok(())
    }
}
