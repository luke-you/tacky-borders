#[path = "test_support.rs"]
mod test_support;

use serial_test::serial;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tacky_borders::border_coordinator::{
    BorderCoordinator, BorderReadiness, OperationKind, OperationOutcome,
};
use tacky_borders::border_registry::{
    BaselineWindow, BorderControl, BorderInstanceId, BorderPhase, BorderRegistry, BorderWorkerExit,
    TrackingWindowFingerprint,
};
use tacky_borders::border_updates::BorderUpdateMailbox;
use tacky_borders::utils::OwnedHWND;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, IsWindow, MSG, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, PostMessageW, QS_ALLINPUT,
    WINDOW_EX_STYLE, WM_CLOSE, WM_NCDESTROY, WM_SYSCOMMAND, WS_POPUP,
};

struct TrackingWindow {
    hwnd: isize,
    stop: mpsc::Sender<()>,
    owner: Option<JoinHandle<anyhow::Result<()>>>,
}

impl TrackingWindow {
    fn new() -> anyhow::Result<Self> {
        let (created, ready) = mpsc::channel();
        let (stop, stopping) = mpsc::channel();
        let owner = thread::spawn(move || -> anyhow::Result<()> {
            let window = OwnedHWND(unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    windows::core::w!("STATIC"),
                    windows::core::w!("fix-01-isolated-tracking"),
                    WS_POPUP,
                    0,
                    0,
                    100,
                    100,
                    None,
                    None,
                    None,
                    None,
                )?
            });
            created.send(window.0.0 as isize)?;
            let mut message = MSG::default();
            while matches!(stopping.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                for _ in 0..64 {
                    if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                        break;
                    }
                    unsafe {
                        DispatchMessageW(&message);
                    }
                }
                unsafe {
                    MsgWaitForMultipleObjectsEx(None, 10, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
                }
            }
            Ok(())
        });
        match ready.recv_timeout(Duration::from_secs(5)) {
            Ok(hwnd) => Ok(Self {
                hwnd,
                stop,
                owner: Some(owner),
            }),
            Err(error) => {
                let _ = stop.send(());
                let _ = owner.join();
                Err(error.into())
            }
        }
    }

    fn close(mut self) -> anyhow::Result<()> {
        self.stop.send(())?;
        if let Some(owner) = self.owner.take() {
            match owner.join() {
                Ok(result) => result?,
                Err(payload) => {
                    tacky_borders::border_registry::discard_panic(payload);
                    anyhow::bail!("test tracking owner panicked");
                }
            }
        }
        anyhow::ensure!(
            !unsafe { IsWindow(Some(HWND(self.hwnd as _))) }.as_bool(),
            "test tracking window survived owner join"
        );
        Ok(())
    }
}

impl Drop for TrackingWindow {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

struct Cohort(BorderCoordinator);

impl Cohort {
    fn new() -> Self {
        Self(BorderCoordinator::new(Arc::new(Mutex::new(
            BorderRegistry::default(),
        ))))
    }

    fn pump_until(&mut self, stage: &str, mut ready: impl FnMut(&mut BorderCoordinator) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.0.service_turn();
            if ready(&mut self.0) {
                return;
            }
            if Instant::now() >= deadline {
                let registry = self.0.registry.lock().unwrap();
                let entries = registry
                    .entries
                    .values()
                    .map(|entry| {
                        (
                            entry.id,
                            entry.phase,
                            entry.border_hwnd,
                            entry.worker_thread_id,
                            entry.launch_pending,
                            entry.control.is_stopped(),
                            entry.updates.is_closed(),
                        )
                    })
                    .collect::<Vec<_>>();
                drop(registry);
                let outcomes = std::iter::from_fn(|| self.0.take_outcome()).collect::<Vec<_>>();
                panic!(
                    "test-owned cohort failed at {stage}; awaiting_baseline={:?}; \
                     settled={}; entries={entries:?}; outcomes={outcomes:?}; \
                     dropped_outcomes={}; signal_failures={}",
                    self.0.awaiting_baseline(),
                    self.0.is_settled(),
                    self.0.dropped_outcomes,
                    self.0.signal_failures,
                );
            }
            thread::yield_now();
        }
    }

    fn reload(&mut self, tracking: &TrackingWindow) -> (BorderInstanceId, isize) {
        self.reload_many(std::slice::from_ref(tracking))[0]
    }

    fn reload_many(&mut self, tracking: &[TrackingWindow]) -> Vec<(BorderInstanceId, isize)> {
        let operation = self
            .0
            .begin_operation(
                OperationKind::Reload,
                Instant::now() + Duration::from_secs(10),
            )
            .unwrap();
        self.pump_until("reload awaiting baseline", |coordinator| {
            coordinator.awaiting_baseline() == Some(operation)
        });
        let windows = tracking
            .iter()
            .map(|tracking| {
                let control = Arc::new(BorderControl::new().unwrap());
                let updates = Arc::new(BorderUpdateMailbox::new(control).unwrap());
                BaselineWindow {
                    fingerprint: TrackingWindowFingerprint::capture(HWND(tracking.hwnd as _))
                        .unwrap(),
                    updates: Some(updates),
                }
            })
            .collect();
        self.0.admit_baseline(operation, windows, 0, 0).unwrap();
        let outcome = self.0.take_operation_outcome().unwrap();
        let ids = match outcome {
            OperationOutcome::Reloaded {
                operation: actual,
                old_generation_retired,
                admitted_instances,
                readiness,
                ..
            } => {
                assert_eq!(actual, operation);
                assert!(old_generation_retired);
                assert_eq!(readiness, BorderReadiness::NotAwaited);
                assert_eq!(admitted_instances.len(), tracking.len());
                admitted_instances
            }
            other => panic!("unexpected reload outcome: {other:?}"),
        };
        self.pump_until("reload waiting for workers to run", |coordinator| {
            let registry = coordinator.registry.lock().unwrap();
            ids.iter().all(|id| {
                registry
                    .entries
                    .get(id)
                    .is_some_and(|entry| entry.phase == BorderPhase::Running)
            })
        });
        let registry = self.0.registry.lock().unwrap();
        ids.into_iter()
            .map(|id| {
                let hwnd = registry.entries[&id].border_hwnd.unwrap();
                assert!(unsafe { IsWindow(Some(HWND(hwnd as _))) }.as_bool());
                (id, hwnd)
            })
            .collect()
    }

    fn stop(&mut self, kind: OperationKind) {
        let operation = self
            .0
            .begin_operation(kind, Instant::now() + Duration::from_secs(10))
            .unwrap();
        let mut outcome = None;
        self.pump_until("stop waiting for operation outcome", |coordinator| {
            outcome = coordinator.take_operation_outcome();
            outcome.is_some()
        });
        assert_eq!(outcome, Some(OperationOutcome::Stopped { operation }));
        assert!(self.0.is_settled());
    }
}

impl Drop for Cohort {
    fn drop(&mut self) {
        let _ = self.0.begin_operation(
            OperationKind::Shutdown,
            Instant::now() + Duration::from_secs(10),
        );
        while !self.0.is_settled() {
            self.0.service_turn();
            thread::yield_now();
        }
    }
}

#[test]
#[serial]
fn test_destroy_borders() -> anyhow::Result<()> {
    tacky_borders::register_border_window_class()?;
    let tracking = TrackingWindow::new()?;
    let mut cohort = Cohort::new();
    let (id, hwnd) = cohort.reload(&tracking);
    cohort.stop(OperationKind::Stop);
    assert!(!unsafe { IsWindow(Some(HWND(hwnd as _))) }.as_bool());
    assert_eq!(
        cohort.0.take_outcome(),
        Some((id, BorderWorkerExit::completed()))
    );
    assert!(unsafe { IsWindow(Some(HWND(tracking.hwnd as _))) }.as_bool());
    Ok(())
}

#[test]
#[serial]
fn test_reload_borders() -> anyhow::Result<()> {
    tacky_borders::register_border_window_class()?;
    let tracking = TrackingWindow::new()?;
    let mut cohort = Cohort::new();
    let (mut previous, _) = cohort.reload(&tracking);
    for _ in 0..5 {
        let (next, _) = cohort.reload(&tracking);
        assert_ne!(next, previous);
        assert!(
            !cohort
                .0
                .registry
                .lock()
                .unwrap()
                .entries
                .contains_key(&previous)
        );
        assert_eq!(
            cohort.0.take_outcome(),
            Some((previous, BorderWorkerExit::completed()))
        );
        previous = next;
    }
    cohort.stop(OperationKind::Shutdown);
    assert_eq!(
        cohort.0.take_outcome(),
        Some((previous, BorderWorkerExit::completed()))
    );
    assert!(cohort.0.registry.lock().unwrap().shutdown_requested);
    Ok(())
}

#[test]
#[serial]
fn public_messages_cannot_retire_test_owned_worker() -> anyhow::Result<()> {
    tacky_borders::register_border_window_class()?;
    let tracking = TrackingWindow::new()?;
    let mut cohort = Cohort::new();
    let (id, hwnd) = cohort.reload(&tracking);
    for message in [WM_CLOSE, WM_NCDESTROY, WM_SYSCOMMAND] {
        unsafe {
            PostMessageW(Some(HWND(hwnd as _)), message, WPARAM(0xf060), LPARAM(1))?;
        }
    }
    let deadline = Instant::now() + Duration::from_millis(150);
    while Instant::now() < deadline {
        cohort.0.service_turn();
        thread::yield_now();
    }
    assert!(unsafe { IsWindow(Some(HWND(hwnd as _))) }.as_bool());
    assert_eq!(
        cohort.0.registry.lock().unwrap().entries[&id].phase,
        BorderPhase::Running
    );
    assert!(cohort.0.take_outcome().is_none());
    cohort.stop(OperationKind::Shutdown);
    assert_eq!(
        cohort.0.take_outcome(),
        Some((id, BorderWorkerExit::completed()))
    );
    Ok(())
}

#[test]
#[serial]
fn equal_integrity_helper_cannot_replace_private_state() -> anyhow::Result<()> {
    tacky_borders::register_border_window_class()?;
    let tracking = TrackingWindow::new()?;
    let mut cohort = Cohort::new();
    let (id, hwnd) = cohort.reload(&tracking);
    let output = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "foreign_message_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("FIX01_TEST_OWNED_HWND", hwnd.to_string())
        .output()?;
    assert!(
        output.status.success(),
        "helper failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("former_payload_sends=48"));
    println!("{}", String::from_utf8_lossy(&output.stdout));
    assert!(unsafe { IsWindow(Some(HWND(hwnd as _))) }.as_bool());
    assert_eq!(
        cohort.0.registry.lock().unwrap().entries[&id].phase,
        BorderPhase::Running
    );
    cohort.stop(OperationKind::Shutdown);
    assert_eq!(
        cohort.0.take_outcome(),
        Some((id, BorderWorkerExit::completed()))
    );
    Ok(())
}

struct ResourceSampler {
    process: windows::core::Owned<windows::Win32::Foundation::HANDLE>,
    pid: u32,
    query: windows::Win32::System::Performance::PDH_HQUERY,
    counters: [windows::Win32::System::Performance::PDH_HCOUNTER; 3],
}

#[derive(Debug, serde::Serialize)]
struct ResourceSample {
    handles: u32,
    gdi: u32,
    threads: u32,
    border_workers: u32,
    animation_workers: u32,
    animation_scheduler_workers: u32,
    working_set: usize,
    private_bytes: usize,
    cpu_seconds: f64,
    gpu_dedicated_bytes: Option<f64>,
    gpu_shared_bytes: Option<f64>,
    gpu_peak_engine_percent: Option<f64>,
    gpu_status: [u32; 3],
}

impl ResourceSample {
    fn include_memory_peaks(&mut self, sample: &Self) {
        self.working_set = self.working_set.max(sample.working_set);
        self.private_bytes = self.private_bytes.max(sample.private_bytes);
        self.gpu_dedicated_bytes = self
            .gpu_dedicated_bytes
            .into_iter()
            .chain(sample.gpu_dedicated_bytes)
            .reduce(f64::max);
        self.gpu_shared_bytes = self
            .gpu_shared_bytes
            .into_iter()
            .chain(sample.gpu_shared_bytes)
            .reduce(f64::max);
    }
}

impl ResourceSampler {
    fn new() -> anyhow::Result<Self> {
        Self::for_process(std::process::id())
    }

    fn for_process(pid: u32) -> anyhow::Result<Self> {
        use windows::Win32::System::Performance::*;
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
        };
        let process = unsafe {
            windows::core::Owned::new(OpenProcess(
                PROCESS_QUERY_INFORMATION | PROCESS_VM_READ,
                false,
                pid,
            )?)
        };
        let mut query = PDH_HQUERY::default();
        let status = unsafe { PdhOpenQueryW(None, 0, &mut query) };
        anyhow::ensure!(status == 0, "PdhOpenQueryW: {status:#x}");
        let mut sampler = Self {
            process,
            pid,
            query,
            counters: [PDH_HCOUNTER::default(); 3],
        };
        for (counter, path) in sampler.counters.iter_mut().zip([
            format!("\\GPU Process Memory(pid_{pid}_*)\\Dedicated Usage"),
            format!("\\GPU Process Memory(pid_{pid}_*)\\Shared Usage"),
            format!("\\GPU Engine(pid_{pid}_*)\\Utilization Percentage"),
        ]) {
            let path = windows::core::HSTRING::from(path);
            let status = unsafe { PdhAddEnglishCounterW(query, &path, 0, counter) };
            anyhow::ensure!(status == 0, "PdhAddEnglishCounterW: {status:#x}");
        }
        Ok(sampler)
    }

    fn gpu_values(
        counter: windows::Win32::System::Performance::PDH_HCOUNTER,
    ) -> Result<Vec<f64>, u32> {
        use windows::Win32::System::Performance::*;
        let mut bytes = 0;
        let mut count = 0;
        let status = unsafe {
            PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut bytes, &mut count, None)
        };
        if status != PDH_MORE_DATA {
            return Err(status);
        }
        if bytes as usize > 1024 * 1024 {
            return Err(PDH_MORE_DATA);
        }
        let mut buffer = vec![
            PDH_FMT_COUNTERVALUE_ITEM_W::default();
            (bytes as usize)
                .div_ceil(std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>())
        ];
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE,
                &mut bytes,
                &mut count,
                Some(buffer.as_mut_ptr()),
            )
        };
        if status != 0 {
            return Err(status);
        }
        let values = buffer
            .iter()
            .take(count as usize)
            .filter(|item| item.FmtValue.CStatus <= 1)
            .map(|item| unsafe { item.FmtValue.Anonymous.doubleValue })
            .collect::<Vec<_>>();
        if values.is_empty() {
            Err(PDH_NO_DATA)
        } else {
            Ok(values)
        }
    }

    fn sample(&mut self) -> anyhow::Result<ResourceSample> {
        use windows::Win32::Foundation::{CloseHandle, FILETIME, HLOCAL, LocalFree};
        use windows::Win32::System::Diagnostics::ToolHelp::*;
        use windows::Win32::System::Performance::PdhCollectQueryData;
        use windows::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
        };
        use windows::Win32::System::Threading::*;
        let process = *self.process;
        let mut handles = 0;
        unsafe {
            GetProcessHandleCount(process, &mut handles)?;
        }
        let mut memory = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        unsafe {
            GetProcessMemoryInfo(
                process,
                (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
                memory.cb,
            )?;
        }
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe {
            GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user)?;
        }
        let seconds = |time: FILETIME| {
            ((u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)) as f64
                / 10_000_000.0
        };
        let snapshot =
            unsafe { windows::core::Owned::new(CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)?) };
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut threads = 0;
        let mut border_workers = 0;
        let mut animation_workers = 0;
        let mut animation_scheduler_workers = 0;
        unsafe {
            Thread32First(*snapshot, &mut entry)?;
        }
        loop {
            if entry.th32OwnerProcessID == self.pid {
                threads += 1;
                if let Ok(thread) = unsafe {
                    OpenThread(THREAD_QUERY_LIMITED_INFORMATION, false, entry.th32ThreadID)
                } {
                    if let Ok(description) = unsafe { GetThreadDescription(thread) } {
                        let name = unsafe { description.to_string() }.unwrap_or_default();
                        unsafe {
                            LocalFree(Some(HLOCAL(description.0.cast())));
                        }
                        border_workers += u32::from(name == "border-worker");
                        animation_workers += u32::from(name == "border-animation");
                        animation_scheduler_workers +=
                            u32::from(name == "border-animation-scheduler");
                    }
                    unsafe {
                        CloseHandle(thread)?;
                    }
                }
            }
            if unsafe { Thread32Next(*snapshot, &mut entry) }.is_err() {
                break;
            }
        }
        let collect_status = unsafe { PdhCollectQueryData(self.query) };
        let values = self.counters.map(|counter| {
            if collect_status == 0 {
                Self::gpu_values(counter)
            } else {
                Err(collect_status)
            }
        });
        Ok(ResourceSample {
            handles,
            gdi: unsafe { GetGuiResources(process, GR_GDIOBJECTS) },
            threads,
            border_workers,
            animation_workers,
            animation_scheduler_workers,
            working_set: memory.WorkingSetSize,
            private_bytes: memory.PrivateUsage,
            cpu_seconds: seconds(kernel) + seconds(user),
            gpu_dedicated_bytes: values[0].as_ref().ok().map(|values| values.iter().sum()),
            gpu_shared_bytes: values[1].as_ref().ok().map(|values| values.iter().sum()),
            gpu_peak_engine_percent: values[2]
                .as_ref()
                .ok()
                .map(|values| values.iter().copied().fold(0.0, f64::max)),
            gpu_status: values.map(|result| result.err().unwrap_or(0)),
        })
    }
}

impl Drop for ResourceSampler {
    fn drop(&mut self) {
        unsafe {
            windows::Win32::System::Performance::PdhCloseQuery(self.query);
        }
    }
}

const ANIMATION_BENCHMARK_WINDOWS: usize = 64;
const ANIMATION_BENCHMARK_DURATION: Duration = Duration::from_secs(20);

#[test]
#[ignore = "manual before/after animation timer resource benchmark"]
#[serial]
fn many_animated_borders_resource_benchmark() -> anyhow::Result<()> {
    use std::io::{BufRead, Write};
    use std::os::windows::io::AsRawHandle;

    thread::scope(|scope| -> anyhow::Result<()> {
        let mut child = StressChild(
            std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "many_animated_borders_resource_benchmark_helper",
                    "--ignored",
                    "--nocapture",
                ])
                .env("TACKY_BORDERS_ANIMATION_BENCHMARK_CHILD", "1")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()?,
        );
        let output = child.0.stdout.take().unwrap();
        let (events, receiver) = mpsc::channel();
        scope.spawn(move || {
            for line in std::io::BufReader::new(output).lines() {
                let Ok(line) = line else {
                    break;
                };
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line)
                    && event.get("phase").is_some()
                {
                    if events.send(event).is_err() {
                        break;
                    }
                } else {
                    println!("{line}");
                }
            }
        });

        let mut sampler = ResourceSampler::for_process(child.0.id())?;
        let mut observations = Vec::new();
        let mut started_at = None;
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => {
                    let phase = event["phase"].as_str().unwrap();
                    match phase {
                        "ready" => {
                            observations.push(("ready", sampler.sample()?));
                        }
                        "measurement_start" => {
                            let sample = sampler.sample()?;
                            anyhow::ensure!(
                                sample.border_workers >= ANIMATION_BENCHMARK_WINDOWS as u32,
                                "benchmark did not start all border workers: {sample:?}"
                            );
                            observations.push(("measurement_start", sample));
                            started_at = Some(Instant::now());
                        }
                        "measurement_end" => {
                            observations.push(("measurement_end", sampler.sample()?));
                        }
                        phase => anyhow::bail!("unexpected benchmark phase: {phase}"),
                    }
                    child.0.stdin.as_mut().unwrap().write_all(b"continue\n")?;
                    if phase == "measurement_end" {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    anyhow::ensure!(
                        Instant::now() < deadline,
                        "animation benchmark exceeded external deadline"
                    );
                    if started_at.is_some() {
                        observations.push(("during_measurement", sampler.sample()?));
                    }
                    if let Some(status) = child.0.try_wait()? {
                        anyhow::bail!("animation benchmark helper exited early: {status}");
                    }
                    unsafe {
                        windows::Win32::System::Threading::WaitForSingleObject(
                            windows::Win32::Foundation::HANDLE(child.0.as_raw_handle()),
                            10,
                        );
                    }
                }
                Err(error) => anyhow::bail!("animation benchmark helper output ended: {error}"),
            }
        }

        let status = child.0.wait()?;
        anyhow::ensure!(
            status.success(),
            "animation benchmark helper failed: {status}"
        );
        let elapsed = started_at.unwrap().elapsed().as_secs_f64();
        let artifact = std::env::temp_dir().join(format!(
            "tacky-borders-animation-benchmark-{}.json",
            child.0.id()
        ));
        std::fs::write(
            &artifact,
            serde_json::to_vec_pretty(&serde_json::json!({
                "windows": ANIMATION_BENCHMARK_WINDOWS,
                "duration_seconds": elapsed,
                "observations": observations,
            }))?,
        )?;
        println!(
            "animation_benchmark_artifact={}; duration_seconds={elapsed}; observations={observations:?}",
            artifact.display()
        );
        Ok(())
    })
}

#[test]
#[ignore = "launched only by many_animated_borders_resource_benchmark"]
fn many_animated_borders_resource_benchmark_helper() -> anyhow::Result<()> {
    use std::io::Write;
    use windows::Win32::UI::WindowsAndMessaging::{
        SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos,
    };

    anyhow::ensure!(
        std::env::var_os("TACKY_BORDERS_ANIMATION_BENCHMARK_CHILD").is_some(),
        "benchmark helper must be launched by its parent test"
    );
    test_support::initialize();
    tacky_borders::register_border_window_class()?;
    let animations: tacky_borders::animations::AnimationsConfig =
        serde_json::from_value(serde_json::json!({
            "active": [{ "type": "Spiral" }],
            "inactive": [{ "type": "Spiral" }],
            "fps": 120,
            "enabled": true
        }))?;
    tacky_borders::APP_STATE.update_config(|config| {
        config.global.follow_native_border = false;
        config.global.animations = animations;
        config.global.border_z_order = tacky_borders::config::ZOrderMode::BelowWindow;
    })?;

    fn phase(phase: &str) -> anyhow::Result<()> {
        println!("{}", serde_json::json!({"phase": phase}));
        std::io::stdout().flush()?;
        let mut acknowledgment = String::new();
        std::io::stdin().read_line(&mut acknowledgment)?;
        anyhow::ensure!(
            acknowledgment.trim() == "continue",
            "missing benchmark supervisor acknowledgment"
        );
        Ok(())
    }

    fn pump_for(cohort: &mut Cohort, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            cohort.0.service_turn();
            let mut message = MSG::default();
            for _ in 0..64 {
                if !unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
                    break;
                }
                unsafe {
                    DispatchMessageW(&message);
                }
            }
            unsafe {
                MsgWaitForMultipleObjectsEx(None, 10, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            }
        }
    }

    let tracking = (0..ANIMATION_BENCHMARK_WINDOWS)
        .map(|_| TrackingWindow::new())
        .collect::<anyhow::Result<Vec<_>>>()?;
    for window in &tracking {
        unsafe {
            SetWindowPos(
                HWND(window.hwnd as _),
                None,
                -32000,
                -32000,
                100,
                100,
                SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
            )?;
        }
    }
    let mut cohort = Cohort::new();
    let instances = cohort.reload_many(&tracking);
    anyhow::ensure!(
        instances.len() == ANIMATION_BENCHMARK_WINDOWS,
        "not all benchmark border workers started"
    );

    pump_for(&mut cohort, Duration::from_secs(8));
    phase("ready")?;
    phase("measurement_start")?;
    pump_for(&mut cohort, ANIMATION_BENCHMARK_DURATION);
    phase("measurement_end")?;

    cohort.stop(OperationKind::Shutdown);
    let mut outcomes = std::collections::HashMap::new();
    while let Some((id, outcome)) = cohort.0.take_outcome() {
        assert!(outcomes.insert(id, outcome.clone()).is_none());
        assert_eq!(outcome, BorderWorkerExit::completed());
    }
    assert_eq!(outcomes.len(), instances.len());
    for (id, hwnd) in instances {
        assert!(!unsafe { IsWindow(Some(HWND(hwnd as _))) }.as_bool());
        assert!(outcomes.contains_key(&id));
    }
    for window in tracking {
        window.close()?;
    }
    Ok(())
}

#[test]
#[serial]
fn native_resource_sampler_observes_owned_thread_lifetime() -> anyhow::Result<()> {
    let mut sampler = ResourceSampler::new()?;
    let before = sampler.sample()?;
    let (running, after) = thread::scope(|scope| -> anyhow::Result<_> {
        let (stop, receiver) = mpsc::channel();
        let (ready, started) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("border-animation".into())
            .spawn_scoped(scope, move || {
                ready.send(()).unwrap();
                let _ = receiver.recv();
            })?;
        started.recv_timeout(Duration::from_secs(2))?;
        let running = sampler.sample()?;
        stop.send(())?;
        worker.join().unwrap();
        let after = sampler.sample()?;
        Ok((running, after))
    })?;
    println!("native_sampler_before={before:?}; running={running:?}; after={after:?}");
    assert_eq!(running.animation_workers, before.animation_workers + 1);
    assert_eq!(after.animation_workers, before.animation_workers);
    assert!(after.working_set > 0 && after.private_bytes > 0);
    assert!(after.cpu_seconds >= before.cpu_seconds);
    Ok(())
}

#[test]
#[serial]
fn repeated_native_cohorts_keep_process_handles_bounded() -> anyhow::Result<()> {
    use windows::Win32::System::Threading::{
        GR_GDIOBJECTS, GetCurrentProcess, GetGuiResources, GetProcessHandleCount,
    };
    fn cycle() -> anyhow::Result<()> {
        let tracking = TrackingWindow::new()?;
        let mut cohort = Cohort::new();
        let (id, hwnd) = cohort.reload(&tracking);
        cohort.stop(OperationKind::Shutdown);
        assert_eq!(
            cohort.0.take_outcome(),
            Some((id, BorderWorkerExit::completed()))
        );
        assert!(!unsafe { IsWindow(Some(HWND(hwnd as _))) }.as_bool());
        assert!(cohort.0.registry.lock().unwrap().entries.is_empty());
        drop(cohort);
        tracking.close()?;
        Ok(())
    }
    fn handle_count() -> anyhow::Result<u32> {
        let mut count = 0;
        unsafe {
            GetProcessHandleCount(GetCurrentProcess(), &mut count)?;
        }
        Ok(count)
    }
    tacky_borders::register_border_window_class()?;
    for _ in 0..3 {
        cycle()?;
    }
    let baseline = handle_count()?;
    let gdi_baseline = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
    let mut samples = Vec::with_capacity(32);
    let mut gdi_samples = Vec::with_capacity(32);
    for _ in 0..32 {
        cycle()?;
        samples.push(handle_count()?);
        gdi_samples.push(unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) });
    }
    println!("native_cohort_handle_baseline={baseline}; samples={samples:?}");
    println!("native_cohort_gdi_baseline={gdi_baseline}; samples={gdi_samples:?}");
    assert!(
        samples.iter().all(|count| *count <= baseline + 4),
        "process handles grew beyond bounded background variation: baseline={baseline}, samples={samples:?}"
    );
    assert!(
        gdi_samples.iter().all(|count| *count == gdi_baseline),
        "owned GDI resources did not return to baseline: baseline={gdi_baseline}, samples={gdi_samples:?}"
    );
    Ok(())
}

struct StressChild(std::process::Child);

impl Drop for StressChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[test]
#[serial]
fn sustained_animated_lifecycle_resources_are_bounded() -> anyhow::Result<()> {
    use std::io::{BufRead, Write};
    use std::os::windows::io::AsRawHandle;
    thread::scope(|scope| -> anyhow::Result<()> {
        let mut child = StressChild(
            std::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "animated_resource_stress_helper",
                    "--ignored",
                    "--nocapture",
                ])
                .env("FIX01_STRESS_CHILD", "1")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()?,
        );
        let output = child.0.stdout.take().unwrap();
        let (events, receiver) = mpsc::channel();
        scope.spawn(move || {
            for line in std::io::BufReader::new(output).lines() {
                let Ok(line) = line else {
                    break;
                };
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(&line)
                    && event.get("phase").is_some()
                {
                    if events.send(event).is_err() {
                        break;
                    }
                } else {
                    println!("{line}");
                }
            }
        });
        let mut sampler = ResourceSampler::for_process(child.0.id())?;
        let mut baseline = None;
        let mut active = None;
        let mut warm_samples = Vec::new();
        let mut samples = Vec::new();
        let mut started = None;
        let deadline = Instant::now() + Duration::from_secs(150);
        loop {
            while let Ok(event) = receiver.try_recv() {
                let mut sample = sampler.sample()?;
                match event["phase"].as_str().unwrap() {
                    "warm_active" => {
                        active = Some(sample);
                    }
                    "warm_retired" => {
                        warm_samples.push((active.take().unwrap(), sample));
                    }
                    "baseline" => {
                        for (active, retired) in &warm_samples {
                            sample.include_memory_peaks(active);
                            sample.include_memory_peaks(retired);
                        }
                        baseline = Some(sample);
                        started = Some(Instant::now());
                    }
                    "active" => {
                        active = Some(sample);
                    }
                    "retired" => {
                        samples.push((active.take().unwrap(), sample));
                    }
                    phase => anyhow::bail!("unexpected resource phase: {phase}"),
                }
                child.0.stdin.as_mut().unwrap().write_all(b"continue\n")?;
            }
            if let Some(status) = child.0.try_wait()? {
                anyhow::ensure!(
                    status.success(),
                    "animated resource helper failed: {status}"
                );
                break;
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "animated resource helper exceeded external deadline"
            );
            unsafe {
                windows::Win32::System::Threading::WaitForSingleObject(
                    windows::Win32::Foundation::HANDLE(child.0.as_raw_handle()),
                    100,
                );
            }
        }
        let baseline = baseline.unwrap();
        let elapsed = started.unwrap().elapsed().as_secs_f64();
        let artifact =
            std::env::temp_dir().join(format!("tacky-borders-fix01-stress-{}.json", child.0.id()));
        std::fs::write(
            &artifact,
            serde_json::to_vec_pretty(
                &serde_json::json!({"warmup_seconds": 30, "warmup_samples": warm_samples, "baseline": baseline, "elapsed_seconds": elapsed, "cycles": samples.len(), "samples": samples}),
            )?,
        )?;
        println!(
            "animated_stress_artifact={}; baseline={baseline:?}; cycles={}; elapsed_seconds={elapsed}",
            artifact.display(),
            samples.len()
        );
        assert!(samples.len() >= 64 && elapsed >= 60.0);
        for (active, retired) in warm_samples.iter().chain(&samples) {
            assert_eq!(
                active.border_workers, 1,
                "missing retained worker: {active:?}"
            );
            assert_eq!(
                active.animation_workers, 0,
                "per-border timer thread was spawned: {active:?}"
            );
            assert_eq!(
                active.animation_scheduler_workers, 1,
                "missing shared scheduler thread: {active:?}"
            );
            assert_eq!(
                retired.border_workers, 0,
                "border thread survived retirement: {retired:?}"
            );
            assert_eq!(
                retired.animation_workers, 0,
                "per-border timer thread survived retirement: {retired:?}"
            );
            assert_eq!(
                retired.animation_scheduler_workers, 1,
                "shared scheduler did not remain available: {retired:?}"
            );
        }
        for (_, retired) in &samples {
            assert!(
                retired.handles <= baseline.handles + 4,
                "handle growth: {retired:?}"
            );
            assert_eq!(retired.gdi, baseline.gdi, "GDI growth: {retired:?}");
            assert!(
                retired.threads <= baseline.threads + 2,
                "thread growth: {retired:?}"
            );
            assert!(
                retired.working_set <= baseline.working_set + 32 * 1024 * 1024,
                "working-set growth: {retired:?}"
            );
            assert!(
                retired.private_bytes <= baseline.private_bytes + 32 * 1024 * 1024,
                "private-byte growth: {retired:?}"
            );
            for (value, before) in [
                (retired.gpu_dedicated_bytes, baseline.gpu_dedicated_bytes),
                (retired.gpu_shared_bytes, baseline.gpu_shared_bytes),
            ] {
                if let (Some(value), Some(before)) = (value, before) {
                    assert!(
                        value <= before + (32 * 1024 * 1024) as f64,
                        "GPU memory growth: {retired:?}"
                    );
                }
            }
        }
        let final_sample = &samples.last().unwrap().1;
        let cpu_cores = (final_sample.cpu_seconds - baseline.cpu_seconds) / elapsed;
        println!("animated_stress_cpu_cores={cpu_cores}; final={final_sample:?}");
        assert!(cpu_cores < 2.0, "runaway fixture CPU usage: {cpu_cores}");
        Ok(())
    })
}

#[test]
#[ignore = "invoked only by the externally bounded resource owner"]
fn animated_resource_stress_helper() -> anyhow::Result<()> {
    use tacky_borders::border_registry::{BorderWorkerResult, DesiredVisibility};
    use tacky_borders::{APP_STATE, animations::AnimationsConfig, config::ZOrderMode};
    use windows::Win32::UI::WindowsAndMessaging::{
        IsWindowVisible, SW_SHOWMINNOACTIVE, SW_SHOWNOACTIVATE, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE,
        SWP_NOZORDER, SWP_SHOWWINDOW, SetWindowPos, ShowWindow,
    };
    anyhow::ensure!(std::env::var("FIX01_STRESS_CHILD")? == "1");
    test_support::initialize();
    tacky_borders::register_border_window_class()?;
    let animations: AnimationsConfig = serde_json::from_value(serde_json::json!({
        "active": [{ "type": "Spiral" }], "inactive": [{ "type": "Spiral" }], "fps": 30, "enabled": true
    }))?;
    APP_STATE.update_config(|config| {
        config.global.follow_native_border = false;
        config.global.animations = animations;
        config.global.border_z_order = ZOrderMode::BelowWindow;
    })?;
    fn phase(phase: &str) -> anyhow::Result<()> {
        use std::io::Write;
        println!("{}", serde_json::json!({"phase": phase}));
        std::io::stdout().flush()?;
        let mut acknowledgment = String::new();
        std::io::stdin().read_line(&mut acknowledgment)?;
        anyhow::ensure!(
            acknowledgment.trim() == "continue",
            "missing supervisor acknowledgment"
        );
        Ok(())
    }
    fn cycle(fault: bool, warm: bool) -> anyhow::Result<()> {
        let tracking = TrackingWindow::new()?;
        let tracking_hwnd = HWND(tracking.hwnd as _);
        unsafe {
            SetWindowPos(
                tracking_hwnd,
                None,
                -32000,
                -32000,
                100,
                100,
                SWP_ASYNCWINDOWPOS | SWP_NOACTIVATE | SWP_NOZORDER | SWP_SHOWWINDOW,
            )?;
        }
        let mut cohort = Cohort::new();
        cohort.pump_until("tracking window to become visible", |_| {
            unsafe { IsWindowVisible(tracking_hwnd) }.as_bool()
        });
        let (id, hwnd) = cohort.reload(&tracking);
        let border = HWND(hwnd as _);
        let control = cohort.0.registry.lock().unwrap().entries[&id]
            .control
            .clone();
        unsafe {
            let _ = ShowWindow(tracking_hwnd, SW_SHOWMINNOACTIVE);
        }
        control.set_visibility(DesiredVisibility::Hidden);
        cohort.pump_until("border to hide", |_| {
            !unsafe { IsWindowVisible(border) }.as_bool()
        });
        unsafe {
            let _ = ShowWindow(tracking_hwnd, SW_SHOWNOACTIVATE);
        }
        control.set_visibility(DesiredVisibility::Visible);
        cohort.pump_until("border to become visible", |_| {
            unsafe { IsWindowVisible(border) }.as_bool()
        });
        let frames_until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < frames_until {
            cohort.0.service_turn();
            unsafe {
                MsgWaitForMultipleObjectsEx(None, 10, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            }
        }
        phase(if warm { "warm_active" } else { "active" })?;
        if fault {
            control.record_callback_failure();
            cohort.pump_until("faulted worker to settle", |coordinator| {
                coordinator.is_settled()
            });
        } else {
            cohort.stop(OperationKind::Shutdown);
        }
        let (retired, outcome) = cohort.0.take_outcome().unwrap();
        assert_eq!(retired, id);
        if fault {
            assert!(
                matches!(outcome.primary, BorderWorkerResult::CallbackFailed),
                "unexpected simulated callback outcome: {outcome:?}"
            );
        } else {
            assert_eq!(outcome, BorderWorkerExit::completed());
        }
        assert!(outcome.cleanup_failures.is_empty());
        assert!(!unsafe { IsWindow(Some(border)) }.as_bool());
        assert!(cohort.0.registry.lock().unwrap().entries.is_empty());
        drop(cohort);
        tracking.close()?;
        unsafe {
            windows::Win32::Graphics::Dwm::DwmFlush()?;
        }
        phase(if warm { "warm_retired" } else { "retired" })?;
        Ok(())
    }
    let warming = Instant::now();
    let mut warm_cycles = 0;
    while warm_cycles < 5 || warming.elapsed() < Duration::from_secs(30) {
        cycle(warm_cycles % 2 == 0, true)?;
        warm_cycles += 1;
        if warm_cycles % 8 == 0 {
            println!(
                "animated_stress_warmup_cycles={warm_cycles}; elapsed={:?}",
                warming.elapsed()
            );
        }
    }
    phase("baseline")?;
    let started = Instant::now();
    let mut cycles = 0;
    while cycles < 64 || started.elapsed() < Duration::from_secs(60) {
        cycle(cycles % 8 == 0, false)?;
        cycles += 1;
        if cycles % 8 == 0 {
            println!(
                "animated_stress_cycles={cycles}; elapsed={:?}",
                started.elapsed()
            );
        }
    }
    Ok(())
}

#[test]
#[serial]
fn sixty_five_native_workers_retire_without_losing_owned_resources() -> anyhow::Result<()> {
    tacky_borders::register_border_window_class()?;
    let tracking = (0..65)
        .map(|_| TrackingWindow::new())
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut cohort = Cohort::new();
    let instances = cohort.reload_many(&tracking);
    let mailboxes = {
        let registry = cohort.0.registry.lock().unwrap();
        instances
            .iter()
            .map(|(id, _)| registry.entries[id].updates.clone())
            .collect::<Vec<_>>()
    };
    cohort.stop(OperationKind::Shutdown);
    let mut retired = std::collections::HashSet::new();
    while let Some((id, outcome)) = cohort.0.take_outcome() {
        assert_eq!(outcome, BorderWorkerExit::completed());
        assert!(retired.insert(id), "worker finalized twice");
    }
    assert_eq!(retired.len(), instances.len());
    for (id, hwnd) in instances {
        assert!(retired.contains(&id));
        assert!(!unsafe { IsWindow(Some(HWND(hwnd as _))) }.as_bool());
    }
    assert!(
        mailboxes
            .iter()
            .all(|mailbox| mailbox.is_closed() && mailbox.control().is_stopped())
    );
    for window in tracking {
        window.close()?;
    }
    Ok(())
}

#[test]
#[ignore = "invoked only by the owning cohort with its explicit HWND"]
fn foreign_message_helper() -> anyhow::Result<()> {
    use windows::Win32::Foundation::{GetLastError, SetLastError, WIN32_ERROR};
    use windows::Win32::UI::WindowsAndMessaging::{
        GWLP_USERDATA, SMTO_ABORTIFHUNG, SendMessageTimeoutW, SetWindowLongPtrW, WM_APP, WM_TIMER,
    };
    let hwnd = HWND(std::env::var("FIX01_TEST_OWNED_HWND")?.parse::<isize>()? as _);
    anyhow::ensure!(
        unsafe { IsWindow(Some(hwnd)) }.as_bool(),
        "test-owned border is missing"
    );
    let mut sends = 0;
    for public_data in [0, 1, 0x10001] {
        unsafe {
            SetLastError(WIN32_ERROR(0));
        }
        let previous = unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, public_data) };
        anyhow::ensure!(
            previous != 0 || unsafe { GetLastError() } == WIN32_ERROR(0),
            "could not overwrite public user data"
        );
        for message in [WM_APP + 10, WM_APP + 11, WM_APP + 12, WM_APP + 13] {
            for pointer in [0, 1, 3, isize::MAX] {
                let mut result = 0;
                let sent = unsafe {
                    SendMessageTimeoutW(
                        hwnd,
                        message,
                        WPARAM(usize::MAX),
                        LPARAM(pointer),
                        SMTO_ABORTIFHUNG,
                        1000,
                        Some(&mut result),
                    )
                };
                anyhow::ensure!(sent.0 != 0, "former payload send failed: {:?}", unsafe {
                    GetLastError()
                });
                sends += 1;
            }
        }
    }
    for message in [WM_CLOSE, WM_NCDESTROY, WM_SYSCOMMAND] {
        unsafe {
            PostMessageW(Some(hwnd), message, WPARAM(0xf060), LPARAM(1))?;
        }
    }
    let timer = unsafe { PostMessageW(Some(hwnd), WM_TIMER, WPARAM(1), LPARAM(1)) };
    println!("former_payload_sends={sends}; timer_post={timer:?}");
    let mut result = 0;
    let sent = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_APP + 10,
            WPARAM(0),
            LPARAM(1),
            SMTO_ABORTIFHUNG,
            1000,
            Some(&mut result),
        )
    };
    anyhow::ensure!(sent.0 != 0, "worker did not survive forged messages");
    Ok(())
}
