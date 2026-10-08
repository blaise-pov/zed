use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_settings::TerminalWatchdogSettings;
use gpui::AsyncApp;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

use crate::TerminalHandle;
use util::ResultExt;

pub const DISK_ACTIVITY_THRESHOLD_BYTES: u64 = 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct ActivitySample {
    pub cpu_percent: f32,
    pub disk_bytes_delta: u64,
    pub available: bool,
}

pub trait ActivityProbe: 'static + Send {
    fn sample(&mut self, root_pid: Option<u32>, interval: Duration) -> ActivitySample;
}

impl ActivityProbe for Box<dyn ActivityProbe> {
    fn sample(&mut self, root_pid: Option<u32>, interval: Duration) -> ActivitySample {
        (**self).sample(root_pid, interval)
    }
}

#[cfg(test)]
#[derive(Clone)]
pub struct FakeActivityProbe {
    pub sample: ActivitySample,
}

#[cfg(test)]
impl ActivityProbe for FakeActivityProbe {
    fn sample(&mut self, root_pid: Option<u32>, _interval: Duration) -> ActivitySample {
        if root_pid.is_none() {
            ActivitySample {
                cpu_percent: 0.0,
                disk_bytes_delta: 0,
                available: false,
            }
        } else {
            self.sample.clone()
        }
    }
}

pub struct SysinfoActivityProbe {
    system: System,
    refresh_kind: ProcessRefreshKind,
}

impl SysinfoActivityProbe {
    pub fn new() -> Self {
        let refresh_kind = ProcessRefreshKind::nothing()
            .without_tasks()
            .with_cpu()
            .with_disk_usage();
        // Initialize empty System to avoid eager machine-wide process snapshot (#58651).
        let system = System::new();
        Self {
            system,
            refresh_kind,
        }
    }
}

impl Default for SysinfoActivityProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityProbe for SysinfoActivityProbe {
    fn sample(&mut self, root_pid: Option<u32>, _interval: Duration) -> ActivitySample {
        let Some(root_u32) = root_pid else {
            return ActivitySample {
                cpu_percent: 0.0,
                disk_bytes_delta: 0,
                available: false,
            };
        };

        let root = sysinfo::Pid::from_u32(root_u32);

        // Refresh with remove-dead semantics to keep System bounded (#58651).
        self.system
            .refresh_processes_specifics(ProcessesToUpdate::All, true, self.refresh_kind);

        if self.system.process(root).is_none() {
            return ActivitySample {
                cpu_percent: 0.0,
                disk_bytes_delta: 0,
                available: false,
            };
        }

        let mut parent_map: collections::HashMap<sysinfo::Pid, Vec<sysinfo::Pid>> =
            collections::HashMap::default();
        for (pid, process) in self.system.processes() {
            if let Some(parent) = process.parent() {
                parent_map.entry(parent).or_default().push(*pid);
            }
        }

        let mut total_cpu = 0.0f32;
        let mut total_disk_delta = 0u64;

        let mut visited = collections::HashSet::default();
        let mut stack = vec![root];
        while let Some(current) = stack.pop() {
            if !visited.insert(current) {
                continue;
            }
            if let Some(process) = self.system.process(current) {
                total_cpu += process.cpu_usage();
                let disk = process.disk_usage();
                total_disk_delta = total_disk_delta
                    .saturating_add(disk.read_bytes.saturating_add(disk.written_bytes));
            }

            if let Some(children) = parent_map.get(&current) {
                for child in children {
                    if !visited.contains(child) {
                        stack.push(*child);
                    }
                }
            }
        }

        ActivitySample {
            cpu_percent: total_cpu,
            disk_bytes_delta: total_disk_delta,
            available: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogStop {
    HardCap { cap_ms: u64 },
    Idle { window_ms: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogDecision {
    Continue,
    Stop(WatchdogStop),
}

#[derive(Debug, Clone)]
pub struct WatchdogState {
    #[allow(dead_code)]
    pub started_at: Instant,
    pub last_progress: Instant,
    pub last_output_counter: u64,
}

impl WatchdogState {
    pub fn new(now: Instant, initial_output_counter: u64) -> Self {
        Self {
            started_at: now,
            last_progress: now,
            last_output_counter: initial_output_counter,
        }
    }
}

pub fn decide(
    state: &mut WatchdogState,
    sample: &ActivitySample,
    current_output_counter: u64,
    settings: &TerminalWatchdogSettings,
    now: Instant,
) -> WatchdogDecision {
    if !settings.enabled {
        return WatchdogDecision::Continue;
    }

    let output_delta = current_output_counter.saturating_sub(state.last_output_counter);
    state.last_output_counter = current_output_counter;

    let output_active = output_delta > 0;
    let cpu_active = sample.available && sample.cpu_percent >= settings.idle_cpu_threshold_percent;
    let disk_active = sample.available && sample.disk_bytes_delta > DISK_ACTIVITY_THRESHOLD_BYTES;

    if output_active || cpu_active || disk_active {
        state.last_progress = now;
    }

    if let Some(idle_timeout_ms) = settings.idle_timeout_ms {
        let window_ms = if sample.available {
            idle_timeout_ms
        } else {
            settings.idle_timeout_no_probe_ms
        };

        if now.saturating_duration_since(state.last_progress) > Duration::from_millis(window_ms) {
            return WatchdogDecision::Stop(WatchdogStop::Idle { window_ms });
        }
    }

    WatchdogDecision::Continue
}

pub fn cap_timer(
    hard_timeout_ms: Option<u64>,
    executor: &gpui::BackgroundExecutor,
) -> impl Future<Output = WatchdogStop> {
    let timer = hard_timeout_ms.map(|ms| executor.timer(Duration::from_millis(ms)));
    async move {
        if let (Some(ms), Some(timer)) = (hard_timeout_ms, timer) {
            timer.await;
            WatchdogStop::HardCap { cap_ms: ms }
        } else {
            futures::future::pending().await
        }
    }
}

pub struct TerminalWatchdog<P: ActivityProbe = SysinfoActivityProbe> {
    settings: TerminalWatchdogSettings,
    probe: P,
    state: WatchdogState,
    now_fn: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl<P: ActivityProbe> TerminalWatchdog<P> {
    pub fn new(settings: TerminalWatchdogSettings, probe: P) -> Self {
        Self::new_with_clock(settings, probe, Arc::new(Instant::now))
    }

    pub fn new_with_clock(
        settings: TerminalWatchdogSettings,
        probe: P,
        now_fn: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        let now = (now_fn)();
        Self {
            settings,
            probe,
            state: WatchdogState::new(now, 0),
            now_fn,
        }
    }

    pub fn with_initial_output_counter(mut self, counter: u64) -> Self {
        self.state.last_output_counter = counter;
        self
    }

    #[allow(dead_code)]
    pub fn with_now_fn(mut self, now_fn: Arc<dyn Fn() -> Instant + Send + Sync>) -> Self {
        let now = (now_fn)();
        self.state.started_at = now;
        self.state.last_progress = now;
        self.now_fn = now_fn;
        self
    }

    pub async fn run_idle_loop(
        &mut self,
        terminal: &dyn TerminalHandle,
        cx: &AsyncApp,
    ) -> WatchdogStop {
        let poll_interval = Duration::from_millis(self.settings.poll_interval_ms);
        let executor = cx.background_executor();

        loop {
            executor.timer(poll_interval).await;

            let root_pid = terminal.process_tree_root_pid(cx).log_err().flatten();
            let sample = self.probe.sample(root_pid, poll_interval);
            let current_output = terminal
                .output_activity_counter(cx)
                .log_err()
                .unwrap_or(self.state.last_output_counter);
            let now = (self.now_fn)();

            if let WatchdogDecision::Stop(stop) = decide(
                &mut self.state,
                &sample,
                current_output,
                &self.settings,
                now,
            ) {
                return stop;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    fn test_settings() -> TerminalWatchdogSettings {
        TerminalWatchdogSettings {
            enabled: true,
            idle_timeout_ms: Some(300_000),
            idle_cpu_threshold_percent: 5.0,
            hard_timeout_ms: Some(3_600_000),
            poll_interval_ms: 5000,
            idle_timeout_no_probe_ms: 600_000,
        }
    }

    #[test]
    fn test_decide_flat_signals_probe_available_stops_after_idle_timeout() {
        let settings = test_settings();
        let start = Instant::now();
        let mut state = WatchdogState::new(start, 0);

        let quiet_sample = ActivitySample {
            cpu_percent: 0.0,
            disk_bytes_delta: 0,
            available: true,
        };

        // Before idle timeout -> Continue
        let t1 = start + Duration::from_millis(200_000);
        assert_eq!(
            decide(&mut state, &quiet_sample, 0, &settings, t1),
            WatchdogDecision::Continue
        );

        // At exactly idle timeout (300_000ms) -> not strictly greater -> Continue
        let t_exact = start + Duration::from_millis(300_000);
        assert_eq!(
            decide(&mut state, &quiet_sample, 0, &settings, t_exact),
            WatchdogDecision::Continue
        );

        // Past idle timeout (300_001ms) -> Stop(Idle)
        let t_after = start + Duration::from_millis(300_001);
        assert_eq!(
            decide(&mut state, &quiet_sample, 0, &settings, t_after),
            WatchdogDecision::Stop(WatchdogStop::Idle { window_ms: 300_000 })
        );
    }

    #[test]
    fn test_decide_output_ticking_never_idle() {
        let settings = test_settings();
        let start = Instant::now();
        let mut state = WatchdogState::new(start, 0);

        let quiet_sample = ActivitySample {
            cpu_percent: 0.0,
            disk_bytes_delta: 0,
            available: true,
        };

        // Output ticks at 200s
        let t1 = start + Duration::from_millis(200_000);
        assert_eq!(
            decide(&mut state, &quiet_sample, 10, &settings, t1),
            WatchdogDecision::Continue
        );
        assert_eq!(state.last_progress, t1);

        // Output ticks at 400s
        let t2 = start + Duration::from_millis(400_000);
        assert_eq!(
            decide(&mut state, &quiet_sample, 20, &settings, t2),
            WatchdogDecision::Continue
        );
        assert_eq!(state.last_progress, t2);

        // Output ticks at 600s
        let t3 = start + Duration::from_millis(600_000);
        assert_eq!(
            decide(&mut state, &quiet_sample, 30, &settings, t3),
            WatchdogDecision::Continue
        );
        assert_eq!(state.last_progress, t3);
    }

    #[test]
    fn test_decide_cpu_above_threshold_never_idle() {
        let settings = test_settings();
        let start = Instant::now();
        let mut state = WatchdogState::new(start, 0);

        let busy_cpu_sample = ActivitySample {
            cpu_percent: 10.0, // > 5.0%
            disk_bytes_delta: 0,
            available: true,
        };

        let t1 = start + Duration::from_millis(250_000);
        assert_eq!(
            decide(&mut state, &busy_cpu_sample, 0, &settings, t1),
            WatchdogDecision::Continue
        );
        assert_eq!(state.last_progress, t1);

        let t2 = start + Duration::from_millis(500_000);
        assert_eq!(
            decide(&mut state, &busy_cpu_sample, 0, &settings, t2),
            WatchdogDecision::Continue
        );
        assert_eq!(state.last_progress, t2);
    }

    #[test]
    fn test_decide_disk_above_threshold_never_idle() {
        let settings = test_settings();
        let start = Instant::now();
        let mut state = WatchdogState::new(start, 0);

        let busy_disk_sample = ActivitySample {
            cpu_percent: 0.0,
            disk_bytes_delta: 2048, // > 1024 bytes
            available: true,
        };

        let t1 = start + Duration::from_millis(250_000);
        assert_eq!(
            decide(&mut state, &busy_disk_sample, 0, &settings, t1),
            WatchdogDecision::Continue
        );
        assert_eq!(state.last_progress, t1);

        let t2 = start + Duration::from_millis(500_000);
        assert_eq!(
            decide(&mut state, &busy_disk_sample, 0, &settings, t2),
            WatchdogDecision::Continue
        );
        assert_eq!(state.last_progress, t2);
    }

    #[test]
    fn test_decide_probe_unavailable_output_only_widened_window() {
        let settings = test_settings();
        let start = Instant::now();
        let mut state = WatchdogState::new(start, 0);

        // When probe is unavailable, high CPU and disk must be ignored
        let unavailable_sample = ActivitySample {
            cpu_percent: 99.0,
            disk_bytes_delta: 100_000,
            available: false,
        };

        // At 300_001ms, the standard 300s window has expired, but widened window (600s) has not
        let t1 = start + Duration::from_millis(300_001);
        assert_eq!(
            decide(&mut state, &unavailable_sample, 0, &settings, t1),
            WatchdogDecision::Continue
        );
        // last_progress was NOT updated because CPU/disk were ignored
        assert_eq!(state.last_progress, start);

        // At 600_001ms, widened window expires
        let t2 = start + Duration::from_millis(600_001);
        assert_eq!(
            decide(&mut state, &unavailable_sample, 0, &settings, t2),
            WatchdogDecision::Stop(WatchdogStop::Idle { window_ms: 600_000 })
        );
    }

    #[test]
    fn test_decide_busy_forever_continues() {
        let settings = test_settings();
        let start = Instant::now();
        let mut state = WatchdogState::new(start, 0);

        let busy_sample = ActivitySample {
            cpu_percent: 50.0,
            disk_bytes_delta: 5000,
            available: true,
        };

        let t_cap = start + Duration::from_millis(3_600_001);
        assert_eq!(
            decide(&mut state, &busy_sample, 50, &settings, t_cap),
            WatchdogDecision::Continue
        );
    }

    #[gpui::test]
    async fn test_cap_timer_triggers_hard_cap(cx: &mut TestAppContext) {
        let executor = cx.background_executor.clone();
        let cap_future = cap_timer(Some(50), &executor);
        let stop = cap_future.await;
        assert_eq!(stop, WatchdogStop::HardCap { cap_ms: 50 });
    }

    #[test]
    fn test_decide_disabled_never_stops() {
        let mut settings = test_settings();
        settings.enabled = false;

        let start = Instant::now();
        let mut state = WatchdogState::new(start, 0);

        let quiet_sample = ActivitySample {
            cpu_percent: 0.0,
            disk_bytes_delta: 0,
            available: true,
        };

        let t_far = start + Duration::from_millis(10_000_000);
        assert_eq!(
            decide(&mut state, &quiet_sample, 0, &settings, t_far),
            WatchdogDecision::Continue
        );
    }

    #[test]
    fn test_decide_none_timeouts_never_stop() {
        let mut settings = test_settings();
        settings.idle_timeout_ms = None;
        settings.hard_timeout_ms = None;

        let start = Instant::now();
        let mut state = WatchdogState::new(start, 0);

        let quiet_sample = ActivitySample {
            cpu_percent: 0.0,
            disk_bytes_delta: 0,
            available: true,
        };

        let t_far = start + Duration::from_millis(10_000_000);
        assert_eq!(
            decide(&mut state, &quiet_sample, 0, &settings, t_far),
            WatchdogDecision::Continue
        );
    }

    #[gpui::test]
    async fn test_watchdog_run_idle_loop_stops_on_idle(cx: &mut TestAppContext) {
        let counter = Arc::new(AtomicU64::new(0));
        let handle = cx.update(|cx| {
            crate::tests::FakeTerminalHandle::new_never_exits(cx)
                .with_output_activity_counter(counter.clone())
                .with_process_tree_root_pid(Some(12345))
        });

        let settings = TerminalWatchdogSettings {
            enabled: true,
            idle_timeout_ms: Some(20),
            idle_cpu_threshold_percent: 5.0,
            hard_timeout_ms: Some(10_000),
            poll_interval_ms: 10,
            idle_timeout_no_probe_ms: 50,
        };

        let probe = FakeActivityProbe {
            sample: ActivitySample {
                cpu_percent: 0.0,
                disk_bytes_delta: 0,
                available: true,
            },
        };

        let executor = cx.background_executor.clone();
        let mut watchdog =
            TerminalWatchdog::new(settings, probe).with_now_fn(Arc::new(move || executor.now()));
        let idle_task = cx.spawn(|cx| async move { watchdog.run_idle_loop(&handle, &cx).await });

        let stop = idle_task.await;
        assert_eq!(stop, WatchdogStop::Idle { window_ms: 20 });
    }
}
