//! Moteur audio basse latence sur AAudio.
//!
//! `libaaudio.so` est chargée à l'exécution (dlopen) : aucune bibliothèque à lier
//! au moment de la compilation, et un message clair si AAudio est absent.
//!
//! Règle d'or : le callback audio n'alloue rien, ne prend aucun verrou et ne fait
//! aucun appel système. Il ne lit que des atomiques et son propre état.

use std::ffi::{c_char, c_long, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::synth::Poly;
use std::sync::{Mutex, OnceLock};

type Builder = c_void;
type Stream = c_void;
type DataCb = unsafe extern "C" fn(*mut Stream, *mut c_void, *mut c_void, i32) -> i32;
type ErrorCb = unsafe extern "C" fn(*mut Stream, *mut c_void, i32);

const AAUDIO_OK: i32 = 0;
const AAUDIO_FORMAT_PCM_FLOAT: i32 = 2;
const AAUDIO_SHARING_MODE_EXCLUSIVE: i32 = 0;
const AAUDIO_PERFORMANCE_MODE_LOW_LATENCY: i32 = 12;
const AAUDIO_CALLBACK_RESULT_CONTINUE: i32 = 0;
const CLOCK_MONOTONIC: i32 = 1;

// ---------------------------------------------------------------------------
// Chargement dynamique de l'API
// ---------------------------------------------------------------------------

extern "C" {
    fn dlopen(name: *const c_char, flags: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn clock_gettime(clock: i32, ts: *mut Timespec) -> i32;
}

#[repr(C)]
struct Timespec {
    tv_sec: c_long,
    tv_nsec: c_long,
}

#[cfg(target_pointer_width = "64")]
const RTLD_NOW: i32 = 2;
#[cfg(not(target_pointer_width = "64"))]
const RTLD_NOW: i32 = 0;

macro_rules! aaudio_api {
    ($($name:ident : fn($($arg:ty),*) -> $ret:ty;)*) => {
        #[allow(non_snake_case)]
        struct Api { $($name: unsafe extern "C" fn($($arg),*) -> $ret,)* }

        impl Api {
            unsafe fn load() -> Result<Api, String> {
                let lib = dlopen(c"libaaudio.so".as_ptr(), RTLD_NOW);
                if lib.is_null() {
                    return Err("libaaudio.so introuvable (Android 8+ requis)".into());
                }
                Ok(Api { $($name: {
                    let p = dlsym(lib, concat!(stringify!($name), "\0").as_ptr() as *const c_char);
                    if p.is_null() {
                        return Err(format!("symbole absent : {}", stringify!($name)));
                    }
                    std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) -> $ret>(p)
                },)* })
            }
        }
    };
}

aaudio_api! {
    AAudio_createStreamBuilder: fn(*mut *mut Builder) -> i32;
    AAudio_convertResultToText: fn(i32) -> *const c_char;
    AAudioStreamBuilder_setPerformanceMode: fn(*mut Builder, i32) -> ();
    AAudioStreamBuilder_setSharingMode: fn(*mut Builder, i32) -> ();
    AAudioStreamBuilder_setFormat: fn(*mut Builder, i32) -> ();
    AAudioStreamBuilder_setChannelCount: fn(*mut Builder, i32) -> ();
    AAudioStreamBuilder_setDataCallback: fn(*mut Builder, Option<DataCb>, *mut c_void) -> ();
    AAudioStreamBuilder_setErrorCallback: fn(*mut Builder, Option<ErrorCb>, *mut c_void) -> ();
    AAudioStreamBuilder_openStream: fn(*mut Builder, *mut *mut Stream) -> i32;
    AAudioStreamBuilder_delete: fn(*mut Builder) -> i32;
    AAudioStream_requestStart: fn(*mut Stream) -> i32;
    AAudioStream_close: fn(*mut Stream) -> i32;
    AAudioStream_getSampleRate: fn(*mut Stream) -> i32;
    AAudioStream_getChannelCount: fn(*mut Stream) -> i32;
    AAudioStream_getFramesPerBurst: fn(*mut Stream) -> i32;
    AAudioStream_getBufferSizeInFrames: fn(*mut Stream) -> i32;
    AAudioStream_setBufferSizeInFrames: fn(*mut Stream, i32) -> i32;
    AAudioStream_getBufferCapacityInFrames: fn(*mut Stream) -> i32;
    AAudioStream_getXRunCount: fn(*mut Stream) -> i32;
    AAudioStream_getSharingMode: fn(*mut Stream) -> i32;
    AAudioStream_getPerformanceMode: fn(*mut Stream) -> i32;
    AAudioStream_getFramesWritten: fn(*mut Stream) -> i64;
    AAudioStream_getTimestamp: fn(*mut Stream, i32, *mut i64, *mut i64) -> i32;
}

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| unsafe { Api::load() }).as_ref().map_err(|e| e.clone())
}

fn result_text(api: &Api, code: i32) -> String {
    unsafe {
        let p = (api.AAudio_convertResultToText)(code);
        if p.is_null() { format!("code {code}") } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
    }
}

fn now_ns() -> i64 {
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

// ---------------------------------------------------------------------------
// Callbacks du flux (thread audio)
// ---------------------------------------------------------------------------

/// Posé par le callback d'erreur (ex. casque branché) ; le moniteur rouvre le flux.
static DISCONNECTED: AtomicBool = AtomicBool::new(false);

struct Synth {
    channels: usize,
    poly: Poly,
}

unsafe extern "C" fn data_callback(_: *mut Stream, user: *mut c_void, audio: *mut c_void, frames: i32) -> i32 {
    let s = &mut *(user as *mut Synth);
    let out = std::slice::from_raw_parts_mut(audio as *mut f32, frames as usize * s.channels);
    s.poly.render(out, s.channels);
    AAUDIO_CALLBACK_RESULT_CONTINUE
}

unsafe extern "C" fn error_callback(_: *mut Stream, _: *mut c_void, _: i32) {
    // Interdit de fermer le flux ici : on signale seulement.
    DISCONNECTED.store(true, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Gestion du flux
// ---------------------------------------------------------------------------

struct Engine {
    stream: *mut Stream,
    synth: *mut Synth,
    last_xruns: i32,
}
unsafe impl Send for Engine {}

static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

#[derive(Clone, Default)]
pub struct Stats {
    pub running: bool,
    pub low_latency: bool,
    pub exclusive: bool,
    pub sample_rate: i32,
    pub channels: i32,
    pub burst: i32,
    pub buffer: i32,
    pub capacity: i32,
    pub xruns: i32,
    pub latency_ms: Option<f64>,
    pub error: Option<String>,
}

fn set_error(e: String) {
    *LAST_ERROR.lock().unwrap() = Some(e);
}

pub fn start() {
    let mut guard = ENGINE.lock().unwrap();
    if guard.is_some() {
        return;
    }
    let api = match api() {
        Ok(a) => a,
        Err(e) => return set_error(e),
    };
    unsafe {
        let mut builder: *mut Builder = std::ptr::null_mut();
        let r = (api.AAudio_createStreamBuilder)(&mut builder);
        if r != AAUDIO_OK {
            return set_error(format!("createStreamBuilder : {}", result_text(api, r)));
        }

        // On laisse AAudio choisir fréquence et taille de rafale natives :
        // c'est ce qui évite tout rééchantillonnage.
        let synth = Box::into_raw(Box::new(Synth { channels: 2, poly: Poly::new(48_000.0) }));

        (api.AAudioStreamBuilder_setPerformanceMode)(builder, AAUDIO_PERFORMANCE_MODE_LOW_LATENCY);
        (api.AAudioStreamBuilder_setSharingMode)(builder, AAUDIO_SHARING_MODE_EXCLUSIVE);
        (api.AAudioStreamBuilder_setFormat)(builder, AAUDIO_FORMAT_PCM_FLOAT);
        (api.AAudioStreamBuilder_setChannelCount)(builder, 2);
        (api.AAudioStreamBuilder_setDataCallback)(builder, Some(data_callback), synth as *mut c_void);
        (api.AAudioStreamBuilder_setErrorCallback)(builder, Some(error_callback), std::ptr::null_mut());

        let mut stream: *mut Stream = std::ptr::null_mut();
        let r = (api.AAudioStreamBuilder_openStream)(builder, &mut stream);
        (api.AAudioStreamBuilder_delete)(builder);
        if r != AAUDIO_OK {
            drop(Box::from_raw(synth));
            return set_error(format!("openStream : {}", result_text(api, r)));
        }

        // Le synthé s'adapte à ce que le flux a réellement obtenu (avant le démarrage).
        let rate = (api.AAudioStream_getSampleRate)(stream).max(1) as f32;
        (*synth).poly = Poly::new(rate);
        (*synth).channels = (api.AAudioStream_getChannelCount)(stream).max(1) as usize;

        // Départ au plus serré : 2 rafales. Le moniteur agrandit si ça craque.
        let burst = (api.AAudioStream_getFramesPerBurst)(stream);
        (api.AAudioStream_setBufferSizeInFrames)(stream, burst * 2);

        let r = (api.AAudioStream_requestStart)(stream);
        if r != AAUDIO_OK {
            (api.AAudioStream_close)(stream);
            drop(Box::from_raw(synth));
            return set_error(format!("requestStart : {}", result_text(api, r)));
        }

        DISCONNECTED.store(false, Ordering::Relaxed);
        *LAST_ERROR.lock().unwrap() = None;
        *guard = Some(Engine { stream, synth, last_xruns: (api.AAudioStream_getXRunCount)(stream) });
    }
}

pub fn stop() {
    let Ok(api) = api() else { return };
    if let Some(e) = ENGINE.lock().unwrap().take() {
        unsafe {
            // close() arrête le flux et attend la fin du callback : on peut libérer ensuite.
            (api.AAudioStream_close)(e.stream);
            drop(Box::from_raw(e.synth));
        }
    }
}

/// Appelé régulièrement par le moniteur : reprise après déconnexion,
/// ajustement du tampon, mesure de la latence.
pub fn tick() -> Stats {
    // Flux coupé (casque branché/débranché…) : on le rouvre, sauf si l'app est en pause.
    if DISCONNECTED.swap(false, Ordering::AcqRel) && ENGINE.lock().unwrap().is_some() {
        stop();
        start();
    }

    let mut stats = Stats { error: LAST_ERROR.lock().unwrap().clone(), ..Default::default() };
    let Ok(api) = api() else { return stats };
    let mut guard = ENGINE.lock().unwrap();
    let Some(e) = guard.as_mut() else { return stats };

    unsafe {
        let s = e.stream;
        stats.running = true;
        stats.low_latency = (api.AAudioStream_getPerformanceMode)(s) == AAUDIO_PERFORMANCE_MODE_LOW_LATENCY;
        stats.exclusive = (api.AAudioStream_getSharingMode)(s) == AAUDIO_SHARING_MODE_EXCLUSIVE;
        stats.sample_rate = (api.AAudioStream_getSampleRate)(s);
        stats.channels = (api.AAudioStream_getChannelCount)(s);
        stats.burst = (api.AAudioStream_getFramesPerBurst)(s);
        stats.capacity = (api.AAudioStream_getBufferCapacityInFrames)(s);
        stats.xruns = (api.AAudioStream_getXRunCount)(s);

        // Des coupures depuis la dernière fois ? On grossit le tampon d'une rafale.
        if stats.xruns > e.last_xruns {
            let size = (api.AAudioStream_getBufferSizeInFrames)(s);
            if size + stats.burst <= stats.capacity {
                (api.AAudioStream_setBufferSizeInFrames)(s, size + stats.burst);
            }
            e.last_xruns = stats.xruns;
        }
        stats.buffer = (api.AAudioStream_getBufferSizeInFrames)(s);

        // Latence de sortie : quand la dernière trame écrite sortira-t-elle du haut-parleur ?
        let (mut hw_frame, mut hw_time) = (0i64, 0i64);
        if (api.AAudioStream_getTimestamp)(s, CLOCK_MONOTONIC, &mut hw_frame, &mut hw_time) == AAUDIO_OK {
            let written = (api.AAudioStream_getFramesWritten)(s);
            let ahead_ns = (written - hw_frame) as f64 * 1e9 / stats.sample_rate.max(1) as f64;
            let ms = (hw_time as f64 + ahead_ns - now_ns() as f64) / 1e6;
            if ms > 0.0 && ms < 1000.0 {
                stats.latency_ms = Some(ms);
            }
        }
    }
    stats
}
