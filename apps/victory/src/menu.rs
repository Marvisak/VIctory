//! What opening `victory.exe` from Explorer shows: a small console menu,
//! where the command-line help used to flash and close at once (#527).
//!
//! Run a game (its executable dragged onto the window, or typed), run the
//! last one again, check this PC's setup (Windows 10's shader
//! model, AVX2, the GPU's shader model, mesh shaders and wave size, the
//! configured paths), or print the data-folder path. A title runs in a child
//! `victory run`, so the menu stays open after it exits and reports its
//! status.
//!
//! S sets the game folder in `victory.toml`, K shows the keyboard's
//! stand-in pad, and M switches the saved console profile.
//!
//! `NO_COLOR` disables terminal color, as does a console without
//! virtual-terminal sequences.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;

/// Whether `victory` was started with no arguments in a console of its own
/// (a double-click), or `VICTORY_MENU=1` asks for the menu anyway.
pub fn wanted() -> bool {
    if std::env::args_os().len() != 1 {
        return false;
    }
    std::env::var("VICTORY_MENU").is_ok_and(|v| v == "1") || own_console()
}

/// The console has no other process attached: Explorer made it for us.
#[cfg(windows)]
fn own_console() -> bool {
    let mut ids = [0u32; 2];
    // SAFETY: the buffer holds `ids.len()` process ids.
    let n = unsafe {
        windows_sys::Win32::System::Console::GetConsoleProcessList(
            ids.as_mut_ptr(),
            ids.len() as u32,
        )
    };
    n == 1
}

#[cfg(not(windows))]
fn own_console() -> bool {
    false
}

/// A 24-bit colour.
type Rgb = (u8, u8, u8);

/// The sunset, top to bottom: dusk blue, violet, orchid, pink, peach, amber.
const SUNSET: [Rgb; 6] = [
    (86, 98, 196),
    (134, 96, 204),
    (198, 104, 184),
    (236, 128, 164),
    (246, 160, 124),
    (250, 190, 110),
];
const CREAM: Rgb = (255, 244, 230);
const DIM: Rgb = (176, 150, 190);
const GOOD: Rgb = (120, 220, 200);
const WARN: Rgb = (250, 190, 110);
const BAD: Rgb = (240, 100, 140);

/// Writes text in colour, or plain.
#[derive(Clone, Copy)]
struct Paint {
    color: bool,
}

impl Paint {
    fn rgb(self, text: &str, (r, g, b): Rgb) -> String {
        if self.color {
            format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    fn bold(self, text: &str, c: Rgb) -> String {
        if self.color {
            format!("\x1b[1m{}", self.rgb(text, c))
        } else {
            text.to_owned()
        }
    }

    /// Each character of `text` at its place along `stops`.
    fn gradient(self, text: &str, stops: &[Rgb]) -> String {
        if !self.color {
            return text.to_owned();
        }
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len().max(2) - 1;
        let mut out = String::from("\x1b[1m");
        for (i, ch) in chars.iter().enumerate() {
            let (r, g, b) = along(stops, i as f32 / n as f32);
            out.push_str(&format!("\x1b[38;2;{r};{g};{b}m{ch}"));
        }
        out.push_str("\x1b[0m");
        out
    }
}

/// The colour at `t` (0..=1) along evenly spaced `stops`.
fn along(stops: &[Rgb], t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
    let i = (t.floor() as usize).min(stops.len() - 2);
    let f = t - i as f32;
    let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * f).round() as u8;
    let (a, b) = (stops[i], stops[i + 1]);
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// Turns on virtual-terminal sequences; false where the console has none.
/// `NO_COLOR` turns colour off, `CLICOLOR_FORCE=1` on (output to a file).
#[cfg(windows)]
fn enable_color() -> bool {
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE,
        SetConsoleMode,
    };
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if std::env::var("CLICOLOR_FORCE").is_ok_and(|v| v == "1") {
        return true;
    }
    // SAFETY: plain console calls on our own output handle.
    unsafe {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        GetConsoleMode(out, &mut mode) != 0
            && SetConsoleMode(out, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

#[cfg(not(windows))]
fn enable_color() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

/// What a menu answer asks for.
#[derive(Debug, PartialEq, Eq)]
enum Choice {
    Run,
    Again,
    Check,
    Data,
    Translate,
    Folder,
    Keys,
    Model,
    Quit,
    Unknown,
}

fn choice(input: &str) -> Choice {
    // (A byte-order mark leads piped input from some shells.)
    match input
        .trim_start_matches('\u{feff}')
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "1" => Choice::Run,
        "2" | "r" => Choice::Again,
        "3" | "c" => Choice::Check,
        "4" | "d" => Choice::Data,
        "5" | "t" => Choice::Translate,
        "s" => Choice::Folder,
        "k" => Choice::Keys,
        "m" => Choice::Model,
        "q" | "0" | "x" | "exit" | "quit" => Choice::Quit,
        _ => Choice::Unknown,
    }
}

/// A path as a console gives it when a file is dragged onto the window or
/// pasted: spaces around it, quotes around it, PowerShell's `& '...'`.
fn clean_path(input: &str) -> PathBuf {
    let mut s = input.trim_start_matches('\u{feff}').trim();
    if let Some(rest) = s.strip_prefix('&') {
        s = rest.trim();
    }
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        return PathBuf::from(&s[1..s.len() - 1]);
    }
    if s.len() >= 2 && s.starts_with('\'') && s.ends_with('\'') {
        // PowerShell doubles an apostrophe inside single quotes.
        return PathBuf::from(s[1..s.len() - 1].replace("''", "'"));
    }
    PathBuf::from(s)
}

/// Where the last game run from the menu is remembered.
fn last_file(data_dir: &Path) -> PathBuf {
    data_dir.join("menu-last-game.txt")
}

fn load_last(data_dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(last_file(data_dir)).ok()?;
    let path = PathBuf::from(text.trim());
    path.is_file().then_some(path)
}

fn save_last(data_dir: &Path, exe: &Path) {
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(last_file(data_dir), exe.display().to_string());
}

/// The console model runs from the menu use (`victory run --console`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Model {
    SeriesX,
    SeriesS,
}

impl Model {
    fn arg(self) -> &'static str {
        match self {
            Model::SeriesX => "series-x",
            Model::SeriesS => "series-s",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Model::SeriesX => "Series X",
            Model::SeriesS => "Series S",
        }
    }

    fn other(self) -> Self {
        match self {
            Model::SeriesX => Model::SeriesS,
            Model::SeriesS => Model::SeriesX,
        }
    }
}

/// Where the menu's console model is remembered.
fn model_file(data_dir: &Path) -> PathBuf {
    data_dir.join("menu-console.txt")
}

fn load_model(data_dir: &Path) -> Model {
    match std::fs::read_to_string(model_file(data_dir)) {
        Ok(t) if t.trim() == Model::SeriesS.arg() => Model::SeriesS,
        _ => Model::SeriesX,
    }
}

fn save_model(data_dir: &Path, model: Model) {
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(model_file(data_dir), model.arg());
}

/// The config file the tools read: `VICTORY_CONFIG`, else `victory.toml`
/// in the current folder (beside `victory.exe` when it was double-clicked).
fn config_path() -> PathBuf {
    std::env::var_os("VICTORY_CONFIG")
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from("victory.toml"), PathBuf::from)
}

/// `text` (a config file, possibly empty) with `[paths].game_dir` set to
/// `dir`: the active `game_dir` line replaced, else one added under
/// `[paths]`, else a `[paths]` table appended. Comments and every other line
/// stay as they were.
fn set_game_dir(text: &str, dir: &Path) -> String {
    let dir = dir.display().to_string();
    // A literal string keeps backslashes as they are; it cannot hold an
    // apostrophe, which a basic string with escapes can.
    let value = if dir.contains('\'') {
        format!("\"{}\"", dir.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        format!("'{dir}'")
    };
    let line = format!("game_dir = {value}");
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let header = |l: &str| l.trim_start().starts_with('[');
    match lines.iter().position(|l| l.trim() == "[paths]") {
        Some(at) => {
            let end = lines[at + 1..]
                .iter()
                .position(|l| header(l))
                .map_or(lines.len(), |n| at + 1 + n);
            let active = lines[at + 1..end].iter().position(|l| {
                let l = l.trim_start();
                l.strip_prefix("game_dir")
                    .is_some_and(|rest| rest.trim_start().starts_with('='))
            });
            match active {
                Some(n) => lines[at + 1 + n] = line,
                None => lines.insert(at + 1, line),
            }
        }
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("[paths]".to_owned());
            lines.push(line);
        }
    }
    let mut out = lines.join(newline);
    out.push_str(newline);
    out
}

/// The keyboard's stand-in pad (`gameinput.rs`, as the README lists it).
fn keys_text(p: Paint) -> String {
    let rows = [
        ("W A S D / I J K L", "left / right stick"),
        ("arrows", "D-pad"),
        ("Enter, Backspace, X, Y", "A, B, X, Y"),
        ("Q / E", "left / right shoulder"),
        ("Z / C", "left / right trigger"),
        ("F / H", "left / right stick click"),
        ("Esc, Tab", "Menu, View"),
    ];
    let mut out = format!("\n  {}\n\n", p.bold("Keyboard controls", SUNSET[2]));
    for (keys, pad) in rows {
        out.push_str(&format!(
            "  {:<24}{}\n",
            p.rgb(keys, CREAM),
            p.rgb(pad, DIM)
        ));
    }
    out.push_str(&format!(
        "\n  {}\n",
        p.rgb(
            "With no XInput pad connected, the keyboard is a pad while the game's window has focus.",
            DIM
        )
    ));
    out
}

/// One line of the setup check.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Good,
    Warn,
    Bad,
}

/// What [`check`] found, as facts.
#[derive(Debug, Clone, Default)]
struct Pc {
    /// Windows build number, if read.
    windows_build: Option<u32>,
    avx2: bool,
    /// The GPU device made with shader model 6.6, and if so its mesh
    /// shader support and widest wave.
    sm66: Option<bool>,
    mesh_shaders: Option<bool>,
    max_lanes: Option<u32>,
    /// The Agility runtime folder beside this executable.
    agility_beside: bool,
    /// The default adapter's name and user-mode driver version, as DXGI
    /// gives them (also without shader model 6.6).
    gpu_name: Option<String>,
    driver: Option<String>,
}

/// The setup check's lines: status, what, and what to do.
fn verdicts(pc: &Pc) -> Vec<(Status, String, Option<String>)> {
    let mut out = Vec::new();
    match pc.windows_build {
        Some(b) if b >= 22000 => out.push((Status::Good, format!("Windows 11 (build {b})"), None)),
        Some(b) if pc.sm66 == Some(true) => {
            out.push((Status::Good, format!("Windows 10 (build {b})"), None))
        }
        Some(b) => out.push((
            Status::Warn,
            format!("Windows 10 (build {b})"),
            Some(if pc.agility_beside {
                "Its built-in D3D12 stops at shader model 6.5. A D3D12\\ folder is beside victory.exe; this build must also be made to load it (#175).".to_owned()
            } else {
                "Its built-in D3D12 stops at shader model 6.5. VIctory needs 6.6 from the D3D12 Agility SDK runtime (#84, #175).".to_owned()
            }),
        )),
        None => out.push((Status::Warn, "Windows version unknown".to_owned(), None)),
    }
    out.push(if pc.avx2 {
        (Status::Good, "CPU has AVX2".to_owned(), None)
    } else {
        (
            Status::Bad,
            "CPU has no AVX2".to_owned(),
            Some("The title needs it.".to_owned()),
        )
    });
    if let Some(name) = &pc.gpu_name {
        let what = match &pc.driver {
            Some(d) => format!("GPU: {name} (driver {d})"),
            None => format!("GPU: {name}"),
        };
        out.push((Status::Good, what, None));
    }
    match pc.sm66 {
        Some(true) => out.push((Status::Good, "GPU: shader model 6.6".to_owned(), None)),
        Some(false) => out.push((
            Status::Bad,
            "GPU: no shader model 6.6 device".to_owned(),
            Some("Translated shaders need it. On Windows 10, see above.".to_owned()),
        )),
        None => out.push((Status::Warn, "GPU: not checked".to_owned(), None)),
    }
    if pc.sm66 == Some(true) {
        match pc.mesh_shaders {
            Some(true) => out.push((Status::Good, "GPU: mesh shaders".to_owned(), None)),
            Some(false) => out.push((
                Status::Warn,
                "GPU: no mesh shaders".to_owned(),
                Some("Menus can run; the world's mesh draws are refused (#3).".to_owned()),
            )),
            None => {}
        }
        if let Some(lanes) = pc.max_lanes {
            out.push(if lanes >= 64 {
                (Status::Good, format!("GPU: waves up to {lanes} lanes"), None)
            } else {
                (
                    Status::Warn,
                    format!("GPU: waves of {lanes} lanes"),
                    Some("The title's programs are written for 64; some run in halves, some are refused (#578, #778).".to_owned()),
                )
            });
        }
    }
    out
}

/// The configured paths' lines, as [`verdicts`] gives them.
fn path_verdicts(cfg: &victory_core::Config) -> Vec<(Status, String, Option<String>)> {
    let mut out = Vec::new();
    match &cfg.game_dir {
        Some(d) if d.is_dir() => {
            out.push((Status::Good, format!("Game folder: {}", d.display()), None));
        }
        Some(d) => out.push((
            Status::Warn,
            format!("Game folder: {} (not there)", d.display()),
            Some(
                "Fix game_dir in victory.toml. Running from this menu does not need it.".to_owned(),
            ),
        )),
        None => out.push((
            Status::Warn,
            "Game folder: not set".to_owned(),
            Some(
                "S in the menu sets it (game_dir in victory.toml); 1 asks for the .exe.".to_owned(),
            ),
        )),
    }
    for (what, dir) in [
        ("Data folder", &cfg.data_dir),
        ("Cache folder", &cfg.cache_dir),
    ] {
        match victory_core::ensure_outside_repo(dir) {
            Ok(()) => out.push((Status::Good, format!("{what}: {}", dir.display()), None)),
            Err(e) => out.push((
                Status::Bad,
                format!("{what}: {}", dir.display()),
                Some(e.to_string()),
            )),
        }
    }
    out
}

/// The facts of this PC.
fn check() -> Pc {
    let mut pc = Pc {
        windows_build: windows_build(),
        avx2: std::arch::is_x86_feature_detected!("avx2"),
        agility_beside: std::env::current_exe()
            .ok()
            .and_then(|e| {
                e.parent()
                    .map(|d| d.join("D3D12").join("D3D12Core.dll").is_file())
            })
            .unwrap_or(false),
        ..Pc::default()
    };
    adapter(&mut pc);
    gpu(&mut pc);
    pc
}

/// A driver version as DXGI packs it (four 16-bit parts, high first).
fn driver_version(v: u64) -> String {
    format!(
        "{}.{}.{}.{}",
        v >> 48,
        (v >> 32) & 0xFFFF,
        (v >> 16) & 0xFFFF,
        v & 0xFFFF
    )
}

/// The default adapter's name and driver, from DXGI.
#[cfg(windows)]
fn adapter(pc: &mut Pc) {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIDevice, IDXGIFactory1};
    use windows::core::Interface;
    // SAFETY: plain DXGI queries; the descriptor is a local copy.
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return;
        };
        let Ok(adapter) = factory.EnumAdapters1(0) else {
            return;
        };
        if let Ok(desc) = adapter.GetDesc1() {
            let name = &desc.Description;
            let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            let name = String::from_utf16_lossy(&name[..len]).trim().to_owned();
            if !name.is_empty() {
                pc.gpu_name = Some(name);
            }
        }
        if let Ok(v) = adapter.CheckInterfaceSupport(&IDXGIDevice::IID) {
            pc.driver = Some(driver_version(v as u64));
        }
    }
}

#[cfg(not(windows))]
fn adapter(_: &mut Pc) {}

#[cfg(windows)]
fn windows_build() -> Option<u32> {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (key, name) = (
        wide("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
        wide("CurrentBuildNumber"),
    );
    let mut buf = [0u16; 32];
    let mut len = std::mem::size_of_val(&buf) as u32;
    // SAFETY: NUL-terminated names and a buffer of `len` bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if status != 0 {
        return None;
    }
    String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)])
        .trim()
        .parse()
        .ok()
}

#[cfg(not(windows))]
fn windows_build() -> Option<u32> {
    None
}

/// The default adapter's shader model, mesh shaders and wave size, asked
/// in this process: what a run from this executable would get.
#[cfg(windows)]
fn gpu(pc: &mut Pc) {
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_FEATURE_D3D12_OPTIONS1, D3D12_FEATURE_D3D12_OPTIONS7,
        D3D12_FEATURE_DATA_D3D12_OPTIONS1, D3D12_FEATURE_DATA_D3D12_OPTIONS7,
        D3D12_MESH_SHADER_TIER_NOT_SUPPORTED,
    };
    let Ok(host) = victory_shader::emit::host::Host::new(false) else {
        pc.sm66 = Some(false);
        return;
    };
    pc.sm66 = Some(true);
    let mut o7 = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
    let mut o1 = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    // SAFETY: each query writes its own structure of the given size.
    unsafe {
        if host
            .device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS7,
                (&raw mut o7).cast(),
                size_of_val(&o7) as u32,
            )
            .is_ok()
        {
            pc.mesh_shaders = Some(o7.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED);
        }
        if host
            .device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS1,
                (&raw mut o1).cast(),
                size_of_val(&o1) as u32,
            )
            .is_ok()
        {
            pc.max_lanes = Some(o1.WaveLaneCountMax);
        }
    }
}

#[cfg(not(windows))]
fn gpu(_: &mut Pc) {}

/// A plain heading for the terminal menu.
fn banner(p: Paint) -> String {
    let title = p.gradient("Console menu", &SUNSET);
    let horizon = if p.color {
        p.gradient(&"\u{2500}".repeat(40), &SUNSET)
    } else {
        "-".repeat(40)
    };
    format!(
        "\n  {title}\n  {}\n  {horizon}\n",
        p.rgb("local title runner", DIM)
    )
}

fn menu_text(p: Paint, last: Option<&Path>, game_dir: Option<&Path>, model: Model) -> String {
    let key = |k: &str| p.bold(k, SUNSET[3]);
    let mut out = banner(p);
    out.push('\n');
    out.push_str(&format!("  {}  Run a game\n", key("1")));
    match last {
        Some(path) => out.push_str(&format!(
            "  {}  Run again   {}\n",
            key("2"),
            p.rgb(&path.display().to_string(), DIM)
        )),
        None => out.push_str(&p.rgb("  2  Run again   (nothing run yet)\n", DIM)),
    }
    out.push_str(&format!("  {}  Check this PC\n", key("3")));
    out.push_str(&format!(
        "  {}  Show data folder path   {}\n",
        key("4"),
        p.rgb("saves, crashes.log", DIM)
    ));
    out.push_str(&format!("  {}  Translate shaders & pipelines\n", key("5")));
    let folder = game_dir.map_or_else(|| "(not set)".to_owned(), |d| d.display().to_string());
    out.push('\n');
    out.push_str(&format!(
        "  {}  Set the game folder   {}\n",
        key("S"),
        p.rgb(&folder, DIM)
    ));
    out.push_str(&format!("  {}  Keyboard controls\n", key("K")));
    out.push_str(&format!(
        "  {}  Console: {}   {}\n",
        key("M"),
        model.name(),
        p.rgb(&format!("(M switches to {})", model.other().name()), DIM)
    ));
    out.push_str(&format!("  {}  Quit\n\n", key("Q")));
    out.push_str(&format!("  {} ", p.rgb("Choose:", CREAM)));
    out
}

/// Hints start this far in, under the line they explain.
const HINT_INDENT: usize = 5;

/// `text` in lines of at most `width` characters, broken at spaces (a word
/// longer than that gets a line of its own).
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(20);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let len = line.chars().count();
        if len > 0 && len + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// The console window's width in columns (80 where it cannot be read).
#[cfg(windows)]
fn console_columns() -> usize {
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE,
    };
    // SAFETY: a plain query of our own output handle into a local structure.
    unsafe {
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) == 0 {
            return 80;
        }
        usize::try_from(info.srWindow.Right - info.srWindow.Left + 1).unwrap_or(80)
    }
}

#[cfg(not(windows))]
fn console_columns() -> usize {
    80
}

fn check_text(p: Paint, pc: &Pc, cfg: Option<&victory_core::Config>, columns: usize) -> String {
    let mut out = String::new();
    let mut section = |title: &str, lines: Vec<(Status, String, Option<String>)>| {
        out.push_str(&format!("\n  {}\n\n", p.bold(title, SUNSET[2])));
        for (status, what, hint) in lines {
            let mark = match status {
                Status::Good => p.rgb("\u{2713}", GOOD),
                Status::Warn => p.rgb("!", WARN),
                Status::Bad => p.rgb("\u{2717}", BAD),
            };
            out.push_str(&format!("  {mark}  {what}\n"));
            if let Some(h) = hint {
                // One column spare: a line that fills the window wraps early
                // in some consoles.
                for line in wrap(&h, columns.saturating_sub(HINT_INDENT + 1)) {
                    out.push_str(&format!("{:HINT_INDENT$}{}\n", "", p.rgb(&line, DIM)));
                }
            }
        }
    };
    section("This PC", verdicts(pc));
    if let Some(cfg) = cfg {
        section("Paths", path_verdicts(cfg));
    }
    out
}

fn read_line(input: &mut impl BufRead) -> Option<String> {
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line),
    }
}

fn pause(p: Paint, input: &mut impl BufRead) {
    print!("\n  {} ", p.rgb("Press Enter to go back to the menu.", DIM));
    let _ = std::io::stdout().flush();
    let _ = read_line(input);
}

fn crash_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("crashes.log")
}

/// Prints the last 40 lines without loading the whole file into memory.
fn print_crash_tail(path: &Path) {
    use std::collections::VecDeque;
    use std::io::BufReader;

    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("\n  No diagnostics have been recorded yet.");
            return;
        }
        Err(e) => {
            println!("\n  Could not read {}: {e}", path.display());
            return;
        }
    };
    let mut lines = VecDeque::with_capacity(40);
    for line in BufReader::new(file).lines() {
        match line {
            Ok(line) => {
                if lines.len() == 40 {
                    lines.pop_front();
                }
                lines.push_back(line);
            }
            Err(e) => {
                println!("\n  Could not read {}: {e}", path.display());
                return;
            }
        }
    }
    if lines.is_empty() {
        println!("\n  The diagnostics log is empty.");
    } else {
        println!();
        for line in lines {
            println!("{line}");
        }
    }
}

/// The command that runs `game` the way the menu does: `victory run <game>
/// --execute --console <model>` from this same executable, in this console.
fn run_command(victory: &Path, game: &Path, model: Model) -> std::process::Command {
    let mut c = std::process::Command::new(victory);
    c.arg("run")
        .arg(game)
        .arg("--execute")
        .arg("--console")
        .arg(model.arg());
    c
}

/// The configured-path pretranslation command: `victory translate --cache
/// --pipelines <path>...`, inheriting this console's input and output.
fn translate_command(victory: &Path, paths: &[PathBuf]) -> std::process::Command {
    let mut c = std::process::Command::new(victory);
    c.arg("translate")
        .arg("--cache")
        .arg("--pipelines")
        .args(paths);
    c
}

/// Runs translation for the configured paths, or prompts for one path if
/// none are saved.
fn run_translation(p: Paint, input: &mut impl BufRead) -> Result<()> {
    let mut paths: Vec<PathBuf> = victory_core::Config::load()
        .ok()
        .map(|cfg| cfg.game_dir.into_iter().chain(cfg.test_titles).collect())
        .unwrap_or_default();
    if paths.is_empty() {
        print!(
            "\n  {}\n  {} ",
            p.rgb(
                "Enter a game folder or title executable path (leave blank to cancel).",
                CREAM
            ),
            p.rgb(">", SUNSET[3])
        );
        let _ = std::io::stdout().flush();
        let Some(answer) = read_line(input) else {
            return Ok(());
        };
        if answer.trim().is_empty() {
            return Ok(());
        }
        paths.push(clean_path(&answer));
    }

    println!("\n  Translating shaders and pipelines for the configured paths.\n");
    match std::env::current_exe().and_then(|me| translate_command(&me, &paths).status()) {
        Ok(status) if status.success() => println!("\n  Translation finished."),
        Ok(status) => println!("\n  Translation stopped with {status}."),
        Err(e) => println!("\n  Translation could not start: {e}"),
    }
    pause(p, input);
    Ok(())
}

/// How a run ended, for the line after it: the exit code, and in hex where
/// it reads as an NTSTATUS (a crash such as 0xC0000005).
fn ended(code: Option<i32>) -> (Status, String) {
    match code {
        Some(0) => (Status::Good, "The run ended.".to_owned()),
        Some(c) if c < 0 => (
            Status::Bad,
            format!("The run stopped: exit code {:#010X}.", c as u32),
        ),
        Some(c) => (Status::Bad, format!("The run stopped: exit code {c}.")),
        None => (Status::Bad, "The run was stopped.".to_owned()),
    }
}

/// Runs `f` with Ctrl+C and Ctrl+Break left to the child process sharing
/// the console: the run stops, the menu stays.
#[cfg(windows)]
fn sparing_the_menu<T>(f: impl FnOnce() -> T) -> T {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
    };
    unsafe extern "system" fn handled(ctrl: u32) -> windows_sys::core::BOOL {
        i32::from(ctrl == CTRL_C_EVENT || ctrl == CTRL_BREAK_EVENT)
    }
    // SAFETY: a handler of the expected signature, removed again below.
    unsafe { SetConsoleCtrlHandler(Some(handled), 1) };
    let out = f();
    // SAFETY: as above.
    unsafe { SetConsoleCtrlHandler(Some(handled), 0) };
    out
}

#[cfg(not(windows))]
fn sparing_the_menu<T>(f: impl FnOnce() -> T) -> T {
    f()
}

/// Runs `exe` in a child `victory run <exe> --execute`, then waits for
/// Enter whatever happened to it.
fn run_game(p: Paint, exe: &Path, data_dir: &Path, model: Model, input: &mut impl BufRead) {
    if !exe.is_file() {
        println!(
            "\n  {} {}",
            p.rgb("\u{2717}", BAD),
            p.rgb("No file there.", CREAM)
        );
        pause(p, input);
        return;
    }
    save_last(data_dir, exe);
    println!(
        "\n  {} {}   {}\n",
        p.rgb("\u{25b6}", SUNSET[4]),
        exe.display(),
        p.rgb(model.name(), DIM)
    );
    let status = std::env::current_exe()
        .and_then(|me| sparing_the_menu(|| run_command(&me, exe, model).status()));
    let failed = match &status {
        Ok(status) => !status.success(),
        Err(_) => true,
    };
    let (mark, text) = match status {
        Ok(s) => {
            let (status, text) = ended(s.code());
            let mark = match status {
                Status::Good => p.rgb("\u{2713}", GOOD),
                _ => p.rgb("\u{2717}", BAD),
            };
            (mark, p.rgb(&text, CREAM))
        }
        Err(e) => (
            p.rgb("\u{2717}", BAD),
            p.rgb(&format!("The run could not start: {e}"), CREAM),
        ),
    };
    println!("\n  {mark} {text}");
    if failed {
        let log = crash_log_path(data_dir);
        println!("  Crash log: {}", log.display());
        print!("  Press L to print recent diagnostics, or Enter to continue: ");
        let _ = std::io::stdout().flush();
        if read_line(input).is_some_and(|answer| answer.trim().eq_ignore_ascii_case("l")) {
            print_crash_tail(&log);
        }
    }
    pause(p, input);
}

/// Saves `dropped` (a folder, or a file in it) as the game folder in the
/// config file; what happened, as a line for the menu.
fn set_folder(dropped: &Path) -> (Status, String) {
    let dir = if dropped.is_file() {
        dropped.parent().map(Path::to_path_buf)
    } else {
        Some(dropped.to_path_buf())
    };
    let Some(dir) = dir.filter(|d| d.is_dir()) else {
        return (Status::Bad, "No folder there.".to_owned());
    };
    if let Err(e) = victory_core::ensure_outside_repo(&dir) {
        return (Status::Bad, e.to_string());
    }
    let path = config_path();
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if let Err(e) = std::fs::write(&path, set_game_dir(&text, &dir)) {
        return (
            Status::Bad,
            format!("Could not write {}: {e}", path.display()),
        );
    }
    let mut said = format!("Game folder set in {}.", path.display());
    if std::env::var_os("VICTORY_GAME_DIR").is_some_and(|v| !v.is_empty()) {
        said.push_str(" VICTORY_GAME_DIR is set, and it wins over the file.");
    }
    (Status::Good, said)
}

pub fn run() -> Result<()> {
    let p = Paint {
        color: enable_color(),
    };
    let data_dir = victory_core::Config::load()
        .map(|c| c.data_dir)
        .unwrap_or_else(|_| std::env::temp_dir().join("victory"));
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    loop {
        let last = load_last(&data_dir);
        let model = load_model(&data_dir);
        let game_dir = victory_core::Config::load().ok().and_then(|c| c.game_dir);
        print!(
            "{}",
            menu_text(p, last.as_deref(), game_dir.as_deref(), model)
        );
        let _ = std::io::stdout().flush();
        let Some(line) = read_line(&mut input) else {
            return Ok(());
        };
        match choice(&line) {
            Choice::Run => {
                print!(
                    "\n  {}\n  {} ",
                    p.rgb("Drag the game's .exe onto this window, or type its path, then press Enter.", CREAM),
                    p.rgb(">", SUNSET[3])
                );
                let _ = std::io::stdout().flush();
                let Some(path) = read_line(&mut input) else {
                    return Ok(());
                };
                if !path.trim().is_empty() {
                    run_game(p, &clean_path(&path), &data_dir, model, &mut input);
                }
            }
            Choice::Again => match last {
                Some(exe) => run_game(p, &exe, &data_dir, model, &mut input),
                None => {
                    println!("\n  {}", p.rgb("Nothing run yet: choose 1 first.", DIM));
                    pause(p, &mut input);
                }
            },
            Choice::Check => {
                // Making the GPU device can take a moment.
                print!("\n  {}", p.rgb("Checking this PC...", DIM));
                let _ = std::io::stdout().flush();
                let pc = check();
                println!();
                let cfg = victory_core::Config::load().ok();
                print!("{}", check_text(p, &pc, cfg.as_ref(), console_columns()));
                pause(p, &mut input);
            }
            Choice::Data => {
                let _ = std::fs::create_dir_all(&data_dir);
                println!("\n  Data folder: {}", data_dir.display());
                println!("  Crash log: {}", crash_log_path(&data_dir).display());
            }
            Choice::Translate => run_translation(p, &mut input)?,
            Choice::Folder => {
                print!(
                    "\n  {}\n  {} ",
                    p.rgb(
                        "Drag the game's folder (or its .exe) onto this window, or type its path, then press Enter.",
                        CREAM
                    ),
                    p.rgb(">", SUNSET[3])
                );
                let _ = std::io::stdout().flush();
                let Some(answer) = read_line(&mut input) else {
                    return Ok(());
                };
                if !answer.trim().is_empty() {
                    let said = set_folder(&clean_path(&answer));
                    let (mark, colour) = match said.0 {
                        Status::Good => ("\u{2713}", GOOD),
                        _ => ("\u{2717}", BAD),
                    };
                    println!("\n  {} {}", p.rgb(mark, colour), p.rgb(&said.1, CREAM));
                    pause(p, &mut input);
                }
            }
            Choice::Keys => {
                print!("{}", keys_text(p));
                pause(p, &mut input);
            }
            Choice::Model => save_model(&data_dir, model.other()),
            Choice::Quit => return Ok(()),
            Choice::Unknown if line.trim().is_empty() => {}
            Choice::Unknown => {
                println!(
                    "\n  {}",
                    p.rgb("Type 1 to 5, S, K or M and press Enter (Q to quit).", WARN)
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_data_dir() -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("victory-menu-test-{}-{stamp}", std::process::id()))
    }

    #[test]
    fn console_profiles_persist_as_cli_values() {
        let data_dir = temporary_data_dir();
        for model in [Model::SeriesX, Model::SeriesS] {
            save_model(&data_dir, model);
            assert_eq!(
                std::fs::read_to_string(model_file(&data_dir)).unwrap(),
                model.arg()
            );
            assert_eq!(load_model(&data_dir), model);
        }
        std::fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn unknown_console_profile_defaults_to_series_x() {
        let data_dir = temporary_data_dir();
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(model_file(&data_dir), "unknown").unwrap();
        assert_eq!(load_model(&data_dir), Model::SeriesX);
        std::fs::remove_dir_all(data_dir).unwrap();
    }

    #[test]
    fn run_command_passes_executable_and_console_profile() {
        let command = run_command(
            Path::new("victory.exe"),
            Path::new("title.exe"),
            Model::SeriesS,
        );
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["run", "title.exe", "--execute", "--console", "series-s"]
        );
    }

    #[test]
    fn translation_command_uses_cache_pipelines_and_saved_paths() {
        let paths = [PathBuf::from("game folder"), PathBuf::from("title.exe")];
        let command = translate_command(Path::new("victory.exe"), &paths);
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "translate",
                "--cache",
                "--pipelines",
                "game folder",
                "title.exe"
            ]
        );
    }
}
