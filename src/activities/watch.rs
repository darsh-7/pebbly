//! The PC, watched on one thread: its health (CPU, memory, C:) and, now and then, a glance at its
//! numbers. Trouble becomes a live item that waits (it pops, with the alert sound) until it's
//! over, then says so for a moment; the glance shows for a few seconds and goes. The thread only
//! runs while one of the two is on in the settings.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::activities::live::{LiveMsg, LiveState};
use crate::i18n::{tr, trf};
use crate::platform::log::log;
use crate::settings;

static RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether the settings want the thread.
fn wanted(s: &settings::Settings) -> bool {
    s.enabled && (s.health_on || s.info_glance_on)
}

/// Starts the thread when health or the glance was turned on (it ends by itself once both are off).
pub fn follow_settings() {
    if wanted(&settings::current()) && !RUNNING.swap(true, Ordering::SeqCst) && std::thread::Builder::new().name("watch".into()).spawn(run).is_err() {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

fn send(m: LiveMsg) {
    crate::platform::ipc::queue(crate::platform::ipc::Command::Live(Box::new(m)));
    crate::platform::windows::post_watch();
}

fn msg(id: &str, title: String, text: String, state: LiveState) -> LiveMsg {
    LiveMsg { id: format!("watch:{id}"), title, text, progress: None, state, claude: None, app: None }
}

/// The first glance comes this soon after the start, the next ones every `info_glance_every_min`.
const FIRST_GLANCE: Duration = Duration::from_secs(60);

fn run() {
    log("watch: started");
    let mut health = Health::default();
    let mut next_health = Instant::now();
    let mut next_glance = Instant::now() + FIRST_GLANCE;
    // The glance on screen now: when it goes.
    let mut glance_until: Option<Instant> = None;
    loop {
        let s = settings::current();
        if !wanted(&s) {
            break;
        }
        let now = Instant::now();
        if s.health_on {
            if now >= next_health {
                health.poll(&s);
                next_health = now + Duration::from_secs(s.health_every_s as u64);
            }
        } else {
            health.clear_all();
        }
        if glance_until.is_some_and(|until| now >= until) || (glance_until.is_some() && !s.info_glance_on) {
            send(msg("info", String::new(), String::new(), LiveState::Clear));
            glance_until = None;
        }
        if s.info_glance_on && now >= next_glance {
            if let Some(text) = health.glance() {
                send(msg("info", tr("Bilgisayar").into(), text, LiveState::Run));
                glance_until = Some(now + Duration::from_secs(s.info_glance_s as u64));
            }
            next_glance = now + Duration::from_secs(s.info_glance_every_min as u64 * 60);
        }
        // Wakes often enough to follow the settings, sleeps the rest.
        std::thread::sleep(Duration::from_secs(1));
    }
    health.clear_all();
    if glance_until.is_some() {
        send(msg("info", String::new(), String::new(), LiveState::Clear));
    }
    RUNNING.store(false, Ordering::SeqCst);
    log("watch: stopped");
}

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
    /// The last CPU % measured (the glance's, between polls).
    last_cpu: Option<u8>,
    started: Option<Instant>,
}

fn filetime(f: windows::Win32::Foundation::FILETIME) -> u64 {
    (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64
}

/// "3 sa 12 dk", "2 gün 4 sa": how long Windows has been up.
pub fn uptime_text(ms: u64) -> String {
    let m = ms / 60_000;
    match (m / 1440, m % 1440 / 60, m % 60) {
        (0, 0, m) => trf("{} dk", &[&m]),
        (0, h, m) => trf("{} sa {} dk", &[&h, &m]),
        (d, h, _) => trf("{} gün {} sa", &[&d, &h]),
    }
}

impl Health {
    /// CPU % since the last call (None on the first).
    fn cpu_pct(&mut self) -> Option<u8> {
        use windows::Win32::Foundation::FILETIME;
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        unsafe { windows::Win32::System::Threading::GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)).ok()? };
        // Kernel time includes idle time.
        let (idle, total) = (filetime(idle), filetime(kernel) + filetime(user));
        let (li, lt) = self.last_times.replace((idle, total))?;
        let (di, dt) = (idle.saturating_sub(li), total.saturating_sub(lt));
        let pct = (dt > 0).then(|| (100 - (di * 100 / dt).min(100)) as u8);
        self.last_cpu = pct.or(self.last_cpu);
        pct
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

    /// "CPU %12 · Bellek %48 · C: 120 GB · 3 sa": the glance's line.
    fn glance(&mut self) -> Option<String> {
        // A fresh CPU reading needs two samples: the second a moment after the first.
        if self.cpu_pct().is_none() {
            std::thread::sleep(Duration::from_millis(500));
            self.cpu_pct();
        }
        let mut parts = Vec::new();
        if let Some(cpu) = self.last_cpu {
            parts.push(trf("CPU %{}", &[&cpu]));
        }
        if let Some(ram) = Self::ram_pct() {
            parts.push(trf("Bellek %{}", &[&ram]));
        }
        if let Some(free) = Self::disk_free_gb() {
            parts.push(format!("C: {free} GB"));
        }
        parts.push(uptime_text(unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }));
        (!parts.is_empty()).then(|| parts.join(" · "))
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
    fn uptime_reads_in_minutes_hours_days() {
        assert_eq!(uptime_text(5 * 60_000), "5 dk");
        assert_eq!(uptime_text((3 * 60 + 12) * 60_000), "3 sa 12 dk");
        assert_eq!(uptime_text((2 * 1440 + 4 * 60 + 30) * 60_000), "2 gün 4 sa");
    }
}
