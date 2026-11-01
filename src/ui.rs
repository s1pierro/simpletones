//! Rendu logiciel de l'écran et zones tactiles.
//! Indépendant d'Android (simple tableau de pixels), donc testable n'importe où.
//!
//! Disposition (paysage) :
//!   ┌────────────────────────┬───────────────┐
//!   │ diagnostic audio       │ curseur ATTAQUE│
//!   │                        │ curseur RELACHE│
//!   ├────────────────────────┴───────────────┤
//!   │ clavier : 15 touches, Do4 → Do6         │
//!   └────────────────────────────────────────┘

use crate::audio::Stats;
use crate::font;

/// Pixels RGBA_8888 lus en u32 little-endian : 0xAABBGGRR.
const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    0xFF00_0000 | (b as u32) << 16 | (g as u32) << 8 | r as u32
}

const BG: u32 = rgb(0x16, 0x18, 0x1D);
const TEXT: u32 = rgb(0xE6, 0xE8, 0xEC);
const DIM: u32 = rgb(0x8A, 0x90, 0x9C);
const GOOD: u32 = rgb(0x5C, 0xD6, 0x8A);
const WARN: u32 = rgb(0xF2, 0xB1, 0x4C);
const BAD: u32 = rgb(0xF0, 0x5A, 0x5A);
const KEY_ALT: u32 = rgb(0x32, 0x37, 0x42);
const KEY_WHITE: u32 = rgb(0x3A, 0x40, 0x4C);
const KEY_BLACK: u32 = rgb(0x0B, 0x0C, 0x10);
const ACCENT: u32 = rgb(0x4C, 0x9E, 0xF2);

// ---------------------------------------------------------------------------
// Clavier
// ---------------------------------------------------------------------------

/// Clavier chromatique de Do4 à Do6 : 25 touches, indexées par demi-ton (0 = Do4).
pub const KEYS: usize = 25;
/// Touches blanches visibles (2 octaves + le Do final).
const WHITES: usize = 15;
/// Demi-tons des touches blanches dans une octave.
const WHITE_STEPS: [usize; 7] = [0, 2, 4, 5, 7, 9, 11];
const NAMES: [&str; 7] = ["DO", "RE", "MI", "FA", "SOL", "LA", "SI"];

/// Fréquence du demi-ton `i`, à partir du Do4 (261,6 Hz) : les petits
/// haut-parleurs de tablette rendent mal les graves.
pub fn key_freq(i: usize) -> f32 {
    261.63 * 2f32.powf(i as f32 / 12.0)
}

fn is_black(semi: usize) -> bool {
    matches!(semi % 12, 1 | 3 | 6 | 8 | 10)
}

/// Rang (parmi les blanches) de la touche blanche `semi`, ou de la blanche
/// juste à gauche pour une noire.
fn white_index(semi: usize) -> usize {
    let s = if is_black(semi) { semi - 1 } else { semi };
    (s / 12) * 7 + WHITE_STEPS.iter().position(|&x| x == s % 12).unwrap()
}

fn white_semi(j: usize) -> usize {
    (j / 7) * 12 + WHITE_STEPS[j % 7]
}

// ---------------------------------------------------------------------------
// Curseurs (échelle logarithmique)
// ---------------------------------------------------------------------------

pub const SLIDERS: usize = 2;
const SLIDER_LABEL: [&str; SLIDERS] = ["ATTAQUE", "RELACHE"];
/// Bornes en secondes.
const SLIDER_RANGE: [(f32, f32); SLIDERS] = [(0.002, 2.0), (0.01, 4.0)];

/// Position du curseur (0..1) → valeur en secondes.
pub fn slider_value(i: usize, t: f32) -> f32 {
    let (lo, hi) = SLIDER_RANGE[i];
    lo * (hi / lo).powf(t.clamp(0.0, 1.0))
}

/// Valeur en secondes → position du curseur (0..1).
pub fn slider_pos(i: usize, v: f32) -> f32 {
    let (lo, hi) = SLIDER_RANGE[i];
    ((v / lo).ln() / (hi / lo).ln()).clamp(0.0, 1.0)
}

fn fmt_time(s: f32) -> String {
    if s >= 1.0 { format!("{s:.2} S") } else { format!("{:.0} MS", s * 1000.0) }
}

// ---------------------------------------------------------------------------
// Disposition et zones tactiles
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x as f32 && x < (self.x + self.w) as f32 && y >= self.y as f32 && y < (self.y + self.h) as f32
    }
}

pub struct Layout {
    pub w: usize,
    pub h: usize,
    pub scale: usize,
    pub margin: usize,
    pub kb_top: usize,
    /// Zone tactile de chaque curseur (généreuse) et sa piste visible.
    pub slider_zone: [Rect; SLIDERS],
    pub slider_track: [Rect; SLIDERS],
    /// Rectangle visible de chaque touche, indexé par demi-ton.
    pub keys: [Rect; KEYS],
    pub gap: usize,
}

pub fn layout(w: usize, h: usize) -> Layout {
    let kb_top = h / 2;
    // Texte : ~40 caractères sur 58 % de la largeur, 10 lignes dans la moitié haute.
    let scale = ((w * 58 / 100) / (40 * (font::W + 1))).min(kb_top / (10 * (font::H + 3))).max(1);
    let margin = 2 * scale * font::W;

    let sx = w * 62 / 100;
    let sw = w.saturating_sub(sx + margin);
    let row_h = kb_top / SLIDERS;
    let label_h = (font::H + 4) * scale;
    let track_h = (3 * scale).max(6);
    let mut slider_zone = [Rect { x: 0, y: 0, w: 0, h: 0 }; SLIDERS];
    let mut slider_track = slider_zone;
    for i in 0..SLIDERS {
        let ry = i * row_h;
        slider_zone[i] = Rect { x: sx, y: ry, w: w - sx, h: row_h };
        slider_track[i] = Rect { x: sx, y: ry + margin / 2 + label_h + 3 * scale, w: sw, h: track_h };
    }

    // Clavier : blanches sur toute la hauteur, noires sur ~60 % en haut,
    // centrées sur la jonction entre deux blanches.
    let gap = scale.max(2);
    let ww = w / WHITES;
    let kb_h = h - kb_top;
    let bw = ww * 62 / 100;
    let mut keys = [Rect { x: 0, y: 0, w: 0, h: 0 }; KEYS];
    for (k, r) in keys.iter_mut().enumerate() {
        let j = white_index(k);
        *r = if is_black(k) {
            Rect { x: (j + 1) * ww - bw / 2, y: kb_top + gap, w: bw, h: kb_h * 58 / 100 }
        } else {
            Rect { x: j * ww + gap, y: kb_top + gap, w: ww - 2 * gap, h: kb_h - 2 * gap }
        };
    }
    Layout { w, h, scale, margin, kb_top, slider_zone, slider_track, keys, gap }
}

pub enum Hit {
    Key(usize),
    /// Curseur et position 0..1 sur sa piste.
    Slider(usize, f32),
    Nothing,
}

impl Layout {
    pub fn hit(&self, x: f32, y: f32) -> Hit {
        if self.w == 0 {
            return Hit::Nothing;
        }
        if y >= self.kb_top as f32 {
            // Les noires sont devant les blanches : on les teste d'abord.
            if let Some(k) = (0..KEYS).find(|&k| is_black(k) && self.keys[k].contains(x, y)) {
                return Hit::Key(k);
            }
            let j = (x.max(0.0) as usize * WHITES / self.w).min(WHITES - 1);
            return Hit::Key(white_semi(j));
        }
        for i in 0..SLIDERS {
            if self.slider_zone[i].contains(x, y) {
                let t = self.slider_track[i];
                return Hit::Slider(i, ((x - t.x as f32) / t.w.max(1) as f32).clamp(0.0, 1.0));
            }
        }
        Hit::Nothing
    }
}

// ---------------------------------------------------------------------------
// Rendu
// ---------------------------------------------------------------------------

/// Ce que l'interface doit afficher en plus des statistiques audio.
#[derive(Clone)]
pub struct View {
    pub held: [bool; KEYS],
    pub held_count: usize,
    /// Valeurs des curseurs en secondes.
    pub sliders: [f32; SLIDERS],
}

pub struct Canvas<'a> {
    pub px: &'a mut [u32],
    pub w: usize,
    pub h: usize,
    pub stride: usize,
}

impl Canvas<'_> {
    fn rect(&mut self, r: Rect, c: u32) {
        let x1 = (r.x + r.w).min(self.w);
        for yy in r.y..(r.y + r.h).min(self.h) {
            let row = yy * self.stride;
            if r.x < x1 {
                self.px[row + r.x..row + x1].fill(c);
            }
        }
    }

    fn text(&mut self, x: usize, y: usize, s: &str, scale: usize, c: u32) {
        let mut cx = x;
        for ch in s.chars() {
            let g = font::glyph(ch);
            for (gy, bits) in g.iter().enumerate() {
                for gx in 0..font::W {
                    if bits & (0x10 >> gx) != 0 {
                        self.rect(Rect { x: cx + gx * scale, y: y + gy * scale, w: scale, h: scale }, c);
                    }
                }
            }
            cx += (font::W + 1) * scale;
        }
    }
}

fn text_width(s: &str, scale: usize) -> usize {
    s.chars().count() * (font::W + 1) * scale
}

pub fn render(cv: &mut Canvas, st: &Stats, view: &View) {
    let l = layout(cv.w, cv.h);
    let (w, h, scale) = (l.w, l.h, l.scale);
    cv.rect(Rect { x: 0, y: 0, w, h }, BG);

    // --- Diagnostic -------------------------------------------------------
    let line = (font::H + 3) * scale;
    let mut y = l.margin / 2;
    let ms = |frames: i32| frames as f64 * 1000.0 / st.sample_rate.max(1) as f64;
    let put = |cv: &mut Canvas, y: &mut usize, s: &str, c: u32| {
        cv.text(l.margin, *y, s, scale, c);
        *y += line;
    };

    put(cv, &mut y, "SYNTHE AAUDIO 0.3", TEXT);
    y += line / 3;
    if let Some(err) = &st.error {
        put(cv, &mut y, &format!("ERREUR : {err}"), BAD);
    }
    if !st.running {
        put(cv, &mut y, "FLUX AUDIO ARRETE", WARN);
    } else {
        if st.low_latency {
            put(cv, &mut y, "MODE : BASSE LATENCE OBTENU", GOOD);
        } else {
            put(cv, &mut y, "MODE : BASSE LATENCE REFUSE", BAD);
        }
        if st.exclusive {
            put(cv, &mut y, "ACCES : EXCLUSIF (MMAP)", GOOD);
        } else {
            put(cv, &mut y, "ACCES : PARTAGE (VIA LE MIXEUR)", WARN);
        }
        put(cv, &mut y, &format!("FREQUENCE : {} HZ, {} CANAUX", st.sample_rate, st.channels), TEXT);
        put(cv, &mut y, &format!("RAFALE : {} TRAMES ({:.1} MS)", st.burst, ms(st.burst)), TEXT);
        let tampon = format!("TAMPON : {} TRAMES ({:.1} MS) / MAX {}", st.buffer, ms(st.buffer), st.capacity);
        put(cv, &mut y, &tampon, TEXT);
        put(cv, &mut y, &format!("COUPURES (XRUNS) : {}", st.xruns), if st.xruns == 0 { GOOD } else { WARN });
        match st.latency_ms {
            Some(lat) => {
                let c = if lat < 20.0 { GOOD } else if lat < 45.0 { WARN } else { BAD };
                put(cv, &mut y, &format!("LATENCE DE SORTIE : {lat:.1} MS"), c)
            }
            None => put(cv, &mut y, "LATENCE DE SORTIE : MESURE...", DIM),
        }
    }
    y += line / 3;
    put(cv, &mut y, &format!("NOTES TENUES : {}", view.held_count), if view.held_count > 0 { ACCENT } else { DIM });

    // --- Curseurs ---------------------------------------------------------
    for i in 0..SLIDERS {
        let zone = l.slider_zone[i];
        let track = l.slider_track[i];
        let label = format!("{} : {}", SLIDER_LABEL[i], fmt_time(view.sliders[i]));
        cv.text(track.x, zone.y + l.margin / 2, &label, scale, TEXT);
        cv.rect(track, KEY_ALT);
        let fill = (slider_pos(i, view.sliders[i]) * track.w as f32) as usize;
        cv.rect(Rect { w: fill, ..track }, ACCENT);
        let knob = 4 * scale;
        let kx = (track.x + fill).saturating_sub(knob / 2).min(track.x + track.w - knob / 2);
        cv.rect(Rect { x: kx, y: track.y + track.h / 2 - knob / 2, w: knob, h: knob }, TEXT);
    }

    // --- Clavier ----------------------------------------------------------
    for k in (0..KEYS).filter(|&k| !is_black(k)) {
        let key = l.keys[k];
        cv.rect(key, if view.held[k] { ACCENT } else { KEY_WHITE });
        let name = NAMES[white_index(k) % 7];
        let nx = key.x + key.w.saturating_sub(text_width(name, scale)) / 2;
        let ny = key.y + key.h.saturating_sub((font::H + 6) * scale);
        cv.text(nx, ny, name, scale, if view.held[k] { BG } else { DIM });
    }
    for k in (0..KEYS).filter(|&k| is_black(k)) {
        let key = l.keys[k];
        // Liseré couleur de fond pour détacher la noire des blanches voisines.
        let g = l.gap;
        cv.rect(Rect { x: key.x - g, y: key.y, w: key.w + 2 * g, h: key.h + g }, BG);
        cv.rect(key, if view.held[k] { ACCENT } else { KEY_BLACK });
    }
}
