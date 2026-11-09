//! Finds what Steam and the new Epic launcher download, update and run: Steam's registry key
//! and manifests, Epic's journal and catalog cache, how far its downloads are (its Install API
//! on the loopback, else its log), and the processes running from Epic's game folders. One
//! thread, woken by Steam's key and Epic's folder changing, and every 2 s while something
//! downloads or runs (10 s otherwise, for Epic's processes). Reads only.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{FindFirstChangeNotificationW, FindNextChangeNotification, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegNotifyChangeKeyValue, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_NOTIFY, KEY_READ,
    REG_NOTIFY_CHANGE_LAST_SET, REG_NOTIFY_CHANGE_NAME, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSEnumerateProcessesW, WTSFreeMemory, WTS_PROCESS_INFOW};
use windows::Win32::System::Threading::{
    CreateEventW, GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, WaitForMultipleObjects, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};

use super::games::{
    epic_titles, epic_updating, game_key, log_state, ops_in, parse_acf, parse_installations, parse_operations, read_epic_log, steam_libraries, EpicInstall,
    EpicLogDownload, EpicOp, EpicTitle, Game, GameEvent, GameNow, GameState, GameTracker, Rate, SteamApp, Store,
};
use crate::platform::log::log;

/// What the games are doing now, and what happened that the UI hasn't taken yet.
#[derive(Default)]
pub struct GamesBox {
    pub now: Vec<GameNow>,
    pub events: Vec<GameEvent>,
}

pub type GamesMailbox = Arc<Mutex<GamesBox>>;

const STEAM: PCWSTR = w!(r"Software\Valve\Steam");

/// Looks this often while something downloads or runs (Steam writes its manifest's bytes every
/// few seconds), else for Epic's processes, else (no Epic) only when woken.
const BUSY_MS: u32 = 2_000;
const IDLE_MS: u32 = 10_000;
const QUIET_MS: u32 = 60_000;

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// A DWORD value under HKCU\`sub`.
fn reg_dword(sub: &str, name: &str) -> Option<u32> {
    let (sub, name) = (wide(sub), wide(name));
    let (mut v, mut size) = (0u32, 4u32);
    let ok = unsafe { RegGetValueW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), PCWSTR(name.as_ptr()), RRF_RT_REG_DWORD, None, Some(&mut v as *mut u32 as *mut _), Some(&mut size)) };
    (ok == ERROR_SUCCESS).then_some(v)
}

/// A string value under HKCU\`sub`.
fn reg_string(sub: &str, name: &str) -> Option<String> {
    let (sub, name) = (wide(sub), wide(name));
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let ok = unsafe { RegGetValueW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), PCWSTR(name.as_ptr()), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut size)) };
    (ok == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..(size as usize / 2).saturating_sub(1)]))
}

/// The subkeys of HKCU\`sub`.
fn reg_subkeys(sub: &str) -> Vec<String> {
    let mut out = Vec::new();
    let sub = wide(sub);
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()), None, KEY_READ, &mut key) != ERROR_SUCCESS {
            return out;
        }
        let mut buf = [0u16; 256];
        for i in 0.. {
            let mut len = buf.len() as u32;
            if RegEnumKeyExW(key, i, Some(PWSTR(buf.as_mut_ptr())), &mut len, None, None, None, None) != ERROR_SUCCESS {
                break;
            }
            out.push(String::from_utf16_lossy(&buf[..len as usize]));
        }
        let _ = RegCloseKey(key);
    }
    out
}

/// Steam's folder (`SteamPath`, written with forward slashes).
fn steam_root() -> Option<PathBuf> {
    reg_string(r"Software\Valve\Steam", "SteamPath").map(|p| PathBuf::from(p.replace('/', "\\"))).filter(|p| p.exists())
}

/// The program running as `pid`.
fn image_of(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Steam is running (`ActiveProcess\pid` is its own process): a Steam that crashed leaves
/// `Updating` and `RunningAppID` behind, which would show forever.
fn steam_alive() -> bool {
    reg_dword(r"Software\Valve\Steam\ActiveProcess", "pid")
        .filter(|pid| *pid != 0)
        .and_then(image_of)
        .is_some_and(|path| path.to_ascii_lowercase().ends_with("\\steam.exe"))
}

/// A game's manifest in any of the libraries.
fn steam_manifest(libraries: &[PathBuf], id: u32) -> Option<SteamApp> {
    libraries.iter().find_map(|l| std::fs::read_to_string(l.join("steamapps").join(format!("appmanifest_{id}.acf"))).ok()).and_then(|t| parse_acf(&t))
}

/// A Steam game's pictures from the library cache: the tall capsule and the 32 px icon (the
/// cache's own folder per game since 2024, its flat files before).
fn steam_art(root: &Path, id: u32) -> (Option<PathBuf>, Option<PathBuf>) {
    let cache = root.join("appcache").join("librarycache");
    let dir = cache.join(id.to_string());
    let mut cover = None;
    let mut icon = None;
    // In the game's folder itself, or in one of its hash-named subfolders.
    for name in ["library_capsule.jpg", "library_600x900.jpg"] {
        if cover.is_none() && dir.join(name).exists() {
            cover = Some(dir.join(name));
        }
    }
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                for name in ["library_capsule.jpg", "library_600x900.jpg"] {
                    if cover.is_none() && path.join(name).exists() {
                        cover = Some(path.join(name));
                    }
                }
            } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("jpg")) && path.file_stem().is_some_and(|s| s.len() == 40) {
                // The icon's file is named by its hash.
                icon = Some(path);
            }
        }
    }
    let flat = |name: &str| Some(cache.join(format!("{id}_{name}"))).filter(|p| p.exists());
    (cover.or_else(|| flat("library_600x900.jpg")), icon.or_else(|| flat("icon.jpg")))
}

fn steam_game(root: &Path, id: u32, name: &str) -> Game {
    let (cover, icon) = steam_art(root, id);
    let title = if name.is_empty() { "Steam oyunu".to_string() } else { name.to_string() };
    Game { key: game_key(Store::Steam, &id.to_string()), store: Store::Steam, title, cover, icon }
}

/// The new Epic launcher's own folder.
fn epic_root() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("com.epicgames.eglv2");
    p.exists().then_some(p)
}

/// The body of a raw HTTP answer when it's a 200, chunked or not.
fn ok_body(raw: &[u8]) -> Option<String> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let mut body = &raw[split + 4..];
    if head.lines().next()?.split(' ').nth(1)? != "200" {
        return None;
    }
    if !head.lines().any(|l| l.starts_with("transfer-encoding:") && l.contains("chunked")) {
        return String::from_utf8(body.to_vec()).ok();
    }
    let mut out = Vec::new();
    loop {
        let line = body.windows(2).position(|w| w == b"\r\n")?;
        let size = std::str::from_utf8(&body[..line]).ok()?.split(';').next()?.trim();
        let n = usize::from_str_radix(size, 16).ok()?;
        if n == 0 {
            return String::from_utf8(out).ok();
        }
        out.extend_from_slice(body.get(line + 2..line + 2 + n)?);
        body = body.get(line + 4 + n..)?;
    }
}

/// Asks Epic's Install API for its operations: its port and machine secret are in `api.json`
/// (written for games to find it), and it answers on the loopback only. `None` without an
/// answer: no API, a launcher still starting, a secret turned down.
fn epic_operations(root: &Path) -> Option<Vec<EpicOp>> {
    let api: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(root.join("api.json")).ok()?).ok()?;
    let port = u16::try_from(api["port"].as_u64()?).ok()?;
    let secret = api["machineSecret"].as_str()?;
    let mut s = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(500)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
    // HTTP/1.0: the answer ends with the connection.
    let ask = format!("GET /api/v1/installation/operations HTTP/1.0\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\nx-epic-machine-secret: {secret}\r\n\r\n");
    s.write_all(ask.as_bytes()).ok()?;
    let mut raw = Vec::new();
    s.take(1 << 20).read_to_end(&mut raw).ok()?;
    parse_operations(&ok_body(&raw)?)
}

/// Epic's newest log, read on from where the last look stopped: the downloads it told of.
#[derive(Default)]
struct EpicLog {
    path: Option<PathBuf>,
    read: u64,
    downloads: HashMap<String, EpicLogDownload>,
}

impl EpicLog {
    fn downloads(&mut self, root: &Path) -> &HashMap<String, EpicLogDownload> {
        let newest = std::fs::read_dir(root.join("logs"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("log")))
            .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
            .map(|e| e.path());
        let len = newest.as_ref().and_then(|p| std::fs::metadata(p).ok()).map_or(0, |m| m.len());
        // Each launcher run writes a log of its own.
        if newest != self.path || len < self.read {
            *self = EpicLog { path: newest, ..Default::default() };
        }
        let mut buf = Vec::new();
        let read = self.path.as_ref().and_then(|p| std::fs::File::open(p).ok()).and_then(|mut f| {
            f.seek(SeekFrom::Start(self.read)).ok()?;
            f.read_to_end(&mut buf).ok()
        });
        if read.is_some() {
            // Whole lines only: the last may be half written.
            let end = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            read_epic_log(&String::from_utf8_lossy(&buf[..end]), &mut self.downloads);
            self.read += end as u64;
        }
        &self.downloads
    }
}

/// The processes in this session: file name and full path.
fn processes() -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        let mut session = 0u32;
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
        let mut info: *mut WTS_PROCESS_INFOW = std::ptr::null_mut();
        let mut count = 0u32;
        if WTSEnumerateProcessesW(None, 0, 1, &mut info, &mut count).is_err() || info.is_null() {
            return out;
        }
        for p in std::slice::from_raw_parts(info, count as usize) {
            if p.SessionId != session || p.ProcessId == 0 {
                continue;
            }
            let name = p.pProcessName.to_string().unwrap_or_default();
            let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, p.ProcessId) else { continue };
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            if QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok() {
                out.push((name, String::from_utf16_lossy(&buf[..len as usize])));
            }
            let _ = CloseHandle(h);
        }
        WTSFreeMemory(info as *mut _);
    }
    out
}

/// `path` is inside `dir` (case aside).
fn under(path: &str, dir: &Path) -> bool {
    let dir = dir.to_string_lossy().trim_end_matches('\\').to_lowercase();
    !dir.is_empty() && path.to_lowercase().starts_with(&format!("{dir}\\"))
}

/// The thread's memory between looks.
struct Scanner {
    tracker: GameTracker,
    rates: HashMap<u32, Rate>,
    /// Each key's store and id, for asking whether its download finished.
    ids: HashMap<u32, (Store, String)>,
    /// Epic's catalog, read again when its file changes.
    catalog: Option<(SystemTime, HashMap<String, EpicTitle>)>,
    epic_log: EpicLog,
    /// Where Epic's figures came from last (the API or the log), to say when it changes.
    epic_api: Option<bool>,
    started: Instant,
}

impl Scanner {
    fn new() -> Self {
        Scanner {
            tracker: GameTracker::default(),
            rates: HashMap::new(),
            ids: HashMap::new(),
            catalog: None,
            epic_log: EpicLog::default(),
            epic_api: None,
            started: Instant::now(),
        }
    }

    fn epic_titles(&mut self, root: &Path) -> HashMap<String, EpicTitle> {
        let path = root.join("cache").join("prod").join("catalog-cache.json");
        let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        match (&self.catalog, stamp) {
            (Some((at, titles)), Some(s)) if *at == s => titles.clone(),
            (_, Some(s)) => {
                let titles = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).map(|v| epic_titles(&v)).unwrap_or_default();
                self.catalog = Some((s, titles.clone()));
                titles
            }
            _ => HashMap::new(),
        }
    }

    /// The games downloading or running now, and the keys whose downloads finished done.
    fn look(&mut self) -> (Vec<GameNow>, Vec<u32>) {
        let s = crate::settings::current();
        let now_utc = unix_now();
        let now_ms = self.started.elapsed().as_millis() as u64;
        let mut now: Vec<GameNow> = Vec::new();
        let mut done: Vec<u32> = Vec::new();
        let was = self.tracker.downloading();
        let steam = steam_root().filter(|_| s.games_steam && steam_alive());
        if let Some(root) = &steam {
            let libraries = std::fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")).map(|t| steam_libraries(&t)).unwrap_or_default();
            for id in reg_subkeys(r"Software\Valve\Steam\Apps").iter().filter_map(|k| k.parse::<u32>().ok()) {
                let sub = format!(r"Software\Valve\Steam\Apps\{id}");
                let key = game_key(Store::Steam, &id.to_string());
                let updating = reg_dword(&sub, "Updating") == Some(1);
                // A download shown before is read on when Steam stops calling it updating: paused, or done.
                let app = if updating || was.contains(&key) { steam_manifest(&libraries, id) } else { None };
                let paused = app.as_ref().is_some_and(|a| a.paused());
                if updating || (paused && was.contains(&key)) {
                    let name = app.as_ref().map(|a| a.name.clone()).filter(|n| !n.is_empty()).or_else(|| reg_string(&sub, "Name")).unwrap_or_default();
                    let (bytes, total) = app.as_ref().map_or((0, 0), |a| (a.bytes_downloaded, a.bytes_to_download));
                    let rate = self.rates.entry(key).or_default();
                    let speed = if paused {
                        // A resumed download's speed starts afresh.
                        *rate = Rate::default();
                        None
                    } else {
                        rate.push(now_ms, bytes)
                    };
                    self.ids.insert(key, (Store::Steam, id.to_string()));
                    now.push(GameNow { game: steam_game(root, id, &name), state: GameState::Downloading { done: bytes, total, speed, paused }, since_utc: now_utc });
                } else if was.contains(&key) && app.is_some_and(|a| a.up_to_date()) {
                    done.push(key);
                }
            }
            let running = reg_dword(r"Software\Valve\Steam", "RunningAppID").unwrap_or(0);
            if running != 0 {
                let sub = format!(r"Software\Valve\Steam\Apps\{running}");
                let name = steam_manifest(&libraries, running).map(|a| a.name).filter(|n| !n.is_empty()).or_else(|| reg_string(&sub, "Name")).unwrap_or_default();
                now.push(GameNow { game: steam_game(root, running, &name), state: GameState::Playing, since_utc: now_utc });
            }
        }
        if let Some(root) = epic_root().filter(|_| s.games_epic) {
            let installs = std::fs::read_to_string(root.join("installations.jsonl")).map(|t| parse_installations(&t)).unwrap_or_default();
            let titles = self.epic_titles(&root);
            // The process scan only finds something with a game installed.
            let procs = if installs.iter().any(|i| !i.exe.is_empty()) { processes() } else { Vec::new() };
            let launcher = procs.iter().any(|(name, _)| name.eq_ignore_ascii_case("EpicGamesLauncher.exe"));
            // How far the downloads are: the Install API, else what the log told.
            let busy = launcher && installs.iter().any(|i| epic_updating(i, now_utc));
            let ops = if busy { epic_operations(&root) } else { None };
            if busy && self.epic_api != Some(ops.is_some()) {
                log(if ops.is_some() { "games: Epic's figures from its Install API" } else { "games: Epic's Install API didn't answer; reading its log" });
                self.epic_api = Some(ops.is_some());
            }
            let told = if busy && ops.is_none() { Some(self.epic_log.downloads(&root).clone()) } else { None };
            // A game is the installation with a program; its add-ons share its folder and update with it.
            let mut seen: Vec<&Path> = Vec::new();
            for game in installs.iter().filter(|i| !i.exe.is_empty()) {
                if seen.contains(&game.dir.as_path()) {
                    continue;
                }
                seen.push(&game.dir);
                let parts: Vec<&EpicInstall> = installs.iter().filter(|i| i.dir == game.dir).collect();
                let key = game_key(Store::Epic, &game.item_id);
                let t = titles.get(&game.item_id);
                let title = t.map(|t| t.title.clone()).or_else(|| game.dir.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_default();
                let cover = t.and_then(|t| t.tall.as_ref()).map(|md5| root.join("images").join(format!("{md5}.jpg"))).filter(|p| p.exists());
                let g = Game { key, store: Store::Epic, title, cover, icon: None };
                let updating = parts.iter().any(|i| epic_updating(i, now_utc));
                if launcher && updating {
                    let state = match (&ops, &told) {
                        // Not among its operations: queued, or cancelled with the journal still pending.
                        (Some(ops), _) => ops_in(ops, &game.dir).map(|op| {
                            let rate = self.rates.entry(key).or_default();
                            let speed = if op.paused {
                                // A resumed download's speed starts afresh.
                                *rate = Rate::default();
                                None
                            } else {
                                op.bps.or_else(|| rate.push(now_ms, op.done))
                            };
                            GameState::Downloading { done: op.done, total: op.total, speed, paused: op.paused }
                        }),
                        (None, told) => {
                            let said: Vec<&EpicLogDownload> = parts.iter().filter(|i| epic_updating(i, now_utc)).filter_map(|i| told.as_ref()?.get(&i.artifact)).collect();
                            log_state(said.iter().find(|d| !d.ended).or(said.first()).copied())
                        }
                    };
                    if let Some(state) = state {
                        self.ids.insert(key, (Store::Epic, game.item_id.clone()));
                        now.push(GameNow { game: g.clone(), state, since_utc: now_utc });
                    }
                } else if was.contains(&key) && game.complete && !updating {
                    // The game itself in; an add-on never installed stays incomplete for good.
                    done.push(key);
                }
                if procs.iter().any(|(_, path)| under(path, &game.dir)) {
                    now.push(GameNow { game: g, state: GameState::Playing, since_utc: now_utc });
                }
            }
        }
        // Speeds of downloads that ended are forgotten.
        self.rates.retain(|k, _| now.iter().any(|n| n.game.key == *k));
        (now, done)
    }
}

/// "3 oyun · THE FINALS, Feign, Fortnite…": the settings window's line for a store.
fn summary(names: &[String]) -> String {
    match names.len() {
        0 => crate::i18n::tr("kurulu oyun yok").to_string(),
        n => {
            let shown = names.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
            crate::i18n::trf("{} oyun · {}{}", &[&n, &shown, &if n > 3 { "…" } else { "" }])
        }
    }
}

/// What this PC has, for the settings window: Steam's and Epic's installed games (`None`
/// without the store).
pub fn libraries() -> (Option<String>, Option<String>) {
    let steam = steam_root().map(|root| {
        let libraries = std::fs::read_to_string(root.join("steamapps").join("libraryfolders.vdf")).map(|t| steam_libraries(&t)).unwrap_or_default();
        let mut names: Vec<String> = Vec::new();
        for lib in &libraries {
            let Ok(entries) = std::fs::read_dir(lib.join("steamapps")) else { continue };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                // Steam's own redistributables (228980) aren't a game.
                if name.starts_with("appmanifest_") && name.ends_with(".acf") && name != "appmanifest_228980.acf" {
                    if let Some(app) = std::fs::read_to_string(e.path()).ok().and_then(|t| parse_acf(&t)) {
                        names.push(app.name);
                    }
                }
            }
        }
        summary(&names)
    });
    let epic = epic_root().map(|root| {
        let catalog: Option<serde_json::Value> = std::fs::read_to_string(root.join("cache").join("prod").join("catalog-cache.json")).ok().and_then(|t| serde_json::from_str(&t).ok());
        let titles = catalog.map(|v| epic_titles(&v)).unwrap_or_default();
        let installs = std::fs::read_to_string(root.join("installations.jsonl")).map(|t| parse_installations(&t)).unwrap_or_default();
        let mut names: Vec<String> = Vec::new();
        // An installation the old launcher left behind (an empty folder) isn't a game here.
        for i in installs.iter().filter(|i| !i.exe.is_empty() && i.dir.join(&i.exe).exists()) {
            let name = titles.get(&i.item_id).map(|t| t.title.clone()).unwrap_or_else(|| i.dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
            if !names.contains(&name) {
                names.push(name);
            }
        }
        summary(&names)
    });
    (steam, epic)
}

/// An event that fires when Steam's key changes (armed again after each).
struct SteamWatch {
    key: HKEY,
    event: HANDLE,
}

impl SteamWatch {
    fn new() -> Option<Self> {
        unsafe {
            let mut key = HKEY::default();
            if RegOpenKeyExW(HKEY_CURRENT_USER, STEAM, None, KEY_NOTIFY | KEY_READ, &mut key) != ERROR_SUCCESS {
                return None;
            }
            let event = CreateEventW(None, false, false, None).ok()?;
            let watch = SteamWatch { key, event };
            watch.arm();
            Some(watch)
        }
    }

    fn arm(&self) {
        unsafe {
            let _ = RegNotifyChangeKeyValue(self.key, true, REG_NOTIFY_CHANGE_LAST_SET | REG_NOTIFY_CHANGE_NAME, Some(self.event), true);
        }
    }
}

impl Drop for SteamWatch {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.key);
            let _ = CloseHandle(self.event);
        }
    }
}

/// Starts the thread; `post` runs on it after each change (the mailbox then has it).
pub fn start(mailbox: GamesMailbox, post: fn()) {
    let spawned = std::thread::Builder::new().name("games".into()).spawn(move || {
        let mut scanner = Scanner::new();
        let steam = SteamWatch::new();
        // Epic's journal and play files are in its folder's top level.
        let epic = epic_root().and_then(|r| {
            let path = wide(&r.to_string_lossy());
            unsafe { FindFirstChangeNotificationW(PCWSTR(path.as_ptr()), false, FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_FILE_NAME) }.ok()
        });
        let mut posted: Option<Vec<GameNow>> = None;
        loop {
            // Both stores off (or Pebbly off): no scanning at all, only a slow look at the settings.
            let s = crate::settings::current();
            if !s.enabled || !(s.games_steam || s.games_epic) {
                std::thread::sleep(std::time::Duration::from_millis(QUIET_MS as u64));
                continue;
            }
            let (now, done) = scanner.look();
            let (now, events) = scanner.tracker.update(now, &done, unix_now());
            for e in &events {
                log(&format!("games: {e:?}"));
            }
            if posted.as_ref() != Some(&now) || !events.is_empty() {
                if posted.as_ref().is_none_or(|p| p.len() != now.len()) {
                    let names: Vec<String> = now.iter().map(|n| format!("{} {:?}", n.game.title, n.state)).collect();
                    log(&format!("games: {names:?}"));
                }
                if let Ok(mut b) = mailbox.lock() {
                    b.now = now.clone();
                    b.events.extend(events);
                }
                post();
                posted = Some(now.clone());
            }
            let wait = if !now.is_empty() { BUSY_MS } else if epic.is_some() { IDLE_MS } else { QUIET_MS };
            let handles: Vec<HANDLE> = steam.iter().map(|s| s.event).chain(epic).collect();
            let woke = if handles.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(wait as u64));
                None
            } else {
                Some(unsafe { WaitForMultipleObjects(&handles, false, wait) })
            };
            if woke == Some(WAIT_FAILED) {
                std::thread::sleep(std::time::Duration::from_millis(wait as u64));
            }
            // Re-arm only what fired: every RegNotifyChangeKeyValue call adds a wait of its own.
            let fired = woke.map(|w| w.0.wrapping_sub(WAIT_OBJECT_0.0) as usize);
            if let (Some(s), Some(0)) = (&steam, fired) {
                s.arm();
            }
            if let (Some(h), Some(i)) = (epic, fired) {
                if i == usize::from(steam.is_some()) {
                    unsafe {
                        let _ = FindNextChangeNotification(h);
                    }
                }
            }
            // A burst of writes (a running Steam changes its key's values often) is one look a second.
            std::thread::sleep(std::time::Duration::from_millis(1_000));
        }
    });
    if let Err(e) = spawned {
        log(&format!("games: no thread ({e})"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_line_names_the_first_three() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(summary(&[]), "kurulu oyun yok");
        assert_eq!(summary(&names(&["Fortnite", "Rocket League"])), "2 oyun · Fortnite, Rocket League");
        assert_eq!(summary(&names(&["A", "B", "C", "D"])), "4 oyun · A, B, C…");
    }

    #[test]
    fn a_process_is_under_a_folder_only_inside_it() {
        let dir = Path::new(r"C:\Program Files\Epic Games\Fortnite");
        assert!(under(r"C:\Program Files\Epic Games\Fortnite\FortniteGame\Binaries\Win64\FortniteClient-Win64-Shipping.exe", dir));
        assert!(under(r"c:\program files\epic games\FORTNITE\x.exe", dir), "case aside");
        assert!(!under(r"C:\Program Files\Epic Games\FortniteBeta\x.exe", dir), "a folder that starts the same");
        assert!(!under(r"C:\x.exe", Path::new("")));
    }

    #[test]
    fn an_answer_is_its_body_when_it_is_a_200() {
        assert_eq!(ok_body(b"HTTP/1.0 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}").as_deref(), Some("{}"));
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3;x=1\r\n:1}\r\n0\r\n\r\n";
        assert_eq!(ok_body(chunked).as_deref(), Some("{\"a\":1}"));
        assert_eq!(ok_body(b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\n\r\n"), None, "a secret turned down");
        assert_eq!(ok_body(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\n{}"), None, "cut short");
        assert_eq!(ok_body(b"garbage"), None);
    }

    /// Asks this PC's Epic Install API once: `cargo t --lib live_epic_api -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_epic_api() {
        let root = epic_root().expect("Epic's folder");
        println!("operations: {:?}", epic_operations(&root));
        println!("log: {:?}", EpicLog::default().downloads(&root));
    }

    /// Reads this PC's games once: `cargo t --lib live_games -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_games() {
        let mut s = Scanner::new();
        println!("steam: {:?}", steam_root());
        if let Some(root) = steam_root() {
            for id in reg_subkeys(r"Software\Valve\Steam\Apps").iter().filter_map(|k| k.parse::<u32>().ok()).take(5) {
                println!("  {id}: art {:?}", steam_art(&root, id));
            }
        }
        println!("epic: {:?}", epic_root());
        if let Some(root) = epic_root() {
            let titles = s.epic_titles(&root);
            for i in std::fs::read_to_string(root.join("installations.jsonl")).map(|t| parse_installations(&t)).unwrap_or_default() {
                println!("  {} {:?} complete {} exe {:?} → {:?}", i.item_id, i.dir, i.complete, i.exe, titles.get(&i.item_id));
            }
        }
        println!("now: {:?}", s.look());
    }
}
