#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use chrono::TimeZone;
use eframe::egui::{self, epaint::Vertex, *};
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    io::{BufRead, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SPLASH: f64 = 2.6;
const GB: f64 = 1073741824.0;
const VOL: f32 = 0.8;
const SR: f32 = 44100.0;
const BASES: [&str; 2] = [
    "https://cdn.cloudflare.steamstatic.com/steam/apps",
    "https://shared.akamai.steamstatic.com/store_item_assets/steam/apps",
];
const CORNERS: [&str; 4] = ["Top-left", "Top-right", "Bottom-left", "Bottom-right"];

type Stats = Arc<Mutex<Map<String, Value>>>;

const ACC: Color32 = Color32::from_rgb(10, 132, 255);
fn fb(sz: f32) -> FontId {
    FontId::new(sz, FontFamily::Name("bold".into()))
}

#[derive(Clone, PartialEq)]
enum Kind {
    Folder,
    Zip,
}

struct Game {
    name: String,
    path: PathBuf,
    kind: Kind,
    exe: Option<PathBuf>,
}

#[derive(Default, Clone)]
struct Meta {
    appid: u64,
    desc: String,
    dev: String,
    genres: String,
    release: String,
}

#[derive(Default)]
struct Assets {
    poster: Option<TextureHandle>,
    poster_s: Option<TextureHandle>,
    hero: Option<TextureHandle>,
    blur: Option<TextureHandle>,
    logo: Option<TextureHandle>,
    meta: Option<Meta>,
    size: Option<u64>,
    born: f64, // when the poster/hero bundle arrived (for the fade-in)
}

enum Msg {
    Poster(ColorImage, ColorImage),
    Hero(ColorImage, ColorImage),
    Logo(ColorImage),
    Meta(Meta),
    Size(u64),
    Batch(Vec<Msg>), // everything for one game, applied in a single frame
}

#[derive(Default)]
struct Job {
    progress: f32,
    done: bool,
    error: Option<String>,
}

#[derive(Default, Clone)]
struct Metrics {
    running: bool,
    name: String,
    fps: Option<f32>,
    low1: Option<f32>,
    fps_hist: Vec<f32>,
    cpu_hist: Vec<f32>,
    game_cpu: f32,
    game_ram: u64,
    sys_cpu: f32,
    ram_used: u64,
    ram_total: u64,
    gpu: Option<(f32, u64, u64, f32)>, // util %, vram used MB, vram total MB, temp C
    uptime: u64,
    pm_found: bool,
}

#[derive(Default)]
struct Prefs {
    fav: HashSet<String>,
    exe: HashMap<String, String>,
    corner: u8,
    mute: bool,
}

impl Prefs {
    fn load(p: &Path) -> Self {
        let v = fs::read_to_string(p).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null);
        Prefs {
            fav: v["fav"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
            exe: v["exe"]
                .as_object()
                .map(|o| o.iter().filter_map(|(k, x)| x.as_str().map(|s| (k.clone(), s.to_string()))).collect())
                .unwrap_or_default(),
            corner: v["corner"].as_u64().unwrap_or(0) as u8,
            mute: v["mute"].as_bool().unwrap_or(false),
        }
    }
    fn save(&self, p: &Path) {
        let _ = fs::write(p, json!({"fav":self.fav,"exe":self.exe,"corner":self.corner,"mute":self.mute}).to_string());
    }
}

impl Meta {
    fn to_json(&self) -> Value {
        json!({"appid":self.appid,"desc":self.desc,"dev":self.dev,"genres":self.genres,"release":self.release})
    }
    fn from_json(v: &Value) -> Self {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        Meta { appid: v["appid"].as_u64().unwrap_or(0), desc: s("desc"), dev: s("dev"), genres: s("genres"), release: s("release") }
    }
}

#[derive(Clone, Copy)]
enum Pad {
    Left,
    Right,
    A,
    B,
    X,
    Y,
    LB,
    RB,
}

#[derive(Default)]
struct Act {
    left: bool,
    right: bool,
    ok: bool,
    back: bool,
    props: bool,
    fav: bool,
    prev: bool,
    next: bool,
    mute: bool,
}

// ---------- helpers ----------
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
fn fmt_size(b: u64) -> String {
    let g = b as f64 / GB;
    if g >= 1.0 { format!("{g:.1} GB") } else { format!("{:.0} MB", b as f64 / 1048576.0) }
}
fn fmt_time(s: u64) -> String {
    if s < 60 { "—".into() } else if s < 3600 { format!("{}m", s / 60) } else { format!("{}h {}m", s / 3600, (s % 3600) / 60) }
}
fn fmt_last(ts: u64) -> String {
    if ts == 0 {
        return "Never".into();
    }
    chrono::Local.timestamp_opt(ts as i64, 0).single().map(|d| d.format("%b %d, %Y").to_string()).unwrap_or("—".into())
}
fn dash(s: &str) -> String {
    if s.is_empty() { "—".into() } else { s.to_string() }
}
fn trunc(s: &str, n: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() <= n { dash(s) } else { format!("{}…", c[..n - 1].iter().collect::<String>()) }
}
fn elide(s: &str, n: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() <= n { s.into() } else { format!("…{}", c[c.len() - n + 1..].iter().collect::<String>()) }
}
fn tint(name: &str) -> Color32 {
    let mut h: u32 = 2166136261;
    for b in name.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    Color32::from(Rgba::from(egui::ecolor::Hsva::new((h % 360) as f32 / 360.0, 0.55, 0.5, 1.0)))
}
fn open_path(p: &Path) {
    #[cfg(windows)]
    let c = "explorer";
    #[cfg(target_os = "macos")]
    let c = "open";
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let c = "xdg-open";
    let _ = Command::new(c).arg(p).spawn();
}
fn quiet(c: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x08000000);
    }
    #[cfg(not(windows))]
    {
        let _ = c;
    }
}
fn csense() -> Sense {
    Sense { focusable: false, ..Sense::click() }
}

// ---------- sound effects (synthesized; override with sfx/<name>.wav next to the exe) ----------
#[derive(Clone, Copy)]
enum Sfx {
    Startup,
    Move,
    Bump,
    Tick,
    Tab,
    Select,
    Back,
    Fav,
    Unfav,
    Open,
    Close,
    Launch,
    Done,
    Error,
}

impl Sfx {
    fn file(self) -> &'static str {
        match self {
            Sfx::Startup => "startup",
            Sfx::Move => "move",
            Sfx::Bump => "bump",
            Sfx::Tick => "tick",
            Sfx::Tab => "tab",
            Sfx::Select => "select",
            Sfx::Back => "back",
            Sfx::Fav => "fav",
            Sfx::Unfav => "unfav",
            Sfx::Open => "open",
            Sfx::Close => "close",
            Sfx::Launch => "launch",
            Sfx::Done => "done",
            Sfx::Error => "error",
        }
    }
}

struct Rng(u32);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0 as f32 / u32::MAX as f32 * 2.0 - 1.0
    }
}

fn lowpass(v: &mut [f32], hz: f32) {
    let a = 1.0 - (-2.0 * std::f32::consts::PI * hz / SR).exp();
    let mut y = 0.0;
    for x in v.iter_mut() {
        y += a * (*x - y);
        *x = y;
    }
}

/// Soft bell-like note: sine + faint octave + faint inharmonic partial, smooth attack, exponential decay.
fn note(dur: f32, f0: f32, f1: f32, vol: f32, atk: f32, decay: f32) -> Vec<f32> {
    let n = (dur * SR) as usize;
    let tau = 2.0 * std::f32::consts::PI;
    let (mut p1, mut p2, mut p3) = (0.0f32, 0.0f32, 0.0f32);
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f32 / SR;
        let k = i as f32 / n as f32;
        let f = f0 + (f1 - f0) * k;
        p1 += tau * f / SR;
        p2 += tau * f * 2.0 / SR;
        p3 += tau * f * 2.76 / SR;
        let a = if t < atk { 0.5 - 0.5 * (std::f32::consts::PI * t / atk).cos() } else { 1.0 };
        let tail = ((1.0 - k) / 0.1).min(1.0);
        let e = a * (-t * decay).exp() * tail;
        let s = p1.sin() + 0.22 * p2.sin() * (-t * decay * 1.8).exp() + 0.10 * p3.sin() * (-t * decay * 3.0).exp();
        v.push(s * e * vol);
    }
    v
}

/// Soft filtered-noise "air" burst.
fn puff(dur: f32, hz: f32, vol: f32, seed: u32) -> Vec<f32> {
    let n = (dur * SR) as usize;
    let mut r = Rng(seed | 1);
    let mut v: Vec<f32> = (0..n)
        .map(|i| {
            let k = i as f32 / n as f32;
            r.next() * (std::f32::consts::PI * k).sin().powi(2) * vol
        })
        .collect();
    lowpass(&mut v, hz);
    v
}

fn mix(dst: &mut Vec<f32>, src: &[f32], at: f32) {
    let off = (at * SR) as usize;
    if dst.len() < off + src.len() {
        dst.resize(off + src.len(), 0.0);
    }
    for (i, s) in src.iter().enumerate() {
        dst[off + i] += s;
    }
}

/// Small room reverb (4 feedback combs) for the airy, "console UI" feel.
fn reverb(v: &mut Vec<f32>, wet: f32, tail: f32) {
    v.resize(v.len() + (tail * SR) as usize, 0.0);
    let dry = v.clone();
    for (d, g) in [(0.0297f32, 0.62f32), (0.0371, 0.58), (0.0411, 0.54), (0.0437, 0.5)] {
        let dl = (d * SR) as usize;
        let mut buf = dry.clone();
        for i in dl..buf.len() {
            buf[i] += buf[i - dl] * g;
        }
        for i in 0..v.len() {
            v[i] += (buf[i] - dry[i]) * wet * 0.3;
        }
    }
    lowpass(v, 6500.0);
}

fn fin(mut o: Vec<f32>, peak: f32) -> Vec<f32> {
    let m = o.iter().fold(0.0f32, |a, x| a.max(x.abs())).max(1e-6);
    let g = peak / m;
    let n = o.len();
    let f = ((0.015 * SR) as usize).max(1);
    for (i, x) in o.iter_mut().enumerate() {
        *x *= g;
        if i + f > n {
            *x *= (n - i) as f32 / f as f32;
        }
    }
    o
}

fn synth(s: Sfx) -> Vec<f32> {
    let mut o: Vec<f32> = vec![];
    // (reverb wet, reverb tail seconds, peak level)
    let (wet, tail, peak) = match s {
        Sfx::Move => {
            mix(&mut o, &note(0.10, 784.0, 740.0, 1.0, 0.004, 34.0), 0.0);
            mix(&mut o, &puff(0.025, 3000.0, 0.15, 7), 0.0);
            (0.2, 0.12, 0.20)
        }
        Sfx::Bump => {
            mix(&mut o, &note(0.12, 196.0, 170.0, 1.0, 0.006, 26.0), 0.0);
            (0.1, 0.1, 0.30)
        }
        Sfx::Tick => {
            mix(&mut o, &note(0.04, 1568.0, 1500.0, 1.0, 0.002, 80.0), 0.0);
            (0.1, 0.05, 0.12)
        }
        Sfx::Tab => {
            mix(&mut o, &note(0.12, 659.0, 659.0, 1.0, 0.004, 22.0), 0.0);
            mix(&mut o, &note(0.16, 880.0, 880.0, 1.0, 0.004, 20.0), 0.045);
            (0.3, 0.2, 0.24)
        }
        Sfx::Select => {
            mix(&mut o, &note(0.28, 659.0, 659.0, 1.0, 0.004, 11.0), 0.0);
            mix(&mut o, &note(0.40, 988.0, 988.0, 1.0, 0.004, 9.0), 0.07);
            (0.45, 0.35, 0.32)
        }
        Sfx::Back => {
            mix(&mut o, &note(0.20, 784.0, 784.0, 1.0, 0.004, 14.0), 0.0);
            mix(&mut o, &note(0.28, 587.0, 587.0, 1.0, 0.004, 12.0), 0.07);
            (0.35, 0.3, 0.26)
        }
        Sfx::Fav => {
            for (f, at) in [(1047.0, 0.0), (1319.0, 0.07), (1568.0, 0.14), (2093.0, 0.21)] {
                mix(&mut o, &note(0.35, f, f, 1.0, 0.004, 10.0), at);
            }
            (0.5, 0.4, 0.26)
        }
        Sfx::Unfav => {
            for (f, at) in [(1319.0, 0.0), (1047.0, 0.07)] {
                mix(&mut o, &note(0.25, f, f, 1.0, 0.004, 14.0), at);
            }
            (0.3, 0.25, 0.22)
        }
        Sfx::Open => {
            mix(&mut o, &puff(0.22, 1800.0, 0.25, 11), 0.0);
            mix(&mut o, &note(0.30, 440.0, 660.0, 1.0, 0.06, 8.0), 0.0);
            mix(&mut o, &note(0.40, 988.0, 988.0, 0.7, 0.01, 9.0), 0.12);
            (0.4, 0.3, 0.30)
        }
        Sfx::Close => {
            mix(&mut o, &note(0.22, 660.0, 440.0, 1.0, 0.01, 14.0), 0.0);
            (0.35, 0.25, 0.24)
        }
        Sfx::Launch => {
            for f in [130.8, 196.0, 261.6, 392.0, 523.3] {
                mix(&mut o, &note(1.6, f, f, 0.5, 0.35, 2.2), 0.0);
            }
            mix(&mut o, &note(1.0, 1568.0, 1568.0, 0.2, 0.2, 3.0), 0.35);
            mix(&mut o, &puff(1.2, 2200.0, 0.08, 5), 0.0);
            lowpass(&mut o, 3500.0);
            (0.7, 0.8, 0.40)
        }
        Sfx::Done => {
            mix(&mut o, &note(0.35, 784.0, 784.0, 1.0, 0.004, 9.0), 0.0);
            mix(&mut o, &note(0.60, 1047.0, 1047.0, 1.0, 0.004, 6.0), 0.12);
            (0.5, 0.5, 0.30)
        }
        Sfx::Error => {
            mix(&mut o, &note(0.18, 165.0, 140.0, 1.0, 0.006, 20.0), 0.0);
            mix(&mut o, &note(0.22, 150.0, 125.0, 1.0, 0.006, 18.0), 0.16);
            lowpass(&mut o, 900.0);
            (0.15, 0.15, 0.30)
        }
        Sfx::Startup => {
            for (f, at) in [(130.8, 0.0), (196.0, 0.1), (261.6, 0.2), (329.6, 0.35), (392.0, 0.5)] {
                mix(&mut o, &note(2.4, f, f, 0.5, 0.5, 1.5), at);
            }
            for (f, at) in [(1047.0, 0.9), (1568.0, 1.1), (2093.0, 1.3)] {
                mix(&mut o, &note(1.2, f, f, 0.18, 0.15, 3.0), at);
            }
            mix(&mut o, &puff(1.5, 1500.0, 0.05, 3), 0.1);
            lowpass(&mut o, 3500.0);
            (0.8, 1.0, 0.42)
        }
    };
    reverb(&mut o, wet, tail);
    fin(o, peak)
}

/// Audio thread: opens the output device on demand, closes it after 20 s idle.
fn spawn_audio(rx: mpsc::Receiver<Sfx>) {
    thread::spawn(move || {
        use rodio::Source;
        let sfx_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("sfx")));
        let mut bank: HashMap<&'static str, Vec<f32>> = HashMap::new();
        let mut out: Option<(rodio::OutputStream, rodio::OutputStreamHandle)> = None;
        loop {
            let s = match rx.recv_timeout(Duration::from_secs(20)) {
                Ok(s) => s,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    out = None;
                    continue;
                }
                Err(_) => break,
            };
            if out.is_none() {
                out = rodio::OutputStream::try_default().ok();
                if out.is_none() {
                    continue;
                }
            }
            let h = &out.as_ref().unwrap().1;
            if let Some(dir) = &sfx_dir {
                if let Ok(f) = fs::File::open(dir.join(format!("{}.wav", s.file()))) {
                    if let Ok(d) = rodio::Decoder::new(std::io::BufReader::new(f)) {
                        let _ = h.play_raw(d.convert_samples::<f32>().amplify(VOL));
                        continue;
                    }
                }
            }
            let buf = bank.entry(s.file()).or_insert_with(|| synth(s)).clone();
            let _ = h.play_raw(rodio::buffer::SamplesBuffer::new(1, 44100, buf).amplify(VOL));
        }
    });
}

// ---------- scanning ----------
fn ok_name(n: &str) -> bool {
    let ok = n.ends_with(".exe") || n.ends_with(".sh") || n.ends_with(".appimage") || n.ends_with(".x86_64");
    let bad = ["unins", "crash", "redist", "setup", "dxsetup", "vc_"].iter().any(|b| n.contains(b));
    ok && !bad
}

fn find_exe(dir: &Path, depth: u32) -> Option<PathBuf> {
    let mut subs = vec![];
    for e in fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            subs.push(p);
            continue;
        }
        let Some(n) = p.file_name().and_then(|n| n.to_str()).map(|n| n.to_lowercase()) else { continue };
        if ok_name(&n) {
            return Some(p);
        }
    }
    if depth > 0 {
        for s in subs {
            if let Some(x) = find_exe(&s, depth - 1) {
                return Some(x);
            }
        }
    }
    None
}

fn find_exes(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if depth > 0 {
                find_exes(&p, depth - 1, out);
            }
            continue;
        }
        if p.file_name().and_then(|n| n.to_str()).map_or(false, |n| ok_name(&n.to_lowercase())) {
            out.push(p);
        }
        if out.len() >= 12 {
            return;
        }
    }
}

fn scan(dir: &Path, prefs: &Prefs) -> Vec<Game> {
    let mut out = vec![];
    let Ok(rd) = fs::read_dir(dir) else { return out };
    for p in rd.flatten().map(|e| e.path()) {
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        if stem.starts_with('.') || stem.is_empty() {
            continue;
        }
        if p.is_dir() {
            let exe = prefs.exe.get(&stem).map(PathBuf::from).filter(|x| x.exists()).or_else(|| find_exe(&p, 2));
            out.push(Game { name: stem, exe, path: p, kind: Kind::Folder });
        } else if p.extension().map_or(false, |e| e.eq_ignore_ascii_case("zip")) && !dir.join(&stem).is_dir() {
            out.push(Game { name: stem, exe: None, path: p, kind: Kind::Zip });
        }
    }
    out.sort_by_key(|g| g.name.to_lowercase());
    out
}

fn extract(zip_path: PathBuf, dest: PathBuf, job: Arc<Mutex<Job>>, ctx: Context) {
    thread::spawn(move || {
        // The source zip is only ever opened read-only; files go to a hidden staging folder first.
        let tmp = dest.with_file_name(format!(".{}.part", dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
        let r = (|| -> Result<(), Box<dyn std::error::Error>> {
            let mut ar = zip::ZipArchive::new(fs::File::open(&zip_path)?)?;
            let total: u64 = (0..ar.len()).filter_map(|i| ar.by_index_raw(i).ok().map(|f| f.size())).sum::<u64>().max(1);
            let (mut done_b, mut last_b) = (0u64, 0u64);
            let mut buf = vec![0u8; 256 * 1024];
            let _ = fs::remove_dir_all(&tmp);
            fs::create_dir_all(&tmp)?;
            for i in 0..ar.len() {
                let mut f = ar.by_index(i)?;
                let Some(rel) = f.enclosed_name() else { continue };
                let out = tmp.join(rel);
                if f.is_dir() {
                    fs::create_dir_all(&out)?;
                } else {
                    if let Some(p) = out.parent() {
                        fs::create_dir_all(p)?;
                    }
                    let mut o = fs::File::create(&out)?;
                    loop {
                        let k = std::io::Read::read(&mut f, &mut buf)?;
                        if k == 0 {
                            break;
                        }
                        std::io::Write::write_all(&mut o, &buf[..k])?;
                        done_b += k as u64;
                        if done_b - last_b > 1_000_000 {
                            last_b = done_b;
                            job.lock().unwrap().progress = (done_b as f32 / total as f32).min(0.99);
                            ctx.request_repaint();
                        }
                    }
                }
            }
            fs::rename(&tmp, &dest)?;
            Ok(())
        })();
        if r.is_err() {
            let _ = fs::remove_dir_all(&tmp);
        }
        let mut j = job.lock().unwrap();
        j.error = r.err().map(|e| e.to_string());
        j.done = true;
        ctx.request_repaint();
    });
}

// ---------- asset fetching (background threads) ----------
fn http_get(url: &str) -> Option<Vec<u8>> {
    let r = ureq::AgentBuilder::new().timeout(Duration::from_secs(8)).build().get(url).call().ok()?;
    let mut v = vec![];
    r.into_reader().take(30_000_000).read_to_end(&mut v).ok()?;
    Some(v)
}
fn enc(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}
fn clean(n: &str) -> String {
    let cut = n.find(|c| c == '(' || c == '[').unwrap_or(n.len());
    n[..cut].replace(|c: char| matches!(c, '_' | '.' | '-'), " ").split_whitespace().collect::<Vec<_>>().join(" ")
}
fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"").replace("&amp;", "&").replace("&#39;", "'").replace("&nbsp;", " ")
}
fn steam_search(name: &str) -> Option<u64> {
    let q = clean(name);
    let url = format!("https://store.steampowered.com/api/storesearch/?term={}&l=english&cc=US", enc(&q));
    let v: Value = serde_json::from_slice(&http_get(&url)?).ok()?;
    let items = v["items"].as_array()?;
    let best = items.iter().find(|i| i["name"].as_str().map_or(false, |n| n.eq_ignore_ascii_case(&q))).or(items.first())?;
    best["id"].as_u64()
}
fn fetch_details(id: u64) -> Meta {
    let mut m = Meta { appid: id, ..Default::default() };
    let url = format!("https://store.steampowered.com/api/appdetails?appids={id}&l=english");
    if let Some(v) = http_get(&url).and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
        let d = &v[id.to_string()]["data"];
        m.desc = unescape(d["short_description"].as_str().unwrap_or(""));
        m.dev = d["developers"][0].as_str().unwrap_or("").into();
        m.release = d["release_date"]["date"].as_str().unwrap_or("").into();
        m.genres = d["genres"]
            .as_array()
            .map(|a| a.iter().filter_map(|g| g["description"].as_str()).take(3).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
    }
    m
}
fn read_meta(cache: &Path) -> Option<Meta> {
    let v: Value = serde_json::from_str(&fs::read_to_string(cache.join("meta.json")).ok()?).ok()?;
    Some(Meta::from_json(&v))
}
fn resolve_meta(name: &str, folder: Option<&Path>, cache: &Path) -> Option<Meta> {
    let over = |d: &Path| fs::read_to_string(d.join("appid.txt")).ok().and_then(|s| s.trim().parse::<u64>().ok());
    let id = folder.and_then(over).or_else(|| over(cache)).or_else(|| steam_search(name))?;
    let m = fetch_details(id);
    let _ = fs::write(cache.join("meta.json"), m.to_json().to_string());
    Some(m)
}
/// Finds poster / backdrop / logo images inside a game folder (up to 2 levels deep) by file name,
/// falling back to image shape for loose files in the folder root. Best candidate first.
fn scan_images(dir: &Path) -> [Vec<PathBuf>; 3] {
    // kind order: 0 poster, 1 hero, 2 logo
    const KEYS: [&[&str]; 3] = [
        &["poster", "cover", "library_600x900", "portrait", "boxart", "box_art"],
        &["hero", "library_hero", "background", "backdrop", "fanart", "wallpaper", "banner", "header"],
        &["library_logo", "logo"],
    ];
    let mut c: [Vec<(usize, usize, PathBuf)>; 3] = Default::default();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    let mut seen = 0usize;
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > 800 {
                break;
            }
            let p = e.path();
            if p.is_dir() {
                if depth < 2 {
                    stack.push((p, depth + 1));
                }
                continue;
            }
            let ext = p.extension().and_then(|x| x.to_str()).map(|x| x.to_lowercase()).unwrap_or_default();
            if !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "webp" | "bmp") {
                continue;
            }
            if !(4_000..=30_000_000).contains(&e.metadata().map_or(0, |m| m.len())) {
                continue;
            }
            let stem = p.file_stem().and_then(|x| x.to_str()).map(|x| x.to_lowercase()).unwrap_or_default();
            let Ok((w, h)) = image::image_dimensions(&p) else { continue };
            let mut kind = None;
            for k in [2usize, 0, 1] {
                if let Some(pr) = KEYS[k].iter().position(|w| stem.contains(w)) {
                    kind = Some((k, pr));
                    break;
                }
            }
            if kind.is_none() && matches!(stem.as_str(), "folder" | "front" | "box") {
                kind = Some((0, 90));
            }
            if kind.is_none() && depth == 0 {
                // unnamed loose image: guess from its shape
                if h * 10 >= w * 13 && w >= 300 {
                    kind = Some((0, 99));
                } else if w * 10 >= h * 17 && w >= 1280 {
                    kind = Some((1, 99));
                }
            }
            let Some((k, pr)) = kind else { continue };
            if (k == 1 && w < 800) || (k == 0 && w < 150) {
                continue;
            }
            c[k].push((depth, pr, p));
        }
    }
    let mut out: [Vec<PathBuf>; 3] = Default::default();
    for k in 0..3 {
        c[k].sort_by_key(|(d, pr, _)| (*d, *pr));
        out[k] = c[k].iter().map(|(_, _, p)| p.clone()).collect();
    }
    out
}
fn find_or_get(local: &[PathBuf], cache: &Path, urls: &[String]) -> Option<PathBuf> {
    if let Some(p) = local.iter().find(|p| p.exists()) {
        // keep a copy in the cache so the image survives the folder moving / a slow or missing drive
        let same = match (fs::metadata(p), fs::metadata(cache)) {
            (Ok(a), Ok(b)) => a.len() == b.len(),
            _ => false,
        };
        if !same {
            let _ = fs::copy(p, cache);
        }
        return Some(p.clone());
    }
    if fs::metadata(cache).map_or(false, |m| m.len() > 0) {
        return Some(cache.to_path_buf());
    }
    for u in urls {
        if let Some(b) = http_get(u) {
            if !b.is_empty() && fs::write(cache, &b).is_ok() {
                return Some(cache.to_path_buf());
            }
        }
    }
    None
}
fn dir_size(d: &Path) -> u64 {
    fs::read_dir(d)
        .map(|rd| {
            rd.flatten()
                .map(|e| match e.metadata() {
                    Ok(m) if m.is_dir() => dir_size(&e.path()),
                    Ok(m) => m.len(),
                    _ => 0,
                })
                .sum()
        })
        .unwrap_or(0)
}
fn ci(img: &image::RgbaImage) -> ColorImage {
    ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw())
}
fn decode(k: usize, p: &Path) -> Option<Msg> {
    use image::imageops::FilterType::{CatmullRom, Triangle};
    let img = image::ImageReader::open(p).ok()?.with_guessed_format().ok()?.decode().ok()?;
    Some(match k {
        0 => Msg::Poster(
            ci(&img.resize_to_fill(560, 840, CatmullRom).to_rgba8()),
            ci(&img.resize_to_fill(260, 390, CatmullRom).to_rgba8()),
        ),
        1 => {
            let small = image::imageops::blur(&img.resize(256, 256, Triangle).to_rgba8(), 3.0);
            let hero = if img.width() > 1920 { img.resize(1920, 1920, CatmullRom) } else { img };
            Msg::Hero(ci(&hero.to_rgba8()), ci(&small))
        }
        _ => {
            let lg = if img.width() > 600 || img.height() > 240 { img.resize(600, 240, CatmullRom) } else { img };
            Msg::Logo(ci(&lg.to_rgba8()))
        }
    })
}

fn apply_msg(ctx: &Context, n: &str, a: &mut Assets, m: Msg, now: f64) {
    let lt = |s: &str, c: ColorImage| ctx.load_texture(format!("{n}-{s}"), c, TextureOptions::LINEAR);
    match m {
        Msg::Poster(c, sm) => {
            a.poster = Some(lt("p", c));
            a.poster_s = Some(lt("ps", sm));
            a.born = now;
        }
        Msg::Hero(c, b) => {
            a.hero = Some(lt("h", c));
            a.blur = Some(lt("b", b));
            a.born = now;
        }
        Msg::Logo(c) => a.logo = Some(lt("l", c)),
        Msg::Meta(m) => a.meta = Some(m),
        Msg::Size(s) => a.size = Some(s),
        Msg::Batch(v) => {
            for m in v {
                apply_msg(ctx, n, a, m, now);
            }
        }
    }
}

fn load_game(name: String, folder: Option<PathBuf>, zip_len: u64, cache: PathBuf, tx: mpsc::Sender<(String, Msg)>, ctx: Context) {
    thread::spawn(move || {
        let send = |m: Msg| {
            let _ = tx.send((name.clone(), m));
            ctx.request_repaint();
        };
        let _ = fs::create_dir_all(&cache);
        let found = folder.as_deref().map(scan_images).unwrap_or_default();
        let kinds: [(&str, &str); 3] = [("poster", "library_600x900.jpg"), ("hero", "library_hero.jpg"), ("logo", "logo.png")];
        let mut got: [Option<Msg>; 3] = [None, None, None];
        let fresh_none = |stem: &str| {
            fs::metadata(cache.join(format!("{stem}.none")))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map_or(false, |e| e.as_secs() < 1_800)
        };
        let mut meta = read_meta(&cache);
        for pass in 0..2 {
            if pass == 1 && got.iter().all(|g| g.is_some()) {
                break;
            }
            if pass == 1 && meta.is_none() {
                meta = resolve_meta(&name, folder.as_deref(), &cache);
            }
            let id = if pass == 1 { meta.as_ref().map(|m| m.appid).filter(|&i| i > 0) } else { None };
            if pass == 1 && id.is_none() {
                break;
            }
            for (k, (stem, remote)) in kinds.iter().enumerate() {
                if got[k].is_some() || (pass == 1 && fresh_none(stem)) {
                    continue;
                }
                let urls: Vec<String> = id.map(|id| BASES.iter().map(|b| format!("{b}/{id}/{remote}")).collect()).unwrap_or_default();
                if let Some(p) = find_or_get(&found[k], &cache.join(format!("{stem}.img")), &urls) {
                    got[k] = decode(k, &p);
                }
                if pass == 1 && got[k].is_none() {
                    let _ = fs::write(cache.join(format!("{stem}.none")), b"");
                }
            }
        }
        // one message per game: poster, backdrop, logo and meta show up together
        let mut batch = vec![];
        if let Some(m) = &meta {
            batch.push(Msg::Meta(m.clone()));
        }
        batch.extend(got.into_iter().flatten());
        send(Msg::Batch(batch));
        let sz = match &folder {
            Some(d) => dir_size(d),
            None => zip_len,
        };
        send(Msg::Size(sz));
    });
}

fn stat_add(stats: &Stats, file: &Path, name: &str, secs: u64, launch: bool) {
    let mut m = stats.lock().unwrap();
    let e = m.entry(name.to_string()).or_insert_with(|| json!({"last":0,"secs":0,"n":0}));
    if launch {
        e["n"] = json!(e["n"].as_u64().unwrap_or(0) + 1);
        e["last"] = json!(now());
    }
    e["secs"] = json!(e["secs"].as_u64().unwrap_or(0) + secs);
    let _ = fs::write(file, serde_json::to_string(&*m).unwrap_or_default());
}

// ---------- gamepad (polled on its own thread, paused while gaming) ----------
fn spawn_pad(tx: mpsc::Sender<Pad>, paused: Arc<AtomicBool>, ctx: Context) {
    thread::spawn(move || {
        let Ok(mut g) = gilrs::Gilrs::new() else { return };
        let mut sx = 0i8;
        loop {
            if paused.load(Ordering::Relaxed) {
                while g.next_event().is_some() {}
                thread::sleep(Duration::from_millis(500));
                continue;
            }
            while let Some(ev) = g.next_event() {
                use gilrs::{Axis, Button as B, EventType as E};
                let p = match ev.event {
                    E::ButtonPressed(B::DPadLeft, _) => Some(Pad::Left),
                    E::ButtonPressed(B::DPadRight, _) => Some(Pad::Right),
                    E::ButtonPressed(B::South, _) => Some(Pad::A),
                    E::ButtonPressed(B::East, _) => Some(Pad::B),
                    E::ButtonPressed(B::West, _) => Some(Pad::X),
                    E::ButtonPressed(B::North, _) => Some(Pad::Y),
                    E::ButtonPressed(B::LeftTrigger, _) => Some(Pad::LB),
                    E::ButtonPressed(B::RightTrigger, _) => Some(Pad::RB),
                    E::AxisChanged(Axis::LeftStickX, v, _) => {
                        let s = if v > 0.6 { 1 } else if v < -0.6 { -1 } else { 0 };
                        if s != sx {
                            sx = s;
                            match s {
                                1 => Some(Pad::Right),
                                -1 => Some(Pad::Left),
                                _ => None,
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(p) = p {
                    let _ = tx.send(p);
                    ctx.request_repaint();
                }
            }
            let any = g.gamepads().next().is_some();
            thread::sleep(Duration::from_millis(if any { 20 } else { 700 }));
        }
    });
}

// ---------- in-game resource monitor ----------
fn nvidia() -> Option<(f32, u64, u64, f32)> {
    let mut c = Command::new("nvidia-smi");
    c.args(["--query-gpu=utilization.gpu,memory.used,memory.total,temperature.gpu", "--format=csv,noheader,nounits"]);
    quiet(&mut c);
    let o = c.output().ok()?;
    if !o.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&o.stdout).to_string();
    let v: Vec<f32> = s.lines().next()?.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    if v.len() < 4 {
        return None;
    }
    Some((v[0], v[1] as u64, v[2] as u64, v[3]))
}

fn find_presentmon() -> Option<PathBuf> {
    let base = std::env::current_exe().ok()?.parent()?.to_path_buf();
    for d in [base.clone(), base.join("tools")] {
        if let Ok(rd) = fs::read_dir(&d) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_lowercase();
                if n.starts_with("presentmon") && n.ends_with(".exe") {
                    return Some(e.path());
                }
            }
        }
    }
    None
}

fn pm_args() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|p| p.join("presentmon_args.txt")))
        .and_then(|p| fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "-process_name {exe} -output_stdout -stop_existing_session -terminate_on_proc_exit".into())
}

fn start_presentmon(pm: &Path, args: &str, exe: &str, m: Arc<Mutex<Metrics>>, ctx: Context) -> Option<Child> {
    let a: Vec<String> = args.split_whitespace().map(|s| s.replace("{exe}", exe)).collect();
    let mut c = Command::new(pm);
    c.args(a).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    quiet(&mut c);
    let mut child = c.spawn().ok()?;
    let out = child.stdout.take()?;
    thread::spawn(move || {
        let rd = std::io::BufReader::new(out);
        let mut idx: Option<usize> = None;
        let mut ft: VecDeque<f32> = VecDeque::new();
        let mut last = Instant::now();
        for line in rd.lines().map_while(Result::ok) {
            let cols: Vec<&str> = line.split(',').collect();
            let Some(i) = idx else {
                idx = cols.iter().position(|c| c.eq_ignore_ascii_case("msBetweenPresents") || c.eq_ignore_ascii_case("FrameTime"));
                continue;
            };
            let Some(v) = cols.get(i).and_then(|s| s.trim().parse::<f32>().ok()) else { continue };
            if v <= 0.0 {
                continue;
            }
            ft.push_back(v);
            if ft.len() > 300 {
                ft.pop_front();
            }
            if last.elapsed() >= Duration::from_millis(250) {
                last = Instant::now();
                let recent: Vec<f32> = ft.iter().rev().take(60).cloned().collect();
                let fps = 1000.0 / (recent.iter().sum::<f32>() / recent.len() as f32);
                let mut all: Vec<f32> = ft.iter().cloned().collect();
                all.sort_by(|a, b| b.partial_cmp(a).unwrap());
                let k = (all.len() / 100).max(1);
                let low = 1000.0 / (all[..k].iter().sum::<f32>() / k as f32);
                {
                    let mut s = m.lock().unwrap();
                    s.fps = Some(fps);
                    s.low1 = Some(low);
                    s.fps_hist.push(fps);
                    if s.fps_hist.len() > 120 {
                        s.fps_hist.remove(0);
                    }
                }
                ctx.request_repaint();
            }
        }
    });
    Some(child)
}

struct MonCfg {
    pid: u32,
    dir: PathBuf,
    name: String,
    pm: Option<PathBuf>,
    args: String,
    stats: Stats,
    stats_file: PathBuf,
}

fn monitor(cfg: MonCfg, m: Arc<Mutex<Metrics>>, ctx: Context) {
    thread::spawn(move || {
        use sysinfo::*;
        let mut sys = System::new();
        let kind = ProcessRefreshKind::nothing().with_cpu().with_memory().with_exe(UpdateKind::OnlyIfNotSet);
        let t0 = Instant::now();
        let (mut dead, mut tick, mut nv, mut pm_tried) = (0u32, 0u32, true, false);
        let mut pm_child: Option<Child> = None;
        let mut gpu = None;
        loop {
            sys.refresh_cpu_usage();
            sys.refresh_memory();
            sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
            let procs = sys.processes();
            let mut set: HashSet<Pid> = HashSet::new();
            for (pid, p) in procs {
                if pid.as_u32() == cfg.pid || p.exe().map_or(false, |e| e.starts_with(&cfg.dir)) {
                    set.insert(*pid);
                }
            }
            loop {
                let n = set.len();
                for (pid, p) in procs {
                    if let Some(pp) = p.parent() {
                        if set.contains(&pp) {
                            set.insert(*pid);
                        }
                    }
                }
                if set.len() == n {
                    break;
                }
            }
            let cores = sys.cpus().len().max(1) as f32;
            let (mut cpu, mut ram) = (0.0f32, 0u64);
            let mut top: (u64, String) = (0, String::new());
            for pid in &set {
                if let Some(p) = procs.get(pid) {
                    cpu += p.cpu_usage();
                    ram += p.memory();
                    if p.memory() >= top.0 {
                        top = (p.memory(), p.name().to_string_lossy().to_string());
                    }
                }
            }
            if set.is_empty() { dead += 1 } else { dead = 0 }
            if nv && tick % 2 == 0 {
                gpu = nvidia();
                if gpu.is_none() {
                    nv = false;
                }
            }
            if !pm_tried && tick >= 4 && !top.1.is_empty() {
                if let Some(pm) = &cfg.pm {
                    pm_tried = true;
                    pm_child = start_presentmon(pm, &cfg.args, &top.1, m.clone(), ctx.clone());
                }
            }
            {
                let mut s = m.lock().unwrap();
                s.game_cpu = cpu / cores;
                s.game_ram = ram;
                s.sys_cpu = sys.global_cpu_usage();
                s.ram_used = sys.used_memory();
                s.ram_total = sys.total_memory();
                s.gpu = gpu;
                s.uptime = t0.elapsed().as_secs();
                s.cpu_hist.push(cpu / cores);
                if s.cpu_hist.len() > 60 {
                    s.cpu_hist.remove(0);
                }
            }
            ctx.request_repaint();
            if dead >= 5 {
                break;
            }
            tick += 1;
            thread::sleep(Duration::from_millis(1000));
        }
        if let Some(mut c) = pm_child {
            let _ = c.kill();
        }
        stat_add(&cfg.stats, &cfg.stats_file, &cfg.name, t0.elapsed().as_secs(), false);
        {
            let mut s = m.lock().unwrap();
            s.running = false;
            s.fps = None;
        }
        ctx.request_repaint();
    });
}

fn spark(p: &Painter, r: Rect, data: &[f32], col: Color32) {
    if data.len() < 2 {
        return;
    }
    let mx = data.iter().cloned().fold(1.0, f32::max);
    let pts: Vec<Pos2> = data
        .iter()
        .enumerate()
        .map(|(i, v)| pos2(r.left() + r.width() * i as f32 / (data.len() - 1) as f32, r.bottom() - r.height() * (v / mx)))
        .collect();
    p.add(Shape::line(pts, Stroke::new(1.5, col)));
}

fn overlay(ctx: &Context, m: &Arc<Mutex<Metrics>>) {
    let s = m.lock().unwrap().clone();
    if !s.running {
        ctx.send_viewport_cmd(ViewportCommand::Close);
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Focus);
        return;
    }
    egui::CentralPanel::default()
        .frame(Frame::none().fill(Color32::from_rgb(10, 12, 20)).stroke(Stroke::NONE))
        .show(ctx, |ui| {
            let p = ui.painter().clone();
            let r = ui.max_rect().shrink(12.0);
            let dim = Color32::from_white_alpha(140);
            p.text(r.left_top(), Align2::LEFT_TOP, trunc(&s.name, 22), FontId::proportional(15.0), Color32::WHITE);
            p.text(r.right_top(), Align2::RIGHT_TOP, format!("{:02}:{:02}", s.uptime / 60, s.uptime % 60), FontId::proportional(14.0), dim);
            p.text(pos2(r.left(), r.top() + 26.0), Align2::LEFT_TOP, "FPS", FontId::proportional(12.0), dim);
            match s.fps {
                Some(f) => {
                    let c = if f >= 60.0 { Color32::from_rgb(90, 220, 120) } else if f >= 30.0 { Color32::from_rgb(240, 200, 70) } else { Color32::from_rgb(240, 90, 90) };
                    p.text(pos2(r.left() + 34.0, r.top() + 18.0), Align2::LEFT_TOP, format!("{f:.0}"), FontId::proportional(28.0), c);
                    if let Some(l) = s.low1 {
                        p.text(pos2(r.right(), r.top() + 28.0), Align2::RIGHT_TOP, format!("1% low {l:.0}"), FontId::proportional(13.0), dim);
                    }
                }
                None => {
                    let msg = if s.pm_found { "waiting…" } else { "n/a (add PresentMon)" };
                    p.text(pos2(r.left() + 34.0, r.top() + 26.0), Align2::LEFT_TOP, msg, FontId::proportional(13.0), dim);
                }
            }
            let sp = Rect::from_min_size(pos2(r.left(), r.top() + 56.0), vec2(r.width(), 26.0));
            if s.fps_hist.len() > 2 {
                spark(&p, sp, &s.fps_hist, Color32::from_rgb(90, 220, 120));
            } else {
                spark(&p, sp, &s.cpu_hist, Color32::from_rgb(90, 150, 255));
            }
            let row = |y: f32, l: &str, v: String| {
                p.text(pos2(r.left(), r.top() + y), Align2::LEFT_TOP, l, FontId::proportional(12.0), dim);
                p.text(pos2(r.left() + 34.0, r.top() + y - 1.0), Align2::LEFT_TOP, v, FontId::proportional(14.0), Color32::WHITE);
            };
            row(92.0, "CPU", format!("{:.0}%   sys {:.0}%", s.game_cpu, s.sys_cpu));
            row(112.0, "RAM", format!("{:.1} GB   sys {:.1}/{:.0}", s.game_ram as f64 / GB, s.ram_used as f64 / GB, s.ram_total as f64 / GB));
            row(
                132.0,
                "GPU",
                match s.gpu {
                    Some((u, vu, vt, t)) => format!("{u:.0}%  {:.1}/{:.0} GB  {t:.0}°C", vu as f64 / 1024.0, vt as f64 / 1024.0),
                    None => "n/a (NVIDIA only)".into(),
                },
            );
        });
    ctx.request_repaint_after(Duration::from_millis(1000));
}

// ---------- drawing ----------
/// Apple-style continuous corners (superellipse) with dense sampling.
fn rounded_pts(r: Rect, rad: f32) -> Vec<Pos2> {
    rounded_pts_p(r, rad, 3.2)
}

/// `pw` = corner exponent: 2.0 is a true circle arc, ~3.2 is the continuous "squircle" corner.
fn rounded_pts_p(r: Rect, rad: f32, pw: f32) -> Vec<Pos2> {
    let rad = rad.min(r.width() / 2.0).min(r.height() / 2.0);
    let mut v = vec![];
    let n = 48;
    let e = 2.0 / pw;
    let corners = [
        (r.right_top() + vec2(-rad, rad), -90.0_f32),
        (r.right_bottom() + vec2(-rad, -rad), 0.0),
        (r.left_bottom() + vec2(rad, -rad), 90.0),
        (r.left_top() + vec2(rad, rad), 180.0),
    ];
    for (c, start) in corners {
        for i in 0..=n {
            let a = (start + 90.0 * i as f32 / n as f32).to_radians();
            let (co, si) = (a.cos(), a.sin());
            v.push(c + vec2(co.signum() * co.abs().powf(e), si.signum() * si.abs().powf(e)) * rad);
        }
    }
    v
}

/// Outward unit normals of a clockwise polygon (used for 1px anti-aliasing feathers).
fn normals(pts: &[Pos2]) -> Vec<Vec2> {
    let n = pts.len();
    let edge = |a: Pos2, b: Pos2| {
        let d = b - a;
        vec2(d.y, -d.x).normalized()
    };
    (0..n)
        .map(|i| {
            let (p0, p1, p2) = (pts[(i + n - 1) % n], pts[i], pts[(i + 1) % n]);
            let m = edge(p0, p1) + edge(p1, p2);
            if m.length() < 1e-4 { edge(p1, p2) } else { m.normalized() }
        })
        .collect()
}

/// Anti-aliased stroke along a closed polyline (feathered strip, smooth joins at corners).
fn rim(p: &Painter, pts: &[Pos2], w: f32, col: impl Fn(usize) -> Color32) {
    let k = pts.len() as u32;
    let nr = normals(pts);
    let h = w * 0.5;
    let (a, b) = (h + 0.5, (h - 0.5).max(0.0));
    let mut m = Mesh::default();
    for (i, (&pos, &n)) in pts.iter().zip(nr.iter()).enumerate() {
        let c = col(i);
        m.colored_vertex(pos - n * a, Color32::TRANSPARENT);
        m.colored_vertex(pos - n * b, c);
        m.colored_vertex(pos + n * b, c);
        m.colored_vertex(pos + n * a, Color32::TRANSPARENT);
    }
    for i in 0..k {
        let j = (i + 1) % k;
        for s in 0..3 {
            let (a0, a1, b0, b1) = (i * 4 + s, i * 4 + s + 1, j * 4 + s, j * 4 + s + 1);
            m.add_triangle(a0, a1, b1);
            m.add_triangle(a0, b1, b0);
        }
    }
    p.add(Shape::mesh(m));
}

/// Truly soft drop shadow: many feathered rings with a smooth (smootherstep) falloff.
fn shadow(p: &Painter, rect: Rect, rad: f32, blur: f32, dy: f32, alpha: f32) {
    if alpha < 0.5 || blur < 1.0 {
        return;
    }
    const N: usize = 14;
    let r0 = rect.translate(vec2(0.0, dy));
    let mut rings: Vec<Vec<Vertex>> = vec![];
    for i in 0..=N {
        let t = i as f32 / N as f32;
        let e = blur * (t * 1.15 - 0.3);
        let s = 1.0 - t;
        let a = alpha * s * s * s * (s * (s * 6.0 - 15.0) + 10.0);
        let rr_ = r0.expand(e);
        let pts = rounded_pts(rr_, (rad + e).max(2.0));
        let c = Color32::from_black_alpha(a.clamp(0.0, 255.0) as u8);
        rings.push(pts.into_iter().map(|pos| Vertex { pos, uv: epaint::WHITE_UV, color: c }).collect());
    }
    layered(p, None, &rings);
}

/// Smooth rounded rect: anti-aliased squircle fill + optional anti-aliased edge.
fn rr(p: &Painter, rect: Rect, rad: f32, fill: Color32, edge: Option<(f32, Color32)>) {
    let pts = rounded_pts(rect, rad);
    fan(p, &pts, |_| fill);
    if let Some((w, c)) = edge {
        rim(p, &pts, w, |_| c);
    }
}

fn round_img(p: &Painter, pts: &[Pos2], tex: TextureId, uvf: impl Fn(Pos2) -> Pos2, tint: Color32) {
    let mut m = Mesh::with_texture(tex);
    let k = pts.len() as u32;
    for &pos in pts {
        m.vertices.push(Vertex { pos, uv: uvf(pos), color: tint });
    }
    for i in 1..k - 1 {
        m.add_triangle(0, i, i + 1);
    }
    // 1px transparent feather ring => smooth, alias-free silhouette
    for (&pos, nrm) in pts.iter().zip(normals(pts)) {
        let o = pos + nrm;
        m.vertices.push(Vertex { pos: o, uv: uvf(o), color: Color32::TRANSPARENT });
    }
    for i in 0..k {
        let j = (i + 1) % k;
        m.add_triangle(i, j, k + j);
        m.add_triangle(i, k + j, k + i);
    }
    p.add(Shape::mesh(m));
}

fn cover_uv(ts: Vec2, full: Rect) -> Rect {
    let ia = ts.x / ts.y;
    let sa = full.width() / full.height();
    if ia > sa {
        let w = sa / ia;
        Rect::from_min_max(pos2((1.0 - w) / 2.0, 0.0), pos2((1.0 + w) / 2.0, 1.0))
    } else {
        let h = ia / sa;
        Rect::from_min_max(pos2(0.0, (1.0 - h) / 2.0), pos2(1.0, (1.0 + h) / 2.0))
    }
}

fn uv_full() -> Rect {
    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))
}

fn fan(p: &Painter, pts: &[Pos2], f: impl Fn(Pos2) -> Color32) {
    let mut m = Mesh::default();
    let k = pts.len() as u32;
    for &pos in pts {
        m.colored_vertex(pos, f(pos));
    }
    for i in 1..k - 1 {
        m.add_triangle(0, i, i + 1);
    }
    for (&pos, nrm) in pts.iter().zip(normals(pts)) {
        m.colored_vertex(pos + nrm, Color32::TRANSPARENT);
    }
    for i in 0..k {
        let j = (i + 1) % k;
        m.add_triangle(i, j, k + j);
        m.add_triangle(i, k + j, k + i);
    }
    p.add(Shape::mesh(m));
}

/// Layered mesh: rings of identical point count stitched together, innermost ring filled.
fn layered(p: &Painter, tex: Option<TextureId>, rings: &[Vec<Vertex>]) {
    let mut m = match tex {
        Some(t) => Mesh::with_texture(t),
        None => Mesh::default(),
    };
    let k = rings[0].len() as u32;
    for r in rings {
        m.vertices.extend(r.iter().copied());
    }
    for j in 0..rings.len() as u32 - 1 {
        let (a, b) = (j * k, (j + 1) * k);
        for i in 0..k {
            let i2 = (i + 1) % k;
            m.add_triangle(a + i, a + i2, b + i2);
            m.add_triangle(a + i, b + i2, b + i);
        }
    }
    let last = (rings.len() as u32 - 1) * k;
    for i in 1..k - 1 {
        m.add_triangle(last, last + i, last + i + 1);
    }
    p.add(Shape::mesh(m));
}

/// Round control material (same soft dark vibrancy as the bars, no bevel / refraction).
fn glass(p: &Painter, rect: Rect, rad: f32, bl: Option<(&TextureHandle, Rect)>, full: Rect, k: f32) {
    soft_glass(p, rect, rad, bl, full, k, 0.0);
}

/// Dark smoked-glass panel (properties modal): dimmed, lightly see-through backdrop, no white bevel glow.
fn panel(p: &Painter, rect: Rect, rad: f32, bg: Option<(&TextureHandle, Rect)>, full: Rect, k: f32) {
    shadow(p, rect, rad, 48.0, 20.0, 130.0 * k);
    let pts = rounded_pts(rect, rad);
    match bg {
        Some((t, uv)) => round_img(
            p,
            &pts,
            t.id(),
            |pos| {
                pos2(
                    uv.min.x + (pos.x - full.min.x) / full.width() * uv.width(),
                    uv.min.y + (pos.y - full.min.y) / full.height() * uv.height(),
                )
            },
            Color32::from_gray(90).gamma_multiply(k),
        ),
        None => fan(p, &pts, |_| Color32::from_rgb(18, 22, 36).gamma_multiply(k)),
    }
    fan(p, &pts, |_| Color32::from_rgba_unmultiplied(8, 10, 18, (150.0 * k) as u8));
}

/// Very soft liquid glass for the nav / info bars: dimmed blurred backdrop, faint milky tint,
/// gentle top sheen and a light-catching hairline rim. `hot` (0..1) brightens it slightly.
fn soft_glass(p: &Painter, rect: Rect, rad: f32, bl: Option<(&TextureHandle, Rect)>, full: Rect, k: f32, hot: f32) {
    shadow(p, rect, rad, 26.0, 10.0, 55.0 * k);
    let pts = rounded_pts(rect, rad);
    match bl {
        Some((t, uv)) => round_img(
            p,
            &pts,
            t.id(),
            |pos| {
                pos2(
                    uv.min.x + (pos.x - full.min.x) / full.width() * uv.width(),
                    uv.min.y + (pos.y - full.min.y) / full.height() * uv.height(),
                )
            },
            Color32::from_gray((92.0 + 14.0 * hot) as u8).gamma_multiply(k),
        ),
        None => fan(p, &pts, |_| Color32::from_rgba_unmultiplied(20, 24, 38, (190.0 * k) as u8)),
    }
    fan(p, &pts, |_| Color32::from_rgba_unmultiplied(6, 8, 16, (40.0 * k) as u8));
}

fn grad(p: &Painter, r: Rect, c0: Color32, c1: Color32, horizontal: bool) {
    let mut m = Mesh::default();
    if horizontal {
        m.colored_vertex(r.left_top(), c0);
        m.colored_vertex(r.right_top(), c1);
        m.colored_vertex(r.right_bottom(), c1);
        m.colored_vertex(r.left_bottom(), c0);
    } else {
        m.colored_vertex(r.left_top(), c0);
        m.colored_vertex(r.right_top(), c0);
        m.colored_vertex(r.right_bottom(), c1);
        m.colored_vertex(r.left_bottom(), c1);
    }
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(Shape::mesh(m));
}

fn shade(p: &Painter, f: Rect) {
    let t = Color32::TRANSPARENT;
    p.rect_filled(f, 0.0, Color32::from_black_alpha(78));
    grad(p, Rect::from_min_size(f.min, vec2(f.width() * 0.62, f.height())), Color32::from_black_alpha(190), t, true);
    grad(p, Rect::from_min_max(pos2(f.left(), f.top() + f.height() * 0.38), f.max), t, Color32::from_black_alpha(235), false);
    grad(p, Rect::from_min_size(f.min, vec2(f.width(), 130.0)), Color32::from_black_alpha(150), t, false);
}

fn draw_cover(p: &Painter, tex: &TextureHandle, full: Rect, a: f32) {
    p.image(tex.id(), full, cover_uv(tex.size_vec2(), full), Color32::from_white_alpha((a * 255.0) as u8));
}

/// Static background (no per-frame animation = near-zero idle cost). Soft glow, no hard shapes.
fn background(p: &Painter, r: Rect) {
    grad(p, r, Color32::from_rgb(7, 10, 24), Color32::from_rgb(18, 30, 66), false);
}

fn ease(x: f64) -> f32 {
    let x = x.clamp(0.0, 1.0) as f32;
    x * x * (3.0 - 2.0 * x)
}

/// Glass orb/tile over black: bevel rings refract the logo per wavelength (R/G/B drawn additively
/// with different bend), plus chromatic specular rim streaks.
fn orb(p: &Painter, rect: Rect, rad: f32, pw: f32, logo: Option<(&Logo, Rect)>, k: f32, s: f32, grain: f32) {
    if k <= 0.003 {
        return;
    }
    let _light = vec2(-0.55, -0.83);
    let bevel = (rad * 0.85).min(rect.height() * 0.4).max(5.0);
    const R: i32 = 8;
    let mut geo = vec![];
    for j in -1..=R {
        let inset = if j < 0 { -1.0 } else { bevel * j as f32 / R as f32 };
        let pts = rounded_pts_p(rect.shrink(inset), (rad - inset).max(3.0), pw);
        let nr = normals(&pts);
        let t = if j < 0 { 0.0 } else { j as f32 / R as f32 };
        let f = (1.0 - t) * (1.0 - t);
        geo.push((pts, nr, f, j < 0));
    }
    if let Some((lg, lr)) = logo {
        if s > 0.003 {
            // wavelength-split refraction: warm + cool fringes (tinted mask) behind the true logo pass
            for (mask, col, disp, a0) in [(true, (255u8, 150u8, 120u8), 11.0_f32, 0.38_f32), (true, (150, 225, 255), 19.0, 0.38), (false, (255, 255, 255), 15.0, 1.0)] {
                let rings: Vec<Vec<Vertex>> = geo
                    .iter()
                    .map(|(pts, nr, f, feather)| {
                        pts.iter()
                            .zip(nr.iter())
                            .map(|(&pos, &n)| {
                                let q = pos - n * (*f * disp * grain);
                                let uv = pos2((q.x - lr.min.x) / lr.width(), (q.y - lr.min.y) / lr.height());
                                let inside = (0.0..=1.0).contains(&uv.x) && (0.0..=1.0).contains(&uv.y);
                                let a = if *feather || !inside { 0.0 } else { s * a0 };
                                let color = if mask { Color32::from_rgba_unmultiplied(col.0, col.1, col.2, (a * 255.0) as u8) } else { Color32::from_white_alpha((a * 255.0) as u8) };
                                Vertex { pos, uv, color }
                            })
                            .collect()
                    })
                    .collect();
                layered(p, Some(if mask { lg.mask.id() } else { lg.tex.id() }), &rings);
            }
        }
    }
}

struct Logo {
    tex: TextureHandle,
    mask: TextureHandle, // same shape, white: lets the splash tint refraction fringes
}

/// Splash on black: glass sphere -> glass tile -> tile dissolves, leaving the logo alone. No text.
fn splash(p: &Painter, r: Rect, t: f64, logo: Option<&Logo>, reduced: bool) {
    let alive = 1.0 - ease((t - (SPLASH - 0.25)) / 0.25); // exit: everything fades to black (the launcher fades in from black)
    p.rect_filled(r, 0.0, Color32::BLACK);
    let c = r.center();
    // measured fit: 200px logo (longest side), shrunk when the window is small
    let size = 200.0_f32.min(r.height() * 0.42).min(r.width() * 0.42);
    let (lw, lh) = match logo {
        Some(l) => {
            let s = l.tex.size_vec2();
            let sc = size / s.x.max(s.y);
            (s.x * sc, s.y * sc)
        }
        None => (size, size),
    };
    let lrect = Rect::from_center_size(c, vec2(lw, lh));
    let (appear, morph, reveal, glass_k) = if reduced {
        (1.0, 1.0, ease((t - 0.2) / 0.5), 0.0)
    } else {
        (ease((t - 0.1) / 0.6), ease((t - 0.8) / 0.7), ease((t - 1.1) / 0.6), 1.0 - ease((t - 1.7) / 0.45))
    };
    let kk = glass_k * appear * alive;
    let tile = size * 1.45 * (0.6 + 0.4 * appear);
    let rect = Rect::from_center_size(c, vec2(tile, tile));
    let rad = tile * 0.5 + (tile * 0.26 - tile * 0.5) * morph;
    let pw = 2.0 + 1.2 * morph;
    let grain = 2.2 + (1.0 - 2.2) * morph;
    orb(p, rect, rad, pw, logo.map(|l| (l, lrect)), kk, reveal * kk, grain);
    let la = reveal * (1.0 - glass_k * appear) * alive;
    match logo {
        Some(l) => {
            p.image(l.tex.id(), lrect, uv_full(), Color32::from_white_alpha((la * 255.0) as u8));
        }
        None => {
            let a = (la * 255.0) as u8;
            p.circle_stroke(c, size * 0.38, Stroke::new(3.0, Color32::from_white_alpha(a)));
        }
    }
}

/// Project logo, embedded in the exe (assets/logo.png, rasterized from the supplied SVG).
fn load_logo(ctx: &Context) -> Option<Logo> {
    let img = image::load_from_memory(include_bytes!("../assets/logo.png")).ok()?;
    let img = if img.width() > 512 || img.height() > 512 { img.resize(512, 512, image::imageops::FilterType::CatmullRom) } else { img };
    let mut rgba = img.to_rgba8();
    for px in rgba.pixels_mut() {
        px.0[0] = 255; // white logo (alpha kept)
        px.0[1] = 255;
        px.0[2] = 255;
    }
    let mut m = rgba.clone();
    for px in m.pixels_mut() {
        px.0[0] = 255;
        px.0[1] = 255;
        px.0[2] = 255;
    }
    Some(Logo {
        tex: ctx.load_texture("logo", ci(&rgba), TextureOptions::LINEAR),
        mask: ctx.load_texture("logo_mask", ci(&m), TextureOptions::LINEAR),
    })
}

fn fade_in(p: &Painter, full: Rect, t: f64) {
    let fade = (1.0 - (t - SPLASH) / 0.6).clamp(0.0, 1.0);
    if fade > 0.0 {
        p.rect_filled(full, 0.0, Color32::from_black_alpha((255.0 * fade) as u8));
    }
}

// ---------- app ----------
struct App {
    dir: PathBuf,
    cache: PathBuf,
    prefs_path: PathBuf,
    prefs: Prefs,
    games: Vec<Game>,
    view: Vec<usize>,
    tab: usize,
    search: String,
    searching: bool,
    dirty: bool,
    sel: usize,
    last_sel: usize,
    pos: f32,
    start: Option<f64>,
    job: Arc<Mutex<Job>>,
    job_name: String,
    busy: bool,
    ask: bool,
    ask_idx: usize,
    msg: Option<(String, f64)>,
    fullscreen: bool,
    assets: HashMap<String, Assets>,
    requested: HashSet<String>,
    tx: mpsc::Sender<(String, Msg)>,
    rx: mpsc::Receiver<(String, Msg)>,
    pad_rx: mpsc::Receiver<Pad>,
    pad_paused: Arc<AtomicBool>,
    snd: mpsc::Sender<Sfx>,
    stats: Stats,
    mon: Arc<Mutex<Metrics>>,
    was_running: bool,
    mon_size: Vec2,
    pm_path: Option<PathBuf>,
    sys: sysinfo::System,
    sys_t: f64,
    bg_cur: String,
    bg_prev: String,
    bg_fade: f32,
    bg_has: bool,
    props: bool,
    pm: f32,
    anim: bool,
    logo: Option<Logo>,
    reduced: bool,
    nav_x: f32,
    nav_hov: [f32; 3],
    fav_hov: f32,
    heart: TextureHandle,
    heart_fill: TextureHandle,
    wave: TextureHandle,
    wave_hov: f32,
    was_max: bool,
}

impl App {
    fn new(dir: PathBuf, cc: &eframe::CreationContext<'_>) -> Self {
        let icon = |name: &str, b: &[u8]| {
            let im = image::load_from_memory(b).expect("icon").to_rgba8();
            let ci = ColorImage::from_rgba_unmultiplied([im.width() as usize, im.height() as usize], im.as_raw());
            cc.egui_ctx.load_texture(name, ci, TextureOptions::LINEAR)
        };
        let heart = icon("heart", include_bytes!("../assets/heart.png"));
        let heart_fill = icon("heart_fill", include_bytes!("../assets/heart_fill.png"));
        let wave = icon("wave", include_bytes!("../assets/wave.png"));
        let mut fonts = FontDefinitions::default();
        fonts.font_data.insert("hg".into(), FontData::from_static(include_bytes!("../assets/HankenGrotesk-Medium.otf")));
        fonts.font_data.insert("hgb".into(), FontData::from_static(include_bytes!("../assets/HankenGrotesk-SemiBold.otf")));
        if let Some(f) = fonts.families.get_mut(&FontFamily::Proportional) {
            f.insert(0, "hg".into());
        }
        let mut bold = vec!["hgb".to_string()];
        bold.extend(fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
        fonts.families.insert(FontFamily::Name("bold".into()), bold);
        cc.egui_ctx.set_fonts(fonts);
        let cache = dir.join(".cache");
        let _ = fs::create_dir_all(&cache);
        let prefs_path = cache.join("prefs.json");
        let prefs = Prefs::load(&prefs_path);
        let stats = fs::read_to_string(cache.join("stats.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let any_recent = stats.values().any(|e| e["last"].as_u64().unwrap_or(0) > 0);
        let (tx, rx) = mpsc::channel();
        let (ptx, pad_rx) = mpsc::channel();
        let pad_paused = Arc::new(AtomicBool::new(false));
        spawn_pad(ptx, pad_paused.clone(), cc.egui_ctx.clone());
        let (snd, srx) = mpsc::channel();
        spawn_audio(srx);
        if !prefs.mute {
            let _ = snd.send(Sfx::Startup);
        }
        let mut sys = sysinfo::System::new();
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        Self {
            games: scan(&dir, &prefs),
            view: vec![],
            tab: if any_recent { 2 } else { 0 },
            search: String::new(),
            searching: false,
            dirty: true,
            dir,
            cache,
            prefs_path,
            prefs,
            sel: 0,
            last_sel: usize::MAX,
            pos: 0.0,
            start: None,
            job: Arc::new(Mutex::new(Job::default())),
            job_name: String::new(),
            busy: false,
            ask: false,
            ask_idx: 0,
            msg: None,
            fullscreen: false,
            assets: HashMap::new(),
            requested: HashSet::new(),
            tx,
            rx,
            pad_rx,
            pad_paused,
            snd,
            stats: Arc::new(Mutex::new(stats)),
            mon: Arc::new(Mutex::new(Metrics::default())),
            was_running: false,
            mon_size: vec2(1920.0, 1080.0),
            pm_path: find_presentmon(),
            sys,
            sys_t: 0.0,
            bg_cur: String::new(),
            bg_prev: String::new(),
            bg_fade: 1.0,
            bg_has: false,
            props: false,
            pm: 0.0,
            anim: true,
            logo: load_logo(&cc.egui_ctx),
            reduced: std::env::var_os("LAUNCHER_REDUCE_MOTION").is_some(),
            nav_x: -1.0,
            nav_hov: [0.0; 3],
            fav_hov: 0.0,
            heart,
            heart_fill,
            wave,
            wave_hov: 0.0,
            was_max: false,
        }
    }

    fn play(&self, s: Sfx) {
        if !self.prefs.mute {
            let _ = self.snd.send(s);
        }
    }

    fn last_of(&self, name: &str) -> u64 {
        self.stats.lock().unwrap().get(name).and_then(|e| e["last"].as_u64()).unwrap_or(0)
    }

    fn rebuild_view(&mut self) {
        let keep = self.view.get(self.sel).map(|&i| self.games[i].name.clone());
        let q = self.search.to_lowercase();
        let mut v: Vec<usize> = (0..self.games.len())
            .filter(|&i| {
                let g = &self.games[i];
                (q.is_empty() || g.name.to_lowercase().contains(&q))
                    && match self.tab {
                        1 => self.prefs.fav.contains(&g.name),
                        2 => self.last_of(&g.name) > 0,
                        _ => true,
                    }
            })
            .collect();
        if self.tab == 2 {
            v.sort_by_key(|&i| std::cmp::Reverse(self.last_of(&self.games[i].name)));
        }
        self.view = v;
        self.sel = keep
            .and_then(|n| self.view.iter().position(|&i| self.games[i].name == n))
            .unwrap_or(0)
            .min(self.view.len().saturating_sub(1));
        self.dirty = false;
    }

    fn start_install(&mut self, ctx: &Context) {
        let Some(g) = self.games.get(self.ask_idx) else { return };
        if g.kind != Kind::Zip || self.busy {
            return;
        }
        self.play(Sfx::Select);
        let g = &self.games[self.ask_idx];
        *self.job.lock().unwrap() = Job::default();
        self.busy = true;
        self.job_name = g.name.clone();
        extract(g.path.clone(), self.dir.join(&g.name), self.job.clone(), ctx.clone());
    }

    fn activate(&mut self, ctx: &Context, t: f64) {
        let Some(&gi) = self.view.get(self.sel) else { return };
        let g = &self.games[gi];
        match g.kind {
            Kind::Zip => {
                if self.busy {
                    self.play(Sfx::Error);
                    return;
                }
                self.ask = true;
                self.ask_idx = gi;
                self.play(Sfx::Open);
            }
            Kind::Folder => {
                if self.mon.lock().unwrap().running {
                    self.msg = Some(("A game is already running".into(), t));
                    self.play(Sfx::Error);
                    return;
                }
                match &g.exe {
                    Some(exe) => match Command::new(exe).current_dir(exe.parent().unwrap_or(&g.path)).spawn() {
                        Ok(c) => {
                            self.play(Sfx::Launch);
                            let sf = self.cache.join("stats.json");
                            stat_add(&self.stats, &sf, &g.name, 0, true);
                            *self.mon.lock().unwrap() = Metrics { running: true, name: g.name.clone(), pm_found: self.pm_path.is_some(), ..Default::default() };
                            if let Some(s) = ctx.input(|i| i.viewport().monitor_size) {
                                self.mon_size = s;
                            }
                            monitor(
                                MonCfg { pid: c.id(), dir: g.path.clone(), name: g.name.clone(), pm: self.pm_path.clone(), args: pm_args(), stats: self.stats.clone(), stats_file: sf },
                                self.mon.clone(),
                                ctx.clone(),
                            );
                            self.msg = Some((format!("Launching {}…", g.name), t));
                            ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
                        }
                        Err(e) => {
                            self.play(Sfx::Error);
                            self.msg = Some((format!("Failed: {e}"), t));
                        }
                    },
                    None => {
                        self.play(Sfx::Error);
                        self.msg = Some(("No executable found".into(), t));
                    }
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _: &mut eframe::Frame) {
        // responsive: scale the whole UI so the layout always spans ~1100x720 points (windowed, maximized or fullscreen)
        {
            let ppp = ctx.pixels_per_point();
            let sr = ctx.screen_rect();
            let (pw, ph) = (sr.width() * ppp, sr.height() * ppp);
            if pw > 300.0 && ph > 200.0 {
                let nat = ctx.native_pixels_per_point().unwrap_or(1.0).max(0.1);
                let z = ((ph / 720.0).min(pw / 1100.0) / nat).clamp(0.5, 4.0);
                if (z - ctx.zoom_factor()).abs() > 0.02 {
                    ctx.set_zoom_factor(z);
                    ctx.request_repaint();
                }
            }
        }
        let t_abs = ctx.input(|i| i.time);
        let start = *self.start.get_or_insert(t_abs);
        let t = t_abs - start;
        let maxed = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        if maxed && !self.was_max && !self.fullscreen {
            self.fullscreen = true;
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(true));
        }
        self.was_max = maxed;
        let intro = t < SPLASH + 0.65;
        self.anim = intro;

        // receive loaded assets (at most 2 games' worth of texture uploads per frame, no hitching)
        let mut budget = 2;
        while budget > 0 {
            let Ok((n, m)) = self.rx.try_recv() else { break };
            if matches!(m, Msg::Batch(_)) {
                budget -= 1;
            }
            let a = self.assets.entry(n.clone()).or_default();
            apply_msg(ctx, &n, a, m, t_abs);
        }
        if budget == 0 {
            ctx.request_repaint();
        }

        egui::CentralPanel::default().frame(Frame::none()).show(ctx, |ui| {
            let full = ui.max_rect();
            let p = ui.painter().clone();

            if t < SPLASH {
                splash(&p, full, t, self.logo.as_ref(), self.reduced);
                return;
            }

            // game running state
            let running = self.mon.lock().unwrap().running;
            if self.was_running && !running {
                self.was_running = false;
                self.dirty = true;
                self.play(Sfx::Done);
                ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
            if running {
                self.was_running = true;
            }
            self.pad_paused.store(running || !ctx.input(|i| i.focused), Ordering::Relaxed);

            // extraction finished
            let (done, err) = {
                let j = self.job.lock().unwrap();
                (j.done, j.error.clone())
            };
            if self.busy && done {
                self.busy = false;
                self.games = scan(&self.dir, &self.prefs);
                self.dirty = true;
                self.requested.remove(&self.job_name);
                self.play(if err.is_some() { Sfx::Error } else { Sfx::Done });
                self.msg = Some((err.map_or("Installed ✓".into(), |e| format!("Extract failed: {e}")), t));
            }

            // system stats (sampled every 2 s, skipped while gaming)
            if !running && t_abs - self.sys_t >= 2.0 {
                self.sys_t = t_abs;
                self.sys.refresh_cpu_usage();
                self.sys.refresh_memory();
            }

            // ---- input (keyboard + gamepad) ----
            let mut a = Act::default();
            let (typed, bksp, enter_k, esc, f11, slash) = ctx.input(|i| {
                let mut s = String::new();
                for e in &i.events {
                    if let Event::Text(t) = e {
                        s.push_str(t);
                    }
                }
                a.left = i.key_pressed(Key::ArrowLeft);
                a.right = i.key_pressed(Key::ArrowRight);
                if i.key_pressed(Key::Tab) {
                    if i.modifiers.shift { a.prev = true } else { a.next = true }
                }
                if !self.searching {
                    a.props = i.key_pressed(Key::I);
                    a.fav = i.key_pressed(Key::F);
                    a.mute = i.key_pressed(Key::M);
                }
                (s, i.key_pressed(Key::Backspace), i.key_pressed(Key::Enter), i.key_pressed(Key::Escape), i.key_pressed(Key::F11), i.key_pressed(Key::Slash))
            });
            while let Ok(pd) = self.pad_rx.try_recv() {
                match pd {
                    Pad::Left => a.left = true,
                    Pad::Right => a.right = true,
                    Pad::A => a.ok = true,
                    Pad::B => a.back = true,
                    Pad::X => a.props = true,
                    Pad::Y => a.fav = true,
                    Pad::LB => a.prev = true,
                    Pad::RB => a.next = true,
                }
            }
            let mut confirm = false;
            let cancel = self.ask && (a.back || esc);
            let mut changed = false;
            if self.props {
                if a.back || a.props || esc {
                    self.props = false;
                    self.play(Sfx::Close);
                }
            } else {
                if self.searching {
                    if esc || a.back {
                        self.search.clear();
                        self.searching = false;
                        changed = true;
                        self.play(Sfx::Back);
                    } else if enter_k {
                        self.searching = false;
                        self.play(Sfx::Select);
                    }
                    if !typed.is_empty() {
                        self.search.push_str(&typed.chars().filter(|c| !c.is_control()).collect::<String>());
                        changed = true;
                        self.play(Sfx::Tick);
                    }
                    if bksp && self.search.pop().is_some() {
                        changed = true;
                        self.play(Sfx::Tick);
                    }
                } else {
                    if slash {
                        self.searching = true;
                        self.play(Sfx::Tab);
                    }
                    confirm = a.ok || enter_k;
                    if a.mute {
                        self.prefs.mute = !self.prefs.mute;
                        self.prefs.save(&self.prefs_path);
                        self.msg = Some((if self.prefs.mute { "Sound off" } else { "Sound on" }.into(), t));
                        self.play(Sfx::Select);
                    }
                    if a.fav && !self.view.is_empty() {
                        let nm = self.games[self.view[self.sel]].name.clone();
                        if self.prefs.fav.remove(&nm) {
                            self.play(Sfx::Unfav);
                        } else {
                            self.prefs.fav.insert(nm);
                            self.play(Sfx::Fav);
                        }
                        self.prefs.save(&self.prefs_path);
                        changed = true;
                    }
                    if a.props && !self.view.is_empty() {
                        self.props = true;
                        self.play(Sfx::Open);
                    }
                    if a.next {
                        self.tab = (self.tab + 1) % 3;
                        changed = true;
                        self.play(Sfx::Tab);
                    }
                    if a.prev {
                        self.tab = (self.tab + 2) % 3;
                        changed = true;
                        self.play(Sfx::Tab);
                    }
                    if esc && self.fullscreen {
                        self.fullscreen = false;
                        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
                        ctx.send_viewport_cmd(ViewportCommand::Maximized(false));
                    }
                }
                if a.left {
                    if self.sel > 0 {
                        self.sel -= 1;
                        self.play(Sfx::Move);
                    } else {
                        self.play(Sfx::Bump);
                    }
                }
                if a.right {
                    if self.sel + 1 < self.view.len() {
                        self.sel += 1;
                        self.play(Sfx::Move);
                    } else {
                        self.play(Sfx::Bump);
                    }
                }
            }
            if f11 {
                self.fullscreen = !self.fullscreen;
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(self.fullscreen));
                if !self.fullscreen {
                    ctx.send_viewport_cmd(ViewportCommand::Maximized(false));
                }
            }
            if changed {
                self.dirty = true;
            }
            if self.dirty {
                self.rebuild_view();
            }

            let dt = ctx.input(|i| i.stable_dt).min(0.05);
            let diff = self.sel as f32 - self.pos;
            if diff.abs() < 0.003 {
                self.pos = self.sel as f32;
            } else {
                self.pos += diff * (1.0 - (-12.0 * dt).exp());
            }
            let target = self.props as u8 as f32;
            if (target - self.pm).abs() < 0.01 {
                self.pm = target;
            } else {
                self.pm += (target - self.pm) * (1.0 - (-14.0 * dt).exp());
            }

            // asset requests + free textures far from selection
            if !self.view.is_empty() {
                let n = self.view.len();
                let lo = self.sel.saturating_sub(2);
                let hi = (self.sel + 6).min(n - 1);
                for &gi in &self.view[lo..=hi] {
                    let g = &self.games[gi];
                    if self.requested.insert(g.name.clone()) {
                        let (folder, zl) = match g.kind {
                            Kind::Folder => (Some(g.path.clone()), 0),
                            Kind::Zip => (None, fs::metadata(&g.path).map(|m| m.len()).unwrap_or(0)),
                        };
                        load_game(g.name.clone(), folder, zl, self.cache.join(&g.name), self.tx.clone(), ctx.clone());
                    }
                }
                if self.sel != self.last_sel {
                    self.last_sel = self.sel;
                    let klo = self.sel.saturating_sub(8);
                    let khi = (self.sel + 12).min(n - 1);
                    let keep: HashSet<&String> = self.view[klo..=khi].iter().map(|&i| &self.games[i].name).collect();
                    let drop: Vec<String> = self.assets.keys().filter(|k| !keep.contains(k)).cloned().collect();
                    for k in drop {
                        if let Some(a) = self.assets.get_mut(&k) {
                            a.poster = None;
                            a.poster_s = None;
                            a.hero = None;
                            a.blur = None;
                            a.logo = None;
                        }
                        self.requested.remove(&k);
                    }
                }
            }

            // ---- background with cross-fade ----
            background(&p, full);
            let cur = self.view.get(self.sel).map(|&i| self.games[i].name.clone()).unwrap_or_default();
            if cur != self.bg_cur {
                self.bg_prev = std::mem::replace(&mut self.bg_cur, cur);
                self.bg_fade = 0.0;
                self.bg_has = false;
            }
            let cur_hero = self.assets.get(&self.bg_cur).and_then(|a| a.hero.clone());
            if !self.bg_has && cur_hero.is_some() {
                self.bg_has = true;
                self.bg_fade = 0.0;
            }
            if self.bg_fade < 1.0 {
                self.bg_fade = (self.bg_fade + dt * 2.5).min(1.0);
                self.anim = true;
                if let Some(h) = self.assets.get(&self.bg_prev).and_then(|a| a.hero.as_ref()) {
                    draw_cover(&p, h, full, 1.0);
                }
            }
            if let Some(h) = &cur_hero {
                draw_cover(&p, h, full, self.bg_fade);
            }
            shade(&p, full);

            let sense = if self.props { Sense::hover() } else { csense() };
            let blur_tex = self.assets.get(&self.bg_cur).and_then(|a| a.blur.clone());
            let blur_uv = blur_tex.as_ref().map(|b| cover_uv(b.size_vec2(), full));
            let bl = blur_tex.as_ref().zip(blur_uv);

            let left = full.left() + 72.0;
            let right = full.right() - 72.0;

            // system monitor + clock
            p.text(
                pos2(right, full.top() + 45.0),
                Align2::RIGHT_CENTER,
                chrono::Local::now().format("%H:%M").to_string(),
                fb(15.0),
                Color32::from_white_alpha(210),
            );
            // search pill
            if self.searching || !self.search.is_empty() {
                let r = Rect::from_min_size(pos2(full.left() + 24.0 + 88.0 * 3.0 + 8.0 + 16.0, full.top() + 26.0), vec2(270.0, 38.0));
                soft_glass(&p, r, 19.0, bl, full, 1.0, 0.0);
                let cursor = if self.searching { "_" } else { "" };
                p.text(r.center(), Align2::CENTER_CENTER, format!("Search: {}{}", self.search, cursor), FontId::proportional(16.0), Color32::WHITE);
            }
            // ---- top navigation: Home / Favourites / Recent ----
            {
                let labels = ["Home", "Favourites", "Recent"];
                let (iw, ih, pad) = (88.0_f32, 30.0_f32, 4.0_f32);
                let nav = Rect::from_min_size(
                    pos2(full.left() + 24.0, full.top() + 45.0 - (ih + pad * 2.0) / 2.0),
                    vec2(iw * 3.0 + pad * 2.0, ih + pad * 2.0),
                );
                soft_glass(&p, nav, ih / 2.0 + pad, bl, full, 1.0, 0.0);
                if self.nav_x < 0.0 {
                    self.nav_x = self.tab as f32;
                }
                let nd = self.tab as f32 - self.nav_x;
                if nd.abs() < 0.003 {
                    self.nav_x = self.tab as f32;
                } else {
                    self.nav_x += nd * (1.0 - (-11.0 * dt).exp());
                    self.anim = true;
                }
                let ind = Rect::from_min_size(pos2(nav.min.x + pad + self.nav_x * iw, nav.min.y + pad), vec2(iw, ih));
                rr(&p, ind, ih / 2.0, Color32::from_white_alpha(28), None);
                for i in 0..3 {
                    let r = Rect::from_min_size(nav.min + vec2(pad + i as f32 * iw, pad), vec2(iw, ih));
                    let rs = ui.interact(r, Id::new(("nav", i)), sense);
                    let tgt = if rs.hovered() { 1.0 } else { 0.0 };
                    let hv = self.nav_hov[i] + (tgt - self.nav_hov[i]) * (1.0 - (-12.0 * dt).exp());
                    self.nav_hov[i] = hv;
                    if (tgt - hv).abs() > 0.005 {
                        self.anim = true;
                    }
                    let e = ease(hv as f64);
                    let act = (1.0 - (self.nav_x - i as f32).abs()).clamp(0.0, 1.0);
                    if e > 0.0 && act < 0.5 {
                        rr(&p, r, ih / 2.0, Color32::from_white_alpha((10.0 * e * (1.0 - act)) as u8), None);
                    }
                    let a = (150.0 + 90.0 * act.max(e * 0.6)) as u8;
                    p.text(r.center(), Align2::CENTER_CENTER, labels[i], fb(14.0), Color32::from_white_alpha(a));
                    if rs.clicked() && self.tab != i {
                        self.tab = i;
                        self.dirty = true;
                        self.play(Sfx::Tab);
                        ctx.request_repaint();
                    }
                }
            }
            if self.view.is_empty() {
                let m = if self.games.is_empty() {
                    format!("Put games (folders or .zip) in:\n{}", self.dir.display())
                } else if !self.search.is_empty() {
                    "No games match".to_string()
                } else if self.tab == 1 {
                    "No favourites yet - press F on a game to add one".to_string()
                } else {
                    "Nothing played yet".to_string()
                };
                p.text(full.center(), Align2::CENTER_CENTER, m, FontId::proportional(22.0), Color32::from_white_alpha(180));
                fade_in(&p, full, t);
                return;
            }

            let sel = self.sel;
            let n = self.view.len();
            let mut activate = false;
            let mut open_props = false;

            // ---- poster carousel ----
            let (w0, h0, gap) = (108.0_f32, 162.0_f32, 18.0_f32);
            let big = 1.4_f32;
            let grow = w0 * (big - 1.0);
            let top = full.top() + 108.0;
            for i in 0..n {
                let d = i as f32 - self.pos;
                let k = (1.0 - d.abs()).clamp(0.0, 1.0);
                let ke = k * k * (3.0 - 2.0 * k);
                let s = 1.0 + (big - 1.0) * ke;
                let (w, h) = (w0 * s, h0 * s);
                let x = left + d * (w0 + gap) + grow * d.clamp(0.0, 1.0);
                let y = top + (h0 * big - h) / 2.0;
                let rect = Rect::from_min_size(pos2(x, y), vec2(w, h));
                if rect.right() < full.left() || rect.left() > full.right() {
                    continue;
                }
                let gi = self.view[i];
                let name = self.games[gi].name.clone();
                let fav = self.prefs.fav.contains(&name);
                let (pl, ps, born) = {
                    let a = self.assets.get(&name);
                    (a.and_then(|a| a.poster.clone()), a.and_then(|a| a.poster_s.clone()), a.map_or(0.0, |a| a.born))
                };
                let pa = ease((t_abs - born) / 0.35);
                let tex = if ke > 0.3 { pl.or(ps) } else { ps.or(pl) };
                let rad = 10.0 + 4.0 * ke;
                let pts = rounded_pts(rect, rad);
                shadow(&p, rect, rad, 22.0 + 16.0 * ke, 8.0 + 8.0 * ke, 50.0 + 60.0 * ke);
                let dim = (150.0 + 105.0 * ke) as u8;
                match &tex {
                    Some(tx) => {
                        if pa < 1.0 {
                            p.add(Shape::convex_polygon(pts.clone(), tint(&name).gamma_multiply(0.7 + 0.3 * ke), Stroke::NONE));
                        }
                        round_img(&p, &pts, tx.id(), |pt| pos2((pt.x - rect.min.x) / w, (pt.y - rect.min.y) / h), Color32::from_gray(dim).gamma_multiply(pa));
                    }
                    None => {
                        let c0 = tint(&name).gamma_multiply(0.7 + 0.3 * ke);
                        let c1 = c0.gamma_multiply(0.4);
                        let (ry, rh) = (rect.top(), h);
                        fan(&p, &pts, |pt| c0.lerp_to_gamma(c1, ((pt.y - ry) / rh).clamp(0.0, 1.0)));
                        let ch = name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
                        p.text(rect.center(), Align2::CENTER_CENTER, ch, fb(w * 0.42), Color32::from_white_alpha(200));
                    }
                }
                if fav {
                    let c = rect.right_top() + vec2(-15.0, 15.0);
                    p.circle_filled(c, 6.0, Color32::from_rgb(255, 196, 60));
                    p.circle_stroke(c, 6.0, Stroke::new(1.0, Color32::from_black_alpha(90)));
                }
                if ke > 0.02 {
                    let ring = rect.expand(5.0 * ke);
                    rim(&p, &rounded_pts(ring, rad + 5.0 * ke), 2.5, |_| ACC.gamma_multiply(ke));
                }
                if ui.interact(rect, Id::new(("card", i)), sense).clicked() {
                    if i == sel {
                        activate = true;
                    } else {
                        self.sel = i;
                        self.play(Sfx::Move);
                    }
                }
            }

            // ---- selected game info ----
            let gi = self.view[sel];
            let (name, kind, gpath, exe_path) = {
                let g = &self.games[gi];
                (g.name.clone(), g.kind.clone(), g.path.clone(), g.exe.clone())
            };
            let exe_name = exe_path.as_ref().and_then(|e| e.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let has_exe = exe_path.is_some();
            let is_fav = self.prefs.fav.contains(&name);
            let (meta, size, logo, poster) = {
                let a = self.assets.get(&name);
                (
                    a.and_then(|a| a.meta.clone()).unwrap_or_default(),
                    a.and_then(|a| a.size),
                    a.and_then(|a| a.logo.clone()),
                    a.and_then(|a| a.poster.clone().or(a.poster_s.clone())),
                )
            };
            let st = self.stats.lock().unwrap().get(&name).cloned().unwrap_or(Value::Null);
            let (launches, secs, last) = (st["n"].as_u64().unwrap_or(0), st["secs"].as_u64().unwrap_or(0), st["last"].as_u64().unwrap_or(0));
            let size_s = size.map_or("Calculating…".to_string(), fmt_size);

            let btn_y = full.bottom() - 146.0;
            let desc: String = meta.desc.chars().take(230).collect();
            let gal = p.layout(desc, FontId::proportional(15.0), Color32::from_white_alpha(185), 500.0);
            let desc_h = if meta.desc.is_empty() { 0.0 } else { gal.size().y };
            let desc_y = btn_y - 16.0 - desc_h;
            if !meta.desc.is_empty() {
                p.galley(pos2(left, desc_y), gal, Color32::WHITE);
            }
            let tb = desc_y - 14.0;
            match &logo {
                Some(lg) => {
                    let ts = lg.size_vec2();
                    let sc = (420.0 / ts.x).min(110.0 / ts.y).min(1.0);
                    let sz = ts * sc;
                    p.image(lg.id(), Rect::from_min_size(pos2(left, tb - sz.y), sz), uv_full(), Color32::WHITE);
                }
                None => {
                    p.text(pos2(left, tb), Align2::LEFT_BOTTOM, trunc(&name, 38), fb(46.0), Color32::WHITE);
                }
            }

            // buttons
            let (label, enabled) = match (&kind, has_exe) {
                (Kind::Zip, _) if self.busy => ("Installing…", false),
                (Kind::Zip, _) => ("Install", true),
                (Kind::Folder, _) if running => ("Running", false),
                (Kind::Folder, true) => ("Play", true),
                (Kind::Folder, false) => ("Unavailable", false),
            };
            let play = Rect::from_min_size(pos2(left, btn_y), vec2(168.0, 46.0));
            let r1 = ui.interact(play, Id::new("play"), sense);
            if enabled {
                shadow(&p, play, 23.0, 24.0, 10.0, 90.0);
            }
            let pf = if !enabled { Color32::from_white_alpha(22) } else if r1.hovered() { Color32::from_rgb(52, 152, 255) } else { ACC };
            rr(&p, play, 23.0, pf, None);
            p.text(play.center(), Align2::CENTER_CENTER, label, fb(17.0), if enabled { Color32::WHITE } else { Color32::from_white_alpha(110) });
            if r1.clicked() {
                activate = true;
            }
            let more = Rect::from_min_size(pos2(left + 182.0, btn_y), vec2(46.0, 46.0));
            let r2 = ui.interact(more, Id::new("more"), sense);
            glass(&p, more, 23.0, bl, full, 1.0);
            if r2.hovered() {
                rr(&p, more, 23.0, Color32::from_white_alpha(40), None);
            }
            for dx in [-8.0_f32, 0.0, 8.0] {
                p.circle_filled(more.center() + vec2(dx, 0.0), 2.2, Color32::WHITE);
            }
            if r2.clicked() {
                open_props = true;
            }

            // ---- bottom info bar: play time, studio, last played, status, favourite ----
            {
                let status: (&str, Color32) = match (&kind, has_exe) {
                    (Kind::Zip, _) => ("Not installed", Color32::from_rgb(240, 200, 70)),
                    (Kind::Folder, _) if running => ("Running", ACC),
                    (Kind::Folder, true) => ("Available", Color32::from_rgb(90, 220, 120)),
                    (Kind::Folder, false) => ("Unavailable", Color32::from_rgb(240, 90, 90)),
                };
                let bar = Rect::from_min_max(pos2(left, full.bottom() - 68.0), pos2(left + 720.0, full.bottom() - 24.0));
                soft_glass(&p, bar, 22.0, bl, full, 1.0, 0.0);
                let favr = Rect::from_center_size(pos2(bar.right() - 12.0 - 16.0, bar.center().y), vec2(32.0, 32.0));
                let rf = ui.interact(favr, Id::new("favbtn"), sense);
                let tgt = if rf.hovered() { 1.0 } else { 0.0 };
                let hv = self.fav_hov + (tgt - self.fav_hov) * (1.0 - (-12.0 * dt).exp());
                self.fav_hov = hv;
                if (tgt - hv).abs() > 0.005 {
                    self.anim = true;
                }
                let e = ease(hv as f64);
                let gold = Color32::from_rgb(255, 196, 60);
                let (fill, edge) = if is_fav {
                    (gold.gamma_multiply(0.16 + 0.12 * e), gold.gamma_multiply(0.6 + 0.3 * e))
                } else {
                    (Color32::from_white_alpha((10.0 + 22.0 * e) as u8), Color32::TRANSPARENT)
                };
                rr(&p, favr, 16.0, fill, None);
                let _ = edge;
                let ic = Rect::from_center_size(favr.center(), vec2(18.0, 18.0));
                if is_fav {
                    p.image(self.heart_fill.id(), ic, uv_full(), gold);
                } else {
                    p.image(self.heart.id(), ic, uv_full(), Color32::from_white_alpha((190.0 + 65.0 * e) as u8));
                }
                if rf.clicked() {
                    let nm = name.clone();
                    if self.prefs.fav.remove(&nm) {
                        self.play(Sfx::Unfav);
                    } else {
                        self.prefs.fav.insert(nm);
                        self.play(Sfx::Fav);
                    }
                    self.prefs.save(&self.prefs_path);
                    self.dirty = true;
                    ctx.request_repaint();
                }
                let (x0, x1) = (bar.left() + 24.0, favr.left() - 14.0);
                let fr = [0.0_f32, 0.2, 0.52, 0.76];
                let cells: [(&str, String); 4] = [
                    ("PLAY TIME", fmt_time(secs)),
                    ("STUDIO", trunc(&meta.dev, 24)),
                    ("LAST PLAYED", fmt_last(last)),
                    ("STATUS", status.0.to_string()),
                ];
                for (i, (lb, v)) in cells.iter().enumerate() {
                    let cx = x0 + (x1 - x0) * fr[i];
                    if i > 0 {
                        p.line_segment([pos2(cx - 12.0, bar.top() + 10.0), pos2(cx - 12.0, bar.bottom() - 10.0)], Stroke::new(1.0, Color32::from_white_alpha(26)));
                    }
                    p.text(pos2(cx, bar.top() + 8.0), Align2::LEFT_TOP, *lb, FontId::proportional(9.5), Color32::from_white_alpha(130));
                    let vx = if i == 3 { cx + 14.0 } else { cx };
                    p.text(pos2(vx, bar.top() + 21.0), Align2::LEFT_TOP, v, fb(13.0), Color32::WHITE);
                }
                p.circle_filled(pos2(x0 + (x1 - x0) * fr[3] + 4.0, bar.top() + 29.0), 3.5, status.1);

                // ---- greeting button: same height/radius as the info bar, right edge uses the same 72 px margin as the left column; expands left on hover ----
                let wb = Rect::from_min_max(pos2(full.right() - 72.0 - bar.height(), bar.top()), pos2(full.right() - 72.0, bar.bottom()));
                let gal = p.layout_no_wrap("Greetings from Lewis".into(), fb(13.0), Color32::WHITE);
                let full_w = bar.height() + 24.0 + gal.size().x + 8.0;
                let wt = if !self.props && ui.rect_contains_pointer(Rect::from_min_max(pos2(wb.right() - self.wave_hov.max(0.0) * (full_w - bar.height()) - bar.height(), wb.top()), wb.right_bottom())) { 1.0 } else { 0.0 };
                self.wave_hov += (wt - self.wave_hov) * (1.0 - (-12.0 * dt).exp());
                if (wt - self.wave_hov).abs() > 0.005 || wt > 0.5 {
                    self.anim = true;
                }
                let we = ease(self.wave_hov as f64);
                let pill = Rect::from_min_max(pos2(wb.right() - bar.height() - (full_w - bar.height()) * we, wb.top()), wb.right_bottom());
                soft_glass(&p, pill, 22.0, bl, full, 1.0, we);
                if we > 0.02 {
                    let a = ((we - 0.4).max(0.0) / 0.6 * 255.0) as u8;
                    p.text(pos2(pill.left() + 24.0, pill.center().y), Align2::LEFT_CENTER, "Greetings from Lewis", fb(13.0), Color32::from_white_alpha(a));
                }
                // hand icon: pivots at the wrist while hovered
                let ang = if wt > 0.5 { (t_abs as f32 * 9.0).sin() * 0.35 } else { 0.0 } * we;
                let ic = Rect::from_center_size(wb.center(), vec2(24.0, 24.0));
                let pv = ic.min + vec2(0.5, 0.85) * ic.size();
                let (sn, cs) = ang.sin_cos();
                let rot = |q: Pos2| {
                    let d = q - pv;
                    pv + vec2(d.x * cs - d.y * sn, d.x * sn + d.y * cs)
                };
                let mut m = Mesh::with_texture(self.wave.id());
                let tint = Color32::from_white_alpha((190.0 + 65.0 * we) as u8);
                for (q, uv) in [(ic.left_top(), pos2(0.0, 0.0)), (ic.right_top(), pos2(1.0, 0.0)), (ic.right_bottom(), pos2(1.0, 1.0)), (ic.left_bottom(), pos2(0.0, 1.0))] {
                    m.vertices.push(Vertex { pos: rot(q), uv, color: tint });
                }
                m.add_triangle(0, 1, 2);
                m.add_triangle(0, 2, 3);
                p.add(Shape::mesh(m));
            }

            // progress + toast
            if self.busy {
                let pr = self.job.lock().unwrap().progress.clamp(0.0, 1.0);
                let card = Rect::from_center_size(pos2(full.center().x, full.bottom() - 70.0), vec2(520.0, 76.0));
                soft_glass(&p, card, 22.0, bl, full, 1.0, 0.0);
                p.text(pos2(card.left() + 26.0, card.top() + 22.0), Align2::LEFT_CENTER, format!("Installing {}", trunc(&self.job_name, 30)), fb(16.0), Color32::from_white_alpha(235));
                p.text(pos2(card.right() - 26.0, card.top() + 22.0), Align2::RIGHT_CENTER, format!("{}%", (pr * 100.0) as u32), fb(16.0), Color32::from_white_alpha(235));
                let track = Rect::from_min_size(pos2(card.left() + 26.0, card.bottom() - 26.0), vec2(card.width() - 52.0, 8.0));
                rr(&p, track, 4.0, Color32::from_white_alpha(40), None);
                if pr > 0.01 {
                    rr(&p, Rect::from_min_size(track.min, vec2((track.width() * pr).max(8.0), 8.0)), 4.0, ACC, None);
                }
                self.anim = true;
            }
            if let Some((m, at)) = &self.msg {
                let age = (t - at) as f32;
                if age < 3.0 {
                    let kk = ((age.min(3.0 - age)) * 5.0).clamp(0.0, 1.0);
                    let gl = p.layout_no_wrap(m.clone(), fb(14.0), Color32::from_white_alpha((255.0 * kk) as u8));
                    let r = Rect::from_center_size(pos2(full.center().x, full.bottom() - 100.0), gl.size() + vec2(40.0, 20.0));
                    soft_glass(&p, r, r.height() / 2.0, bl, full, kk, 0.0);
                    p.galley(r.center() - gl.size() / 2.0, gl, Color32::WHITE);
                    ctx.request_repaint_after(Duration::from_millis(33));
                }
            }

            // ---- properties modal (glass) ----
            let pm = self.pm;
            if pm > 0.01 {
                if pm < 1.0 {
                    self.anim = true;
                }
                p.rect_filled(full, 0.0, Color32::from_black_alpha((150.0 * pm) as u8));
                let m = Rect::from_center_size(full.center(), vec2(760.0, 470.0));
                let hl = cur_hero.as_ref().map(|h| (h, cover_uv(h.size_vec2(), full)));
                panel(&p, m, 26.0, hl, full, pm);
                let wt = |a: u8| Color32::from_white_alpha((a as f32 * pm) as u8);
                let pr = Rect::from_min_size(m.min + vec2(30.0, 30.0), vec2(140.0, 210.0));
                let pp = rounded_pts(pr, 12.0);
                shadow(&p, pr, 12.0, 22.0, 10.0, 90.0 * pm);
                match &poster {
                    Some(tx) => round_img(&p, &pp, tx.id(), |pt| pos2((pt.x - pr.min.x) / pr.width(), (pt.y - pr.min.y) / pr.height()), Color32::WHITE.gamma_multiply(pm)),
                    None => {
                        p.add(Shape::convex_polygon(pp.clone(), tint(&name).gamma_multiply(pm), Stroke::NONE));
                    }
                }
                let tx0 = m.left() + 30.0 + 140.0 + 30.0;
                p.text(pos2(tx0, m.top() + 28.0), Align2::LEFT_TOP, trunc(&name, 34), fb(28.0), wt(255));
                let rows: Vec<(&str, String)> = vec![
                    ("Type", if kind == Kind::Zip { "Zipped archive".into() } else { "Folder".into() }),
                    ("Location", elide(&gpath.to_string_lossy(), 44)),
                    ("Executable", dash(&exe_name)),
                    ("Size", size_s.clone()),
                    ("Steam App ID", if meta.appid > 0 { meta.appid.to_string() } else { "—".into() }),
                    ("Developer", trunc(&meta.dev, 40)),
                    ("Genre", trunc(&meta.genres, 40)),
                    ("Release", trunc(&meta.release, 40)),
                    ("Favorite", if is_fav { "Yes".into() } else { "No".into() }),
                    ("Launches", launches.to_string()),
                    ("Play time", fmt_time(secs)),
                    ("Last played", fmt_last(last)),
                ];
                for (i, (lb, v)) in rows.iter().enumerate() {
                    let y = m.top() + 76.0 + i as f32 * 25.0;
                    p.text(pos2(tx0, y), Align2::LEFT_TOP, *lb, FontId::proportional(13.0), wt(140));
                    p.text(pos2(tx0 + 124.0, y), Align2::LEFT_TOP, v, FontId::proportional(15.0), wt(240));
                }
                let corner = self.prefs.corner as usize % 4;
                let ov_label = format!("Overlay: {}", CORNERS[corner]);
                let defs: [(&str, f32); 4] = [("Open folder", 150.0), ("Change exe", 140.0), (ov_label.as_str(), 200.0), ("Close", 100.0)];
                let by = m.bottom() - 66.0;
                let mut x = m.left() + 30.0;
                let mut clicks = [false; 4];
                for (i, (txt, w)) in defs.iter().enumerate() {
                    let b = Rect::from_min_size(pos2(x, by), vec2(*w, 42.0));
                    x += *w + 12.0;
                    let rs = ui.interact(b, Id::new(("mb", i)), csense());
                    let (fill, edge) = if rs.hovered() {
                        (ACC.gamma_multiply(0.9 * pm), wt(230))
                    } else {
                        (Color32::from_black_alpha((120.0 * pm) as u8), wt(120))
                    };
                    rr(&p, b, 21.0, fill, None);
                    let _ = edge;
                    p.text(b.center(), Align2::CENTER_CENTER, *txt, fb(14.0), wt(255));
                    clicks[i] = rs.clicked() && self.props;
                }
                if clicks[0] {
                    self.play(Sfx::Select);
                    open_path(if kind == Kind::Zip { gpath.parent().unwrap_or(&gpath) } else { &gpath });
                }
                if clicks[1] {
                    if kind == Kind::Folder {
                        let mut v = vec![];
                        find_exes(&gpath, 3, &mut v);
                        if !v.is_empty() {
                            let cur = exe_path.as_ref().and_then(|e| v.iter().position(|x| x == e)).unwrap_or(v.len() - 1);
                            let next = v[(cur + 1) % v.len()].clone();
                            self.prefs.exe.insert(name.clone(), next.to_string_lossy().to_string());
                            self.games[gi].exe = Some(next);
                            self.prefs.save(&self.prefs_path);
                            self.play(Sfx::Select);
                        } else {
                            self.play(Sfx::Error);
                        }
                    } else {
                        self.play(Sfx::Error);
                    }
                }
                if clicks[2] {
                    self.prefs.corner = (self.prefs.corner + 1) % 4;
                    self.prefs.save(&self.prefs_path);
                    self.play(Sfx::Select);
                }
                if clicks[3] {
                    self.props = false;
                    self.play(Sfx::Close);
                }
            }

            let (mut dlg_yes, mut dlg_no) = (false, false);
            if self.ask {
                p.rect_filled(full, 0.0, Color32::from_black_alpha(150));
                let m = Rect::from_center_size(full.center(), vec2(460.0, 210.0));
                let hl = cur_hero.as_ref().map(|h| (h, cover_uv(h.size_vec2(), full)));
                panel(&p, m, 24.0, hl, full, 1.0);
                let gname = self.games.get(self.ask_idx).map(|g| g.name.clone()).unwrap_or_default();
                p.text(pos2(m.center().x, m.top() + 50.0), Align2::CENTER_CENTER, format!("Install {}?", trunc(&gname, 26)), fb(24.0), Color32::WHITE);
                p.text(pos2(m.center().x, m.top() + 88.0), Align2::CENTER_CENTER, "Are you sure? The game will be extracted into your library.", fb(14.0), Color32::from_white_alpha(170));
                for (i, (txt, hot)) in [("Cancel", false), ("Install", true)].iter().enumerate() {
                    let b = Rect::from_min_size(pos2(m.center().x - 160.0 + i as f32 * 170.0, m.bottom() - 70.0), vec2(150.0, 44.0));
                    let rs = ui.interact(b, Id::new(("ask", i)), csense());
                    let fill = match (*hot, rs.hovered()) {
                        (true, true) => ACC,
                        (true, false) => ACC.gamma_multiply(0.75),
                        (false, true) => Color32::from_white_alpha(60),
                        (false, false) => Color32::from_black_alpha(120),
                    };
                    rr(&p, b, 22.0, fill, None);
                    p.text(b.center(), Align2::CENTER_CENTER, *txt, fb(15.0), Color32::WHITE);
                    if rs.clicked() {
                        if *hot { dlg_yes = true } else { dlg_no = true }
                    }
                }
            }
            if open_props && !self.props {
                self.props = true;
                self.play(Sfx::Open);
            }
            if self.ask {
                if confirm || dlg_yes {
                    self.ask = false;
                    self.start_install(ctx);
                } else if cancel || dlg_no {
                    self.ask = false;
                    self.play(Sfx::Back);
                }
            } else if (activate || confirm) && !self.props {
                self.activate(ctx, t);
            }
            fade_in(&p, full, t);
        });

        // ---- in-game overlay (own always-on-top window, only while a game runs) ----
        let running = self.mon.lock().unwrap().running;
        if running {
            if let Some(s) = ctx.input(|i| i.viewport().monitor_size) {
                self.mon_size = s;
            }
            let (w, h, mg) = (270.0_f32, 172.0_f32, 14.0_f32);
            let ms = self.mon_size;
            let pos = match self.prefs.corner % 4 {
                0 => [mg, mg],
                1 => [ms.x - w - mg, mg],
                2 => [mg, ms.y - h - mg],
                _ => [ms.x - w - mg, ms.y - h - mg],
            };
            let mm = self.mon.clone();
            ctx.show_viewport_deferred(
                ViewportId::from_hash_of("overlay"),
                ViewportBuilder::default()
                    .with_title("Overlay")
                    .with_inner_size([w, h])
                    .with_position(pos)
                    .with_decorations(false)
                    .with_always_on_top()
                    .with_resizable(false)
                    .with_taskbar(false)
                    .with_mouse_passthrough(true)
                    .with_active(false),
                move |ctx, _| overlay(ctx, &mm),
            );
        }

        // ---- repaint scheduling: 60 fps only while animating, otherwise 1 Hz (2 s while gaming) ----
        let wait = if self.anim {
            Duration::from_millis(16)
        } else if running {
            Duration::from_secs(2)
        } else {
            Duration::from_secs(1)
        };
        ctx.request_repaint_after(wait);
    }
}

fn main() -> eframe::Result<()> {
    let dir = std::env::args().nth(1).map(PathBuf::from).unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|p| p.join("games")))
            .unwrap_or_else(|| "games".into())
    });
    let _ = fs::create_dir_all(&dir);
    let opts = eframe::NativeOptions {
        viewport: {
            let mut vb = ViewportBuilder::default().with_inner_size([1280.0, 720.0]).with_title("Launcher");
            if let Ok(img) = image::load_from_memory(include_bytes!("../assets/icon.png")) {
                let rgba = img.to_rgba8();
                vb = vb.with_icon(egui::IconData { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() });
            }
            vb
        },
        multisampling: 4,
        ..Default::default()
    };
    eframe::run_native("Launcher", opts, Box::new(move |cc| Ok(Box::new(App::new(dir, cc)))))
}