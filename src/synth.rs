//! Synthé polyphonique : 10 voix, enveloppe attaque / maintien / relâche.
//!
//! Le thread principal (tactile) envoie des événements dans une file circulaire
//! sans verrou ; le thread audio les lit au début de chaque rafale.
//! Rien ici n'alloue ni ne bloque une fois le synthé créé.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub const MAX_VOICES: usize = 10;

// ---------------------------------------------------------------------------
// Paramètres réglables en direct (secondes, f32 stocké en bits)
// ---------------------------------------------------------------------------

static ATTACK_S: AtomicU32 = AtomicU32::new(0x3C23D70A); // 0.01 s
static RELEASE_S: AtomicU32 = AtomicU32::new(0x3ECCCCCD); // 0.4 s

pub fn set_attack(s: f32) {
    ATTACK_S.store(s.to_bits(), Ordering::Relaxed);
}
pub fn set_release(s: f32) {
    RELEASE_S.store(s.to_bits(), Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// File d'événements : un seul producteur (thread principal), un seul consommateur (audio)
// ---------------------------------------------------------------------------

const QUEUE_LEN: usize = 256;
static QUEUE: [AtomicU64; QUEUE_LEN] = [const { AtomicU64::new(0) }; QUEUE_LEN];
static HEAD: AtomicUsize = AtomicUsize::new(0); // lu par l'audio
static TAIL: AtomicUsize = AtomicUsize::new(0); // écrit par le thread principal

const EV_ON: u64 = 1;
const EV_OFF: u64 = 2;
const EV_ALL_OFF: u64 = 3;

fn push(kind: u64, id: u32, freq: f32) {
    let tail = TAIL.load(Ordering::Relaxed);
    let next = (tail + 1) % QUEUE_LEN;
    if next == HEAD.load(Ordering::Acquire) {
        return; // file pleine (improbable : 256 événements d'avance)
    }
    QUEUE[tail].store(kind << 40 | ((id & 0xff) as u64) << 32 | freq.to_bits() as u64, Ordering::Relaxed);
    TAIL.store(next, Ordering::Release);
}

fn pop() -> Option<(u64, u32, f32)> {
    let head = HEAD.load(Ordering::Relaxed);
    if head == TAIL.load(Ordering::Acquire) {
        return None;
    }
    let v = QUEUE[head].load(Ordering::Relaxed);
    HEAD.store((head + 1) % QUEUE_LEN, Ordering::Release);
    Some((v >> 40, (v >> 32 & 0xff) as u32, f32::from_bits(v as u32)))
}

/// Un doigt (identifié par `id`) se pose sur une note.
pub fn note_on(id: u32, freq: f32) {
    push(EV_ON, id, freq);
}
/// Ce doigt se lève : la note passe en relâche.
pub fn note_off(id: u32) {
    push(EV_OFF, id, 0.0);
}
pub fn all_off() {
    push(EV_ALL_OFF, 0, 0.0);
}

// ---------------------------------------------------------------------------
// Voix
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Off,
    Attack,
    Sustain,
    Release,
}

#[derive(Clone, Copy)]
struct Voice {
    id: u32,
    freq: f32,
    phase: f32,
    env: f32,
    stage: Stage,
    age: u32,
}

impl Voice {
    const OFF: Voice = Voice { id: 0, freq: 0.0, phase: 0.0, env: 0.0, stage: Stage::Off, age: 0 };
    fn held(&self) -> bool {
        matches!(self.stage, Stage::Attack | Stage::Sustain)
    }
}

pub struct Poly {
    rate: f32,
    voices: [Voice; MAX_VOICES],
    clock: u32,
}

/// Gain par voix : 4 à 5 notes tiennent sans saturer ; au-delà, la saturation douce arrondit.
const VOICE_GAIN: f32 = 0.2;

impl Poly {
    pub fn new(rate: f32) -> Poly {
        Poly { rate, voices: [Voice::OFF; MAX_VOICES], clock: 0 }
    }

    fn start(&mut self, id: u32, freq: f32) {
        // Même doigt déjà actif (glissé d'une touche à l'autre) : on relâche l'ancienne note.
        self.stop(id);
        // Voix libre, sinon la plus faible en relâche, sinon la plus ancienne.
        let idx = self
            .voices
            .iter()
            .position(|v| v.stage == Stage::Off)
            .or_else(|| {
                (0..MAX_VOICES)
                    .filter(|&i| self.voices[i].stage == Stage::Release)
                    .min_by(|&a, &b| self.voices[a].env.total_cmp(&self.voices[b].env))
            })
            .unwrap_or_else(|| (0..MAX_VOICES).min_by_key(|&i| self.voices[i].age).unwrap());

        self.clock = self.clock.wrapping_add(1);
        let v = &mut self.voices[idx];
        // L'enveloppe repart du niveau courant (pas de clic si la voix était volée).
        *v = Voice { id, freq, phase: v.phase, env: v.env, stage: Stage::Attack, age: self.clock };
    }

    fn stop(&mut self, id: u32) {
        for v in self.voices.iter_mut().filter(|v| v.held() && v.id == id) {
            v.stage = Stage::Release;
        }
    }

    /// Nombre de voix qui sonnent (tenues ou en relâche).
    #[allow(dead_code)]
    pub fn sounding(&self) -> usize {
        self.voices.iter().filter(|v| v.stage != Stage::Off).count()
    }

    /// Remplit `out` (entrelacé, `channels` canaux).
    pub fn render(&mut self, out: &mut [f32], channels: usize) {
        while let Some((kind, id, freq)) = pop() {
            match kind {
                EV_ON => self.start(id, freq),
                EV_OFF => self.stop(id),
                EV_ALL_OFF => self.voices.iter_mut().filter(|v| v.held()).for_each(|v| v.stage = Stage::Release),
                _ => {}
            }
        }

        let attack = f32::from_bits(ATTACK_S.load(Ordering::Relaxed)).max(0.002);
        let release = f32::from_bits(RELEASE_S.load(Ordering::Relaxed)).max(0.005);
        let attack_step = 1.0 / (attack * self.rate);
        // Relâche exponentielle : -60 dB au bout de `release` secondes.
        let release_coef = (0.001f32.ln() / (release * self.rate)).exp();

        out.fill(0.0);
        for v in self.voices.iter_mut().filter(|v| v.stage != Stage::Off) {
            let inc = v.freq / self.rate;
            for frame in out.chunks_exact_mut(channels) {
                match v.stage {
                    Stage::Attack => {
                        v.env += attack_step;
                        if v.env >= 1.0 {
                            v.env = 1.0;
                            v.stage = Stage::Sustain;
                        }
                    }
                    Stage::Release => {
                        v.env *= release_coef;
                        if v.env < 1e-3 { // -60 dB : la relâche dure exactement le temps réglé
                            v.env = 0.0;
                            v.stage = Stage::Off;
                            break;
                        }
                    }
                    _ => {}
                }
                let p = v.phase * std::f32::consts::TAU;
                // Fondamentale + octave + douzième : timbre doux d'orgue.
                frame[0] += (p.sin() + 0.4 * (2.0 * p).sin() + 0.15 * (3.0 * p).sin()) * v.env * VOICE_GAIN;
                v.phase += inc;
                if v.phase >= 1.0 {
                    v.phase -= 1.0;
                }
            }
        }

        // Saturation douce (approximation de tanh) puis copie sur les autres canaux.
        for frame in out.chunks_exact_mut(channels) {
            let x = frame[0].clamp(-3.0, 3.0);
            let y = x * (27.0 + x * x) / (27.0 + 9.0 * x * x);
            frame.fill(y);
        }
    }
}
