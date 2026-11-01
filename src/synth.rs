//! Synthé polyphonique : 10 voix, quatre timbres, enveloppe attaque / maintien / relâche.
//!
//! Chaque voix garde le timbre choisi au moment où la note part : changer de
//! timbre ne coupe pas les notes qui sonnent encore.
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
// Timbres
// ---------------------------------------------------------------------------

pub const TIMBRES: usize = 4;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Timbre {
    /// Fondamentale + octave + douzième : timbre doux d'orgue.
    Orgue,
    /// Corde pincée (Karplus-Strong) : du bruit bref circule dans une boucle
    /// à retard, dont la longueur donne la note et le filtre le timbre.
    Corde,
    /// Piano électrique en synthèse FM : corps en rapport 1:1, attaque en 1:14.
    Piano,
    /// Deux dents de scie désaccordées dans un filtre passe-bas qui se referme.
    Lead,
}

const ALL_TIMBRES: [Timbre; TIMBRES] = [Timbre::Orgue, Timbre::Corde, Timbre::Piano, Timbre::Lead];

static TIMBRE: AtomicUsize = AtomicUsize::new(0);

/// Timbre des prochaines notes.
pub fn set_timbre(i: usize) {
    TIMBRE.store(i.min(TIMBRES - 1), Ordering::Relaxed);
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
    timbre: Timbre,
    /// Volume propre au timbre : décroît seul, même doigt posé (corde, piano).
    level: f32,
    level_coef: f32,
    /// Brillance : monte le filtre du lead et l'indice FM, puis retombe.
    bright: f32,
    bright_coef: f32,
    /// Lead : seconde dent de scie et état du filtre.
    phase2: f32,
    low: f32,
    band: f32,
    /// Piano : part de l'attaque aiguë (nulle si elle dépasserait Nyquist).
    tine: f32,
    /// Corde : retard de la boucle en échantillons, position, filtre.
    delay: f32,
    write: usize,
    damp: f32,
    prev: f32,
}

impl Voice {
    const OFF: Voice = Voice {
        id: 0,
        freq: 0.0,
        phase: 0.0,
        env: 0.0,
        stage: Stage::Off,
        age: 0,
        timbre: Timbre::Orgue,
        level: 1.0,
        level_coef: 1.0,
        bright: 0.0,
        bright_coef: 1.0,
        phase2: 0.0,
        low: 0.0,
        band: 0.0,
        tine: 0.0,
        delay: 0.0,
        write: 0,
        damp: 0.0,
        prev: 0.0,
    };
    fn held(&self) -> bool {
        matches!(self.stage, Stage::Attack | Stage::Sustain)
    }
}

pub struct Poly {
    rate: f32,
    voices: [Voice; MAX_VOICES],
    clock: u32,
    /// Boucles à retard des cordes, réservées d'avance : `KS_LEN` échantillons par voix.
    strings: Vec<f32>,
    /// Générateur de bruit (xorshift) pour l'attaque des cordes.
    noise: u32,
}

/// Gain par voix : 4 à 5 notes tiennent sans saturer ; au-delà, la saturation douce arrondit.
const VOICE_GAIN: f32 = 0.2;
/// Volume de chaque timbre, pour qu'ils sonnent à peu près aussi fort.
const CORDE_GAIN: f32 = 3.5;
const PIANO_GAIN: f32 = 1.4;
const LEAD_GAIN: f32 = 1.3;

/// Assez pour un Do1 (32,7 Hz) jusqu'à 130 kHz d'échantillonnage.
const KS_LEN: usize = 4096;
/// Sécurité : le continu n'est pas atténué par le filtre de la boucle.
const KS_LOOP_GAIN: f32 = 0.9999;
const LEAD_DETUNE: f32 = 1.006_96; // 12 centièmes de demi-ton
const LEAD_Q: f32 = 0.8;

/// Coefficient par échantillon pour perdre 60 dB en `seconds`.
fn decay_coef(seconds: f32, rate: f32) -> f32 {
    (0.001f32.ln() / (seconds * rate)).exp()
}

/// Durée de résonance : les notes graves tiennent plus longtemps.
fn ring_time(base: f32, freq: f32) -> f32 {
    (base * (110.0 / freq).sqrt()).clamp(0.5, 8.0)
}

/// Correction polyBLEP : retire le repliement du saut d'une dent de scie.
fn poly_blep(t: f32, dt: f32) -> f32 {
    if t < dt {
        let x = t / dt;
        2.0 * x - x * x - 1.0
    } else if t > 1.0 - dt {
        let x = (t - 1.0) / dt;
        x * x + 2.0 * x + 1.0
    } else {
        0.0
    }
}

fn saw(t: f32, dt: f32) -> f32 {
    2.0 * t - 1.0 - poly_blep(t, dt)
}

fn wrap(mut p: f32) -> f32 {
    if p >= 1.0 {
        p -= 1.0;
    }
    p
}

impl Poly {
    pub fn new(rate: f32) -> Poly {
        Poly { rate, voices: [Voice::OFF; MAX_VOICES], clock: 0, strings: vec![0.0; KS_LEN * MAX_VOICES], noise: 0x9E37_79B9 }
    }

    fn next_noise(&mut self) -> f32 {
        let mut x = self.noise;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.noise = x;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    /// Pince la corde de la voix `idx` : remplit sa boucle de bruit filtré, centré sur zéro.
    fn pluck(&mut self, idx: usize) {
        let filled = (self.voices[idx].delay.ceil() as usize + 2).min(KS_LEN);
        // Attaque plus douce dans le grave, sinon le bruit s'y entend trop.
        let tone = (self.voices[idx].freq / 800.0).clamp(0.3, 0.9);
        let start = idx * KS_LEN + KS_LEN - filled;
        let (mut last, mut sum) = (0.0, 0.0);
        for k in 0..filled {
            last += tone * (self.next_noise() - last);
            self.strings[start + k] = last;
            sum += last;
        }
        let mean = sum / filled as f32;
        self.strings[start..start + filled].iter_mut().for_each(|s| *s -= mean);
    }

    fn start(&mut self, id: u32, freq: f32, timbre: Timbre) {
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
        *v = Voice { id, freq, phase: v.phase, env: v.env, stage: Stage::Attack, age: self.clock, timbre, ..Voice::OFF };
        let rate = self.rate;
        match timbre {
            Timbre::Orgue => {}
            Timbre::Corde => {
                // Filtre de boucle : moins marqué dans l'aigu, où il sert beaucoup plus souvent.
                v.damp = (0.5 * 220.0 / freq).clamp(0.08, 0.5);
                v.delay = (rate / freq - v.damp).clamp(2.0, (KS_LEN - 4) as f32);
                v.level_coef = decay_coef(ring_time(6.0, freq), rate);
                self.pluck(idx);
            }
            Timbre::Piano => {
                v.level_coef = decay_coef(ring_time(5.0, freq), rate);
                v.bright = 1.0;
                v.bright_coef = decay_coef(0.8, rate);
                v.tine = if 14.0 * freq < 0.45 * rate { 0.25 } else { 0.0 };
            }
            Timbre::Lead => {
                v.bright = 1.0;
                v.bright_coef = decay_coef(0.6, rate);
                v.phase2 = 0.37; // évite l'annulation des deux ondes au départ
            }
        }
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
                EV_ON => self.start(id, freq, ALL_TIMBRES[TIMBRE.load(Ordering::Relaxed)]),
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
        let rate = self.rate;
        for (v, string) in self.voices.iter_mut().zip(self.strings.chunks_exact_mut(KS_LEN)) {
            if v.stage == Stage::Off {
                continue;
            }
            let inc = v.freq / rate;
            let fm_scale = (1200.0 / v.freq).min(1.0); // moins de FM dans l'aigu : pas de repliement
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
                let s = match v.timbre {
                    Timbre::Orgue => p.sin() + 0.4 * (2.0 * p).sin() + 0.15 * (3.0 * p).sin(),
                    Timbre::Corde => {
                        let read = (v.write + KS_LEN) as f32 - v.delay;
                        let i0 = read as usize;
                        let frac = read - i0 as f32;
                        let (a, b) = (string[i0 % KS_LEN], string[(i0 + 1) % KS_LEN]);
                        let x = a + frac * (b - a);
                        let y = x + v.damp * (v.prev - x);
                        v.prev = x;
                        string[v.write] = y * KS_LOOP_GAIN;
                        v.write = (v.write + 1) % KS_LEN;
                        y * CORDE_GAIN
                    }
                    Timbre::Piano => {
                        let index = 1.6 * v.bright * fm_scale;
                        ((p + index * p.sin()).sin() + v.tine * v.bright * v.bright * (14.0 * p).sin()) * PIANO_GAIN
                    }
                    Timbre::Lead => {
                        let x = 0.5 * (saw(v.phase, inc) + saw(v.phase2, inc * LEAD_DETUNE));
                        let cutoff = (v.freq * (3.0 + 9.0 * v.bright)).min(0.16 * rate);
                        let f = std::f32::consts::TAU * cutoff / rate;
                        v.low += f * v.band;
                        let high = x - v.low - LEAD_Q * v.band;
                        v.band += f * high;
                        v.phase2 = wrap(v.phase2 + inc * LEAD_DETUNE);
                        v.low * LEAD_GAIN
                    }
                };
                frame[0] += s * v.level * v.env * VOICE_GAIN;
                v.level *= v.level_coef;
                v.bright *= v.bright_coef;
                v.phase = wrap(v.phase + inc);
                if v.level < 1e-3 {
                    // Corde ou piano éteints sous le doigt : la voix redevient libre.
                    v.env = 0.0;
                    v.stage = Stage::Off;
                    break;
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

#[cfg(test)]
mod tests {
    use super::*;

    const RATES: [f32; 2] = [44_100.0, 48_000.0];

    fn note(p: &mut Poly, freq: f32, timbre: Timbre, seconds: f32) -> Vec<f32> {
        p.start(1, freq, timbre);
        let mut out = vec![0.0; (seconds * p.rate) as usize];
        for chunk in out.chunks_mut(192) {
            p.render(chunk, 1);
        }
        out
    }

    /// Fréquence par autocorrélation, cherchée à ±6 % de la fréquence attendue.
    fn measure(x: &[f32], rate: f32, expected: f32) -> f32 {
        let corr = |lag: usize| x[..x.len() - lag].iter().zip(&x[lag..]).map(|(a, b)| a * b).sum::<f32>();
        let centre = rate / expected;
        let lo = ((centre * 0.94) as usize).max(2);
        let hi = (centre * 1.06).ceil() as usize + 1;
        let best = (lo..=hi).max_by(|&a, &b| corr(a).total_cmp(&corr(b))).unwrap();
        let (a, b, c) = (corr(best - 1), corr(best), corr(best + 1));
        rate / (best as f32 + 0.5 * (a - c) / (a - 2.0 * b + c))
    }

    fn cents(a: f32, b: f32) -> f32 {
        1200.0 * (a / b).log2()
    }

    #[test]
    fn stables_et_bornes_aux_extremes() {
        for rate in RATES {
            for t in ALL_TIMBRES {
                for freq in [32.7, 261.63, 4186.0] {
                    let mut p = Poly::new(rate);
                    let x = note(&mut p, freq, t, 1.0);
                    assert!(x.iter().all(|s| s.is_finite() && s.abs() <= 1.0), "{t:?} {freq}");
                    assert!(x.iter().any(|s| s.abs() > 0.005), "{t:?} {freq} muet");
                }
            }
        }
    }

    #[test]
    fn justesse() {
        for rate in RATES {
            for t in ALL_TIMBRES {
                for freq in [65.41, 130.81, 261.63, 440.0, 1046.5, 2093.0] {
                    let mut p = Poly::new(rate);
                    let x = note(&mut p, freq, t, 0.3);
                    let window = &x[(0.05 * rate) as usize..(0.05 * rate) as usize + (0.12 * rate) as usize];
                    let c = cents(measure(window, rate, freq), freq);
                    assert!(c.abs() < 8.0, "{t:?} {freq} Hz a {rate} : {c:.1} cents");
                }
            }
        }
    }

    #[test]
    fn corde_et_piano_s_eteignent_sous_le_doigt() {
        for t in [Timbre::Corde, Timbre::Piano] {
            let mut p = Poly::new(48_000.0);
            note(&mut p, 261.63, t, 9.0);
            assert_eq!(p.sounding(), 0, "{t:?}");
        }
        for t in [Timbre::Orgue, Timbre::Lead] {
            let mut p = Poly::new(48_000.0);
            note(&mut p, 261.63, t, 9.0);
            assert_eq!(p.sounding(), 1, "{t:?}");
        }
    }

    #[test]
    fn relache_termine_la_note() {
        for t in ALL_TIMBRES {
            let mut p = Poly::new(48_000.0);
            note(&mut p, 440.0, t, 0.2);
            p.stop(1);
            let mut tail = vec![0.0; 48_000];
            p.render(&mut tail, 1);
            assert_eq!(p.sounding(), 0, "{t:?}");
            assert!(tail[47_000..].iter().all(|s| *s == 0.0), "{t:?}");
        }
    }

    #[test]
    fn dix_voix_de_chaque_timbre_sans_exces() {
        for t in ALL_TIMBRES {
            let mut p = Poly::new(48_000.0);
            for i in 0..14 {
                p.start(i, 110.0 * 2f32.powf(i as f32 / 5.0), t);
            }
            let mut out = vec![0.0; 48_000];
            p.render(&mut out, 1);
            assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0), "{t:?}");
        }
    }
}
