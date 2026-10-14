// Benchmark fxdax's virtio-fs device, with DAX and without, against the guest's own filesystem:
// ext4 on a virtio-blk disk backed by a raw image file on the same host filesystem. libkrun's own
// virtio-fs device, passing a host directory through, measures the transport they share. The
// guest kernel's own Fxfs (fxdax's guest kernel, FXDAX_GUEST_KERNEL), on an extent-store device,
// with a DAX window and without, compares with ext4 booted on the same kernel.
//
// Each case boots an Alpine guest, with fio, a compiler and e2fsprogs, onto one filesystem
// mounted at /mnt/bench, and runs the same workloads on it several times: sequential and
// random reads and writes, cold and warm, fsync-heavy writes, I/O through mmap, metadata
// operations on many small files, and a build. The guest prints a measurement per workload
// and iteration, and the host reports their medians and spread. The workloads are sized for an
// iteration of each case to take well under a minute.
//
// The cases only run when FXDAX_BENCH_ITERATIONS is set. FXDAX_BENCH_GROUPS limits them to some
// of the groups of workloads: io, metadata and build, separated by commas. With
// FXDAX_BENCH_RESULTS set to a directory, the host also writes each case's measurements there as
// JSON, which scripts/bench-report.py turns into tables comparing the filesystems. With
// FXDAX_BENCH_PERF set to `record`, perf profiles each case's VMM into perf.data in its directory,
// or with `record:EVENTS` records the events, such as the scheduler's tracepoints, with stacks;
// set to `stat:EVENTS`, perf counts the events, such as KVM's tracepoints, each 100 ms of the run,
// into the case's log, where they line up with the guest's results.

use macros::{guest, host};

/// The filesystem a case benchmarks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// fxdax, with the guest mapping files into a DAX window.
    FxdaxDax,
    /// fxdax, with the guest caching files in its own page cache.
    FxdaxNoDax,
    /// ext4 on a virtio-blk disk.
    Ext4,
    /// libkrun's own virtio-fs device, passing a host directory through: the transport fxdax's
    /// device shares, without fxdax.
    Passthrough,
    /// ext4 on a virtio-blk disk, with fxdax's guest kernel, which the guest's Fxfs compares with.
    Ext4GuestKernel,
    /// The guest kernel's Fxfs on an extent-store device, with files in the page cache.
    FxfsGuest,
    /// The guest kernel's Fxfs on an extent-store device, mapping files from its DAX window.
    FxfsGuestDax,
}

impl Backend {
    /// Whether the case boots fxdax's guest kernel.
    #[cfg_attr(feature = "guest", allow(dead_code))]
    fn guest_kernel(self) -> bool {
        matches!(self, Backend::Ext4GuestKernel | Backend::FxfsGuest | Backend::FxfsGuestDax)
    }
}

pub struct TestFsBench {
    pub backend: Backend,
}

/// The size of the filesystems' images, which are sparse.
#[cfg_attr(feature = "guest", allow(dead_code))]
const IMAGE_SIZE: u64 = 8 << 30;

/// The source tree the build workload untars and builds.
#[cfg_attr(feature = "host", allow(dead_code))]
const SOURCE_TARBALL: &str = "/opt/lua-5.4.7.tar.gz";

#[host]
mod host {
    use super::*;

    use crate::common::{add_extent_store, set_guest_kernel, setup_rootfs};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup, krun_call, krun_call_u32};
    use futures::executor::block_on;
    use fxdax::{Filesystem, Storage};
    use krun_sys::*;
    use nix::libc;
    use std::collections::BTreeMap;
    use std::ffi::{CString, c_char};
    use std::os::unix::ffi::OsStrExt;
    use std::ptr::null;

    const CONTAINERFILE: &str = "\
FROM alpine:3.22
RUN apk add --no-cache fio gcc make musl-dev e2fsprogs
RUN wget -q -O /opt/lua-5.4.7.tar.gz https://www.lua.org/ftp/lua-5.4.7.tar.gz \\
    && echo '9fbf5e28ef86c69858f6d3d34eccc32e911c1a28b4120ff3e84aaa70cfbf1e30  /opt/lua-5.4.7.tar.gz' \\
    | sha256sum -c
";

    type KrunAddVirtiofsFxdaxFn = unsafe extern "C" fn(
        ctx_id: u32,
        c_tag: *const c_char,
        c_image_path: *const c_char,
        shm_size: u64,
    ) -> i32;

    fn get_krun_add_virtiofs_fxdax() -> KrunAddVirtiofsFxdaxFn {
        let symbol = CString::new("krun_add_virtiofs_fxdax").unwrap();
        let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, symbol.as_ptr()) };
        assert!(!ptr.is_null(), "krun_add_virtiofs_fxdax not found");
        unsafe { std::mem::transmute(ptr) }
    }

    fn iterations() -> Option<u32> {
        std::env::var("FXDAX_BENCH_ITERATIONS").ok()?.parse().ok()
    }

    /// A workload's measurements, one per iteration.
    #[derive(serde::Serialize)]
    struct Measurements {
        workload: String,
        unit: String,
        higher_is_better: bool,
        values: Vec<f64>,
    }

    impl Measurements {
        /// Returns the median, minimum and maximum.
        fn summary(&self) -> (f64, f64, f64) {
            let mut values = self.values.clone();
            values.sort_by(f64::total_cmp);
            let n = values.len();
            let median = if n % 2 == 1 {
                values[n / 2]
            } else {
                (values[n / 2 - 1] + values[n / 2]) / 2.0
            };
            (median, values[0], values[n - 1])
        }
    }

    #[derive(Default, serde::Serialize)]
    struct BenchReport {
        case: String,
        backend: String,
        host_os: &'static str,
        iterations: usize,
        results: Vec<Measurements>,
        /// How many iterations of each workload failed, which have no measurement.
        failures: BTreeMap<String, u32>,
        /// What the guest observed besides the workloads' measurements, per iteration.
        observations: BTreeMap<String, Vec<f64>>,
    }

    /// Parses the guest's `RESULT <workload> <unit> <higher|lower> <value>`, `FAILED <workload>`
    /// and `OBSERVED <name> <value>` lines.
    fn parse(stdout: &str, report: &mut BenchReport) -> anyhow::Result<()> {
        let results = &mut report.results;
        let mut index = BTreeMap::new();
        for line in stdout.lines() {
            if let Some(workload) = line.strip_prefix("FAILED ") {
                *report.failures.entry(workload.to_string()).or_default() += 1;
                continue;
            }
            if let Some(observed) = line.strip_prefix("OBSERVED ") {
                let Some((name, value)) = observed.split_once(' ') else {
                    anyhow::bail!("malformed observation {line:?}");
                };
                report
                    .observations
                    .entry(name.to_string())
                    .or_default()
                    .push(value.parse()?);
                continue;
            }
            let Some(result) = line.strip_prefix("RESULT ") else {
                continue;
            };
            let fields: Vec<&str> = result.split(' ').collect();
            let [workload, unit, better, value] = fields[..] else {
                anyhow::bail!("malformed result {line:?}");
            };
            let value: f64 = value.parse()?;
            let i = *index.entry(workload.to_string()).or_insert_with(|| {
                results.push(Measurements {
                    workload: workload.to_string(),
                    unit: unit.to_string(),
                    higher_is_better: better == "higher",
                    values: Vec::new(),
                });
                results.len() - 1
            });
            results[i].values.push(value);
        }
        anyhow::ensure!(!results.is_empty(), "no results");
        report.iterations = results.iter().map(|m| m.values.len()).max().unwrap_or(0);
        Ok(())
    }

    impl crate::ReportImpl for BenchReport {
        fn fmt_text(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let i = f.width().unwrap_or(0);
            writeln!(
                f,
                "{:i$}{} ({} iterations)\n",
                "", self.backend, self.iterations
            )?;
            writeln!(
                f,
                "{:i$}{:<22} {:>12} {:>12} {:>12}  unit",
                "", "workload", "median", "min", "max"
            )?;
            for m in &self.results {
                let (median, min, max) = m.summary();
                writeln!(
                    f,
                    "{:i$}{:<22} {:>12.1} {:>12.1} {:>12.1}  {}",
                    "", m.workload, median, min, max, m.unit
                )?;
            }
            for (workload, count) in &self.failures {
                writeln!(f, "{:i$}{workload} failed {count} times", "")?;
            }
            for (name, values) in &self.observations {
                writeln!(f, "{:i$}{name}: {values:?}", "")?;
            }
            Ok(())
        }

        fn fmt_gh_markdown(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            writeln!(f, "### {} ({} iterations)\n", self.backend, self.iterations)?;
            writeln!(f, "| Workload | Median | Min | Max | Unit |")?;
            write!(f, "|---|---|---|---|---|")?;
            for m in &self.results {
                let (median, min, max) = m.summary();
                write!(
                    f,
                    "\n| {} | {median:.1} | {min:.1} | {max:.1} | {} |",
                    m.workload, m.unit
                )?;
            }
            for (workload, count) in &self.failures {
                write!(f, "\n\n{workload} failed {count} times.")?;
            }
            Ok(())
        }
    }

    impl TestFsBench {
        fn name(&self) -> &'static str {
            match self.backend {
                Backend::FxdaxDax => "fxdax-dax",
                Backend::FxdaxNoDax => "fxdax-nodax",
                Backend::Ext4 => "ext4-virtio-blk",
                Backend::Passthrough => "virtiofs-passthrough",
                Backend::Ext4GuestKernel => "ext4-guest-kernel",
                Backend::FxfsGuest => "fxfs-guest",
                Backend::FxfsGuestDax => "fxfs-guest-dax",
            }
        }
    }

    impl Test for TestFsBench {
        fn should_run(&self) -> ShouldRun {
            if iterations().is_none() {
                return ShouldRun::No("FXDAX_BENCH_ITERATIONS not set");
            }
            if self.backend.guest_kernel() && std::env::var_os("FXDAX_GUEST_KERNEL").is_none() {
                return ShouldRun::No("FXDAX_GUEST_KERNEL does not name a guest kernel");
            }
            let feature = match self.backend {
                Backend::FxdaxDax
                | Backend::FxdaxNoDax
                | Backend::FxfsGuest
                | Backend::FxfsGuestDax => KRUN_FEATURE_FXDAX,
                Backend::Ext4 | Backend::Ext4GuestKernel => KRUN_FEATURE_BLK,
                Backend::Passthrough => return ShouldRun::Yes,
            };
            if unsafe { krun_call_u32!(krun_has_feature(feature.into())) }.ok() != Some(1) {
                return ShouldRun::No("libkrun compiled without the backend's feature");
            }
            ShouldRun::Yes
        }

        fn rootfs_image(&self) -> Option<&'static str> {
            Some(CONTAINERFILE)
        }

        fn timeout_secs(&self) -> u64 {
            1800 * u64::from(iterations().unwrap_or(1))
        }

        fn start_vm(self: Box<Self>, test_setup: TestSetup) -> anyhow::Result<()> {
            let root_dir = setup_rootfs(&test_setup)?;
            let c_root = CString::new(root_dir.as_os_str().as_bytes())?;
            let test_case = CString::new(test_setup.test_case.clone())?;
            let iterations = CString::new(format!(
                "FXDAX_BENCH_ITERATIONS={}",
                iterations().unwrap_or(1)
            ))?;
            let groups = CString::new(format!(
                "FXDAX_BENCH_GROUPS={}",
                std::env::var("FXDAX_BENCH_GROUPS").unwrap_or_default()
            ))?;

            // With FXDAX_BENCH_PERF set, this process, which becomes the VMM, runs again under
            // perf, which samples all its threads, its vCPUs', its devices' and fxdax's, or counts
            // events.
            let perf = std::env::var("FXDAX_BENCH_PERF").unwrap_or_default();
            if !perf.is_empty() && std::env::var_os("FXDAX_BENCH_PERF_RUNNING").is_none() {
                use std::os::unix::process::CommandExt;
                let mut command = std::process::Command::new("perf");
                match perf.strip_prefix("stat:") {
                    Some(events) => command.args(["stat", "-I", "100", "-e", events]),
                    None => {
                        let record = match perf.strip_prefix("record:") {
                            Some(events) => vec!["record", "-g", "-e", events],
                            None if perf == "record" => vec!["record", "-g", "-F", "999"],
                            None => anyhow::bail!(
                                "FXDAX_BENCH_PERF is record, record:EVENTS or stat:EVENTS"
                            ),
                        };
                        command
                            .args(record)
                            .arg("-o")
                            .arg(test_setup.tmp_dir.join("perf.data"))
                    }
                };
                let error = command
                    .arg("--")
                    .arg(std::env::current_exe()?)
                    .args(std::env::args_os().skip(1))
                    .env("FXDAX_BENCH_PERF_RUNNING", "1")
                    .exec();
                return Err(error.into());
            }

            unsafe {
                // Warnings only, whatever RUST_LOG says: logging every request costs time.
                krun_call!(krun_init_log(
                    KRUN_LOG_TARGET_DEFAULT,
                    KRUN_LOG_LEVEL_WARN,
                    KRUN_LOG_STYLE_NEVER,
                    KRUN_LOG_OPTION_NO_ENV,
                ))?;
                let ctx = krun_call_u32!(krun_create_ctx())?;
                krun_call!(krun_set_vm_config(ctx, 2, 2048))?;
                krun_call!(krun_set_root(ctx, c_root.as_ptr()))?;
                if self.backend.guest_kernel() {
                    set_guest_kernel(ctx)?;
                }
                match self.backend {
                    Backend::FxdaxDax | Backend::FxdaxNoDax => {
                        let image = test_setup.tmp_dir.join("fxdax.img");
                        block_on(async {
                            let fs =
                                Filesystem::format(Storage::create(&image, IMAGE_SIZE)?).await?;
                            fs.close().await?;
                            anyhow::Ok(())
                        })?;
                        let c_image = CString::new(image.as_os_str().as_bytes())?;
                        let dax = self.backend == Backend::FxdaxDax;
                        krun_call!(get_krun_add_virtiofs_fxdax()(
                            ctx,
                            c"bench".as_ptr(),
                            c_image.as_ptr(),
                            if dax { 1 << 31 } else { 0 },
                        ))?;
                    }
                    Backend::FxfsGuest | Backend::FxfsGuestDax => {
                        // A new store, which the guest's Fxfs formats as it mounts it.
                        let store = test_setup.tmp_dir.join("extents");
                        let window = if self.backend == Backend::FxfsGuestDax { 1 << 31 } else { 0 };
                        add_extent_store(ctx, c"bench", &store, IMAGE_SIZE, window)?;
                    }
                    Backend::Ext4 | Backend::Ext4GuestKernel => {
                        let image = test_setup.tmp_dir.join("ext4.img");
                        std::fs::File::create(&image)?.set_len(IMAGE_SIZE)?;
                        let c_image = CString::new(image.as_os_str().as_bytes())?;
                        // The defaults: the host's page cache, and flushes that reach the
                        // host's disk cache on macOS, as libkrun recommends.
                        krun_call!(krun_add_disk3(
                            ctx,
                            c"bench".as_ptr(),
                            c_image.as_ptr(),
                            KRUN_DISK_FORMAT_RAW,
                            false,
                            false,
                            KRUN_SYNC_RELAXED,
                        ))?;
                    }
                    Backend::Passthrough => {
                        let dir = test_setup.tmp_dir.join("passthrough");
                        std::fs::create_dir(&dir)?;
                        let c_dir = CString::new(dir.as_os_str().as_bytes())?;
                        krun_call!(krun_add_virtiofs(ctx, c"bench".as_ptr(), c_dir.as_ptr()))?;
                    }
                }
                krun_call!(krun_set_workdir(ctx, c"/".as_ptr()))?;
                let argv = [test_case.as_ptr(), null()];
                // The compiler finds the assembler and linker through PATH.
                let envp = [
                    iterations.as_ptr(),
                    groups.as_ptr(),
                    c"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".as_ptr(),
                    c"HOME=/root".as_ptr(),
                    null(),
                ];
                krun_call!(krun_set_exec(
                    ctx,
                    c"/guest-agent".as_ptr(),
                    argv.as_ptr(),
                    envp.as_ptr(),
                ))?;
                krun_call!(krun_start_enter(ctx))?;
            }
            unreachable!()
        }

        fn check(self: Box<Self>, stdout: Vec<u8>, test_setup: TestSetup) -> TestOutcome {
            let stdout = String::from_utf8_lossy(&stdout);
            if !stdout.lines().any(|line| line == "DONE") {
                return TestOutcome::Fail(format!("the guest did not finish: {stdout}"));
            }
            let mut report = BenchReport {
                case: test_setup.test_case.clone(),
                backend: self.name().to_string(),
                host_os: std::env::consts::OS,
                ..BenchReport::default()
            };
            if let Err(err) = parse(&stdout, &mut report) {
                return TestOutcome::Fail(format!("{err}: {stdout}"));
            }
            if let Some(dir) = std::env::var_os("FXDAX_BENCH_RESULTS") {
                let path = std::path::Path::new(&dir).join(format!("{}.json", report.case));
                let json = serde_json::to_string_pretty(&report).unwrap();
                if let Err(err) = std::fs::write(&path, json) {
                    return TestOutcome::Fail(format!("writing {}: {err}", path.display()));
                }
            }
            TestOutcome::Report(Box::new(report))
        }
    }
}

#[guest]
mod guest {
    use super::*;
    use crate::Test;

    use nix::libc;
    use std::ffi::CString;
    use std::fs;
    use std::io::Write;
    use std::process::Command;
    use std::time::Instant;

    const MNT: &str = "/mnt/bench";
    /// The file the I/O workloads use.
    const DATA: &str = "/mnt/bench/data";
    /// Small enough for fxdax's in-process kernel, of 1 GiB by default, to cache whole.
    const DATA_SIZE: u64 = 256 << 20;
    /// What the random I/O workloads that are not timed read or write, and the timed ones' time.
    const RANDOM_SIZE: u64 = 16 << 20;
    const RUNTIME: &str = "--runtime=2";
    const MIB: f64 = (1 << 20) as f64;
    /// The number of files the metadata workloads create, stat, list and remove.
    const FILES: usize = 2_000;
    /// The files created and fsynced one at a time.
    const FSYNCED: usize = 200;
    /// The tree's directories, and the files in each.
    const TREE_DIRS: usize = 50;
    const TREE_FILES: usize = 40;
    /// The files in the large directory.
    const LARGE_DIR: usize = 20_000;
    /// The threads of the concurrent workloads, each making `FILES` files.
    const THREADS: usize = 4;

    fn run(program: &str, args: &[&str]) {
        let status = Command::new(program)
            .args(args)
            .status()
            .unwrap_or_else(|err| panic!("{program}: {err}"));
        assert!(status.success(), "{program} {args:?}: {status}");
    }

    fn mount(source: &str, fstype: &str, options: &str) {
        fs::create_dir_all(MNT).unwrap();
        let source = CString::new(source).unwrap();
        let target = CString::new(MNT).unwrap();
        let fstype = CString::new(fstype).unwrap();
        let options = CString::new(options).unwrap();
        let ret = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                fstype.as_ptr(),
                0,
                options.as_ptr().cast(),
            )
        };
        assert_eq!(ret, 0, "mount: {}", std::io::Error::last_os_error());
    }

    /// Writes back and drops the guest's caches, so that the next reads miss them.
    fn drop_caches() {
        unsafe { libc::sync() };
        fs::write("/proc/sys/vm/drop_caches", b"3").unwrap();
    }

    fn result(workload: &str, unit: &str, higher_is_better: bool, value: f64) {
        let better = if higher_is_better { "higher" } else { "lower" };
        println!("RESULT {workload} {unit} {better} {value:.3}");
        std::io::stdout().flush().unwrap();
    }

    /// What fio measured of a job, and how long it took.
    struct Fio {
        /// The job's reads per second, and bytes read per second.
        read_iops: f64,
        read_bytes_per_sec: f64,
        /// From starting fio to its exit, including a write job's final fsync.
        secs: f64,
    }

    /// Runs fio on the data file.
    fn fio(args: &[&str]) -> Fio {
        let output = "/tmp/fio.json";
        let size = format!("--size={DATA_SIZE}");
        let mut all = vec![
            "--name=bench",
            "--filename",
            DATA,
            &size,
            // fio would drop the file's cached pages before each job: drop_caches makes the cold
            // jobs cold.
            "--invalidate=0",
            "--output-format=json",
            "--output",
            output,
        ];
        all.extend_from_slice(args);
        let start = Instant::now();
        run("fio", &all);
        let secs = start.elapsed().as_secs_f64();
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(output).unwrap()).expect("fio's JSON");
        let read = &json["jobs"][0]["read"];
        Fio {
            read_iops: read["iops"].as_f64().expect("read iops"),
            read_bytes_per_sec: read["bw_bytes"].as_f64().expect("read bw_bytes"),
            secs,
        }
    }

    /// Reports the rate of 4 KiB writes over `bytes` done in `secs`.
    fn write_iops(workload: &str, bytes: u64, secs: f64) {
        result(workload, "IOPS", true, (bytes / 4096) as f64 / secs);
    }

    /// Reads report fio's own rates, which leave out fio starting up. Writes are timed from
    /// starting fio to its exit, so that they include the fsync that makes them durable.
    fn io_workloads() {
        let job = fio(&["--rw=write", "--bs=1M", "--end_fsync=1"]);
        result(
            "seq-write",
            "MiB/s",
            true,
            DATA_SIZE as f64 / MIB / job.secs,
        );

        let random = format!("--io_size={RANDOM_SIZE}");
        drop_caches();
        let job = fio(&["--rw=randread", "--bs=4k", &random]);
        result("rand-read-cold", "IOPS", true, job.read_iops);
        drop_caches();
        let job = fio(&["--rw=read", "--bs=1M"]);
        result("seq-read-cold", "MiB/s", true, job.read_bytes_per_sec / MIB);
        // A warm read of the file takes milliseconds, which fio's runtimes would quantize, so
        // it reads for a while.
        let job = fio(&["--rw=read", "--bs=1M", "--time_based", RUNTIME]);
        result("seq-read-warm", "MiB/s", true, job.read_bytes_per_sec / MIB);
        let job = fio(&["--rw=randread", "--bs=4k", "--time_based", RUNTIME]);
        result("rand-read-warm", "IOPS", true, job.read_iops);

        let job = fio(&["--rw=randwrite", "--bs=4k", &random, "--end_fsync=1"]);
        write_iops("rand-write", RANDOM_SIZE, job.secs);

        let job = fio(&[
            "--ioengine=mmap",
            "--rw=randread",
            "--bs=4k",
            "--time_based",
            RUNTIME,
        ]);
        result("mmap-rand-read", "IOPS", true, job.read_iops);
        let job = fio(&[
            "--ioengine=mmap",
            "--rw=randwrite",
            "--bs=4k",
            &random,
            "--end_fsync=1",
        ]);
        write_iops("mmap-rand-write", RANDOM_SIZE, job.secs);
        fs::remove_file(DATA).unwrap();

        // A log: each 4 KiB append is fsynced.
        let log = format!("{MNT}/log");
        let appended = 1 << 20;
        let start = Instant::now();
        run(
            "fio",
            &[
                "--name=log",
                "--filename",
                &log,
                "--size=1M",
                "--rw=write",
                "--bs=4k",
                "--fsync=1",
                "--output=/dev/null",
            ],
        );
        write_iops("fsync-append", appended, start.elapsed().as_secs_f64());
        fs::remove_file(&log).unwrap();
    }

    /// Reports the rate of `count` operations done in `secs`.
    fn ops(workload: &str, count: usize, secs: f64) {
        result(workload, "ops/s", true, count as f64 / secs);
    }

    /// Times `f`, which does `count` operations, as `workload`.
    fn timed<T>(workload: &str, count: usize, f: impl FnOnce() -> T) -> T {
        let start = Instant::now();
        let value = f();
        ops(workload, count, start.elapsed().as_secs_f64());
        value
    }

    /// Runs `f(thread)` on `THREADS` threads at once, and times them all as `workload`, of `count`
    /// operations in all.
    fn parallel(workload: &str, count: usize, f: impl Fn(usize) + Sync) {
        timed(workload, count, || {
            std::thread::scope(|scope| {
                for thread in 0..THREADS {
                    let f = &f;
                    scope.spawn(move || f(thread));
                }
            })
        });
    }

    fn setxattr(path: &str, name: &str, value: &[u8]) {
        let path = CString::new(path).unwrap();
        let name = CString::new(name).unwrap();
        let ret = unsafe {
            libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0)
        };
        assert_eq!(ret, 0, "setxattr: {}", std::io::Error::last_os_error());
    }

    /// Operations on the names and attributes of many small files in one directory.
    fn files_workloads() {
        let dir = format!("{MNT}/meta");
        let other = format!("{MNT}/meta-other");
        fs::create_dir(&dir).unwrap();
        fs::create_dir(&other).unwrap();
        let paths: Vec<String> = (0..FILES).map(|i| format!("{dir}/file-{i}")).collect();

        timed("create", FILES, || {
            for path in &paths {
                fs::write(path, b"a small file").unwrap();
            }
        });
        drop_caches();
        timed("stat-cold", FILES, || {
            for path in &paths {
                fs::metadata(path).unwrap();
            }
        });
        timed("stat-warm", FILES, || {
            for path in &paths {
                fs::metadata(path).unwrap();
            }
        });
        timed("lookup-missing", FILES, || {
            for i in 0..FILES {
                assert!(fs::metadata(format!("{dir}/missing-{i}")).is_err());
            }
        });
        timed("readdir", FILES, || {
            assert_eq!(fs::read_dir(&dir).unwrap().count(), FILES);
        });
        drop_caches();
        timed("read-small-cold", FILES, || {
            for path in &paths {
                assert_eq!(fs::read(path).unwrap().len(), 12);
            }
        });
        timed("chmod", FILES, || {
            for path in &paths {
                fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                    .unwrap();
            }
        });
        timed("setxattr", FILES, || {
            for path in &paths {
                setxattr(path, "user.bench", b"a value");
            }
        });
        timed("link", FILES, || {
            for (i, path) in paths.iter().enumerate() {
                fs::hard_link(path, format!("{dir}/link-{i}")).unwrap();
            }
        });
        timed("rename", FILES, || {
            for i in 0..FILES {
                fs::rename(format!("{dir}/link-{i}"), format!("{dir}/renamed-{i}")).unwrap();
            }
        });
        timed("rename-across", FILES, || {
            for i in 0..FILES {
                fs::rename(format!("{dir}/renamed-{i}"), format!("{other}/moved-{i}")).unwrap();
            }
        });
        timed("unlink", FILES, || {
            for path in &paths {
                fs::remove_file(path).unwrap();
            }
        });
        for i in 0..FILES {
            fs::remove_file(format!("{other}/moved-{i}")).unwrap();
        }
        fs::remove_dir(&dir).unwrap();
        fs::remove_dir(&other).unwrap();
    }

    /// Small files created and fsynced one at a time, each with its directory, as a database or
    /// a mail server makes them durable.
    fn fsync_workloads() {
        let dir = format!("{MNT}/fsynced");
        fs::create_dir(&dir).unwrap();
        let handle = fs::File::open(&dir).unwrap();
        timed("create-fsync", FSYNCED, || {
            for i in 0..FSYNCED {
                let mut file = fs::File::create(format!("{dir}/file-{i}")).unwrap();
                file.write_all(b"a small file").unwrap();
                file.sync_all().unwrap();
                handle.sync_all().unwrap();
            }
        });
        run("rm", &["-rf", &dir]);
    }

    /// A tree of directories, made, walked cold, and removed.
    fn tree_workloads() {
        let root = format!("{MNT}/tree");
        let entries = TREE_DIRS * (TREE_FILES + 1);
        timed("mkdir-tree", entries, || {
            fs::create_dir(&root).unwrap();
            for d in 0..TREE_DIRS {
                let dir = format!("{root}/dir-{d}");
                fs::create_dir(&dir).unwrap();
                for f in 0..TREE_FILES {
                    fs::write(format!("{dir}/file-{f}"), b"x").unwrap();
                }
            }
        });
        drop_caches();
        timed("find-cold", entries, || {
            let mut found = 0;
            for dir in fs::read_dir(&root).unwrap() {
                let dir = dir.unwrap().path();
                fs::symlink_metadata(&dir).unwrap();
                for file in fs::read_dir(&dir).unwrap() {
                    fs::symlink_metadata(file.unwrap().path()).unwrap();
                    found += 1;
                }
                found += 1;
            }
            assert_eq!(found, entries);
        });
        timed("rm-tree", entries, || run("rm", &["-rf", &root]));
    }

    /// One large directory.
    fn large_dir_workloads() {
        let dir = format!("{MNT}/large");
        fs::create_dir(&dir).unwrap();
        timed("large-dir-create", LARGE_DIR, || {
            for i in 0..LARGE_DIR {
                fs::File::create(format!("{dir}/f{i}")).unwrap();
            }
        });
        drop_caches();
        timed("large-dir-stat-cold", LARGE_DIR, || {
            for i in 0..LARGE_DIR {
                fs::metadata(format!("{dir}/f{i}")).unwrap();
            }
        });
        timed("large-dir-unlink", LARGE_DIR, || {
            for i in 0..LARGE_DIR {
                fs::remove_file(format!("{dir}/f{i}")).unwrap();
            }
        });
        fs::remove_dir(&dir).unwrap();
    }

    /// `THREADS` threads creating, statting and removing files at once: each in a directory of its
    /// own, and all in one.
    fn concurrent_workloads() {
        let count = THREADS * FILES;
        let own = |thread: usize| format!("{MNT}/thread-{thread}");
        for thread in 0..THREADS {
            fs::create_dir(own(thread)).unwrap();
        }
        parallel("create-parallel", count, |thread| {
            for i in 0..FILES {
                fs::File::create(format!("{}/f{i}", own(thread))).unwrap();
            }
        });
        parallel("stat-parallel", count, |thread| {
            for i in 0..FILES {
                fs::metadata(format!("{}/f{i}", own(thread))).unwrap();
            }
        });
        parallel("unlink-parallel", count, |thread| {
            for i in 0..FILES {
                fs::remove_file(format!("{}/f{i}", own(thread))).unwrap();
            }
        });
        for thread in 0..THREADS {
            fs::remove_dir(own(thread)).unwrap();
        }

        let shared = format!("{MNT}/shared");
        fs::create_dir(&shared).unwrap();
        parallel("create-shared-dir", count, |thread| {
            for i in 0..FILES {
                fs::File::create(format!("{shared}/t{thread}-f{i}")).unwrap();
            }
        });
        parallel("unlink-shared-dir", count, |thread| {
            for i in 0..FILES {
                fs::remove_file(format!("{shared}/t{thread}-f{i}")).unwrap();
            }
        });
        fs::remove_dir(&shared).unwrap();
    }

    fn metadata_workloads() {
        files_workloads();
        fsync_workloads();
        tree_workloads();
        large_dir_workloads();
        concurrent_workloads();
    }

    /// Observes how far ahead of the guest's clock a file it writes is timestamped, in
    /// milliseconds: what `make` compares the guest's clock with.
    fn observe_mtime() {
        let path = format!("{MNT}/mtime");
        fs::write(&path, b"now").unwrap();
        let now = std::time::SystemTime::now();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let ahead = match modified.duration_since(now) {
            Ok(ahead) => ahead.as_secs_f64(),
            Err(behind) => -behind.duration().as_secs_f64(),
        };
        println!("OBSERVED mtime-ahead-of-guest-ms {:.3}", ahead * 1000.0);
        fs::remove_file(&path).unwrap();
    }

    /// Untars and builds Lua: small files, short-lived processes and a compiler.
    fn build_workload() {
        let dir = format!("{MNT}/build");
        fs::create_dir(&dir).unwrap();
        let source = format!("{dir}/lua-5.4.7");
        let timed = |workload: &str, program: &str, args: &[&str], cwd: &str| {
            let start = Instant::now();
            let status = Command::new(program)
                .args(args)
                .current_dir(cwd)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap_or_else(|err| panic!("{program}: {err}"));
            assert!(status.success(), "{program} {args:?}: {status}");
            let secs = start.elapsed().as_secs_f64();
            result(workload, "s", false, secs);
            secs
        };
        let total = timed("untar", "tar", &["-xzf", SOURCE_TARBALL], &dir)
            + timed("make", "make", &["-j2", "linux"], &source);
        result("build-total", "s", false, total);
        run("rm", &["-rf", &dir]);
    }

    /// Prints the kernel's messages, from /dev/kmsg, to say what a filesystem logged when a
    /// workload fails.
    fn print_kernel_log() {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let Ok(mut kmsg) = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/kmsg")
        else {
            return;
        };
        let mut record = [0u8; 8192];
        // Each read is a record, until there are no more.
        while let Ok(n) = kmsg.read(&mut record) {
            eprint!("kernel: {}", String::from_utf8_lossy(&record[..n]));
        }
    }

    impl Test for TestFsBench {
        fn in_guest(self: Box<Self>) {
            let default_hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                default_hook(info);
                print_kernel_log();
            }));
            match self.backend {
                Backend::FxdaxDax => mount("bench", "virtiofs", "dax=always"),
                Backend::FxdaxNoDax | Backend::Passthrough => {
                    mount("bench", "virtiofs", "dax=never")
                }
                Backend::Ext4 | Backend::Ext4GuestKernel => {
                    run("mkfs.ext4", &["-q", "-F", "/dev/vda"]);
                    mount("/dev/vda", "ext4", "");
                }
                Backend::FxfsGuest | Backend::FxfsGuestDax => mount("bench", "fxfs", ""),
            }
            let iterations: u32 = std::env::var("FXDAX_BENCH_ITERATIONS")
                .ok()
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
            let groups = std::env::var("FXDAX_BENCH_GROUPS").unwrap_or_default();
            let runs = |group: &str| groups.is_empty() || groups.split(',').any(|g| g == group);
            for i in 0..iterations {
                eprintln!("iteration {}/{iterations}", i + 1);
                observe_mtime();
                if runs("io") {
                    io_workloads();
                }
                if runs("metadata") {
                    metadata_workloads();
                }
                // A failed build is recorded, and the other iterations still run.
                if runs("build") && std::panic::catch_unwind(build_workload).is_err() {
                    println!("FAILED build");
                    run("rm", &["-rf", &format!("{MNT}/build")]);
                }
                drop_caches();
            }
            println!("DONE");
        }
    }
}
