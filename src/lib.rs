//! Synthé AAudio 0.2 : polyphonique, notes tenues, enveloppe réglable.
//!
//! - Un doigt posé = une note tenue ; levé = relâche. Plusieurs doigts = accord.
//! - Glisser d'une touche à l'autre change de note sans lever le doigt.
//! - Curseurs ATTAQUE et RELACHE en haut à droite.
//!
//! Trois threads :
//! - principal : événements tactiles → file sans verrou vers l'audio (aucun dessin ici,
//!   pour que la note parte au plus vite) ;
//! - rendu : redessine l'écran quand quelque chose a changé, et toutes les 250 ms
//!   ajuste le tampon et mesure la latence ;
//! - audio : géré par AAudio, appelle le synthé.

#![allow(non_camel_case_types)]

mod audio;
mod font;
mod synth;
mod ui;

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// API NativeActivity (cf. <android/native_activity.h>)
// ---------------------------------------------------------------------------

type ANativeWindow = c_void;
type AInputQueue = c_void;
type AInputEvent = c_void;
type ALooper = c_void;

#[repr(C)]
pub struct ANativeActivity {
    callbacks: *mut ANativeActivityCallbacks,
    vm: *mut c_void,
    env: *mut c_void,
    clazz: *mut c_void,
    internal_data_path: *const i8,
    external_data_path: *const i8,
    sdk_version: i32,
    instance: *mut c_void,
    asset_manager: *mut c_void,
    obb_path: *const i8,
}

type Cb = Option<unsafe extern "C" fn(*mut ANativeActivity)>;
type CbWindow = Option<unsafe extern "C" fn(*mut ANativeActivity, *mut ANativeWindow)>;
type CbQueue = Option<unsafe extern "C" fn(*mut ANativeActivity, *mut AInputQueue)>;

#[repr(C)]
pub struct ANativeActivityCallbacks {
    on_start: Cb,
    on_resume: Cb,
    on_save_instance_state: Option<unsafe extern "C" fn(*mut ANativeActivity, *mut usize) -> *mut c_void>,
    on_pause: Cb,
    on_stop: Cb,
    on_destroy: Cb,
    on_window_focus_changed: Option<unsafe extern "C" fn(*mut ANativeActivity, i32)>,
    on_native_window_created: CbWindow,
    on_native_window_resized: CbWindow,
    on_native_window_redraw_needed: CbWindow,
    on_native_window_destroyed: CbWindow,
    on_input_queue_created: CbQueue,
    on_input_queue_destroyed: CbQueue,
    on_content_rect_changed: Option<unsafe extern "C" fn(*mut ANativeActivity, *const c_void)>,
    on_configuration_changed: Cb,
    on_low_memory: Cb,
}

#[repr(C)]
struct ANativeWindow_Buffer {
    width: i32,
    height: i32,
    stride: i32,
    format: i32,
    bits: *mut c_void,
    reserved: [u32; 6],
}

const WINDOW_FORMAT_RGBA_8888: i32 = 1;
const AINPUT_EVENT_TYPE_MOTION: i32 = 2;
const ACTION_DOWN: i32 = 0;
const ACTION_UP: i32 = 1;
const ACTION_MOVE: i32 = 2;
const ACTION_CANCEL: i32 = 3;
const ACTION_POINTER_DOWN: i32 = 5;
const ACTION_POINTER_UP: i32 = 6;
const ACTION_MASK: i32 = 0xff;
const ACTION_POINTER_INDEX_MASK: i32 = 0xff00;
const ACTION_POINTER_INDEX_SHIFT: i32 = 8;

type ALooperCallback = unsafe extern "C" fn(fd: i32, events: i32, data: *mut c_void) -> i32;

#[link(name = "android")]
extern "C" {
    fn ANativeWindow_setBuffersGeometry(w: *mut ANativeWindow, width: i32, height: i32, format: i32) -> i32;
    fn ANativeWindow_lock(w: *mut ANativeWindow, buf: *mut ANativeWindow_Buffer, dirty: *mut c_void) -> i32;
    fn ANativeWindow_unlockAndPost(w: *mut ANativeWindow) -> i32;
    fn ANativeWindow_getWidth(w: *mut ANativeWindow) -> i32;
    fn ANativeWindow_getHeight(w: *mut ANativeWindow) -> i32;

    fn ALooper_forThread() -> *mut ALooper;
    fn AInputQueue_attachLooper(q: *mut AInputQueue, l: *mut ALooper, ident: i32, cb: Option<ALooperCallback>, data: *mut c_void);
    fn AInputQueue_detachLooper(q: *mut AInputQueue);
    fn AInputQueue_getEvent(q: *mut AInputQueue, out: *mut *mut AInputEvent) -> i32;
    fn AInputQueue_preDispatchEvent(q: *mut AInputQueue, e: *mut AInputEvent) -> i32;
    fn AInputQueue_finishEvent(q: *mut AInputQueue, e: *mut AInputEvent, handled: i32);
    fn AInputEvent_getType(e: *const AInputEvent) -> i32;
    fn AMotionEvent_getAction(e: *const AInputEvent) -> i32;
    fn AMotionEvent_getPointerCount(e: *const AInputEvent) -> usize;
    fn AMotionEvent_getPointerId(e: *const AInputEvent, pointer_index: usize) -> i32;
    fn AMotionEvent_getX(e: *const AInputEvent, pointer_index: usize) -> f32;
    fn AMotionEvent_getY(e: *const AInputEvent, pointer_index: usize) -> f32;
}

// ---------------------------------------------------------------------------
// État des doigts (thread principal) — lu par le rendu sous un verrou bref
// ---------------------------------------------------------------------------

const MAX_POINTERS: usize = 32;

#[derive(Clone, Copy, PartialEq)]
enum Finger {
    Up,
    Key(usize),
    Slider(usize),
}

struct Input {
    width: usize,
    height: usize,
    fingers: [Finger; MAX_POINTERS],
    sliders: [f32; ui::SLIDERS],
}

static INPUT: Mutex<Input> = Mutex::new(Input {
    width: 0,
    height: 0,
    fingers: [Finger::Up; MAX_POINTERS],
    sliders: [0.01, 0.4], // mêmes valeurs par défaut que le synthé
});

/// Quelque chose a changé à l'écran : le thread de rendu redessinera.
static DIRTY: AtomicBool = AtomicBool::new(true);

impl Input {
    fn view(&self) -> ui::View {
        let mut held = [false; ui::KEYS];
        let mut held_count = 0;
        for f in &self.fingers {
            if let Finger::Key(k) = f {
                held[*k] = true;
                held_count += 1;
            }
        }
        ui::View { held, held_count, sliders: self.sliders }
    }

    fn set_slider(&mut self, i: usize, t: f32) {
        let v = ui::slider_value(i, t);
        self.sliders[i] = v;
        match i {
            0 => synth::set_attack(v),
            _ => synth::set_release(v),
        }
        DIRTY.store(true, Ordering::Relaxed);
    }

    /// Doigt posé ou déplacé.
    fn finger(&mut self, id: usize, x: f32, y: f32, is_down: bool) {
        if id >= MAX_POINTERS {
            return;
        }
        let layout = ui::layout(self.width, self.height);
        let hit = layout.hit(x, y);
        let prev = self.fingers[id];
        let next = match (prev, hit) {
            // Un doigt qui a saisi un curseur le garde, même s'il sort de sa zone.
            (Finger::Slider(i), _) if !is_down => {
                let track = layout.slider_track[i];
                self.set_slider(i, ((x - track.x as f32) / track.w.max(1) as f32).clamp(0.0, 1.0));
                return;
            }
            (_, ui::Hit::Slider(i, t)) if is_down => {
                self.set_slider(i, t);
                Finger::Slider(i)
            }
            (_, ui::Hit::Key(k)) => Finger::Key(k),
            _ => Finger::Up,
        };
        if next == prev {
            return;
        }
        match next {
            // Nouvelle touche (posée ou atteinte en glissant) : le synthé relâche
            // l'éventuelle note précédente de ce doigt et attaque la nouvelle.
            Finger::Key(k) => synth::note_on(id as u32, ui::key_freq(k)),
            _ => {
                if matches!(prev, Finger::Key(_)) {
                    synth::note_off(id as u32);
                }
            }
        }
        self.fingers[id] = next;
        DIRTY.store(true, Ordering::Relaxed);
    }

    fn lift(&mut self, id: usize) {
        if id >= MAX_POINTERS {
            return;
        }
        if matches!(self.fingers[id], Finger::Key(_)) {
            synth::note_off(id as u32);
        }
        self.fingers[id] = Finger::Up;
        DIRTY.store(true, Ordering::Relaxed);
    }

    fn lift_all(&mut self) {
        synth::all_off();
        self.fingers = [Finger::Up; MAX_POINTERS];
        DIRTY.store(true, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Fenêtre et thread de rendu
// ---------------------------------------------------------------------------

struct Window(*mut ANativeWindow);
unsafe impl Send for Window {}

/// Le verrou garantit qu'on ne dessine jamais dans une fenêtre en cours de destruction.
static WINDOW: Mutex<Window> = Mutex::new(Window(null_mut()));

fn draw(stats: &audio::Stats, view: &ui::View) {
    let win = WINDOW.lock().unwrap();
    if win.0.is_null() {
        return;
    }
    unsafe {
        let mut buf: ANativeWindow_Buffer = std::mem::zeroed();
        if ANativeWindow_lock(win.0, &mut buf, null_mut()) != 0 {
            return;
        }
        let len = buf.stride as usize * buf.height as usize;
        let px = std::slice::from_raw_parts_mut(buf.bits as *mut u32, len);
        let mut cv = ui::Canvas { px, w: buf.width as usize, h: buf.height as usize, stride: buf.stride as usize };
        ui::render(&mut cv, stats, view);
        ANativeWindow_unlockAndPost(win.0);
    }
}

fn spawn_render_thread() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| {
            let mut stats = audio::Stats::default();
            let mut last_tick = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(16));
                if last_tick.elapsed() >= Duration::from_millis(250) {
                    last_tick = Instant::now();
                    stats = audio::tick();
                    DIRTY.store(true, Ordering::Relaxed);
                }
                if DIRTY.swap(false, Ordering::AcqRel) {
                    let view = INPUT.lock().unwrap().view(); // copie rapide, puis on relâche
                    draw(&stats, &view);
                }
            }
        });
    });
}

// ---------------------------------------------------------------------------
// Callbacks de l'activité
// ---------------------------------------------------------------------------

unsafe extern "C" fn on_resume(_: *mut ANativeActivity) {
    audio::start();
}

unsafe extern "C" fn on_pause(_: *mut ANativeActivity) {
    INPUT.lock().unwrap().lift_all();
    audio::stop();
}

unsafe fn update_size(window: *mut ANativeWindow) {
    let mut i = INPUT.lock().unwrap();
    i.width = ANativeWindow_getWidth(window).max(0) as usize;
    i.height = ANativeWindow_getHeight(window).max(0) as usize;
}

unsafe extern "C" fn on_window_created(_: *mut ANativeActivity, window: *mut ANativeWindow) {
    ANativeWindow_setBuffersGeometry(window, 0, 0, WINDOW_FORMAT_RGBA_8888);
    update_size(window);
    WINDOW.lock().unwrap().0 = window;
    DIRTY.store(true, Ordering::Relaxed);
}

unsafe extern "C" fn on_window_changed(_: *mut ANativeActivity, window: *mut ANativeWindow) {
    update_size(window);
    DIRTY.store(true, Ordering::Relaxed);
}

unsafe extern "C" fn on_window_destroyed(_: *mut ANativeActivity, _: *mut ANativeWindow) {
    WINDOW.lock().unwrap().0 = null_mut();
}

/// Lecture des événements tactiles. Chaque événement doit être « fini »,
/// sinon Android considère l'app comme figée (ANR).
unsafe extern "C" fn on_input(_fd: i32, _events: i32, data: *mut c_void) -> i32 {
    let queue = data as *mut AInputQueue;
    let mut event: *mut AInputEvent = null_mut();
    while AInputQueue_getEvent(queue, &mut event) >= 0 {
        if AInputQueue_preDispatchEvent(queue, event) != 0 {
            continue;
        }
        let mut handled = 0;
        if AInputEvent_getType(event) == AINPUT_EVENT_TYPE_MOTION {
            handled = 1;
            let action = AMotionEvent_getAction(event);
            let idx = ((action & ACTION_POINTER_INDEX_MASK) >> ACTION_POINTER_INDEX_SHIFT) as usize;
            let id = |i: usize| AMotionEvent_getPointerId(event, i).max(0) as usize;
            let mut input = INPUT.lock().unwrap();
            match action & ACTION_MASK {
                ACTION_DOWN | ACTION_POINTER_DOWN => {
                    input.finger(id(idx), AMotionEvent_getX(event, idx), AMotionEvent_getY(event, idx), true)
                }
                ACTION_MOVE => {
                    for i in 0..AMotionEvent_getPointerCount(event) {
                        input.finger(id(i), AMotionEvent_getX(event, i), AMotionEvent_getY(event, i), false);
                    }
                }
                ACTION_UP | ACTION_POINTER_UP => input.lift(id(idx)),
                ACTION_CANCEL => input.lift_all(),
                _ => handled = 0,
            }
        }
        AInputQueue_finishEvent(queue, event, handled);
    }
    1
}

unsafe extern "C" fn on_queue_created(_: *mut ANativeActivity, queue: *mut AInputQueue) {
    AInputQueue_attachLooper(queue, ALooper_forThread(), 1, Some(on_input), queue);
}

unsafe extern "C" fn on_queue_destroyed(_: *mut ANativeActivity, queue: *mut AInputQueue) {
    AInputQueue_detachLooper(queue);
}

/// Point d'entrée appelé par `android.app.NativeActivity`.
///
/// # Safety
/// Appelé uniquement par Android, avec une activité valide.
#[no_mangle]
pub unsafe extern "C" fn ANativeActivity_onCreate(activity: *mut ANativeActivity, _: *mut c_void, _: usize) {
    let cb = &mut *(*activity).callbacks;
    cb.on_resume = Some(on_resume);
    cb.on_pause = Some(on_pause);
    cb.on_native_window_created = Some(on_window_created);
    cb.on_native_window_redraw_needed = Some(on_window_changed);
    cb.on_native_window_resized = Some(on_window_changed);
    cb.on_native_window_destroyed = Some(on_window_destroyed);
    cb.on_input_queue_created = Some(on_queue_created);
    cb.on_input_queue_destroyed = Some(on_queue_destroyed);
    spawn_render_thread();
}
