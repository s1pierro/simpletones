//! Rendu logiciel de l'écran et zones tactiles.
//! Indépendant d'Android (simple tableau de pixels), donc testable n'importe où.
//!
//! Disposition (paysage) :
//!   ┌────────────────────────┬───────────────┐
//!   │ diagnostic audio       │ boutons TIMBRE │
//!   │                        │ curseur ATTAQUE│
//!   │                        │ curseur RELACHE│
//!   │                        │ curseur NOTE BASSE
//!   │                        │ curseur ETENDUE│
//!   ├────────────────────────┴───────────────┤
//!   │ clavier configurable (défaut : Do4 → Do6)│
//!   └────────────────────────────────────────┘

use crate::audio::Stats;
use crate::font;
use crate::synth::TIMBRES;

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

// Une note se repère en demi-tons depuis Do0 (Do4 = 48), une touche blanche par
// son rang parmi les blanches depuis Do0 (Do4 = 28).

/// Notes possibles : de Do0 à Do8.
pub const NOTES: usize = 97;
/// Touches au plus sur le clavier : 4 octaves + la note finale.
const MAX_KEYS: usize = 49;
const WHITE_LOWEST: usize = 7; // Do1
const WHITE_HIGHEST: usize = 56; // Do8 : au-delà, les harmoniques approchent de Nyquist
const WHITES_MIN: usize = 5;
const WHITES_MAX: usize = 29;
/// La note basse la plus aiguë possible (clavier le plus court, finissant au Do8).
const LOW_MAX: usize = WHITE_HIGHEST + 1 - WHITES_MIN;

/// Le clavier commence et finit sur une blanche : une noire au bord serait coupée.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Keyboard {
    /// Rang de la blanche la plus grave.
    pub low: usize,
    /// Nombre de touches blanches.
    pub whites: usize,
}

impl Keyboard {
    /// Do4 → Do6 : les petits haut-parleurs de tablette rendent mal les graves.
    pub const DEFAULT: Keyboard = Keyboard { low: 28, whites: 15 };

    /// Réglages relus d'un fichier : rien ne garantit qu'ils soient valides.
    pub fn sanitized(low: usize, whites: usize) -> Keyboard {
        Keyboard { low: Keyboard::DEFAULT.low, whites: whites.clamp(WHITES_MIN, WHITES_MAX) }.with_low(low)
    }

    fn with_low(self, low: usize) -> Keyboard {
        Keyboard { low: low.clamp(WHITE_LOWEST, WHITE_HIGHEST + 1 - self.whites), ..self }
    }

    fn with_whites(self, whites: usize) -> Keyboard {
        Keyboard { whites: whites.clamp(WHITES_MIN, WHITES_MAX.min(WHITE_HIGHEST + 1 - self.low)), ..self }
    }

    /// Applique la position 0..1 du curseur `i` (SLIDER_LOW ou SLIDER_SPAN).
    /// Un curseur s'arrête quand la note la plus aiguë atteint Do8.
    pub fn with_slider(self, i: usize, t: f32) -> Keyboard {
        let step = |lo: usize, hi: usize| lo + (t.clamp(0.0, 1.0) * (hi - lo) as f32).round() as usize;
        if i == SLIDER_LOW {
            self.with_low(step(WHITE_LOWEST, LOW_MAX))
        } else {
            self.with_whites(step(WHITES_MIN, WHITES_MAX))
        }
    }

    fn slider_pos(&self, i: usize) -> f32 {
        if i == SLIDER_LOW {
            (self.low - WHITE_LOWEST) as f32 / (LOW_MAX - WHITE_LOWEST) as f32
        } else {
            (self.whites - WHITES_MIN) as f32 / (WHITES_MAX - WHITES_MIN) as f32
        }
    }

    fn first_semi(&self) -> usize {
        white_semi(self.low)
    }

    fn last_semi(&self) -> usize {
        white_semi(self.low + self.whites - 1)
    }
}

/// Demi-tons des touches blanches dans une octave.
const WHITE_STEPS: [usize; 7] = [0, 2, 4, 5, 7, 9, 11];
const NAMES: [&str; 7] = ["DO", "RE", "MI", "FA", "SOL", "LA", "SI"];

/// Fréquence de la note `semi`, le Do4 valant 261,6 Hz.
pub fn key_freq(semi: usize) -> f32 {
    261.63 * 2f32.powf((semi as f32 - 48.0) / 12.0)
}

/// Nom d'une touche blanche, octave comprise : « SOL4 ».
fn white_name(j: usize) -> String {
    format!("{}{}", NAMES[j % 7], j / 7)
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

pub const SLIDERS: usize = 4;
/// Les premiers curseurs règlent une durée, les suivants le clavier.
pub const TIME_SLIDERS: usize = 2;
const SLIDER_LOW: usize = 2;
const SLIDER_LABEL: [&str; SLIDERS] = ["ATTAQUE", "RELACHE", "NOTE BASSE", "ETENDUE"];
/// Bornes en secondes.
const SLIDER_RANGE: [(f32, f32); TIME_SLIDERS] = [(0.002, 2.0), (0.01, 4.0)];

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

const TIMBRE_NAMES: [&str; TIMBRES] = ["ORGUE", "CORDE", "PIANO", "LEAD"];

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
    /// Zone tactile de chaque bouton de timbre (toute la rangée) et son rectangle visible.
    pub timbre_zone: [Rect; TIMBRES],
    pub timbre_button: [Rect; TIMBRES],
    pub kb: Keyboard,
    /// Rectangle visible de chaque touche, indexé depuis la note la plus grave.
    pub keys: [Rect; MAX_KEYS],
    /// Touches réellement présentes dans `keys`.
    pub count: usize,
    pub gap: usize,
}

pub fn layout(w: usize, h: usize, kb: Keyboard) -> Layout {
    let kb_top = h / 2;
    // Texte : ~40 caractères sur 58 % de la largeur, 10 lignes dans la moitié haute.
    let scale = ((w * 58 / 100) / (40 * (font::W + 1))).min(kb_top / (10 * (font::H + 3))).max(1);
    let margin = 2 * scale * font::W;

    let sx = w * 62 / 100;
    let sw = w.saturating_sub(sx + margin);
    // Une rangée pour les timbres, puis une par curseur.
    let row_h = kb_top / (SLIDERS + 1);
    let label_h = (font::H + 4) * scale;
    let track_h = (3 * scale).max(6);
    let mut slider_zone = [Rect { x: 0, y: 0, w: 0, h: 0 }; SLIDERS];
    let mut slider_track = slider_zone;
    for i in 0..SLIDERS {
        let ry = (i + 1) * row_h;
        slider_zone[i] = Rect { x: sx, y: ry, w: w - sx, h: row_h };
        slider_track[i] = Rect { x: sx, y: ry + margin / 4 + label_h + scale, w: sw, h: track_h };
    }

    let mut timbre_zone = [Rect { x: 0, y: 0, w: 0, h: 0 }; TIMBRES];
    let mut timbre_button = timbre_zone;
    let button_gap = scale;
    let button_w = sw.saturating_sub((TIMBRES - 1) * button_gap) / TIMBRES;
    for i in 0..TIMBRES {
        let x = sx + i * (button_w + button_gap);
        timbre_zone[i] = Rect { x: x - if i == 0 { 0 } else { button_gap }, y: 0, w: button_w + button_gap, h: row_h };
        timbre_button[i] = Rect { x, y: margin / 4, w: button_w, h: row_h.saturating_sub(margin / 2) };
    }

    // Clavier : blanches sur toute la hauteur, noires sur ~60 % en haut,
    // centrées sur la jonction entre deux blanches.
    let ww = w / kb.whites;
    let gap = scale.max(2).min(ww / 8).max(1);
    let kb_h = h - kb_top;
    let bw = ww * 62 / 100;
    // Bord gauche de la j-ième blanche : les blanches occupent toute la largeur.
    let edge = |j: usize| j * w / kb.whites;
    let first = kb.first_semi();
    let count = kb.last_semi() - first + 1;
    let mut keys = [Rect { x: 0, y: 0, w: 0, h: 0 }; MAX_KEYS];
    for (i, r) in keys.iter_mut().take(count).enumerate() {
        let semi = first + i;
        let j = white_index(semi) - kb.low;
        *r = if is_black(semi) {
            Rect { x: edge(j + 1) - bw / 2, y: kb_top + gap, w: bw, h: kb_h * 58 / 100 }
        } else {
            let key_w = (edge(j + 1) - edge(j)).saturating_sub(2 * gap);
            Rect { x: edge(j) + gap, y: kb_top + gap, w: key_w, h: kb_h.saturating_sub(2 * gap) }
        };
    }
    Layout { w, h, scale, margin, kb_top, slider_zone, slider_track, timbre_zone, timbre_button, kb, keys, count, gap }
}

pub enum Hit {
    Key(usize),
    /// Curseur et position 0..1 sur sa piste.
    Slider(usize, f32),
    Timbre(usize),
    Nothing,
}

impl Layout {
    pub fn hit(&self, x: f32, y: f32) -> Hit {
        if self.w == 0 {
            return Hit::Nothing;
        }
        if y >= self.kb_top as f32 {
            // Les noires sont devant les blanches : on les teste d'abord.
            let first = self.kb.first_semi();
            if let Some(i) = (0..self.count).find(|&i| is_black(first + i) && self.keys[i].contains(x, y)) {
                return Hit::Key(first + i);
            }
            let j = (x.max(0.0) as usize * self.kb.whites / self.w).min(self.kb.whites - 1);
            return Hit::Key(white_semi(self.kb.low + j));
        }
        if let Some(i) = (0..TIMBRES).find(|&i| self.timbre_zone[i].contains(x, y)) {
            return Hit::Timbre(i);
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
    /// Notes tenues, indexées par demi-ton depuis Do0.
    pub held: [bool; NOTES],
    pub held_count: usize,
    /// Valeurs des curseurs de durée, en secondes.
    pub sliders: [f32; TIME_SLIDERS],
    pub kb: Keyboard,
    pub timbre: usize,
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
    let l = layout(cv.w, cv.h, view.kb);
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

    put(cv, &mut y, concat!("SIMPLETONES ", env!("CARGO_PKG_VERSION")), TEXT);
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
    let top = view.kb.low + view.kb.whites - 1;
    let clavier = format!("{} A {} - NOTES TENUES : {}", white_name(view.kb.low), white_name(top), view.held_count);
    put(cv, &mut y, &clavier, if view.held_count > 0 { ACCENT } else { DIM });

    // --- Timbres ----------------------------------------------------------
    for i in 0..TIMBRES {
        let button = l.timbre_button[i];
        let selected = view.timbre == i;
        cv.rect(button, if selected { ACCENT } else { KEY_ALT });
        let name = TIMBRE_NAMES[i];
        let nx = button.x + button.w.saturating_sub(text_width(name, scale)) / 2;
        let ny = button.y + button.h.saturating_sub(font::H * scale) / 2;
        cv.text(nx, ny, name, scale, if selected { BG } else { TEXT });
    }

    // --- Curseurs ---------------------------------------------------------
    for i in 0..SLIDERS {
        let zone = l.slider_zone[i];
        let track = l.slider_track[i];
        let (value, pos) = if i < TIME_SLIDERS {
            (fmt_time(view.sliders[i]), slider_pos(i, view.sliders[i]))
        } else if i == SLIDER_LOW {
            (white_name(view.kb.low), view.kb.slider_pos(i))
        } else {
            (format!("{} BLANCHES", view.kb.whites), view.kb.slider_pos(i))
        };
        let label = format!("{} : {value}", SLIDER_LABEL[i]);
        cv.text(track.x, zone.y + l.margin / 4, &label, scale, TEXT);
        cv.rect(track, KEY_ALT);
        let fill = (pos * track.w as f32) as usize;
        cv.rect(Rect { w: fill, ..track }, ACCENT);
        let knob = 4 * scale;
        let kx = (track.x + fill).saturating_sub(knob / 2).min(track.x + track.w - knob / 2);
        cv.rect(Rect { x: kx, y: track.y + track.h / 2 - knob / 2, w: knob, h: knob }, TEXT);
    }

    // --- Clavier ----------------------------------------------------------
    let first = view.kb.first_semi();
    // Touches trop étroites pour tous les noms : seuls les Do restent indiqués.
    let all_names = text_width("SOL", scale) <= l.keys[0].w;
    for i in (0..l.count).filter(|&i| !is_black(first + i)) {
        let (semi, key) = (first + i, l.keys[i]);
        let held = view.held[semi];
        cv.rect(key, if held { ACCENT } else { KEY_WHITE });
        let j = white_index(semi);
        let name = NAMES[j % 7];
        if (all_names || j % 7 == 0) && text_width(name, scale) <= key.w {
            let nx = key.x + key.w.saturating_sub(text_width(name, scale)) / 2;
            let ny = key.y + key.h.saturating_sub((font::H + 6) * scale);
            cv.text(nx, ny, name, scale, if held { BG } else { DIM });
        }
    }
    for i in (0..l.count).filter(|&i| is_black(first + i)) {
        let key = l.keys[i];
        // Liseré couleur de fond pour détacher la noire des blanches voisines.
        let g = l.gap;
        cv.rect(Rect { x: key.x.saturating_sub(g), y: key.y, w: key.w + 2 * g, h: key.h + g }, BG);
        cv.rect(key, if view.held[first + i] { ACCENT } else { KEY_BLACK });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tous les claviers atteignables, curseurs aux quatre coins compris.
    fn all_keyboards() -> Vec<Keyboard> {
        let mut v = Vec::new();
        for low in 0..=70 {
            for whites in 0..=40 {
                let kb = Keyboard::sanitized(low, whites);
                if !v.contains(&kb) {
                    v.push(kb);
                }
            }
        }
        v
    }

    #[test]
    fn defaut_identique_a_la_version_precedente() {
        let kb = Keyboard::DEFAULT;
        assert_eq!((kb.first_semi(), kb.last_semi()), (48, 72));
        assert_eq!(layout(2000, 1200, kb).count, 25);
        assert!((key_freq(48) - 261.63).abs() < 0.01);
        assert!((key_freq(57) - 440.0).abs() < 0.1);
        assert_eq!(white_name(kb.low), "DO4");
        assert_eq!(white_name(kb.low + kb.whites - 1), "DO6");
    }

    #[test]
    fn bornes_respectees() {
        for kb in all_keyboards() {
            assert!(kb.low >= WHITE_LOWEST && kb.low + kb.whites - 1 <= WHITE_HIGHEST, "{kb:?}");
            assert!((WHITES_MIN..=WHITES_MAX).contains(&kb.whites), "{kb:?}");
            assert!(kb.last_semi() < NOTES);
            assert!(kb.last_semi() - kb.first_semi() < MAX_KEYS, "{kb:?}");
            assert!(!is_black(kb.first_semi()) && !is_black(kb.last_semi()));
        }
    }

    #[test]
    fn curseurs_s_arretent_a_do8() {
        let kb = Keyboard::DEFAULT.with_slider(SLIDER_LOW, 1.0);
        assert_eq!(kb.low + kb.whites - 1, WHITE_HIGHEST);
        assert_eq!(kb.with_slider(3, 1.0), kb); // plus de place vers l'aigu
        let grave = Keyboard::DEFAULT.with_slider(SLIDER_LOW, 0.0);
        assert_eq!(grave.low, WHITE_LOWEST);
        assert_eq!(grave.with_slider(3, 1.0).whites, WHITES_MAX);
        assert_eq!(grave.with_slider(3, 0.0).whites, WHITES_MIN);
    }

    #[test]
    fn position_du_curseur_fait_l_aller_retour() {
        for kb in all_keyboards() {
            for i in [SLIDER_LOW, 3] {
                assert_eq!(kb.with_slider(i, kb.slider_pos(i)), kb, "curseur {i}");
            }
        }
    }

    #[test]
    fn toutes_les_touches_visibles_et_touchables() {
        for (w, h) in [(2000, 1200), (2400, 1080), (800, 480), (1, 1)] {
            for kb in all_keyboards() {
                let l = layout(w, h, kb);
                let first = kb.first_semi();
                for i in 0..l.count {
                    let r = l.keys[i];
                    assert!(r.x + r.w <= w, "{kb:?} touche {i} deborde en {w}x{h}");
                    if r.w == 0 || r.h == 0 {
                        continue;
                    }
                    let (cx, cy) = ((r.x + r.w / 2) as f32, (r.y + r.h - 1) as f32);
                    if w > 100 {
                        // Le bas d'une noire tombe sur elle, celui d'une blanche aussi.
                        assert!(matches!(l.hit(cx, cy), Hit::Key(s) if s == first + i), "{kb:?} touche {i}");
                    }
                }
                // Partout sur la largeur, le doigt tombe sur une note du clavier.
                for x in (0..w).step_by(7) {
                    for y in [l.kb_top, h - 1] {
                        if let Hit::Key(s) = l.hit(x as f32, y as f32) {
                            assert!(s >= first && s <= kb.last_semi(), "{kb:?} note {s}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn boutons_de_timbre_touchables_et_lisibles() {
        for (w, h) in [(2000, 1200), (1920, 1200), (2400, 1080), (800, 480)] {
            let l = layout(w, h, Keyboard::DEFAULT);
            for i in 0..TIMBRES {
                let b = l.timbre_button[i];
                assert!(b.x + b.w <= w && b.y + b.h <= l.slider_zone[0].y, "{w}x{h} bouton {i}");
                assert!(text_width(TIMBRE_NAMES[i], l.scale) <= b.w, "{w}x{h} bouton {i}");
                let (cx, cy) = ((b.x + b.w / 2) as f32, (b.y + b.h / 2) as f32);
                assert!(matches!(l.hit(cx, cy), Hit::Timbre(t) if t == i), "{w}x{h} bouton {i}");
            }
            // Le texte des curseurs ne déborde pas sur la rangée suivante.
            for i in 0..SLIDERS {
                let t = l.slider_track[i];
                assert!(t.y + t.h <= l.slider_zone[i].y + l.slider_zone[i].h, "{w}x{h} curseur {i}");
            }
        }
    }

    #[test]
    fn rendu_sans_debordement() {
        let st = Stats::default();
        for (w, h) in [(2000, 1200), (800, 480)] {
            for kb in all_keyboards() {
                let mut px = vec![0u32; w * h];
                let mut cv = Canvas { px: &mut px, w, h, stride: w };
                let view = View { held: [true; NOTES], held_count: 3, sliders: [0.01, 0.4], kb, timbre: 1 };
                render(&mut cv, &st, &view);
            }
        }
    }
}
