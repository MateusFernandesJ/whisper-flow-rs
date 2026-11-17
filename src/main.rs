//! whisper-flow — ditado por voz via Groq (Whisper), para rodar como serviço systemd de usuário.
//!
//! Atalho: Ctrl+Espaço inicia/para a gravação (lido direto do evdev, funciona em Wayland e X11).
//! Feedback: apenas sons (início, fim, erro). Nenhuma notificação.
//!
//! Áudio ≤ 60 s  → WAV 16 kHz mono 16-bit (sem perdas, menor latência).
//! Áudio > 60 s  → Ogg/Opus 24 kbps (~20x menor); se o Groq recusar, cai para WAV.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use evdev::{
    uinput::{VirtualDevice, VirtualDeviceBuilder},
    AttributeSet, Device, EventType, InputEvent, InputEventKind, Key,
};
use reqwest::blocking::{multipart, Client};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    f32::consts::PI,
    fs, io,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
        Arc, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};

const API_URL: &str = "https://api.groq.com/openai/v1/audio/transcriptions";
const DEFAULT_MODEL: &str = "whisper-large-v3-turbo";
const DEFAULT_LANG: &str = "pt-br";
const DEFAULT_HOTKEY: &str = "ctrl+space";
/// Atalho alternativo: captura o áudio de SAÍDA (monitor do headset) e só cola na área de transferência.
const DEFAULT_HOTKEY2: &str = "ctrl+shift+space";

/// O Groq reamostra tudo para 16 kHz mono; enviar já nesse formato não perde nada.
const TARGET_RATE: u32 = 16_000;
/// Acima disso troca WAV por Opus.
const OPUS_THRESHOLD_SECS: f32 = 60.0;
const OPUS_BITRATE: i32 = 24_000;
/// Gravações menores que isso são descartadas (toque acidental no atalho).
const MIN_SECS: f32 = 0.3;
/// Trava de segurança: para sozinho se esquecerem a gravação ligada.
const MAX_RECORD_SECS: u64 = 15 * 60;
/// Limite do plano gratuito do Groq é 25 MB.
const MAX_UPLOAD_BYTES: usize = 24 * 1024 * 1024;
const HOTKEY_DEBOUNCE: Duration = Duration::from_millis(300);
/// Limite do Groq: 20 requisições/min. Avisa ao chegar em 15 dentro da janela.
const RPM_LIMIT: usize = 20;
const RPM_WARN: usize = 15;
const RPM_WINDOW: Duration = Duration::from_secs(60);
const VKBD_NAME: &str = "whisper-flow-virtual-kbd";

// ───────────────────────────── utilidades ─────────────────────────────

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn run_ok(cmd: &mut Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ───────────────────────────── configuração (.env) ─────────────────────────────

/// Precedência: variável de ambiente do processo > arquivo .env > padrão.
struct Env(HashMap<String, String>);

fn parse_dotenv(src: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in src.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue };
        let mut v = v.trim();
        if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
        {
            v = &v[1..v.len() - 1];
        } else if let Some(i) = v.find(" #") {
            v = v[..i].trim_end(); // comentário na mesma linha
        }
        let k = k.trim();
        if !k.is_empty() {
            m.insert(k.to_string(), v.to_string());
        }
    }
    m
}

impl Env {
    /// Lê UMA vez na inicialização: $WHISPER_ENV ou ~/.config/whisper-flow/.env
    fn load() -> Self {
        let path = env::var("WHISPER_ENV").ok().map(PathBuf::from).or_else(|| {
            let base = env::var("XDG_CONFIG_HOME")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .or_else(|| env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))?;
            Some(base.join("whisper-flow/.env"))
        });
        let map = path
            .and_then(|p| fs::read_to_string(&p).ok().map(|s| (p, s)))
            .map(|(p, s)| {
                eprintln!("Configuração carregada de {p:?}");
                parse_dotenv(&s)
            })
            .unwrap_or_default();
        Env(map)
    }

    fn get(&self, key: &str) -> Option<String> {
        env::var(key)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .or_else(|| self.0.get(key).filter(|v| !v.is_empty()).cloned())
    }
}

// ── atalho ──

const MOD_CTRL: u8 = 1;
const MOD_SHIFT: u8 = 2;
const MOD_ALT: u8 = 4;
const MOD_META: u8 = 8;

#[derive(Clone, Copy, PartialEq, Debug)]
struct Hotkey {
    mods: u8,
    key: Key,
}

const KEY_NAMES: &[(&str, Key)] = &[
    ("a", Key::KEY_A), ("b", Key::KEY_B), ("c", Key::KEY_C), ("d", Key::KEY_D),
    ("e", Key::KEY_E), ("f", Key::KEY_F), ("g", Key::KEY_G), ("h", Key::KEY_H),
    ("i", Key::KEY_I), ("j", Key::KEY_J), ("k", Key::KEY_K), ("l", Key::KEY_L),
    ("m", Key::KEY_M), ("n", Key::KEY_N), ("o", Key::KEY_O), ("p", Key::KEY_P),
    ("q", Key::KEY_Q), ("r", Key::KEY_R), ("s", Key::KEY_S), ("t", Key::KEY_T),
    ("u", Key::KEY_U), ("v", Key::KEY_V), ("w", Key::KEY_W), ("x", Key::KEY_X),
    ("y", Key::KEY_Y), ("z", Key::KEY_Z),
    ("0", Key::KEY_0), ("1", Key::KEY_1), ("2", Key::KEY_2), ("3", Key::KEY_3),
    ("4", Key::KEY_4), ("5", Key::KEY_5), ("6", Key::KEY_6), ("7", Key::KEY_7),
    ("8", Key::KEY_8), ("9", Key::KEY_9),
    ("f1", Key::KEY_F1), ("f2", Key::KEY_F2), ("f3", Key::KEY_F3), ("f4", Key::KEY_F4),
    ("f5", Key::KEY_F5), ("f6", Key::KEY_F6), ("f7", Key::KEY_F7), ("f8", Key::KEY_F8),
    ("f9", Key::KEY_F9), ("f10", Key::KEY_F10), ("f11", Key::KEY_F11), ("f12", Key::KEY_F12),
    ("space", Key::KEY_SPACE), ("enter", Key::KEY_ENTER), ("tab", Key::KEY_TAB),
    ("esc", Key::KEY_ESC), ("escape", Key::KEY_ESC), ("backspace", Key::KEY_BACKSPACE),
    ("insert", Key::KEY_INSERT), ("delete", Key::KEY_DELETE),
    ("home", Key::KEY_HOME), ("end", Key::KEY_END),
    ("pageup", Key::KEY_PAGEUP), ("pagedown", Key::KEY_PAGEDOWN),
    ("up", Key::KEY_UP), ("down", Key::KEY_DOWN), ("left", Key::KEY_LEFT), ("right", Key::KEY_RIGHT),
    ("pause", Key::KEY_PAUSE), ("scrolllock", Key::KEY_SCROLLLOCK), ("menu", Key::KEY_MENU),
    ("grave", Key::KEY_GRAVE), ("minus", Key::KEY_MINUS), ("equal", Key::KEY_EQUAL),
    ("comma", Key::KEY_COMMA), ("dot", Key::KEY_DOT), ("slash", Key::KEY_SLASH),
    ("semicolon", Key::KEY_SEMICOLON), ("apostrophe", Key::KEY_APOSTROPHE),
];

fn parse_hotkey(s: &str) -> Result<Hotkey, String> {
    let mut mods = 0u8;
    let mut key: Option<Key> = None;
    for part in s.split('+') {
        let p = part.trim().to_lowercase();
        match p.as_str() {
            "" => return Err(format!("trecho vazio em {s:?}")),
            "ctrl" | "control" => mods |= MOD_CTRL,
            "shift" => mods |= MOD_SHIFT,
            "alt" => mods |= MOD_ALT,
            "super" | "meta" | "win" | "windows" | "cmd" => mods |= MOD_META,
            name => {
                let k = KEY_NAMES
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, k)| *k)
                    .ok_or_else(|| format!("tecla desconhecida: {name:?}"))?;
                if key.replace(k).is_some() {
                    return Err("use apenas UMA tecla principal".to_string());
                }
            }
        }
    }
    let key = key.ok_or("falta a tecla principal (ex.: space)")?;
    Ok(Hotkey { mods, key })
}

// ── idioma ──

/// O Whisper recebe código ISO-639-1 ("pt"); variantes regionais ("pt-br") não existem para ele.
/// Aceita "pt-br"/"pt_BR"/"PT" → "pt"; "auto" → detecção automática (None).
fn normalize_lang(raw: &str) -> Result<Option<String>, String> {
    let l = raw.trim().to_lowercase();
    if l.is_empty() || l == "auto" {
        return Ok(None);
    }
    let base = l.split(|c| c == '-' || c == '_').next().unwrap_or("");
    if (2..=3).contains(&base.len()) && base.chars().all(|c| c.is_ascii_lowercase()) {
        Ok(Some(base.to_string()))
    } else {
        Err(format!("idioma inválido: {raw:?}"))
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum OutputMode {
    Auto,
    Paste,
    Xdotool,
    Ydotool,
}

#[derive(Clone, Copy)]
enum PasteCombo {
    CtrlV,
    CtrlShiftV,
    ShiftInsert,
}

struct Config {
    model: String,
    /// None = detecção automática do idioma.
    language: Option<String>,
    hotkey: Hotkey,
    hotkey_label: String,
    /// Atalho alternativo: captura o áudio de SAÍDA (monitor do headset), não do microfone.
    hotkey2: Hotkey,
    hotkey2_label: String,
    /// Quando true, a transcrição vai só para a área de transferência (sem colar).
    clipboard_only: bool,
    api_key: Option<String>,
    output: OutputMode,
    paste: PasteCombo,
    volume: f32,
}

impl Config {
    fn load(env: &Env) -> Self {
        let model = env.get("GROQ_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string());

        let lang_raw = env.get("WHISPER_LANG").unwrap_or_else(|| DEFAULT_LANG.to_string());
        let language = normalize_lang(&lang_raw).unwrap_or_else(|e| {
            eprintln!("WHISPER_LANG: {e}; usando {DEFAULT_LANG}");
            normalize_lang(DEFAULT_LANG).unwrap_or(None)
        });

        let hk_raw = env.get("WHISPER_HOTKEY").unwrap_or_else(|| DEFAULT_HOTKEY.to_string());
        let (hotkey, hotkey_label) = match parse_hotkey(&hk_raw) {
            Ok(h) => (h, hk_raw.to_lowercase()),
            Err(e) => {
                eprintln!("WHISPER_HOTKEY inválido ({e}); usando {DEFAULT_HOTKEY}");
                (
                    parse_hotkey(DEFAULT_HOTKEY).expect("atalho padrão válido"),
                    DEFAULT_HOTKEY.to_string(),
                )
            }
        };

        let hk2_raw = env
            .get("WHISPER_HOTKEY2")
            .unwrap_or_else(|| DEFAULT_HOTKEY2.to_string());
        let (hotkey2, hotkey2_label) = match parse_hotkey(&hk2_raw) {
            Ok(h) => (h, hk2_raw.to_lowercase()),
            Err(e) => {
                eprintln!("WHISPER_HOTKEY2 inválido ({e}); usando {DEFAULT_HOTKEY2}");
                (
                    parse_hotkey(DEFAULT_HOTKEY2).expect("atalho padrão válido"),
                    DEFAULT_HOTKEY2.to_string(),
                )
            }
        };

        // Só ativa o clipboard-only se o atalho2 estiver definido (o padrão está).
        let clipboard_only = true;

        let output = match env.get("WHISPER_OUTPUT").unwrap_or_default().as_str() {
            "paste" => OutputMode::Paste,
            "xdotool" => OutputMode::Xdotool,
            "ydotool" => OutputMode::Ydotool,
            _ => OutputMode::Auto,
        };

        let paste = match env.get("WHISPER_PASTE").unwrap_or_default().as_str() {
            "ctrl+shift+v" => PasteCombo::CtrlShiftV,
            "shift+insert" => PasteCombo::ShiftInsert,
            _ => PasteCombo::CtrlV,
        };

        let volume = env
            .get("WHISPER_VOLUME")
            .and_then(|v| v.parse::<f32>().ok())
            .unwrap_or(1.0)
            .clamp(0.0, 2.0);

        Config {
            model,
            language,
            hotkey,
            hotkey_label,
            hotkey2,
            hotkey2_label,
            clipboard_only,
            api_key: env.get("GROQ_API_KEY"),
            output,
            paste,
            volume,
        }
    }
}

/// Chave do .env/ambiente; senão ~/.config/groq/key (lida a cada uso, permite trocar sem reiniciar).
fn api_key(cfg: &Config) -> Result<String, String> {
    if let Some(k) = &cfg.api_key {
        return Ok(k.clone());
    }
    let home = env::var("HOME").map_err(|_| "HOME não definido".to_string())?;
    let path = PathBuf::from(home).join(".config/groq/key");
    let key = fs::read_to_string(&path)
        .map_err(|_| format!("Chave não encontrada em {path:?} (ou defina GROQ_API_KEY no .env)"))?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return Err(format!("O arquivo {path:?} está vazio"));
    }
    Ok(key)
}

// ───────────────────────────── sons ─────────────────────────────

#[derive(Clone, Copy)]
enum Cue {
    Start,
    Stop,
    Error,
    Warn,
}

/// (frequência Hz, início s, duração s)
fn cue_notes(cue: Cue) -> &'static [(f32, f32, f32)] {
    match cue {
        // Mi5 → Lá5 (sobe: "pode falar")
        Cue::Start => &[(659.25, 0.0, 0.16), (880.0, 0.08, 0.20)],
        // Lá5 → Mi5 (desce: "gravação encerrada")
        Cue::Stop => &[(880.0, 0.0, 0.14), (659.25, 0.08, 0.20)],
        // Lá3 → Fá3 (grave: algo falhou)
        Cue::Error => &[(220.0, 0.0, 0.14), (174.61, 0.12, 0.22)],
        // Três bips curtos no mesmo tom (Sol5): perto do limite de requisições
        Cue::Warn => &[(783.99, 0.0, 0.09), (783.99, 0.13, 0.09), (783.99, 0.26, 0.12)],
    }
}

/// Sino suave: fundamental + 2º e 3º harmônicos, ataque de 5 ms, decaimento exponencial
/// e fade final de 20 ms (sem "clique" no corte, que o beep antigo tinha).
fn render_cue(cue: Cue, rate: f32, volume: f32) -> Vec<f32> {
    const LEAD: f32 = 0.04; // silêncio inicial: evita o início do som ser cortado pelo servidor de áudio
    const GAIN: f32 = 0.42;

    let notes = cue_notes(cue);
    let total = notes.iter().map(|n| n.1 + n.2).fold(0.0, f32::max) + LEAD;
    let n = (total * rate) as usize + 1;
    let mut out = vec![0.0f32; n];

    for &(freq, start, dur) in notes {
        let s0 = ((start + LEAD) * rate) as usize;
        let len = (dur * rate) as usize;
        for i in 0..len {
            let idx = s0 + i;
            if idx >= n {
                break;
            }
            let t = i as f32 / rate;
            let attack = (t / 0.005).min(1.0);
            let release = ((len - i) as f32 / (0.02 * rate)).min(1.0);
            let decay = (-t * 18.0).exp();
            let w = 2.0 * PI * freq * t;
            let tone = w.sin() + 0.30 * (2.0 * w).sin() + 0.10 * (3.0 * w).sin();
            out[idx] += tone * attack * release * decay * GAIN * volume;
        }
    }
    for s in out.iter_mut() {
        *s = s.clamp(-0.95, 0.95);
    }
    out
}

fn output_stream<T>(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    channels: usize,
    samples: Arc<Vec<f32>>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
{
    let mut idx = 0usize;
    device.build_output_stream(
        cfg,
        move |data: &mut [T], _| {
            for frame in data.chunks_mut(channels) {
                let v = samples.get(idx).copied().unwrap_or(0.0);
                idx += 1;
                let s = T::from_sample(v);
                for o in frame.iter_mut() {
                    *o = s;
                }
            }
        },
        |e| eprintln!("Erro no stream de saída: {e}"),
        None,
    )
}

/// Toca o som e só retorna quando ele terminou.
fn play_cue_blocking(cue: Cue, volume: f32) {
    if volume <= 0.0 {
        return;
    }
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else { return };
    let Ok(supported) = device.default_output_config() else { return };

    let rate = supported.sample_rate().0 as f32;
    let channels = supported.channels() as usize;
    let samples = Arc::new(render_cue(cue, rate, volume));
    let secs = samples.len() as f32 / rate;
    let cfg: cpal::StreamConfig = supported.config();

    let stream = match supported.sample_format() {
        SampleFormat::F32 => output_stream::<f32>(&device, &cfg, channels, samples),
        SampleFormat::I16 => output_stream::<i16>(&device, &cfg, channels, samples),
        SampleFormat::I32 => output_stream::<i32>(&device, &cfg, channels, samples),
        SampleFormat::U16 => output_stream::<u16>(&device, &cfg, channels, samples),
        SampleFormat::F64 => output_stream::<f64>(&device, &cfg, channels, samples),
        _ => return,
    };

    if let Ok(stream) = stream {
        if stream.play().is_ok() {
            thread::sleep(Duration::from_secs_f32(secs + 0.12));
        }
    }
}

fn play_cue(cue: Cue, volume: f32) {
    thread::spawn(move || play_cue_blocking(cue, volume));
}

// ───────────────────────────── captura ─────────────────────────────

struct Recorder {
    stream: cpal::Stream,
    buf: Arc<Mutex<Vec<f32>>>,
    rate: u32,
    started: Instant,
}

fn input_stream<T>(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    channels: usize,
    buf: Arc<Mutex<Vec<f32>>>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device.build_input_stream(
        cfg,
        move |data: &[T], _| {
            // Já converte para mono na captura: usa 1/N da memória.
            let mut b = lock(&*buf);
            for frame in data.chunks(channels) {
                let sum: f32 = frame.iter().map(|&s| f32::from_sample(s)).sum();
                b.push(sum / frame.len() as f32);
            }
        },
        |e| eprintln!("Erro no stream de entrada: {e}"),
        None,
    )
}

impl Recorder {
    fn open(device: cpal::Device, what: &str) -> Result<Self, String> {
        let supported = device
            .default_input_config()
            .map_err(|e| format!("Erro na configuração de {what}: {e}"))?;

        let rate = supported.sample_rate().0;
        let channels = supported.channels() as usize;
        let cfg: cpal::StreamConfig = supported.config();
        let buf = Arc::new(Mutex::new(Vec::with_capacity(rate as usize * 30)));

        let b = Arc::clone(&buf);
        let stream = match supported.sample_format() {
            SampleFormat::F32 => input_stream::<f32>(&device, &cfg, channels, b),
            SampleFormat::I16 => input_stream::<i16>(&device, &cfg, channels, b),
            SampleFormat::I32 => input_stream::<i32>(&device, &cfg, channels, b),
            SampleFormat::U16 => input_stream::<u16>(&device, &cfg, channels, b),
            SampleFormat::F64 => input_stream::<f64>(&device, &cfg, channels, b),
            SampleFormat::I8 => input_stream::<i8>(&device, &cfg, channels, b),
            SampleFormat::U8 => input_stream::<u8>(&device, &cfg, channels, b),
            SampleFormat::U32 => input_stream::<u32>(&device, &cfg, channels, b),
            other => return Err(format!("Formato de amostra não suportado: {other:?}")),
        }
        .map_err(|e| format!("Falha ao abrir {what}: {e}"))?;

        stream
            .play()
            .map_err(|e| format!("Falha ao iniciar {what}: {e}"))?;

        Ok(Recorder { stream, buf, rate, started: Instant::now() })
    }

    fn start() -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or("Microfone padrão não encontrado")?;
        Self::open(device, "microfone")
    }

    /// Captura o áudio de SAÍDA: percorre os dispositivos de saída e usa o primeiro que
    /// suporta loopback (monitor). PipeWire/Pulse: "Monitor of ..."; ALSA: "Loopback ...".
    fn from_output_device() -> Result<Self, String> {
        let host = cpal::default_host();
        let devices = host
            .output_devices()
            .map_err(|e| format!("Não foi listar dispositivos de saída: {e}"))?;

        let mut names = Vec::new();
        for d in devices {
            let name = d.name().unwrap_or_else(|_| "?".to_string());
            let name_l = name.to_lowercase();
            let is_monitor = name_l.contains("monitor")
                || name_l.contains("loopback")
                || name_l.contains("capture");
            if !is_monitor {
                continue;
            }
            names.push(name.clone());
            match Self::open(d, &name) {
                Ok(rec) => {
                    eprintln!("Áudio de saída capturado de {name:?}");
                    return Ok(rec);
                }
                Err(e) => {
                    eprintln!("Monitor {name:?} indisponível ({e}); tentando outro.");
                }
            }
        }
        let listed = if names.is_empty() {
            "nenhum".to_string()
        } else {
            names.join(", ")
        };
        Err(format!("Nenhum monitor de áudio encontrado. Dispositivos de saída: {listed}"))
    }

    /// Descarta o que foi capturado até agora (o próprio som de "início" vaza pelo microfone).
    fn discard(&mut self) {
        lock(&*self.buf).clear();
        self.started = Instant::now();
    }

    fn stop(self) -> (Vec<f32>, u32) {
        let Recorder { stream, buf, rate, .. } = self;
        drop(stream); // fecha o microfone antes de qualquer coisa
        let samples = std::mem::take(&mut *lock(&*buf));
        (samples, rate)
    }
}

// ───────────────────────────── processamento de áudio ─────────────────────────────

/// Reamostragem por sinc janelada (Hann) com filtro passa-baixa embutido.
/// Interpolação linear simples (versão anterior) deixa "aliasing" de frequências acima de 8 kHz
/// caírem na faixa da fala e piora o reconhecimento.
fn resample(input: &[f32], src: u32, dst: u32) -> Vec<f32> {
    if src == dst {
        return input.to_vec();
    }
    const PHASES: usize = 256;
    const TAPS_PER_SIDE: f64 = 16.0;
    let pi = std::f64::consts::PI;

    let ratio = src as f64 / dst as f64;
    let cutoff = (dst as f64 / src as f64).min(1.0) * 0.97;
    let half = TAPS_PER_SIDE / cutoff; // meia-largura, em amostras de entrada
    let h = half.ceil() as isize;
    let taps = (2 * h) as usize;

    // Tabela de coeficientes por sub-fase, já normalizados (ganho DC = 1).
    let mut table = vec![0f32; (PHASES + 1) * taps];
    for p in 0..=PHASES {
        let frac = p as f64 / PHASES as f64;
        let row = &mut table[p * taps..(p + 1) * taps];
        let mut sum = 0.0f64;
        let mut tmp = vec![0.0f64; taps];
        for (k, c) in tmp.iter_mut().enumerate() {
            let d = (k as isize - (h - 1)) as f64 - frac;
            let u = d / half;
            *c = if u.abs() >= 1.0 {
                0.0
            } else {
                let w = 0.5 + 0.5 * (pi * u).cos();
                let s = if d.abs() < 1e-12 {
                    1.0
                } else {
                    (pi * cutoff * d).sin() / (pi * cutoff * d)
                };
                s * w
            };
            sum += *c;
        }
        for (dst_c, c) in row.iter_mut().zip(tmp.iter()) {
            *dst_c = (*c / sum) as f32;
        }
    }

    let out_len = (input.len() as f64 / ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let center = pos.floor() as isize;
        let phase = ((pos - center as f64) * PHASES as f64).round() as usize;
        let row = &table[phase * taps..(phase + 1) * taps];
        let start = center - (h - 1);

        let acc: f32 = if start >= 0 && (start as usize + taps) <= input.len() {
            input[start as usize..start as usize + taps]
                .iter()
                .zip(row)
                .map(|(a, b)| a * b)
                .sum()
        } else {
            let mut a = 0.0f32;
            for (k, c) in row.iter().enumerate() {
                let idx = start + k as isize;
                if idx >= 0 && (idx as usize) < input.len() {
                    a += input[idx as usize] * c;
                }
            }
            a
        };
        out.push(acc);
    }
    out
}

/// mono f32 na taxa do microfone → PCM i16 a 16 kHz.
fn to_16k_pcm(mono: &[f32], src_rate: u32) -> Result<Vec<i16>, String> {
    let peak = mono.iter().fold(0.0f32, |a, &x| a.max(x.abs()));
    if peak < 0.001 {
        return Err("Áudio mudo: verifique o microfone padrão do sistema".to_string());
    }
    let resampled = resample(mono, src_rate, TARGET_RATE);
    Ok(resampled
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i16)
        .collect())
}

struct EncodedAudio {
    bytes: Vec<u8>,
    filename: &'static str,
    mime: &'static str,
}

fn encode_wav(pcm: &[i16]) -> EncodedAudio {
    let data_len = (pcm.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&TARGET_RATE.to_le_bytes());
    v.extend_from_slice(&(TARGET_RATE * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        v.extend_from_slice(&s.to_le_bytes());
    }
    EncodedAudio { bytes: v, filename: "audio.wav", mime: "audio/wav" }
}

/// Ogg/Opus mono 16 kHz, 24 kbps VBR, modo VoIP (otimizado para fala).
fn encode_ogg_opus(pcm: &[i16]) -> Result<EncodedAudio, String> {
    const FRAME: usize = 320; // 20 ms a 16 kHz
    const PRE_SKIP: u16 = 312; // atraso do codificador, em amostras de 48 kHz
    const LOOKAHEAD_16K: usize = 104; // 312 / 3
    const SERIAL: u32 = 0x5746_4C57;

    let mut enc = opus::Encoder::new(TARGET_RATE, opus::Channels::Mono, opus::Application::Voip)
        .map_err(|e| format!("Opus: {e}"))?;
    enc.set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE))
        .map_err(|e| format!("Opus: {e}"))?;

    let mut out = Vec::with_capacity(pcm.len() / 8);
    {
        let mut w = ogg::PacketWriter::new(&mut out);
        let io_err = |e: io::Error| format!("Ogg: {e}");

        // Cabeçalho "OpusHead" (RFC 7845)
        let mut head = Vec::with_capacity(19);
        head.extend_from_slice(b"OpusHead");
        head.push(1); // versão
        head.push(1); // canais
        head.extend_from_slice(&PRE_SKIP.to_le_bytes());
        head.extend_from_slice(&TARGET_RATE.to_le_bytes());
        head.extend_from_slice(&0i16.to_le_bytes()); // ganho
        head.push(0); // mapping family
        w.write_packet(head, SERIAL, ogg::PacketWriteEndInfo::EndPage, 0)
            .map_err(io_err)?;

        // "OpusTags"
        let vendor = b"whisper-flow";
        let mut tags = Vec::new();
        tags.extend_from_slice(b"OpusTags");
        tags.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        tags.extend_from_slice(vendor);
        tags.extend_from_slice(&0u32.to_le_bytes());
        w.write_packet(tags, SERIAL, ogg::PacketWriteEndInfo::EndPage, 0)
            .map_err(io_err)?;

        // Frames extras no fim para "descarregar" o atraso do codificador.
        let total_frames = (pcm.len() + LOOKAHEAD_16K + FRAME - 1) / FRAME;
        // Granule position em unidades de 48 kHz; o decodificador descarta o excesso.
        let final_gp = pcm.len() as u64 * 3 + PRE_SKIP as u64;

        let mut frame = [0i16; FRAME];
        let mut pkt = [0u8; 1500];
        for n in 0..total_frames {
            frame.fill(0);
            let start = n * FRAME;
            if start < pcm.len() {
                let end = (start + FRAME).min(pcm.len());
                frame[..end - start].copy_from_slice(&pcm[start..end]);
            }
            let len = enc.encode(&frame, &mut pkt).map_err(|e| format!("Opus: {e}"))?;
            let last = n + 1 == total_frames;
            let (info, gp) = if last {
                (ogg::PacketWriteEndInfo::EndStream, final_gp)
            } else {
                (ogg::PacketWriteEndInfo::NormalPacket, ((n + 1) * 960) as u64)
            };
            w.write_packet(pkt[..len].to_vec(), SERIAL, info, gp)
                .map_err(io_err)?;
        }
    }
    Ok(EncodedAudio { bytes: out, filename: "audio.ogg", mime: "audio/ogg" })
}

// ───────────────────────────── API Groq ─────────────────────────────

/// Janela deslizante de requisições: O(1) amortizado, sem alocar depois de aquecida.
struct RateLimiter {
    hits: Mutex<VecDeque<Instant>>,
}

impl RateLimiter {
    fn new() -> Self {
        RateLimiter { hits: Mutex::new(VecDeque::with_capacity(RPM_LIMIT + 8)) }
    }

    /// Registra uma requisição agora e devolve quantas houve nos últimos 60 s (incluindo esta).
    fn record(&self) -> usize {
        let now = Instant::now();
        let mut h = lock(&self.hits);
        while h.front().map_or(false, |&t| now.duration_since(t) >= RPM_WINDOW) {
            h.pop_front();
        }
        h.push_back(now);
        h.len()
    }
}

struct SendError {
    /// None = falha de rede.
    status: Option<u16>,
    msg: String,
}

fn send_once(app: &App, key: &str, audio: &EncodedAudio) -> Result<String, SendError> {
    let cfg = &app.cfg;

    // Cada tentativa HTTP (inclusive retry) consome cota do Groq, então conta todas.
    let n = app.limiter.record();
    if n >= RPM_WARN {
        eprintln!("Atenção: {n}/{RPM_LIMIT} requisições no último minuto (limite do Groq).");
        play_cue(Cue::Warn, cfg.volume);
    }

    let net = |e: reqwest::Error| SendError { status: None, msg: e.to_string() };

    let part = multipart::Part::bytes(audio.bytes.clone())
        .file_name(audio.filename)
        .mime_str(audio.mime)
        .map_err(net)?;

    let mut form = multipart::Form::new()
        .part("file", part)
        .text("model", cfg.model.clone())
        .text("response_format", "json")
        .text("temperature", "0");
    if let Some(lang) = &cfg.language {
        form = form.text("language", lang.clone());
    }

    let res = app
        .client
        .post(API_URL)
        .bearer_auth(key)
        .multipart(form)
        .send()
        .map_err(net)?;

    let status = res.status();
    if !status.is_success() {
        let body = res.text().unwrap_or_default();
        let body: String = body.chars().take(300).collect();
        return Err(SendError { status: Some(status.as_u16()), msg: format!("Groq {status}: {body}") });
    }

    let json: serde_json::Value = res.json().map_err(net)?;
    Ok(json["text"].as_str().unwrap_or("").trim().to_string())
}

/// Até 3 tentativas para falhas de rede, 429 e 5xx.
fn send_with_retry(app: &App, key: &str, audio: &EncodedAudio) -> Result<String, SendError> {
    let mut attempt: u64 = 0;
    loop {
        match send_once(app, key, audio) {
            Ok(t) => return Ok(t),
            Err(e) => {
                let transient = e.status.map_or(true, |s| s == 429 || s >= 500);
                attempt += 1;
                if !transient || attempt >= 3 {
                    return Err(e);
                }
                thread::sleep(Duration::from_millis(500 * attempt));
            }
        }
    }
}

fn transcribe(app: &App, key: &str, pcm: &[i16]) -> Result<String, String> {
    let secs = pcm.len() as f32 / TARGET_RATE as f32;

    if secs > OPUS_THRESHOLD_SECS {
        match encode_ogg_opus(pcm) {
            Ok(audio) => match send_with_retry(app, key, &audio) {
                Ok(t) => return Ok(t),
                Err(e) if matches!(e.status, Some(400 | 415 | 422)) => {
                    eprintln!("Groq recusou Opus ({}); tentando WAV", e.msg);
                }
                Err(e) => return Err(e.msg),
            },
            Err(e) => eprintln!("Opus indisponível ({e}); usando WAV"),
        }
    }

    let wav = encode_wav(pcm);
    if wav.bytes.len() > MAX_UPLOAD_BYTES {
        return Err("Áudio grande demais para enviar como WAV".to_string());
    }
    send_with_retry(app, key, &wav).map_err(|e| e.msg)
}

// ───────────────────────────── saída de texto ─────────────────────────────

enum Session {
    Wayland(String),
    X11,
}

/// Não depende só de variáveis de ambiente: em serviço systemd elas podem faltar.
fn detect_session() -> Session {
    if let Ok(d) = env::var("WAYLAND_DISPLAY") {
        if !d.is_empty() {
            return Session::Wayland(d);
        }
    }
    if let Ok(dir) = env::var("XDG_RUNTIME_DIR") {
        if let Ok(rd) = fs::read_dir(&dir) {
            let mut socks: Vec<String> = rd
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with("wayland-") && !n.ends_with(".lock"))
                .collect();
            socks.sort();
            if let Some(s) = socks.into_iter().next() {
                return Session::Wayland(s);
            }
        }
    }
    if env::var("XDG_SESSION_TYPE").map_or(false, |t| t == "wayland") {
        return Session::Wayland("wayland-0".to_string());
    }
    Session::X11
}

fn build_vkbd() -> io::Result<VirtualDevice> {
    let mut keys = AttributeSet::<Key>::new();
    for k in [Key::KEY_LEFTCTRL, Key::KEY_LEFTSHIFT, Key::KEY_V, Key::KEY_INSERT] {
        keys.insert(k);
    }
    VirtualDeviceBuilder::new()?
        .name(VKBD_NAME)
        .with_keys(&keys)?
        .build()
}

fn key_event(kbd: &mut VirtualDevice, key: Key, value: i32) -> io::Result<()> {
    kbd.emit(&[InputEvent::new(EventType::KEY, key.code(), value)])
}

fn send_combo(kbd: &mut VirtualDevice, combo: PasteCombo) -> io::Result<()> {
    let keys: &[Key] = match combo {
        PasteCombo::CtrlV => &[Key::KEY_LEFTCTRL, Key::KEY_V],
        PasteCombo::CtrlShiftV => &[Key::KEY_LEFTCTRL, Key::KEY_LEFTSHIFT, Key::KEY_V],
        PasteCombo::ShiftInsert => &[Key::KEY_LEFTSHIFT, Key::KEY_INSERT],
    };
    for &k in keys {
        key_event(kbd, k, 1)?;
        thread::sleep(Duration::from_millis(8));
    }
    for &k in keys.iter().rev() {
        key_event(kbd, k, 0)?;
        thread::sleep(Duration::from_millis(8));
    }
    Ok(())
}

struct Typer {
    mode: OutputMode,
    combo: PasteCombo,
    vkbd: Mutex<Option<VirtualDevice>>,
}

impl Typer {
    fn new(cfg: &Config) -> Self {
        let vkbd = match build_vkbd() {
            Ok(d) => Some(d),
            Err(e) => {
                eprintln!("Sem teclado virtual (/dev/uinput: {e}); o modo 'paste' ficará indisponível.");
                None
            }
        };
        Typer { mode: cfg.output, combo: cfg.paste, vkbd: Mutex::new(vkbd) }
    }

    /// Copia o texto para a área de transferência (sem colar em nada). Wayland: wl-copy;
    /// X11: xclip/xsel. Usado pelo atalho de captura de saída.
    fn copy_to_clipboard(&self, text: &str) -> Result<(), String> {
        let session = detect_session();
        match session {
            Session::Wayland(d) => {
                let mut cmd = Command::new("wl-copy");
                cmd.args(["--type", "text/plain;charset=utf-8"])
                    .env("WAYLAND_DISPLAY", &d)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                let mut child = cmd.spawn().map_err(|e| format!("wl-copy: {e}"))?;
                {
                    let mut stdin = child.stdin.take().ok_or("wl-copy sem stdin")?;
                    stdin.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
                }
                let status = child.wait().map_err(|e| e.to_string())?;
                if status.success() {
                    Ok(())
                } else {
                    Err("wl-copy falhou".to_string())
                }
            }
            Session::X11 => {
                for (bin, args) in [
                    ("xclip", &["-selection", "clipboard"][..]),
                    ("xsel", &["--clipboard", "--input"][..]),
                ] {
                    if let Ok(mut child) = Command::new(bin)
                        .args(args)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                    {
                        if let Some(mut s) = child.stdin.take() {
                            let _ = s.write_all(text.as_bytes());
                        }
                        if child.wait().map_or(false, |s| s.success()) {
                            return Ok(());
                        }
                    }
                }
                Err("xclip/xsel não instalados".to_string())
            }
        }
    }

    fn output(&self, text: &str, clipboard_only: bool) -> Result<(), String> {
        if clipboard_only {
            return self.copy_to_clipboard(text);
        }
        let session = detect_session();
        let display = match &session {
            Session::Wayland(d) => Some(d.as_str()),
            Session::X11 => None,
        };

        let order: Vec<OutputMode> = match (self.mode, &session) {
            (OutputMode::Auto, Session::Wayland(_)) => {
                vec![OutputMode::Paste, OutputMode::Ydotool, OutputMode::Xdotool]
            }
            (OutputMode::Auto, Session::X11) => vec![OutputMode::Xdotool],
            (m, _) => vec![m],
        };

        let mut errors = Vec::new();
        for m in order {
            let r = match m {
                OutputMode::Paste => self.paste(display, text),
                OutputMode::Xdotool => Self::xdotool(text),
                OutputMode::Ydotool => Self::ydotool(text),
                OutputMode::Auto => unreachable!(),
            };
            match r {
                Ok(()) => return Ok(()),
                Err(e) => errors.push(format!("{m:?}: {e}")),
            }
        }
        Err(errors.join(" | "))
    }

    /// Wayland (KDE e GNOME): coloca no clipboard com wl-copy e envia Ctrl+V por um teclado
    /// virtual (uinput). Preserva acentos em qualquer layout, ao contrário de `ydotool type`.
    fn paste(&self, display: Option<&str>, text: &str) -> Result<(), String> {
        let mut guard = lock(&self.vkbd);
        let kbd = guard.as_mut().ok_or("teclado virtual indisponível")?;

        let mut cmd = Command::new("wl-copy");
        cmd.args(["--type", "text/plain;charset=utf-8"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(d) = display {
            cmd.env("WAYLAND_DISPLAY", d);
        }
        let mut child = cmd.spawn().map_err(|e| format!("wl-copy: {e}"))?;
        {
            let mut stdin = child.stdin.take().ok_or("wl-copy sem stdin")?;
            stdin.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
        } // fecha o stdin → wl-copy assume o clipboard
        let status = child.wait().map_err(|e| e.to_string())?;
        if !status.success() {
            return Err("wl-copy falhou".to_string());
        }

        thread::sleep(Duration::from_millis(60));
        send_combo(kbd, self.combo).map_err(|e| format!("uinput: {e}"))?;
        thread::sleep(Duration::from_millis(250)); // deixa o app ler o clipboard
        Ok(())
    }

    fn xdotool(text: &str) -> Result<(), String> {
        let ok = run_ok(
            Command::new("xdotool")
                .args(["type", "--clearmodifiers", "--delay", "1", "--"])
                .arg(text),
        );
        if ok { Ok(()) } else { Err("xdotool falhou ou não instalado".to_string()) }
    }

    fn ydotool(text: &str) -> Result<(), String> {
        let ok = run_ok(
            Command::new("ydotool")
                .args(["type", "--key-delay", "2", "--"])
                .arg(text),
        );
        if ok { Ok(()) } else { Err("ydotool falhou (ydotoold rodando?)".to_string()) }
    }
}

// ───────────────────────────── atalho global (evdev) ─────────────────────────────

fn is_hotkey_keyboard(device: &Device, hk: Hotkey) -> bool {
    let name = device.name().unwrap_or("");
    if name == VKBD_NAME || name.to_lowercase().contains("ydotool") {
        return false;
    }
    device.supported_keys().map_or(false, |k| {
        k.contains(hk.key)
            && (hk.mods & MOD_CTRL == 0 || k.contains(Key::KEY_LEFTCTRL) || k.contains(Key::KEY_RIGHTCTRL))
            && (hk.mods & MOD_SHIFT == 0 || k.contains(Key::KEY_LEFTSHIFT) || k.contains(Key::KEY_RIGHTSHIFT))
            && (hk.mods & MOD_ALT == 0 || k.contains(Key::KEY_LEFTALT) || k.contains(Key::KEY_RIGHTALT))
            && (hk.mods & MOD_META == 0 || k.contains(Key::KEY_LEFTMETA) || k.contains(Key::KEY_RIGHTMETA))
    })
}

/// bit por tecla física (esq/dir) → máscara de modificadores lógicos.
#[inline]
fn held_mods(h: u8) -> u8 {
    (if h & 0b0000_0011 != 0 { MOD_CTRL } else { 0 })
        | (if h & 0b0000_1100 != 0 { MOD_SHIFT } else { 0 })
        | (if h & 0b0011_0000 != 0 { MOD_ALT } else { 0 })
        | (if h & 0b1100_0000 != 0 { MOD_META } else { 0 })
}

fn listen_device(
    mut device: Device,
    path: PathBuf,
    tx: mpsc::Sender<Hotkey>,
    active: Arc<Mutex<HashSet<PathBuf>>>,
    hk1: Hotkey,
    hk2: Hotkey,
) {
    let mut held: u8 = 0;

    while let Ok(events) = device.fetch_events() {
        for ev in events {
            let InputEventKind::Key(key) = ev.kind() else { continue };
            let val = ev.value();

            let bit: u8 = match key {
                Key::KEY_LEFTCTRL => 1 << 0,
                Key::KEY_RIGHTCTRL => 1 << 1,
                Key::KEY_LEFTSHIFT => 1 << 2,
                Key::KEY_RIGHTSHIFT => 1 << 3,
                Key::KEY_LEFTALT => 1 << 4,
                Key::KEY_RIGHTALT => 1 << 5,
                Key::KEY_LEFTMETA => 1 << 6,
                Key::KEY_RIGHTMETA => 1 << 7,
                _ => 0,
            };
            if bit != 0 {
                if val != 0 {
                    held |= bit;
                } else {
                    held &= !bit;
                }
                continue;
            }
            // val == 1: só o pressionar (ignora soltar=0 e auto-repetição=2)
            let matched = [hk1, hk2].iter().copied().find(|hk| {
                val == 1 && key == hk.key && held_mods(held) == hk.mods
            });
            if let Some(hk) = matched {
                let _ = tx.send(hk);
            }
        }
    }
    // Teclado removido: libera o caminho para ser reaberto se voltar.
    lock(&*active).remove(&path);
}

/// Varre /dev/input a cada 3 s: teclados USB/Bluetooth conectados depois do boot funcionam.
/// Escuta dois atalhos distintos; cada teclado abre uma única thread que filtra ambos.
fn spawn_hotkey_watcher(tx: mpsc::Sender<Hotkey>, hk1: Hotkey, hk2: Hotkey) {
    thread::spawn(move || {
        let active: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
        let mut warned = false;

        loop {
            match fs::read_dir("/dev/input") {
                Err(e) => {
                    if !warned {
                        eprintln!("Não foi possível ler /dev/input: {e}");
                        warned = true;
                    }
                }
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        let is_event = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map_or(false, |n| n.starts_with("event"));
                        if !is_event || lock(&*active).contains(&path) {
                            continue;
                        }
                        let device = match Device::open(&path) {
                            Ok(d) => d,
                            Err(e) => {
                                if e.kind() == io::ErrorKind::PermissionDenied && !warned {
                                    eprintln!(
                                        "Sem permissão em /dev/input (adicione o usuário ao grupo 'input')."
                                    );
                                    warned = true;
                                }
                                continue;
                            }
                        };
                        // Abre o teclado se suportar QUALQUER um dos dois atalhos.
                        if !is_hotkey_keyboard(&device, hk1) && !is_hotkey_keyboard(&device, hk2) {
                            continue;
                        }
                        lock(&*active).insert(path.clone());
                        eprintln!("Monitorando teclado: {} ({path:?})", device.name().unwrap_or("?"));

                        let (tx, active) = (tx.clone(), Arc::clone(&active));
                        thread::spawn(move || listen_device(device, path, tx, active, hk1, hk2));
                    }
                }
            }
            thread::sleep(Duration::from_secs(3));
        }
    });
}

// ───────────────────────────── aplicação ─────────────────────────────

struct App {
    cfg: Config,
    typer: Typer,
    client: Client,
    limiter: RateLimiter,
    processing: AtomicBool,
}

/// Garante que `processing` volta a false mesmo se a thread entrar em pânico.
struct Busy(Arc<App>);
impl Drop for Busy {
    fn drop(&mut self) {
        self.0.processing.store(false, Ordering::SeqCst);
    }
}

fn process(app: &App, samples: Vec<f32>, rate: u32, clipboard_only: bool) {
    let secs = samples.len() as f32 / rate as f32;
    if secs < MIN_SECS {
        return;
    }

    let result = (|| -> Result<String, String> {
        let pcm = to_16k_pcm(&samples, rate)?;
        drop(samples);
        let key = api_key(&app.cfg)?;
        transcribe(app, &key, &pcm)
    })();

    match result {
        Ok(text) => {
            let text = text.trim();
            if text.is_empty() {
                return;
            }
            eprintln!("Transcrição: {} caracteres", text.chars().count());
            if let Err(e) = app.typer.output(text, clipboard_only) {
                eprintln!("Falha ao inserir o texto: {e}");
                play_cue(Cue::Error, app.cfg.volume);
            }
        }
        Err(e) => {
            eprintln!("Erro: {e}");
            play_cue(Cue::Error, app.cfg.volume);
        }
    }
}

fn finish_recording(app: &Arc<App>, rec: Recorder) {
    let (samples, rate) = rec.stop();
    play_cue(Cue::Stop, app.cfg.volume);

    app.processing.store(true, Ordering::SeqCst);
    let app = Arc::clone(app);
    thread::spawn(move || {
        let _busy = Busy(Arc::clone(&app));
        process(&app, samples, rate, false);
    });
}

fn finish_monitor(app: &Arc<App>, rec: Recorder) {
    let (samples, rate) = rec.stop();
    play_cue(Cue::Stop, app.cfg.volume);

    app.processing.store(true, Ordering::SeqCst);
    let app = Arc::clone(app);
    thread::spawn(move || {
        let _busy = Busy(Arc::clone(&app));
        process(&app, samples, rate, app.cfg.clipboard_only);
    });
}

fn main() {
    let cfg = Config::load(&Env::load());
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(90))
        .build()
        .expect("falha ao criar cliente HTTP");

    let app = Arc::new(App {
        typer: Typer::new(&cfg),
        cfg,
        client,
        limiter: RateLimiter::new(),
        processing: AtomicBool::new(false),
    });

    let (tx, rx) = mpsc::channel::<Hotkey>();
    spawn_hotkey_watcher(tx, app.cfg.hotkey, app.cfg.hotkey2);
    eprintln!(
        "whisper-flow pronto. Atalho: {} (microfone) | {} (área de transferência) | idioma: {}",
        app.cfg.hotkey_label,
        app.cfg.hotkey2_label,
        app.cfg.language.as_deref().unwrap_or("auto")
    );

    let mut recorder: Option<Recorder> = None;
    let mut last_toggle: Option<Instant> = None;

    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(hk) => {
                if last_toggle.map_or(false, |t| t.elapsed() < HOTKEY_DEBOUNCE) {
                    continue;
                }
                last_toggle = Some(Instant::now());

                // Mesmo atalho inicia/para. O atalho2 (área de transferência) só inicia.
                if hk == app.cfg.hotkey {
                    if let Some(rec) = recorder.take() {
                        finish_recording(&app, rec);
                        continue;
                    }
                    if app.processing.load(Ordering::SeqCst) {
                        play_cue(Cue::Error, app.cfg.volume); // ainda transcrevendo a anterior
                        continue;
                    }

                    // Abre o microfone ANTES do aviso (sem perder o começo da fala) e descarta
                    // o que foi capturado durante o som para ele não entrar na transcrição.
                    match Recorder::start() {
                        Ok(mut rec) => {
                            play_cue_blocking(Cue::Start, app.cfg.volume);
                            rec.discard();
                            while rx.try_recv().is_ok() {} // ignora toques duplicados durante o aviso
                            recorder = Some(rec);
                        }
                        Err(e) => {
                            eprintln!("{e}");
                            play_cue(Cue::Error, app.cfg.volume);
                        }
                    }
                } else {
                    // Atalho2: captura o áudio de SAÍDA e col só na área de transferência.
                    if recorder.is_some() || app.processing.load(Ordering::SeqCst) {
                        play_cue(Cue::Error, app.cfg.volume);
                        continue;
                    }
                    match Recorder::from_output_device() {
                        Ok(mut rec) => {
                            play_cue_blocking(Cue::Start, app.cfg.volume);
                            rec.discard();
                            while rx.try_recv().is_ok() {}
                            recorder = Some(rec);
                        }
                        Err(e) => {
                            eprintln!("{e}");
                            play_cue(Cue::Error, app.cfg.volume);
                        }
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let expired = recorder
                    .as_ref()
                    .map_or(false, |r| r.started.elapsed().as_secs() >= MAX_RECORD_SECS);
                if expired {
                    eprintln!("Limite de {MAX_RECORD_SECS}s atingido; encerrando gravação.");
                    if let Some(rec) = recorder.take() {
                        // Não sabemos qual atalho acionou a gravação atual; a saída é a mesma
                        // para os dois (área de transferência é o modo mais seguro).
                        finish_monitor(&app, rec);
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

// ───────────────────────────── testes ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f32, rate: u32, secs: f32) -> Vec<f32> {
        (0..(rate as f32 * secs) as usize)
            .map(|i| (2.0 * PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn hotkey2_parsing_default() {
        let h = parse_hotkey(DEFAULT_HOTKEY2).unwrap();
        assert_eq!(h, Hotkey { mods: MOD_CTRL | MOD_SHIFT, key: Key::KEY_SPACE });
    }

    #[test]
    fn resample_keeps_speech_band_and_kills_alias() {
        for src in [48_000u32, 44_100] {
            let keep = resample(&sine(1000.0, src, 0.5), src, TARGET_RATE);
            let alias = resample(&sine(9500.0, src, 0.5), src, TARGET_RATE);
            let mid = |v: &[f32]| rms(&v[400..v.len() - 400]);
            assert!((mid(&keep) - 0.707).abs() < 0.02, "1 kHz deveria passar");
            assert!(mid(&alias) < 0.02, "9,5 kHz deveria ser filtrado");
        }
    }

    #[test]
    fn hotkey_parsing() {
        let h = parse_hotkey("Ctrl+Space").unwrap();
        assert_eq!(h, Hotkey { mods: MOD_CTRL, key: Key::KEY_SPACE });
        let h = parse_hotkey("ctrl + shift + F9").unwrap();
        assert_eq!(h, Hotkey { mods: MOD_CTRL | MOD_SHIFT, key: Key::KEY_F9 });
        assert_eq!(parse_hotkey("pause").unwrap().mods, 0);
        assert!(parse_hotkey("ctrl+").is_err());
        assert!(parse_hotkey("ctrl+shift").is_err());
        assert!(parse_hotkey("a+b").is_err());
        assert!(parse_hotkey("ctrl+banana").is_err());
    }

    #[test]
    fn language_normalization() {
        assert_eq!(normalize_lang("pt-br").unwrap(), Some("pt".into()));
        assert_eq!(normalize_lang("PT_BR").unwrap(), Some("pt".into()));
        assert_eq!(normalize_lang("en").unwrap(), Some("en".into()));
        assert_eq!(normalize_lang("auto").unwrap(), None);
        assert!(normalize_lang("português").is_err());
    }

    #[test]
    fn dotenv_parsing() {
        let m = parse_dotenv(
            "# c\nWHISPER_HOTKEY=ctrl+space # inline\nexport WHISPER_LANG=\"pt-br\"\n\nGROQ_API_KEY='abc'\nlixo\n",
        );
        assert_eq!(m["WHISPER_HOTKEY"], "ctrl+space");
        assert_eq!(m["WHISPER_LANG"], "pt-br");
        assert_eq!(m["GROQ_API_KEY"], "abc");
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn rate_limiter_counts() {
        let r = RateLimiter::new();
        for i in 1..=RPM_WARN {
            assert_eq!(r.record(), i);
        }
    }

    #[test]
    fn wav_header_is_consistent() {
        let w = encode_wav(&[0i16; 160]);
        assert_eq!(&w.bytes[0..4], b"RIFF");
        assert_eq!(w.bytes.len(), 44 + 320);
    }

    #[test]
    fn opus_stream_has_headers() {
        let pcm: Vec<i16> = sine(440.0, TARGET_RATE, 2.0)
            .iter()
            .map(|&s| (s * 8000.0) as i16)
            .collect();
        let o = encode_ogg_opus(&pcm).unwrap();
        assert_eq!(&o.bytes[0..4], b"OggS");
        assert!(o.bytes.windows(8).any(|w| w == b"OpusHead"));
        assert!(o.bytes.len() < pcm.len() * 2 / 5);
    }
}
