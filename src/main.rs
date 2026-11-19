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

const ACC: Color32 = Color32::from_rgb(76, 134, 255);
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
}

enum Msg {
    Poster(ColorImage, ColorImage),
    Hero(ColorImage, ColorImage),
    Logo(ColorImage),
    Meta(Meta),
    Size(u64),
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
        let r = (|| -> Result<(), Box<dyn std::error::Error>> {
            let mut ar = zip::ZipArchive::new(fs::File::open(&zip_path)?)?;
            let n = ar.len().max(1);
            fs::create_dir_all(&dest)?;
            for i in 0..ar.len() {
                let mut f = ar.by_index(i)?;
                let Some(rel) = f.enclosed_name() else { continue };
                let out = dest.join(rel);
                if f.is_dir() {
                    fs::create_dir_all(&out)?;
                } else {
                    if let Some(p) = out.parent() {
                        fs::create_dir_all(p)?;
                    }
                    std::io::copy(&mut f, &mut fs::File::create(&out)?)?;
                }
                if i % 8 == 0 {
                    job.lock().unwrap().progress = (i + 1) as f32 / n as f32;
                    ctx.request_repaint();
                }
            }
            Ok(())
        })();
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
fn find_or_get(local: &[PathBuf], cache: &Path, urls: &[String]) -> Option<PathBuf> {
    if let Some(p) = local.iter().find(|p| p.exists()) {
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

fn load_game(name: String, folder: Option<PathBuf>, zip_len: u64, cache: PathBuf, tx: mpsc::Sender<(String, Msg)>, ctx: Context) {
    thread::spawn(move || {
        let send = |m: Msg| {
            let _ = tx.send((name.clone(), m));
            ctx.request_repaint();
        };
        let _ = fs::create_dir_all(&cache);
        let local = |names: &[&str]| -> Vec<PathBuf> {
            folder.as_ref().map(|d| names.iter().map(|n| d.join(n)).collect()).unwrap_or_default()
        };
        let kinds: [(&str, &[&str], &str); 3] = [
            ("poster", &["poster.jpg", "poster.png", "cover.jpg", "cover.png"][..], "library_600x900.jpg"),
            ("hero", &["hero.jpg", "hero.png", "background.jpg", "background.png"][..], "library_hero.jpg"),
            ("logo", &["logo.png"][..], "logo.png"),
        ];
        let mut sent = [false; 3];
        let mut meta = read_meta(&cache);
        for pass in 0..2 {
            if pass == 1 && meta.is_none() {
                meta = resolve_meta(&name, folder.as_deref(), &cache);
            }
            if let Some(m) = &meta {
                send(Msg::Meta(m.clone()));
            }
            let id = if pass == 1 { meta.as_ref().map(|m| m.appid).filter(|&i| i > 0) } else { None };
            if pass == 1 && id.is_none() {
                break;
            }
            for (k, (stem, locals, remote)) in kinds.iter().enumerate() {
                if sent[k] {
                    continue;
                }
                let urls: Vec<String> = id.map(|id| BASES.iter().map(|b| format!("{b}/{id}/{remote}")).collect()).unwrap_or_default();
                if let Some(p) = find_or_get(&local(locals), &cache.join(format!("{stem}.img")), &urls) {
                    if let Some(m) = decode(k, &p) {
                        send(m);
                        sent[k] = true;
                    }
                }
            }
        }
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
        .frame(Frame::none().fill(Color32::from_rgb(10, 12, 20)).stroke(Stroke::new(1.0, Color32::from_white_alpha(40))))
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
fn rounded_pts(r: Rect, rad: f32) -> Vec<Pos2> {
    let mut v = vec![];
    let n = 8;
    let corners = [
        (r.right_top() + vec2(-rad, rad), -90.0_f32),
        (r.right_bottom() + vec2(-rad, -rad), 0.0),
        (r.left_bottom() + vec2(rad, -rad), 90.0),
        (r.left_top() + vec2(rad, rad), 180.0),
    ];
    for (c, start) in corners {
        for i in 0..=n {
            let a = (start + 90.0 * i as f32 / n as f32).to_radians();
            v.push(c + vec2(a.cos(), a.sin()) * rad);
        }
    }
    v
}

fn round_img(p: &Painter, pts: &[Pos2], tex: TextureId, uvf: impl Fn(Pos2) -> Pos2, tint: Color32) {
    let mut m = Mesh::with_texture(tex);
    for &pos in pts {
        m.vertices.push(Vertex { pos, uv: uvf(pos), color: tint });
    }
    for i in 1..pts.len() - 1 {
        m.add_triangle(0, i as u32, i as u32 + 1);
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
    for &pos in pts {
        m.colored_vertex(pos, f(pos));
    }
    for i in 1..pts.len() - 1 {
        m.add_triangle(0, i as u32, i as u32 + 1);
    }
    p.add(Shape::mesh(m));
}

/// Frosted glass: blurred backdrop (sampled at the panel's own position), frost gradient, light-catching edge.
fn glass(p: &Painter, rect: Rect, rad: f32, bl: Option<(&TextureHandle, Rect)>, full: Rect, k: f32) {
    let pts = rounded_pts(rect, rad);
    for (e, dy, a) in [(10.0_f32, 14.0_f32, 14.0_f32), (5.0, 8.0, 20.0), (1.0, 3.0, 30.0)] {
        p.rect_filled(rect.expand(e).translate(vec2(0.0, dy)), rad + e, Color32::from_black_alpha((a * k) as u8));
    }
    match bl {
        Some((tex, uv)) => round_img(
            p,
            &pts,
            tex.id(),
            |pt| {
                pos2(
                    uv.min.x + (pt.x - full.min.x) / full.width() * uv.width(),
                    uv.min.y + (pt.y - full.min.y) / full.height() * uv.height(),
                )
            },
            Color32::from_gray(86).gamma_multiply(k),
        ),
        None => {
            p.add(Shape::convex_polygon(pts.clone(), Color32::from_rgba_unmultiplied(20, 26, 46, (230.0 * k) as u8), Stroke::NONE));
        }
    }
    fan(p, &pts, |_| Color32::from_black_alpha((48.0 * k) as u8));
    let (y0, h) = (rect.top(), rect.height().max(1.0));
    fan(p, &pts, |pt| {
        let t = ((pt.y - y0) / h).clamp(0.0, 1.0);
        Color32::from_white_alpha(((30.0 - 18.0 * t) * k) as u8)
    });
    let n = pts.len();
    for i in 0..n {
        let (a, b) = (pts[i], pts[(i + 1) % n]);
        let m = a.lerp(b, 0.5);
        let t = ((m.x - rect.min.x) / rect.width().max(1.0) + (m.y - rect.min.y) / h) * 0.5;
        let al = ((120.0 - 90.0 * t.clamp(0.0, 1.0)) * k) as u8;
        p.line_segment([a, b], Stroke::new(1.0, Color32::from_white_alpha(al)));
    }
}

fn keycap(p: &Painter, x: f32, cy: f32, key: &str, label: &str) -> f32 {
    let kg = p.layout_no_wrap(key.to_string(), FontId::proportional(10.5), Color32::from_white_alpha(225));
    let kw = (kg.size().x + 16.0).max(24.0);
    let kr = Rect::from_min_size(pos2(x, cy - 10.0), vec2(kw, 20.0));
    p.rect_filled(kr, 10.0, Color32::from_white_alpha(24));
    p.rect_stroke(kr, 10.0, Stroke::new(1.0, Color32::from_white_alpha(70)));
    p.text(kr.center(), Align2::CENTER_CENTER, key, FontId::proportional(10.5), Color32::from_white_alpha(225));
    let lr = p.text(pos2(kr.right() + 8.0, cy), Align2::LEFT_CENTER, label, FontId::proportional(13.0), Color32::from_white_alpha(150));
    lr.right() + 24.0
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
    let c = pos2(r.left() + r.width() * 0.62, r.top() + r.height() * 0.38);
    for i in 0..14 {
        let t = i as f32 / 13.0;
        p.circle_filled(c, r.height() * (0.75 - 0.55 * t), Color32::from_rgba_unmultiplied(70, 120, 255, 3));
    }
}

fn splash(p: &Painter, r: Rect, t: f64) {
    background(p, r);
    let a = ((t / 0.8).min(1.0) * ((SPLASH - t) / 0.5).clamp(0.0, 1.0)) as f32;
    let c = r.center();
    let pulse = 70.0 + 6.0 * (t * 4.0).sin() as f32;
    p.circle_stroke(c - vec2(0.0, 40.0), pulse, Stroke::new(3.0, Color32::from_white_alpha((200.0 * a) as u8)));
    p.circle_stroke(c - vec2(0.0, 40.0), pulse * 0.7, Stroke::new(2.0, Color32::from_rgba_unmultiplied(90, 150, 255, (220.0 * a) as u8)));
    p.text(c + vec2(0.0, 90.0), Align2::CENTER_CENTER, "L A U N C H E R", fb(30.0), Color32::from_white_alpha((255.0 * a) as u8));
    let w = 220.0;
    let bar = Rect::from_center_size(c + vec2(0.0, 135.0), vec2(w, 4.0));
    p.rect_filled(bar, 2.0, Color32::from_white_alpha((40.0 * a) as u8));
    p.rect_filled(Rect::from_min_size(bar.min, vec2(w * (t / SPLASH).min(1.0) as f32, 4.0)), 2.0, Color32::from_white_alpha((230.0 * a) as u8));
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
}

impl App {
    fn new(dir: PathBuf, cc: &eframe::CreationContext<'_>) -> Self {
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

    fn activate(&mut self, ctx: &Context, t: f64) {
        let Some(&gi) = self.view.get(self.sel) else { return };
        let g = &self.games[gi];
        match g.kind {
            Kind::Zip => {
                if self.busy {
                    self.play(Sfx::Error);
                    return;
                }
                self.play(Sfx::Select);
                *self.job.lock().unwrap() = Job::default();
                self.busy = true;
                self.job_name = g.name.clone();
                extract(g.path.clone(), self.dir.join(&g.name), self.job.clone(), ctx.clone());
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
        let intro = t < SPLASH + 0.65;
        self.anim = intro;

        // receive loaded assets
        while let Ok((n, m)) = self.rx.try_recv() {
            let a = self.assets.entry(n.clone()).or_default();
            let lt = |s: &str, c: ColorImage| ctx.load_texture(format!("{n}-{s}"), c, TextureOptions::LINEAR);
            match m {
                Msg::Poster(c, sm) => {
                    a.poster = Some(lt("p", c));
                    a.poster_s = Some(lt("ps", sm));
                }
                Msg::Hero(c, b) => {
                    a.hero = Some(lt("h", c));
                    a.blur = Some(lt("b", b));
                }
                Msg::Logo(c) => a.logo = Some(lt("l", c)),
                Msg::Meta(m) => a.meta = Some(m),
                Msg::Size(s) => a.size = Some(s),
            }
        }

        egui::CentralPanel::default().frame(Frame::none()).show(ctx, |ui| {
            let full = ui.max_rect();
            let p = ui.painter().clone();

            if t < SPLASH {
                splash(&p, full, t);
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
                let lo = self.sel.saturating_sub(1);
                let hi = (self.sel + 5).min(n - 1);
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
                    let klo = self.sel.saturating_sub(3);
                    let khi = (self.sel + 7).min(n - 1);
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

            // ---- top bar: pill tabs ----
            let mut tx = left;
            for (i, tn) in ["All", "Favorites", "Recent"].iter().enumerate() {
                let on = i == self.tab;
                let col = if on { Color32::WHITE } else { Color32::from_white_alpha(150) };
                let g = p.layout_no_wrap(tn.to_string(), if on { fb(17.0) } else { FontId::proportional(17.0) }, col);
                let pill = Rect::from_min_size(pos2(tx, full.top() + 26.0), vec2(g.size().x + 36.0, 38.0));
                if on {
                    glass(&p, pill, 19.0, bl, full, 1.0);
                }
                let gp = pill.center() - g.size() / 2.0;
                p.galley(gp, g, col);
                if ui.interact(pill, Id::new(("tab", i)), sense).clicked() {
                    self.tab = i;
                    self.dirty = true;
                    self.play(Sfx::Tab);
                    ctx.request_repaint();
                }
                tx += pill.width() + 6.0;
            }
            // system monitor + clock
            let cr = p.text(
                pos2(right, full.top() + 45.0),
                Align2::RIGHT_CENTER,
                chrono::Local::now().format("%H:%M").to_string(),
                fb(24.0),
                Color32::WHITE,
            );
            let sys_txt = format!(
                "CPU {:.0}%    RAM {:.1}/{:.0} GB",
                self.sys.global_cpu_usage(),
                self.sys.used_memory() as f64 / GB,
                self.sys.total_memory() as f64 / GB
            );
            p.text(pos2(cr.left() - 28.0, full.top() + 45.0), Align2::RIGHT_CENTER, sys_txt, FontId::proportional(14.0), Color32::from_white_alpha(160));
            // search pill
            if self.searching || !self.search.is_empty() {
                let r = Rect::from_center_size(pos2(full.center().x, full.top() + 45.0), vec2(320.0, 38.0));
                glass(&p, r, 19.0, bl, full, 1.0);
                let cursor = if self.searching { "_" } else { "" };
                p.text(r.center(), Align2::CENTER_CENTER, format!("Search: {}{}", self.search, cursor), FontId::proportional(16.0), Color32::WHITE);
            }
            // now-playing pill
            if running {
                let s = self.mon.lock().unwrap().clone();
                let fps = s.fps.map_or("FPS n/a".to_string(), |f| format!("{f:.0} FPS"));
                let txt = format!("{}   {}   CPU {:.0}%   RAM {:.1} GB", trunc(&s.name, 18), fps, s.game_cpu, s.game_ram as f64 / GB);
                let r = Rect::from_min_size(pos2(right - 440.0, full.top() + 80.0), vec2(440.0, 36.0));
                glass(&p, r, 18.0, bl, full, 1.0);
                p.circle_filled(r.left_center() + vec2(18.0, 0.0), 5.0, Color32::from_rgb(90, 220, 120));
                p.text(r.left_center() + vec2(32.0, 0.0), Align2::LEFT_CENTER, txt, FontId::proportional(15.0), Color32::WHITE);
            }

            if self.view.is_empty() {
                let m = if self.games.is_empty() {
                    format!("Put games (folders or .zip) in:\n{}", self.dir.display())
                } else {
                    "No games match".to_string()
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
                let zip = self.games[gi].kind == Kind::Zip;
                let fav = self.prefs.fav.contains(&name);
                let (pl, ps) = {
                    let a = self.assets.get(&name);
                    (a.and_then(|a| a.poster.clone()), a.and_then(|a| a.poster_s.clone()))
                };
                let tex = if ke > 0.3 { pl.or(ps) } else { ps.or(pl) };
                let rad = 10.0 + 4.0 * ke;
                let pts = rounded_pts(rect, rad);
                if ke > 0.0 {
                    for (e, dy, a) in [(12.0_f32, 14.0_f32, 26.0_f32), (6.0, 8.0, 38.0)] {
                        p.rect_filled(rect.expand(e * ke).translate(vec2(0.0, dy * ke)), rad + e * ke, Color32::from_black_alpha((a * ke) as u8));
                    }
                }
                let dim = (150.0 + 105.0 * ke) as u8;
                match &tex {
                    Some(tx) => round_img(&p, &pts, tx.id(), |pt| pos2((pt.x - rect.min.x) / w, (pt.y - rect.min.y) / h), Color32::from_gray(dim)),
                    None => {
                        let c0 = tint(&name).gamma_multiply(0.7 + 0.3 * ke);
                        let c1 = c0.gamma_multiply(0.4);
                        let (ry, rh) = (rect.top(), h);
                        fan(&p, &pts, |pt| c0.lerp_to_gamma(c1, ((pt.y - ry) / rh).clamp(0.0, 1.0)));
                        let ch = name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
                        p.text(rect.center(), Align2::CENTER_CENTER, ch, fb(w * 0.42), Color32::from_white_alpha(200));
                    }
                }
                p.add(Shape::closed_line(pts.clone(), Stroke::new(1.0, Color32::from_white_alpha(30))));
                if zip {
                    let b = Rect::from_min_size(rect.left_top() + vec2(8.0, 8.0), vec2(34.0, 18.0));
                    p.rect_filled(b, 9.0, Color32::from_black_alpha(170));
                    p.text(b.center(), Align2::CENTER_CENTER, "ZIP", fb(10.0), Color32::WHITE);
                }
                if fav {
                    let c = rect.right_top() + vec2(-15.0, 15.0);
                    p.circle_filled(c, 6.0, Color32::from_rgb(255, 196, 60));
                    p.circle_stroke(c, 6.0, Stroke::new(1.0, Color32::from_black_alpha(90)));
                }
                if ke > 0.02 {
                    let ring = rect.expand(5.0 * ke);
                    p.add(Shape::closed_line(rounded_pts(ring, rad + 5.0 * ke), Stroke::new(2.5, ACC.gamma_multiply(ke))));
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

            let strip = Rect::from_min_size(pos2(left, full.bottom() - 104.0), vec2(right - left, 62.0));
            let btn_y = strip.top() - 64.0;
            let desc: String = meta.desc.chars().take(230).collect();
            let gal = p.layout(desc, FontId::proportional(16.0), Color32::from_white_alpha(210), 520.0);
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
            let base = if enabled { if r1.hovered() { 255 } else { 238 } } else { 120 };
            if enabled {
                p.rect_filled(play.expand(3.0).translate(vec2(0.0, 5.0)), 26.0, Color32::from_black_alpha(60));
            }
            p.add(Shape::convex_polygon(rounded_pts(play, 23.0), Color32::from_gray(base), Stroke::NONE));
            p.text(play.center(), Align2::CENTER_CENTER, label, fb(18.0), Color32::from_gray(20));
            if r1.clicked() {
                activate = true;
            }
            let more = Rect::from_min_size(pos2(left + 182.0, btn_y), vec2(46.0, 46.0));
            let r2 = ui.interact(more, Id::new("more"), sense);
            glass(&p, more, 23.0, bl, full, 1.0);
            if r2.hovered() {
                p.add(Shape::convex_polygon(rounded_pts(more, 23.0), Color32::from_white_alpha(40), Stroke::NONE));
            }
            for dx in [-8.0_f32, 0.0, 8.0] {
                p.circle_filled(more.center() + vec2(dx, 0.0), 2.2, Color32::WHITE);
            }
            if r2.clicked() {
                open_props = true;
            }

            // glass info strip
            glass(&p, strip, 18.0, bl, full, 1.0);
            let cols = [
                ("Developer", trunc(&meta.dev, 18)),
                ("Genre", trunc(&meta.genres, 22)),
                ("Size", size_s.clone()),
                ("Release", trunc(&meta.release, 14)),
                ("Last played", fmt_last(last)),
                ("Play time", fmt_time(secs)),
            ];
            let cw = (strip.width() - 40.0) / cols.len() as f32;
            for (i, (lb, v)) in cols.iter().enumerate() {
                let x = strip.left() + 22.0 + cw * i as f32;
                if i > 0 {
                    p.line_segment(
                        [pos2(x - 14.0, strip.top() + 14.0), pos2(x - 14.0, strip.bottom() - 14.0)],
                        Stroke::new(1.0, Color32::from_white_alpha(28)),
                    );
                }
                p.text(pos2(x, strip.top() + 12.0), Align2::LEFT_TOP, *lb, FontId::proportional(12.0), Color32::from_white_alpha(150));
                p.text(pos2(x, strip.top() + 30.0), Align2::LEFT_TOP, v, fb(16.0), Color32::WHITE);
            }
            // button hints
            let hy = full.bottom() - 22.0;
            let mut hx = left;
            for (k, l) in [("A / Enter", "Play"), ("X / I", "Details"), ("Y / F", "Favorite"), ("LB RB / Tab", "Tabs"), ("/", "Search"), ("M", "Sound")] {
                hx = keycap(&p, hx, hy, k, l);
            }

            // progress + toast
            if self.busy {
                let pr = self.job.lock().unwrap().progress;
                let bar = Rect::from_min_size(pos2(full.left(), full.bottom() - 4.0), vec2(full.width(), 4.0));
                p.rect_filled(bar, 0.0, Color32::from_white_alpha(40));
                p.rect_filled(Rect::from_min_size(bar.min, vec2(bar.width() * pr, 4.0)), 0.0, ACC);
                self.anim = true;
            }
            if let Some((m, at)) = &self.msg {
                if t - at < 3.0 {
                    p.text(pos2(right, btn_y + 23.0), Align2::RIGHT_CENTER, m, fb(17.0), Color32::from_white_alpha(225));
                    ctx.request_repaint_after(Duration::from_millis(500));
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
                glass(&p, m, 26.0, bl, full, pm);
                let wt = |a: u8| Color32::from_white_alpha((a as f32 * pm) as u8);
                let pr = Rect::from_min_size(m.min + vec2(30.0, 30.0), vec2(140.0, 210.0));
                let pp = rounded_pts(pr, 12.0);
                p.rect_filled(pr.expand(4.0).translate(vec2(0.0, 8.0)), 16.0, Color32::from_black_alpha((70.0 * pm) as u8));
                match &poster {
                    Some(tx) => round_img(&p, &pp, tx.id(), |pt| pos2((pt.x - pr.min.x) / pr.width(), (pt.y - pr.min.y) / pr.height()), Color32::WHITE.gamma_multiply(pm)),
                    None => {
                        p.add(Shape::convex_polygon(pp.clone(), tint(&name).gamma_multiply(pm), Stroke::NONE));
                    }
                }
                p.add(Shape::closed_line(pp, Stroke::new(1.0, wt(40))));
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
                    let a = if rs.hovered() { 70.0 } else { 36.0 };
                    p.add(Shape::convex_polygon(rounded_pts(b, 21.0), Color32::from_white_alpha((a * pm) as u8), Stroke::new(1.0, wt(80))));
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

            if open_props && !self.props {
                self.props = true;
                self.play(Sfx::Open);
            }
            if (activate || confirm) && !self.props {
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
        viewport: ViewportBuilder::default().with_inner_size([1280.0, 720.0]).with_title("Launcher"),
        ..Default::default()
    };
    eframe::run_native("Launcher", opts, Box::new(move |cc| Ok(Box::new(App::new(dir, cc)))))
}