//! Run the benchmarked process in a kill-on-close job and sample its working set.
use std::io::{self, BufRead, BufReader, Read};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::{Child, Command, Stdio};
use std::ptr::null;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows_sys::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0};
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::WaitForSingleObject;

use super::report::Report;

fn job() -> io::Result<OwnedHandle> {
    // SAFETY: null security attributes and name create a new anonymous job.
    let handle = unsafe { CreateJobObjectW(null(), null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call above returned a new, owned handle.
    let job = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: job is live; the information class, pointer and size match.
    if unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            size_of_val(&info) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

fn free_memory() -> io::Result<u64> {
    let mut mem = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: initialized storage with its required length.
    if unsafe { GlobalMemoryStatusEx(&mut mem) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(mem.ullAvailPhys)
}

fn rss(child: &Child) -> io::Result<u64> {
    let size = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size,
        ..Default::default()
    };
    // SAFETY: the child's owned process handle is live, counters has the indicated size.
    if unsafe { GetProcessMemoryInfo(child.as_raw_handle(), &mut counters, size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(counters.WorkingSetSize as u64)
}

pub(super) fn monitor(
    command: &mut Command,
    seconds: u64,
    max_gb: f64,
    min_gb: f64,
) -> Result<(Report, String)> {
    let bytes = |gib| (gib * 1_073_741_824.) as u64;
    let free = free_memory()?;
    if free < bytes(min_gb) {
        let mut report = Report::default();
        report.sample(Duration::ZERO, None, free);
        return Ok((report, "minimum free system memory before launch".into()));
    }

    let job = job().context("creating benchmark job")?;
    let start = Instant::now();
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting benchmark process")?;
    // SAFETY: both handles remain owned for the duration of this call.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
        let error = io::Error::last_os_error();
        if child.try_wait()?.is_none() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("assigning benchmark process to job");
        }
    }
    let stderr = child.stderr.take().expect("stderr piped");
    let report = Mutex::new(Report::started());
    let stop = std::thread::scope(|scope| -> Result<String> {
        let reader = scope.spawn(|| -> io::Result<()> {
            let mut input = BufReader::new(stderr);
            let mut line = Vec::new();
            loop {
                line.clear();
                // An unterminated title diagnostic must not grow the harness without bound.
                const MAX_LINE: usize = 16 * 1024;
                if input
                    .by_ref()
                    .take(MAX_LINE as u64)
                    .read_until(b'\n', &mut line)?
                    == 0
                {
                    return Ok(());
                }
                if line.len() == MAX_LINE && !line.ends_with(b"\n") {
                    input.skip_until(b'\n')?;
                    continue;
                }
                if let Ok(text) = std::str::from_utf8(&line) {
                    report.lock().unwrap().event(text.trim_end());
                }
            }
        });
        let outcome = (|| -> Result<String> {
            let limit = Duration::from_secs(seconds);
            loop {
                if let Some(status) = child.try_wait()? {
                    return Ok(format!("process exit {}", status.code().unwrap_or(-1)));
                }
                let elapsed = start.elapsed();
                if elapsed >= limit {
                    return Ok("time limit".into());
                }
                let rss = match rss(&child) {
                    Ok(v) => v,
                    Err(_) if child.try_wait()?.is_some() => continue,
                    Err(error) => return Err(error.into()),
                };
                let free = free_memory()?;
                report.lock().unwrap().sample(elapsed, Some(rss), free);
                if rss > bytes(max_gb) {
                    return Ok("maximum process RSS".into());
                }
                if free < bytes(min_gb) {
                    return Ok("minimum free system memory".into());
                }
                let wait = limit
                    .saturating_sub(start.elapsed())
                    .min(Duration::from_secs(3));
                // SAFETY: a live process handle and a bound of three seconds.
                match unsafe { WaitForSingleObject(child.as_raw_handle(), wait.as_millis() as u32) }
                {
                    WAIT_FAILED => return Err(io::Error::last_os_error().into()),
                    WAIT_OBJECT_0 => {
                        return Ok(format!(
                            "process exit {}",
                            child.wait()?.code().unwrap_or(-1)
                        ));
                    }
                    _ => (),
                }
            }
        })();
        // Close the job before joining the reader. Descendants can hold the stderr pipe.
        drop(job);
        child.wait()?;
        reader.join().expect("benchmark log reader")?;
        report
            .lock()
            .unwrap()
            .sample(start.elapsed(), None, free_memory()?);
        outcome
    })?;
    Ok((report.into_inner().unwrap(), stop))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_title() {
        if let Some(mode) = std::env::var_os("VICTORY_BENCH_TEST_CHILD") {
            if mode == "wait" {
                std::thread::sleep(Duration::from_secs(30));
            }
            // A line larger than a pipe buffer must drain before the final diagnostic.
            eprintln!("{}", "x".repeat(128 * 1024));
            eprintln!("VIctory: unhandled exception 0x80000003 at synthetic.dll+0x4f");
            std::process::exit(7);
        }
    }

    fn fixture(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "bench::windows::tests::synthetic_title",
                "--nocapture",
            ])
            .env("VICTORY_BENCH_TEST_CHILD", mode);
        command
    }

    #[test]
    fn measures_native_child_and_applies_each_limit() {
        let (report, reason) = monitor(&mut fixture("exit"), 30, 8., 0.).unwrap();
        assert_eq!(reason, "process exit 7");
        use clap::Parser;
        let crate::Cmd::Bench(args) =
            crate::Cli::try_parse_from(["xtask", "bench", "--title", "example"])
                .unwrap()
                .command
        else {
            unreachable!()
        };
        assert!(
            report
                .markdown(&args, &reason, None)
                .contains("| crash RVA | 0x4f |")
        );
        assert_eq!(
            monitor(&mut fixture("wait"), 30, 0.000001, 0.).unwrap().1,
            "maximum process RSS"
        );
        assert_eq!(
            monitor(&mut fixture("wait"), 1, 8., 0.).unwrap().1,
            "time limit"
        );
        let (report, reason) = monitor(&mut fixture("wait"), 30, 8., 1048576.).unwrap();
        assert_eq!(reason, "minimum free system memory before launch");
        let markdown = report.markdown(&args, &reason, None);
        assert!(markdown.contains("| translation failures | unobserved |"));
    }
}
