//! The PC's health (CPU, memory, C:) and the user's dev servers (localhost ports, Docker
//! containers), watched on one thread. Trouble becomes a live item that waits (it pops, with the
//! alert sound) until it's over; then it says so for a moment. The thread only runs while one of
//! the two is on in the settings.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::activities::live::{LiveMsg, LiveState};
use crate::i18n::{tr, trf};
use crate::platform::log::log;
use crate::settings;

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Starts the thread when health or dev watching was turned on (it ends by itself once both are off).
pub fn follow_settings() {
    let s = settings::current();
    if (s.enabled && (s.health_on || s.dev_on)) && !RUNNING.swap(true, Ordering::SeqCst) {
        if std::thread::Builder::new().name("watch".into()).spawn(run).is_err() {
            RUNNING.store(false, Ordering::SeqCst);
        }
    }
}

fn send(m: LiveMsg) {
    crate::platform::ipc::queue(crate::platform::ipc::Command::Live(Box::new(m)));
    crate::platform::windows::post_watch();
}

fn msg(id: &str, title: String, text: String, state: LiveState) -> LiveMsg {
    LiveMsg { id: format!("watch:{id}"), title, text, progress: None, state, claude: None, app: None }
}

fn run() {
    log("watch: started");
    let mut health = Health::default();
    let mut dev = Dev::default();
    let (mut next_health, mut next_dev) = (Instant::now(), Instant::now());
    loop {
        let s = settings::current();
        if !s.enabled || (!s.health_on && !s.dev_on) {
            break;
        }
        let now = Instant::now();
        if s.health_on && now >= next_health {
            health.poll(&s);
            next_health = now + Duration::from_secs(s.health_every_s as u64);
        } else if !s.health_on {
            health.clear_all();
        }
        if s.dev_on && now >= next_dev {
            dev.poll(&s);
            next_dev = now + Duration::from_secs(s.dev_every_s as u64);
        } else if !s.dev_on {
            dev.clear_all();
        }
        // Wakes often enough to follow the settings, sleeps the rest.
        std::thread::sleep(Duration::from_secs(1));
    }
    health.clear_all();
    dev.clear_all();
    RUNNING.store(false, Ordering::SeqCst);
    log("watch: stopped");
}

// ------------------------------------------------------------------ health

/// One limit, with hysteresis: over it for `for_s` seconds raises, back under `clear` lowers.
#[derive(Default, Debug)]
pub struct Limit {
    over_since: Option<u64>,
    pub raised: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    Raised,
    Lowered,
    Same,
}

impl Limit {
    /// `bad`: past the limit now; `good`: back well under it (the gap stops flicker).
    pub fn step(&mut self, bad: bool, good: bool, now_s: u64, for_s: u64) -> Change {
        if bad {
            let since = *self.over_since.get_or_insert(now_s);
            if !self.raised && now_s.saturating_sub(since) >= for_s {
                self.raised = true;
                return Change::Raised;
            }
        } else {
            self.over_since = None;
        }
        if self.raised && good {
            self.raised = false;
            return Change::Lowered;
        }
        Change::Same
    }
}

#[derive(Default)]
struct Health {
    cpu: Limit,
    ram: Limit,
    disk: Limit,
    last_times: Option<(u64, u64)>,
    started: Option<Instant>,
}

fn filetime(f: windows::Win32::Foundation::FILETIME) -> u64 {
    (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64
}

impl Health {
    /// CPU % since the last poll (None on the first).
    fn cpu_pct(&mut self) -> Option<u8> {
        use windows::Win32::Foundation::FILETIME;
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe { windows::Win32::System::Threading::GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()? };
        // Kernel time includes idle time.
        let (idle, total) = (filetime(idle), filetime(kernel) + filetime(user));
        let last = self.last_times.replace((idle, total));
        let (li, lt) = last?;
        let (di, dt) = (idle.saturating_sub(li), total.saturating_sub(lt));
        (dt > 0).then(|| (100 - (di * 100 / dt).min(100)) as u8)
    }

    fn ram_pct() -> Option<u8> {
        use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m = MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
        unsafe { GlobalMemoryStatusEx(&mut m).ok()? };
        Some(m.dwMemoryLoad as u8)
    }

    fn disk_free_gb() -> Option<u64> {
        let mut free = 0u64;
        unsafe { windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(windows::core::w!("C:\\"), Some(&mut free), None, None).ok()? };
        Some(free / (1 << 30))
    }

    fn poll(&mut self, s: &settings::Settings) {
        let now_s = self.started.get_or_insert_with(Instant::now).elapsed().as_secs();
        let for_s = s.health_for_s as u64;
        if let Some(cpu) = self.cpu_pct() {
            let limit = s.health_cpu_pct;
            let change = self.cpu.step(cpu >= limit, cpu + 15 < limit, now_s, for_s);
            report("cpu", &self.cpu, change, trf("İşlemci %{}", &[&cpu]), tr("Yüksek"), tr("Normale döndü"));
        }
        if let Some(ram) = Self::ram_pct() {
            let limit = s.health_ram_pct;
            let change = self.ram.step(ram >= limit, ram + 10 < limit, now_s, for_s);
            report("ram", &self.ram, change, trf("Bellek %{}", &[&ram]), tr("Dolu"), tr("Normale döndü"));
        }
        if let Some(free) = Self::disk_free_gb() {
            let limit = s.health_disk_gb as u64;
            // A full disk doesn't come and go: no wait before telling.
            let change = self.disk.step(free < limit, free >= limit + 2, now_s, 0);
            report("disk", &self.disk, change, trf("C: {} GB boş", &[&free]), tr("Az yer"), tr("Yer açıldı"));
        }
    }

    fn clear_all(&mut self) {
        for (id, limit) in [("cpu", &mut self.cpu), ("ram", &mut self.ram), ("disk", &mut self.disk)] {
            if std::mem::take(limit).raised {
                send(msg(id, String::new(), String::new(), LiveState::Clear));
            }
        }
        self.last_times = None;
    }
}

/// Raised: waits (pops, sounds); still raised: its numbers move quietly; lowered: a moment.
fn report(id: &str, limit: &Limit, change: Change, title: String, bad: &str, good: &str) {
    match change {
        Change::Raised => send(msg(id, title, bad.into(), LiveState::Wait)),
        Change::Lowered => send(msg(id, title, good.into(), LiveState::Done)),
        Change::Same if limit.raised => send(msg(id, title, bad.into(), LiveState::Wait)),
        Change::Same => {}
    }
}

// ------------------------------------------------------------------ dev servers

/// "api:3000, web:5173, 8080" → [("api", 3000), ("web", 5173), ("8080", 8080)]; junk is skipped.
pub fn parse_ports(s: &str) -> Vec<(String, u16)> {
    s.split([',', ';', ' '])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter_map(|p| match p.rsplit_once(':') {
            Some((name, port)) => port.trim().parse().ok().map(|n| (name.trim().to_string(), n)),
            None => p.parse().ok().map(|n| (p.to_string(), n)),
        })
        .filter(|(_, n)| *n > 0)
        .take(20)
        .collect()
}

/// `docker ps -a --format {{.Names}}|{{.State}}` → name → running.
pub fn parse_docker(out: &str, filter: &str) -> BTreeMap<String, bool> {
    let filter = filter.to_lowercase();
    out.lines()
        .filter_map(|l| l.trim().split_once('|'))
        .filter(|(name, _)| filter.is_empty() || name.to_lowercase().contains(&filter))
        .map(|(name, state)| (name.to_string(), state.trim() == "running"))
        .take(40)
        .collect()
}

/// Up or down, by name; what changed since the last look (the first look only learns).
#[derive(Default, Debug)]
pub struct Watched {
    up: BTreeMap<String, bool>,
}

impl Watched {
    /// (name, now up) for each that went down or came back; ones gone from the list are dropped.
    pub fn update(&mut self, now: BTreeMap<String, bool>) -> Vec<(String, bool)> {
        let changes = now.iter().filter(|(k, up)| self.up.get(*k).is_some_and(|was| was != *up)).map(|(k, up)| (k.clone(), *up)).collect();
        self.up = now;
        changes
    }

    fn down(&self) -> impl Iterator<Item = &String> {
        self.up.iter().filter(|(_, up)| !**up).map(|(k, _)| k)
    }
}

#[derive(Default)]
struct Dev {
    ports: Watched,
    docker: Watched,
    /// Docker itself (its daemon) answered last time: None before the first look.
    docker_ok: Option<bool>,
}

fn port_open(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(300)).is_ok()
}

fn docker_ps() -> Option<String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("docker")
        .args(["ps", "-a", "--format", "{{.Names}}|{{.State}}"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

impl Dev {
    fn poll(&mut self, s: &settings::Settings) {
        let ports: BTreeMap<String, bool> = parse_ports(&s.dev_ports).into_iter().map(|(name, n)| (format!("{name} :{n}"), port_open(n))).collect();
        for (name, up) in self.ports.update(ports) {
            server("port", &name, up);
        }
        if s.dev_docker {
            match docker_ps() {
                Some(out) => {
                    if self.docker_ok == Some(false) {
                        send(msg("docker", "Docker".into(), tr("Çalışıyor yine").into(), LiveState::Done));
                    }
                    self.docker_ok = Some(true);
                    for (name, up) in self.docker.update(parse_docker(&out, &s.dev_docker_filter)) {
                        server("ctr", &name, up);
                    }
                }
                None => {
                    if self.docker_ok == Some(true) {
                        send(msg("docker", "Docker".into(), tr("Durdu").into(), LiveState::Wait));
                    }
                    self.docker_ok = Some(false);
                }
            }
        } else {
            self.clear_docker();
        }
    }

    fn clear_docker(&mut self) {
        for name in self.docker.down() {
            send(msg(&format!("ctr:{name}"), String::new(), String::new(), LiveState::Clear));
        }
        if self.docker_ok == Some(false) {
            send(msg("docker", String::new(), String::new(), LiveState::Clear));
        }
        (self.docker, self.docker_ok) = (Watched::default(), None);
    }

    fn clear_all(&mut self) {
        for name in self.ports.down() {
            send(msg(&format!("port:{name}"), String::new(), String::new(), LiveState::Clear));
        }
        self.ports = Watched::default();
        self.clear_docker();
    }
}

fn server(kind: &str, name: &str, up: bool) {
    let (text, state) = if up { (tr("Çalışıyor yine"), LiveState::Done) } else { (tr("Durdu"), LiveState::Wait) };
    send(msg(&format!("{kind}:{name}"), name.to_string(), text.into(), state));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_wait_then_clear_with_a_gap() {
        let mut l = Limit::default();
        assert_eq!(l.step(true, false, 0, 30), Change::Same, "not yet: it must stay over");
        assert_eq!(l.step(true, false, 31, 30), Change::Raised);
        assert_eq!(l.step(false, false, 40, 30), Change::Same, "under the limit but not well under: still raised");
        assert_eq!(l.step(false, true, 45, 30), Change::Lowered);
        assert_eq!(l.step(true, false, 50, 30), Change::Same, "a spike starts the wait again");
    }

    #[test]
    fn ports_and_containers_parse() {
        assert_eq!(parse_ports("api:3000, web:5173;8080 junk x:0"), vec![("api".into(), 3000), ("web".into(), 5173), ("8080".into(), 8080)]);
        let ps = "tapi-redis|running\ntapi-minio|exited\nother|running\n";
        let all = parse_docker(ps, "TAPI");
        assert_eq!(all.len(), 2);
        assert_eq!(all.get("tapi-minio"), Some(&false));
    }

    #[test]
    fn only_changes_alert_and_the_first_look_learns() {
        let mut w = Watched::default();
        let m = |up: bool| BTreeMap::from([("redis".to_string(), up)]);
        assert!(w.update(m(false)).is_empty(), "down at the first look: learned, not news");
        assert_eq!(w.update(m(true)), vec![("redis".to_string(), true)]);
        assert_eq!(w.update(m(false)), vec![("redis".to_string(), false)]);
        assert!(w.update(m(false)).is_empty());
    }
}
