//! Author: TechnoL0g
//! Process memory (RSS) probe + periodic log line so operators can spot leaks on small VPS.

use std::time::Duration;
use tracing::info;

/// Resident set size of this process in bytes (Linux via /proc, macOS/Windows via `ps`/`tasklist`-free fallback = None).
pub fn rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        return Some(pages * 4096);
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Logs `memory: rss …` every `every` seconds (INFO) — compare across hours to detect growth.
pub async fn watchdog(every: Duration) {
    if let Some(limit) = fd_limit() {
        if limit < 8192 {
            tracing::warn!(limit, "open-files limit is low: a public API / many peers hit `Too many open files` (os error 24) — set LimitNOFILE=65536 in the systemd unit (ulimit in a shell does not apply to services)");
        }
    }
    let mut ticker = tokio::time::interval(every);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        if let Some(rss) = rss_bytes() {
            let m = tokio::runtime::Handle::current().metrics();
            info!(
                "memory: rss {:.0} MB · swapped out {:.0} MB · open fds {} / {} · tasks alive {} · workers {} · {}",
                rss as f64 / 1_048_576.0,
                swap_bytes().unwrap_or(0) as f64 / 1_048_576.0,
                open_fds().unwrap_or(0),
                fd_limit().unwrap_or(0),
                m.num_alive_tasks(),
                m.num_workers(),
                breakdown()
            );
        }
    }
}

/// `VmSwap` of this process in bytes (0 = nothing swapped out). Metrics `node.swapBytes`.
pub fn swap_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmSwap:"))?;
    line.split_whitespace().nth(1)?.parse::<u64>().ok().map(|kb| kb * 1024)
}

/// One line telling a frozen runtime's likely cause apart: swapped-out memory, load average, blocked threads.
pub fn pressure_summary() -> String {
    let swap = swap_bytes().map(|b| format!("{:.0} MB", b as f64 / 1_048_576.0)).unwrap_or_else(|| "?".into());
    let load = std::fs::read_to_string("/proc/loadavg").ok().map(|l| l.split_whitespace().take(3).collect::<Vec<_>>().join(" ")).unwrap_or_else(|| "?".into());
    let blocked = std::fs::read_to_string("/proc/stat").ok().and_then(|s| s.lines().find(|l| l.starts_with("procs_blocked")).and_then(|l| l.split_whitespace().nth(1).map(String::from))).unwrap_or_else(|| "?".into());
    let rss = rss_bytes().map(|b| format!("{:.0} MB", b as f64 / 1_048_576.0)).unwrap_or_else(|| "?".into());
    format!("rss {rss}, swapped out {swap}, loadavg {load}, procs blocked on I/O {blocked} (swap > 0 or blocked > 0 = memory/disk pressure, not a deadlock)")
}

/// Open file descriptors of this process (sockets, sled files, …). Metrics `node.openFds`.
pub fn open_fds() -> Option<u64> {
    std::fs::read_dir("/proc/self/fd").ok().map(|d| d.count() as u64)
}

/// Soft `RLIMIT_NOFILE` (`/proc/self/limits`). Metrics `node.fdLimit`.
pub fn fd_limit() -> Option<u64> {
    let limits = std::fs::read_to_string("/proc/self/limits").ok()?;
    let line = limits.lines().find(|l| l.starts_with("Max open files"))?;
    line.split_whitespace().nth(3)?.parse().ok()
}

/// Sizes of the in-memory structures that could grow (registered by the subsystems); printed with the memory line.
static BREAKDOWN: std::sync::Mutex<Vec<(&'static str, Box<dyn Fn() -> usize + Send + Sync>)>> = std::sync::Mutex::new(Vec::new());

/// Register a gauge for the memory line, e.g. `mem::gauge("finality votes", move || votes.len())`.
pub fn gauge(name: &'static str, f: impl Fn() -> usize + Send + Sync + 'static) {
    BREAKDOWN.lock().unwrap_or_else(|e| e.into_inner()).push((name, Box::new(f)));
}

pub fn breakdown() -> String {
    BREAKDOWN.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(n, f)| format!("{n} {}", f())).collect::<Vec<_>>().join(" · ")
}

static HEARTBEAT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Seconds since the tokio runtime last ticked (0 = healthy). Exposed in metrics as `node.runtimeLagSecs`.
pub fn runtime_lag_secs() -> u64 {
    let hb = HEARTBEAT.load(std::sync::atomic::Ordering::Relaxed);
    if hb == 0 { 0 } else { unix_now().saturating_sub(hb) }
}

/// Runtime freeze watchdog. A tokio task bumps a heartbeat every second; a plain OS thread — independent of the runtime —
/// checks it every 5 s. When the runtime has not ticked for `freeze_secs` (all workers blocked: deadlock, swap thrash,
/// disk stall) the thread logs ERROR, posts the alert through its own tiny runtime and ABORTS the process so systemd
/// (`Restart=always`) brings the node back: a frozen delegate misses every slot, a restarted one misses one or two.
/// `freeze_secs = 0` disables the abort (the heartbeat still feeds `runtime_lag_secs`).
pub fn spawn_freeze_watchdog(freeze_secs: u64, alerts: crate::node_config::AlertsConfig, label: String) {
    HEARTBEAT.store(unix_now(), std::sync::atomic::Ordering::Relaxed);
    tokio::spawn(async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            HEARTBEAT.store(unix_now(), std::sync::atomic::Ordering::Relaxed);
        }
    });
    if freeze_secs == 0 {
        return;
    }
    std::thread::Builder::new()
        .name("freeze-watchdog".into())
        .spawn(move || {
            let mut warned = false;
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let lag = runtime_lag_secs();
                if lag >= 15 && !warned {
                    warned = true;
                    eprintln!("[freeze-watchdog] tokio runtime has not ticked for {lag}s (all workers busy or blocked) — abort at {freeze_secs}s; {} ; for a stack dump: gdb -p $(pidof sth-core) -batch -ex 'thread apply all bt'", pressure_summary());
                } else if lag < 5 {
                    warned = false;
                }
                if lag >= freeze_secs {
                    let rss = rss_bytes().map(|b| format!("{:.0} MB", b as f64 / 1_048_576.0)).unwrap_or_else(|| "?".into());
                    let msg = format!("RUNTIME FROZEN: the node's async runtime has not ticked for {lag}s (rss {rss}) — aborting so systemd restarts it (delegates were missing slots)");
                    eprintln!("[freeze-watchdog] {msg}");
                    tracing::error!("{msg}");
                    if alerts.enabled && (!alerts.telegram_bot_token.is_empty() || !alerts.webhook_url.is_empty()) {
                        if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
                            let _ = rt.block_on(async { tokio::time::timeout(Duration::from_secs(10), crate::alerts::send_now(&alerts, &label, "freeze", &msg)).await });
                        }
                    }
                    std::process::abort();
                }
            }
        })
        .expect("freeze watchdog thread");
}
