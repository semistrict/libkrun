// Benchmark the guest kernel's own Fxfs (fxdax's guest kernel, FXDAX_GUEST_KERNEL), on an
// extent-store device that fxdax's extent daemon serves, with a DAX window and without, against
// ext4 booted on the same kernel, on virtio-blk, and with DAX on writable virtio-pmem, where file
// data and metadata are stored in place in memory; and the default kernel's ext4 on a virtio-blk
// disk backed by a raw image file on the same host filesystem, and libkrun's own virtio-fs device,
// passing a host directory through.
//
// Each case boots an Alpine guest, with fio, a compiler and e2fsprogs, onto one filesystem
// mounted at /mnt/bench, and runs the same workloads on it several times: sequential and
// random reads and writes, cold and warm, fsync-heavy writes, I/O through mmap, metadata
// operations on many small files, and a build. The guest prints a measurement per workload
// and iteration, and the host reports their medians and spread. The workloads are sized for an
// iteration of each case to take well under a minute.
//
// The cases only run when FXDAX_BENCH_ITERATIONS is set. FXDAX_BENCH_GROUPS limits them to some
// of the groups of workloads: io, metadata and build, separated by commas, or parts of metadata:
// files, fsync, tree, large-dir and concurrent; pgbench, PostgreSQL's benchmark, runs only when it
// names it. FXDAX_BENCH_FILES sets how many files the metadata workloads make (2000), and
// FXDAX_BENCH_PGBENCH_SECS how long pgbench's transactions run (20). With
// FXDAX_BENCH_RESULTS set to a directory, the host also writes each case's measurements there as
// JSON, which scripts/bench-report.py turns into tables comparing the filesystems. With
// FXDAX_BENCH_PERF set to `record`, perf profiles each case's VMM into perf.data in its directory
// (`record-all`, the whole host, fxdax's extent daemon too),
// or with `record:EVENTS` records the events, such as the scheduler's tracepoints, with stacks;
// set to `stat:EVENTS`, perf counts the events, such as KVM's tracepoints, each 100 ms of the run,
// into the case's log, where they line up with the guest's results. With FXDAX_BENCH_GUEST_PERF
// set, perf in the guest samples its kernel through each metadata workload, and the case's log
// says where each one's time went; set to `record:EVENTS`, it samples the events instead.
//
// fxdax: the fork cases measure what a VM booted on a fork of a base image pays to start and run,
// on the base's data, and what the extent daemon having that data in memory already saves it. A
// base VM, on a store of its own, copies the guest's /usr there and makes a PostgreSQL database
// (pgbench -i), once. Then each iteration forks the base through the daemon (its FORK request)
// and boots a VM on the fork, which mounts it, reads the tree whole, and starts PostgreSQL on the
// database and runs pgbench's read-only transactions. The fork is served by the daemon that served
// the base, whose extent cache holds the base's data (warm), or by a daemon of its own, started
// for it, whose cache holds nothing (cold): FXDAX_EXTENTD_BIN names the daemon's binary. The host
// reports the daemon's counters of what serving the fork cost, as observations.

use macros::{guest, host};

/// The filesystem a case benchmarks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// ext4 on a virtio-blk disk.
    Ext4,
    /// libkrun's own virtio-fs device, passing a host directory through.
    Passthrough,
    /// ext4 on a virtio-blk disk, with fxdax's guest kernel, which the guest's Fxfs compares with.
    Ext4GuestKernel,
    /// ext4 with DAX on writable virtio-pmem, with fxdax's guest kernel: storage as memory.
    Ext4DaxPmem,
    /// The guest kernel's Fxfs on an extent-store device, with files in the page cache.
    FxfsGuest,
    /// The guest kernel's Fxfs on an extent-store device, mapping files from its DAX window.
    FxfsGuestDax,
    /// fxdax: as `FxfsGuestDax`, booted on a fork of a base image whose data the extent daemon
    /// serving it has in memory already: the daemon that served the base.
    FxfsGuestDaxForkWarm,
    /// fxdax: as `FxfsGuestDaxForkWarm`, but served by a daemon of its own, which has nothing of
    /// the base's in memory.
    FxfsGuestDaxForkCold,
}

impl Backend {
    /// Whether the case boots fxdax's guest kernel.
    #[cfg_attr(feature = "guest", allow(dead_code))]
    fn guest_kernel(self) -> bool {
        matches!(
            self,
            Backend::Ext4GuestKernel
                | Backend::Ext4DaxPmem
                | Backend::FxfsGuest
                | Backend::FxfsGuestDax
                | Backend::FxfsGuestDaxForkWarm
                | Backend::FxfsGuestDaxForkCold
        )
    }

    /// Whether the case boots VMs on forks of a base image.
    fn forks(self) -> bool {
        matches!(self, Backend::FxfsGuestDaxForkWarm | Backend::FxfsGuestDaxForkCold)
    }
}

/// fxdax: what a VM of a fork case is: the base, or a fork (`FXDAX_FORK_BENCH_ROLE`).
#[cfg_attr(feature = "host", allow(dead_code))]
const ROLE: &str = "FXDAX_FORK_BENCH_ROLE";

/// fxdax: the store of a fork case's VM, the base's or a fork's.
#[cfg_attr(feature = "guest", allow(dead_code))]
const STORE: &str = "FXDAX_FORK_BENCH_STORE";

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

    use crate::common::{fxdax_guest_should_run, setup_rootfs};
    use crate::vm::{Vm, Vmm};
    use crate::{ShouldRun, Test, TestOutcome, TestSetup, krun_call_u32};
    use krun_sys::*;
    use anyhow::Context as _;
    use std::collections::BTreeMap;
    use std::ffi::CString;

    const CONTAINERFILE: &str = "\
FROM alpine:3.22
RUN apk add --no-cache fio gcc make musl-dev e2fsprogs perf postgresql postgresql-contrib
RUN wget -q -O /opt/lua-5.4.7.tar.gz https://www.lua.org/ftp/lua-5.4.7.tar.gz \\
    && echo '9fbf5e28ef86c69858f6d3d34eccc32e911c1a28b4120ff3e84aaa70cfbf1e30  /opt/lua-5.4.7.tar.gz' \\
    | sha256sum -c
";

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
        /// The VMM: libkrun or Firecracker.
        vmm: &'static str,
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

    /// fxdax: the fork cases' VMs, each run as a process of its own: this program again, as the
    /// role it is given.
    mod forks {
        use super::*;
        use std::path::{Path, PathBuf};
        use std::process::{Child, Command};
        use std::time::{Duration, Instant};

        /// Runs the base's VM, then, each iteration, forks the base and runs a VM on the fork, and
        /// reports what the daemon serving the fork counted.
        pub fn run(backend: Backend, test_setup: &TestSetup) -> anyhow::Result<()> {
            let extentd = PathBuf::from(std::env::var_os("FXDAX_EXTENTD").context("FXDAX_EXTENTD")?);
            let base = test_setup.tmp_dir.join("base");
            role(test_setup, "base", &base, &extentd)?;
            for i in 0..iterations().unwrap_or(1) {
                let fork = test_setup.tmp_dir.join(format!("fork-{i}"));
                {
                    let store = opened(&extentd, &base)?;
                    store
                        .fork(&fork, store.branch() + 1 + u64::from(i))
                        .map_err(|errno| anyhow::anyhow!("forking the base: {errno}"))?;
                }
                // A socket's path is short: the daemon's directory is a temporary one of its own.
                let sockets = tempdir::TempDir::new("fxdax-fork")?;
                let (socket, daemon) = match backend {
                    Backend::FxfsGuestDaxForkCold => {
                        let (socket, daemon) = daemon(test_setup, sockets.path(), i)?;
                        (socket, Some(daemon))
                    }
                    _ => (extentd.clone(), None),
                };
                let before = fxdax::extentd::stats(&socket)?;
                role(test_setup, "fork", &fork, &socket)?;
                let counted = fxdax::extentd::stats(&socket)?.since(&before);
                for (name, value) in counted.named() {
                    println!("OBSERVED extentd-{name} {value}");
                }
                if let Some(mut daemon) = daemon {
                    daemon.kill()?;
                    daemon.wait()?;
                }
            }
            println!("DONE");
            Ok(())
        }

        /// Runs this program again, as `role`'s VM, on the store at `store`, which the daemon at
        /// `extentd` serves, and waits for it: its output is this one's.
        fn role(test_setup: &TestSetup, role: &str, store: &Path, extentd: &Path) -> anyhow::Result<()> {
            let status = Command::new(std::env::current_exe()?)
                .args(std::env::args_os().skip(1))
                .env(ROLE, role)
                .env(STORE, store)
                .env("FXDAX_EXTENTD", extentd)
                .status()?;
            anyhow::ensure!(status.success(), "the {role}'s VM in {}: {status}", test_setup.tmp_dir.display());
            Ok(())
        }

        /// The store at `store`, which the daemon at `extentd` serves: once the daemon has seen the
        /// VM that had it open go.
        fn opened(extentd: &Path, store: &Path) -> anyhow::Result<fxdax::RemoteExtentStore> {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                match fxdax::RemoteExtentStore::connect(extentd, store, IMAGE_SIZE) {
                    Ok(store) => return Ok(store),
                    Err(err)
                        if Instant::now() < deadline
                            && err.errno() == Some(fxdax::Errno::EBUSY) =>
                    {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(err) => return Err(anyhow::anyhow!("{err:#}")),
                }
            }
        }

        /// A daemon of its own for iteration `i`'s fork, FXDAX_EXTENTD_BIN, and its socket in `dir`,
        /// once it serves there.
        fn daemon(test_setup: &TestSetup, dir: &Path, i: u32) -> anyhow::Result<(PathBuf, Child)> {
            let bin = std::env::var_os("FXDAX_EXTENTD_BIN").context("FXDAX_EXTENTD_BIN")?;
            let socket = dir.join("extentd.sock");
            let log = std::fs::File::create(test_setup.tmp_dir.join(format!("extentd-{i}.log")))?;
            let daemon = Command::new(bin).arg(&socket).stderr(log).spawn()?;
            test_setup.register_cleanup_pid(daemon.id());
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                match fxdax::extentd::stats(&socket) {
                    Ok(_) => return Ok((socket, daemon)),
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(err) => return Err(anyhow::anyhow!("the fork's daemon: {err:#}")),
                }
            }
        }
    }

    impl TestFsBench {
        fn name(&self) -> &'static str {
            match self.backend {
                Backend::Ext4 => "ext4-virtio-blk",
                Backend::Passthrough => "virtiofs-passthrough",
                Backend::Ext4GuestKernel => "ext4-guest-kernel",
                Backend::Ext4DaxPmem => "ext4-dax-pmem",
                Backend::FxfsGuest => "fxfs-guest",
                Backend::FxfsGuestDax => "fxfs-guest-dax",
                Backend::FxfsGuestDaxForkWarm => "fxfs-guest-dax-fork-warm",
                Backend::FxfsGuestDaxForkCold => "fxfs-guest-dax-fork-cold",
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
            let vmm = Vmm::from_env();
            if self.backend == Backend::FxfsGuestDaxForkCold
                && std::env::var_os("FXDAX_EXTENTD_BIN").is_none()
            {
                return ShouldRun::No("FXDAX_EXTENTD_BIN does not name the extent daemon's binary");
            }
            let feature = match self.backend {
                Backend::FxfsGuest
                | Backend::FxfsGuestDax
                | Backend::FxfsGuestDaxForkWarm
                | Backend::FxfsGuestDaxForkCold => return fxdax_guest_should_run(),
                Backend::Ext4 | Backend::Passthrough if vmm == Vmm::Firecracker => {
                    return ShouldRun::No("Firecracker boots fxdax's guest kernel only");
                }
                _ if vmm == Vmm::Firecracker => return ShouldRun::Yes,
                Backend::Ext4 | Backend::Ext4GuestKernel => KRUN_FEATURE_BLK,
                Backend::Passthrough | Backend::Ext4DaxPmem => return ShouldRun::Yes,
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
            // A fork case runs its VMs, each a process of its own, from here.
            if self.backend.forks() && std::env::var_os(ROLE).is_none() {
                return forks::run(self.backend, &test_setup);
            }
            let root_dir = setup_rootfs(&test_setup)?;
            let iterations = CString::new(format!(
                "FXDAX_BENCH_ITERATIONS={}",
                iterations().unwrap_or(1)
            ))?;
            let groups = CString::new(format!(
                "FXDAX_BENCH_GROUPS={}",
                std::env::var("FXDAX_BENCH_GROUPS").unwrap_or_default()
            ))?;
            let guest_perf = CString::new(format!(
                "FXDAX_BENCH_GUEST_PERF={}",
                std::env::var("FXDAX_BENCH_GUEST_PERF").unwrap_or_default()
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
                            // The whole host, fxdax's extent daemon included.
                            None if perf == "record-all" => {
                                vec!["record", "-a", "-g", "-F", "999"]
                            }
                            None => anyhow::bail!(
                                "FXDAX_BENCH_PERF is record, record-all, record:EVENTS or \
                                 stat:EVENTS"
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

            let mut vm = Vm::new(2, 2048)?;
            if self.backend.guest_kernel() {
                vm.guest_kernel()?;
            }
            // The filesystem's device in the guest, for those on one.
            let device = match self.backend {
                Backend::FxfsGuest | Backend::FxfsGuestDax => {
                    // A new store, which the guest's Fxfs formats as it mounts it.
                    let store = test_setup.tmp_dir.join("extents");
                    let window = if self.backend == Backend::FxfsGuestDax { 1 << 31 } else { 0 };
                    vm.extent_store("bench", &store, IMAGE_SIZE, window)?;
                    None
                }
                // The base's store, new, or a fork of it.
                Backend::FxfsGuestDaxForkWarm | Backend::FxfsGuestDaxForkCold => {
                    let store = std::env::var_os(STORE).context("no store for a fork case")?;
                    vm.extent_store("bench", std::path::Path::new(&store), IMAGE_SIZE, 1 << 31)?;
                    vm.env(&CString::new(format!("{ROLE}={}", std::env::var(ROLE)?))?);
                    None
                }
                Backend::Ext4DaxPmem => {
                    let image = test_setup.tmp_dir.join("pmem.img");
                    std::fs::File::create(&image)?.set_len(IMAGE_SIZE)?;
                    Some(vm.pmem("bench", &image)?)
                }
                Backend::Ext4 | Backend::Ext4GuestKernel => {
                    let image = test_setup.tmp_dir.join("ext4.img");
                    std::fs::File::create(&image)?.set_len(IMAGE_SIZE)?;
                    Some(vm.disk("bench", &image)?)
                }
                Backend::Passthrough => {
                    let dir = test_setup.tmp_dir.join("passthrough");
                    std::fs::create_dir(&dir)?;
                    vm.virtiofs("bench", &dir)?;
                    None
                }
            };
            if let Some(device) = device {
                vm.env(&CString::new(format!("FXDAX_BENCH_DEVICE={device}"))?);
            }
            vm.env(&iterations);
            vm.env(&groups);
            vm.env(&guest_perf);
            for var in ["FXDAX_BENCH_FILES", "FXDAX_BENCH_PGBENCH_SECS", "FXDAX_BENCH_WATCHDOG_SECS"] {
                if let Ok(value) = std::env::var(var) {
                    vm.env(&CString::new(format!("{var}={value}"))?);
                }
            }
            // The compiler finds the assembler and linker through PATH.
            vm.env(c"PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
            vm.env(c"HOME=/root");
            vm.enter(&root_dir, &test_setup.test_case)
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
                vmm: match Vmm::from_env() {
                    Vmm::Libkrun => "libkrun",
                    Vmm::Firecracker => "firecracker",
                },
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
    /// Small enough for fxdax's extent daemon, of 1 GiB by default, to cache whole.
    const DATA_SIZE: u64 = 256 << 20;
    /// What the random I/O workloads that are not timed read or write, and the timed ones' time.
    /// What the random writes write: 16384 writes, which take seconds, as their time is measured
    /// to a quarter of a second (fio's exit).
    const RANDOM_SIZE: u64 = 64 << 20;
    const RUNTIME: &str = "--runtime=2";
    const MIB: f64 = (1 << 20) as f64;
    /// The number of files the metadata workloads create, stat, list and remove: FXDAX_BENCH_FILES,
    /// or 2000, whose workloads take milliseconds.
    fn files() -> usize {
        std::env::var("FXDAX_BENCH_FILES").ok().and_then(|n| n.parse().ok()).unwrap_or(2_000)
    }
    /// The files created and fsynced one at a time.
    const FSYNCED: usize = 200;
    /// The tree's directories, and the files in each.
    const TREE_DIRS: usize = 50;
    const TREE_FILES: usize = 40;
    /// The files in the large directory.
    const LARGE_DIR: usize = 20_000;
    /// The threads of the concurrent workloads, each making `files()` files.
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

    /// The device of the filesystem the host made, which it names.
    fn bench_device() -> String {
        std::env::var("FXDAX_BENCH_DEVICE").expect("FXDAX_BENCH_DEVICE names the device")
    }

    /// Writes back and drops the guest's caches, so that the next reads miss them.
    fn drop_caches() {
        unsafe { libc::sync() };
        fs::write("/proc/sys/vm/drop_caches", b"3").unwrap();
    }

    /// When the last result was reported, in seconds since the epoch: what the watchdog watches.
    static PROGRESS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// Watches for a workload making no progress for FXDAX_BENCH_WATCHDOG_SECS (180): then the
    /// kernel's blocked tasks' stacks (sysrq w), and its log, go to standard error, once, so that
    /// a hang says where.
    fn start_watchdog() {
        let limit: u64 = std::env::var("FXDAX_BENCH_WATCHDOG_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(180);
        PROGRESS.store(now_secs(), std::sync::atomic::Ordering::Relaxed);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(5));
                let last = PROGRESS.load(std::sync::atomic::Ordering::Relaxed);
                if now_secs().saturating_sub(last) >= limit {
                    eprintln!("WATCHDOG: no progress for {limit} s: blocked tasks follow");
                    let _ = fs::write("/proc/sysrq-trigger", b"w");
                    crate::kernel_log::print();
                    return;
                }
            }
        });
    }

    fn result(workload: &str, unit: &str, higher_is_better: bool, value: f64) {
        PROGRESS.store(now_secs(), std::sync::atomic::Ordering::Relaxed);
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
        let blocks = (DATA_SIZE / 4096) as usize;
        let random_blocks = (RANDOM_SIZE / 4096) as usize;
        let job = observed("seq-write", blocks, || fio(&["--rw=write", "--bs=1M", "--end_fsync=1"]));
        result(
            "seq-write",
            "MiB/s",
            true,
            DATA_SIZE as f64 / MIB / job.secs,
        );

        let random = format!("--io_size={RANDOM_SIZE}");
        drop_caches();
        let job = observed("rand-read-cold", random_blocks, || {
            fio(&["--rw=randread", "--bs=4k", &random])
        });
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

        let job = observed("rand-write", random_blocks, || {
            fio(&["--rw=randwrite", "--bs=4k", &random, "--end_fsync=1"])
        });
        write_iops("rand-write", RANDOM_SIZE, job.secs);

        let job = fio(&[
            "--ioengine=mmap",
            "--rw=randread",
            "--bs=4k",
            "--time_based",
            RUNTIME,
        ]);
        result("mmap-rand-read", "IOPS", true, job.read_iops);
        let job = observed("mmap-rand-write", random_blocks, || {
            fio(&["--ioengine=mmap", "--rw=randwrite", "--bs=4k", &random, "--end_fsync=1"])
        });
        write_iops("mmap-rand-write", RANDOM_SIZE, job.secs);
        fs::remove_file(DATA).unwrap();

        // A log: each 4 KiB append is fsynced.
        let log = format!("{MNT}/log");
        let appended = 1 << 20;
        let start = Instant::now();
        observed("fsync-append", (appended / 4096) as usize, || {
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
            )
        });
        write_iops("fsync-append", appended, start.elapsed().as_secs_f64());
        fs::remove_file(&log).unwrap();
    }

    /// Reports the rate of `count` operations done in `secs`.
    fn ops(workload: &str, count: usize, secs: f64) {
        result(workload, "ops/s", true, count as f64 / secs);
    }

    /// Times `f`, which does `count` operations, as `workload`: with FXDAX_BENCH_GUEST_PERF set,
    /// sampled by perf throughout, whose report of the kernel's hottest symbols goes to the log
    /// (perf's own overhead is then in the measurement).
    fn timed<T>(workload: &str, count: usize, f: impl FnOnce() -> T) -> T {
        observed(workload, count, || {
            let start = Instant::now();
            let value = f();
            ops(workload, count, start.elapsed().as_secs_f64());
            value
        })
    }

    /// Runs `f`, which does `count` operations, as `workload`: with FXDAX_BENCH_GUEST_PERF set,
    /// sampled by perf throughout, whose report goes to the log, and with the extent-store
    /// device's requests per operation logged.
    fn observed<T>(workload: &str, count: usize, f: impl FnOnce() -> T) -> T {
        let requests = device_requests().zip(device_request_ns());
        let profiler = Profiler::start();
        let value = f();
        if let Some(profiler) = profiler {
            profiler.report(workload);
        }
        if let (Some((before, before_ns)), Some(after), Some(after_ns)) =
            (requests, device_requests(), device_request_ns())
        {
            let per_op: Vec<String> = after
                .iter()
                .zip(&before)
                .zip(after_ns.iter().zip(&before_ns))
                .filter(|(((_, after), (_, before)), _)| after > before)
                .map(|(((kind, after), (_, before)), ((_, after_ns), (_, before_ns)))| {
                    let requests = after - before;
                    format!(
                        "{kind} {:.2} ({:.0} µs each)",
                        requests as f64 / count as f64,
                        (after_ns - before_ns) as f64 / requests as f64 / 1000.0
                    )
                })
                .collect();
            eprintln!("REQUESTS {workload}: {} per operation", per_op.join(", "));
        }
        value
    }

    /// The extent-store device's count of the requests it has taken, by type, if the guest's
    /// filesystem is on one.
    fn device_requests() -> Option<Vec<(String, u64)>> {
        device_request_counters("requests")
    }

    /// The time the extent-store device's requests took, queued to completed, by type, in
    /// nanoseconds.
    fn device_request_ns() -> Option<Vec<(String, u64)>> {
        device_request_counters("request_ns")
    }

    /// The extent-store device's attribute `name`: a counter for each type of request.
    fn device_request_counters(name: &str) -> Option<Vec<(String, u64)>> {
        let device = fs::read_dir("/sys/bus/virtio/drivers/virtio_extent").ok()?.find_map(|entry| {
            let path = entry.ok()?.path().join(name);
            path.exists().then_some(path)
        })?;
        let counts = fs::read_to_string(device).ok()?;
        Some(
            counts
                .lines()
                .filter_map(|line| {
                    let (kind, count) = line.split_once(' ')?;
                    Some((kind.to_string(), count.parse().ok()?))
                })
                .collect(),
        )
    }

    /// perf, recording the whole guest into a tmpfs, enabled through its control FIFO once it is
    /// ready, so that it samples exactly what runs until `report`.
    struct Profiler {
        perf: std::process::Child,
        control: fs::File,
        ack: fs::File,
    }

    impl Profiler {
        const DIR: &str = "/run/bench-perf";

        fn start() -> Option<Self> {
            if !std::env::var("FXDAX_BENCH_GUEST_PERF").is_ok_and(|v| !v.is_empty()) {
                return None;
            }
            if !std::path::Path::new(Self::DIR).exists() {
                fs::create_dir_all(Self::DIR).unwrap();
                mount_tmpfs(Self::DIR);
            }
            let (control, ack) = (format!("{}/control", Self::DIR), format!("{}/ack", Self::DIR));
            for fifo in [&control, &ack] {
                let _ = fs::remove_file(fifo);
                let c = CString::new(fifo.as_str()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "mkfifo {fifo}");
            }
            let data = format!("{}/perf.data", Self::DIR);
            let _ = fs::remove_file(&data);
            // `record:EVENTS` samples the events, such as the scheduler's wakeups, rather than
            // the time.
            let setting = std::env::var("FXDAX_BENCH_GUEST_PERF").unwrap();
            let events = match setting.strip_prefix("record:") {
                Some(events) => vec!["-e".to_string(), events.to_string()],
                None => vec!["-F".to_string(), "4999".to_string()],
            };
            let perf = Command::new("perf")
                .args(["record", "-a", "-g"])
                .args(&events)
                .args(["-D", "-1", "-q", "-o", &data])
                .arg(format!("--control=fifo:{control},{ack}"))
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("perf record");
            // Opening the FIFOs waits for perf to open them too.
            let mut control = fs::OpenOptions::new().write(true).open(&control).unwrap();
            let mut ack = fs::File::open(&ack).unwrap();
            Self::command(&mut control, &mut ack, "enable");
            Some(Self { perf, control, ack })
        }

        /// Sends perf `command`, and waits for it to acknowledge it.
        fn command(control: &mut fs::File, ack: &mut fs::File, command: &str) {
            use std::io::Read;
            control.write_all(format!("{command}\n").as_bytes()).unwrap();
            let mut reply = [0u8; 5];
            ack.read_exact(&mut reply).unwrap();
            assert_eq!(&reply, b"ack\n\0", "perf's reply to {command}");
        }

        fn report(mut self, workload: &str) {
            Self::command(&mut self.control, &mut self.ack, "disable");
            unsafe { libc::kill(self.perf.id() as i32, libc::SIGINT) };
            self.perf.wait().expect("perf record");
            let data = format!("{}/perf.data", Self::DIR);
            let report = Command::new("perf")
                .args([
                    "report", "-i", &data, "--stdio", "--sort", "sym", "--no-children", "-g",
                    "none", "--percent-limit", "1", "-q",
                ])
                .output()
                .expect("perf report");
            // And the functions the time is spent in, callees included, by thread: the workload's
            // own apart from the kernel's workers'.
            let callers = Command::new("perf")
                .args([
                    "report", "-i", &data, "--stdio", "--sort", "comm,sym", "--children", "-g",
                    "none", "--percent-limit", "0.5", "-q",
                ])
                .output()
                .expect("perf report");
            // And what calls the functions taking the most time themselves.
            let chains = Command::new("perf")
                .args([
                    "report", "-i", &data, "--stdio", "--sort", "sym", "--no-children", "-g",
                    "caller,2,callee,function,percent", "--max-stack", "24", "--percent-limit",
                    "4", "-q",
                ])
                .output()
                .expect("perf report");
            eprintln!(
                "PROFILE {workload}\n{}\nINCLUSIVE {workload}\n{}\nCALLERS {workload}\n{}",
                String::from_utf8_lossy(&report.stdout),
                String::from_utf8_lossy(&callers.stdout),
                String::from_utf8_lossy(&chains.stdout)
            );
        }
    }

    fn mount_tmpfs(target: &str) {
        let (source, target, fstype) = (c"tmpfs", CString::new(target).unwrap(), c"tmpfs");
        let ret = unsafe {
            libc::mount(source.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, std::ptr::null())
        };
        assert_eq!(ret, 0, "mount tmpfs: {}", std::io::Error::last_os_error());
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

    fn setxattr(path: &str, name: &str, value: &[u8]) -> std::io::Result<()> {
        let path = CString::new(path).unwrap();
        let name = CString::new(name).unwrap();
        let ret = unsafe {
            libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0)
        };
        if ret == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
    }

    /// Operations on the names and attributes of many small files in one directory.
    fn files_workloads() {
        let files = files();
        let dir = format!("{MNT}/meta");
        let other = format!("{MNT}/meta-other");
        fs::create_dir(&dir).unwrap();
        fs::create_dir(&other).unwrap();
        let paths: Vec<String> = (0..files).map(|i| format!("{dir}/file-{i}")).collect();

        timed("create", files, || {
            for path in &paths {
                fs::write(path, b"a small file").unwrap();
            }
        });
        drop_caches();
        timed("stat-cold", files, || {
            for path in &paths {
                fs::metadata(path).unwrap();
            }
        });
        timed("stat-warm", files, || {
            for path in &paths {
                fs::metadata(path).unwrap();
            }
        });
        timed("lookup-missing", files, || {
            for i in 0..files {
                assert!(fs::metadata(format!("{dir}/missing-{i}")).is_err());
            }
        });
        timed("readdir", files, || {
            assert_eq!(fs::read_dir(&dir).unwrap().count(), files);
        });
        drop_caches();
        timed("read-small-cold", files, || {
            for path in &paths {
                assert_eq!(fs::read(path).unwrap().len(), 12);
            }
        });
        timed("chmod", files, || {
            for path in &paths {
                fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                    .unwrap();
            }
        });
        // A filesystem without extended attributes fails the workload.
        match setxattr(&paths[0], "user.bench", b"a value") {
            Err(error) if error.raw_os_error() == Some(libc::EOPNOTSUPP) => {
                println!("FAILED setxattr");
            }
            first => {
                first.unwrap();
                timed("setxattr", files, || {
                    for path in &paths {
                        setxattr(path, "user.bench", b"a value").unwrap();
                    }
                });
            }
        }
        timed("link", files, || {
            for (i, path) in paths.iter().enumerate() {
                fs::hard_link(path, format!("{dir}/link-{i}")).unwrap();
            }
        });
        timed("rename", files, || {
            for i in 0..files {
                fs::rename(format!("{dir}/link-{i}"), format!("{dir}/renamed-{i}")).unwrap();
            }
        });
        timed("rename-across", files, || {
            for i in 0..files {
                fs::rename(format!("{dir}/renamed-{i}"), format!("{other}/moved-{i}")).unwrap();
            }
        });
        timed("unlink", files, || {
            for path in &paths {
                fs::remove_file(path).unwrap();
            }
        });
        for i in 0..files {
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
        let files = files();
        let count = THREADS * files;
        let own = |thread: usize| format!("{MNT}/thread-{thread}");
        for thread in 0..THREADS {
            fs::create_dir(own(thread)).unwrap();
        }
        parallel("create-parallel", count, |thread| {
            for i in 0..files {
                fs::File::create(format!("{}/f{i}", own(thread))).unwrap();
            }
        });
        parallel("stat-parallel", count, |thread| {
            for i in 0..files {
                fs::metadata(format!("{}/f{i}", own(thread))).unwrap();
            }
        });
        parallel("unlink-parallel", count, |thread| {
            for i in 0..files {
                fs::remove_file(format!("{}/f{i}", own(thread))).unwrap();
            }
        });
        for thread in 0..THREADS {
            fs::remove_dir(own(thread)).unwrap();
        }

        let shared = format!("{MNT}/shared");
        fs::create_dir(&shared).unwrap();
        parallel("create-shared-dir", count, |thread| {
            for i in 0..files {
                fs::File::create(format!("{shared}/t{thread}-f{i}")).unwrap();
            }
        });
        parallel("unlink-shared-dir", count, |thread| {
            for i in 0..files {
                fs::remove_file(format!("{shared}/t{thread}-f{i}")).unwrap();
            }
        });
        fs::remove_dir(&shared).unwrap();
    }

    /// The metadata workloads, those of the parts of them that `runs` says run, or all.
    fn metadata_workloads(runs: impl Fn(&str) -> bool) {
        let all = runs("metadata");
        let parts: [(&str, fn()); 5] = [
            ("files", files_workloads),
            ("fsync", fsync_workloads),
            ("tree", tree_workloads),
            ("large-dir", large_dir_workloads),
            ("concurrent", concurrent_workloads),
        ];
        for (part, workloads) in parts {
            if all || runs(part) {
                workloads();
            }
        }
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

    /// PostgreSQL's pgbench, its TPC-B-like transactions, on a database on the filesystem: the
    /// database made (`pgbench -i`, scale 10, about 160 MiB) and FXDAX_BENCH_PGBENCH_SECS (default
    /// 20) of transactions from 4 clients, each committing durably.
    fn pgbench_workload() {
        let data = format!("{MNT}/pgdata");
        let socket = "/run/pgbench";
        // Set and empty, as scripts pass what they were not given, it is the default too.
        let secs: u64 = std::env::var("FXDAX_BENCH_PGBENCH_SECS")
            .ok()
            .filter(|secs| !secs.is_empty())
            .map_or(20, |secs| secs.parse().expect("FXDAX_BENCH_PGBENCH_SECS is a count"));
        // PostgreSQL refuses to run as root.
        let as_postgres = |command: &str| {
            Command::new("su")
                .args(["postgres", "-s", "/bin/sh", "-c", command])
                .output()
                .expect("su")
        };
        let check = |what: &str, output: &std::process::Output| {
            assert!(
                output.status.success(),
                "{what}: {}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        };
        /// Stops the server and removes its data, however the workload ends.
        struct Server<'a>(&'a str, &'a dyn Fn(&str) -> std::process::Output);
        impl Drop for Server<'_> {
            fn drop(&mut self) {
                let stop = (self.1)(&format!("pg_ctl -D {} -w -m fast stop", self.0));
                if !stop.status.success() {
                    eprintln!("pg_ctl stop: {}", String::from_utf8_lossy(&stop.stderr));
                    if let Ok(log) = fs::read_to_string(format!("{}/server.log", self.0)) {
                        eprintln!("server.log:\n{log}");
                    }
                }
                run("rm", &["-rf", self.0]);
            }
        }
        fs::create_dir_all(socket).unwrap();
        fs::create_dir(&data).unwrap();
        run("chown", &["postgres:postgres", &data, socket]);
        check("initdb", &as_postgres(&format!("initdb -D {data} -U postgres -A trust")));
        check(
            "pg_ctl start",
            &as_postgres(&format!(
                "pg_ctl -D {data} -w -l {data}/server.log start -o \"-c listen_addresses='' -c \
                 unix_socket_directories={socket}\""
            )),
        );
        let _server = Server(&data, &as_postgres);
        let start = Instant::now();
        check("pgbench -i", &as_postgres(&format!("pgbench -h {socket} -i -s 10 -q postgres")));
        result("pgbench-init", "s", false, start.elapsed().as_secs_f64());
        // Observed per transaction, at about the rate ext4 commits them.
        let expected = secs as usize * 1600;
        let output = observed("pgbench-tps", expected, || {
            as_postgres(&format!("pgbench -h {socket} -c 4 -j 2 -T {secs} postgres"))
        });
        check("pgbench", &output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        // "tps = 1234.5 (without initial connection time)"
        let tps = stdout
            .lines()
            .find_map(|line| line.strip_prefix("tps = "))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|tps| tps.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("no tps in pgbench's output: {stdout}"));
        result("pgbench-tps", "tps", true, tps);
    }

    /// fxdax: a fork case's VM's work: the base's, which makes the data its forks use, or a
    /// fork's, which uses it.
    fn fork_workloads() {
        let pgdata = format!("{MNT}/pgdata");
        let socket = "/run/pgbench";
        let as_postgres = |command: &str| {
            let output = Command::new("su")
                .args(["postgres", "-s", "/bin/sh", "-c", command])
                .output()
                .expect("su");
            assert!(
                output.status.success(),
                "{command}: {}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            output
        };
        let start_server = || {
            as_postgres(&format!(
                "pg_ctl -D {pgdata} -w -l {pgdata}/server.log start -o \"-c listen_addresses='' -c \
                 unix_socket_directories={socket}\""
            ))
        };
        fs::create_dir_all(socket).unwrap();
        run("chown", &["postgres:postgres", socket]);
        let role = std::env::var(ROLE).expect("a fork case's role");
        if role == "base" {
            mount("bench", "fxfs", "");
            let start = Instant::now();
            run("cp", &["-a", "/usr", &format!("{MNT}/usr")]);
            result("base-copy-usr", "s", false, start.elapsed().as_secs_f64());
            let start = Instant::now();
            fs::create_dir(&pgdata).unwrap();
            run("chown", &["postgres:postgres", &pgdata]);
            as_postgres(&format!("initdb -D {pgdata} -U postgres -A trust"));
            start_server();
            as_postgres(&format!("pgbench -h {socket} -i -s 10 -q postgres"));
            as_postgres(&format!("pg_ctl -D {pgdata} -w -m fast stop"));
            result("base-pgbench-init", "s", false, start.elapsed().as_secs_f64());
            unsafe { libc::sync() };
            let target = CString::new(MNT).unwrap();
            assert_eq!(unsafe { libc::umount(target.as_ptr()) }, 0, "umount");
            return;
        }
        let requests = device_requests().zip(device_request_ns());
        let start = Instant::now();
        mount("bench", "fxfs", "");
        result("fork-mount", "s", false, start.elapsed().as_secs_f64());
        let start = Instant::now();
        run("sh", &["-c", &format!("find {MNT}/usr -type f -exec cat {{}} + > /dev/null")]);
        result("fork-read-usr", "s", false, start.elapsed().as_secs_f64());
        let start = Instant::now();
        start_server();
        result("fork-pg-start", "s", false, start.elapsed().as_secs_f64());
        // Set and empty, as scripts pass what they were not given, it is the default too.
        let secs: u64 = std::env::var("FXDAX_BENCH_PGBENCH_SECS")
            .ok()
            .filter(|secs| !secs.is_empty())
            .map_or(10, |secs| secs.parse().expect("FXDAX_BENCH_PGBENCH_SECS is a count"));
        let output = as_postgres(&format!("pgbench -h {socket} -S -c 4 -j 2 -T {secs} postgres"));
        let stdout = String::from_utf8_lossy(&output.stdout);
        let tps = stdout
            .lines()
            .find_map(|line| line.strip_prefix("tps = "))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|tps| tps.parse::<f64>().ok())
            .unwrap_or_else(|| panic!("no tps in pgbench's output: {stdout}"));
        result("fork-pgbench-select", "tps", true, tps);
        as_postgres(&format!("pg_ctl -D {pgdata} -w -m fast stop"));
        // What the fork's device asked of the daemon, as the guest saw it.
        if let (Some((before, before_ns)), Some(after), Some(after_ns)) =
            (requests, device_requests(), device_request_ns())
        {
            for (((kind, after), (_, before)), ((_, after_ns), (_, before_ns))) in
                after.iter().zip(&before).zip(after_ns.iter().zip(&before_ns))
            {
                println!("OBSERVED guest-{kind}-requests {}", after - before);
                println!("OBSERVED guest-{kind}-ms {:.3}", (after_ns - before_ns) as f64 / 1e6);
            }
        }
    }

    impl Test for TestFsBench {
        fn in_guest(self: Box<Self>) {
            start_watchdog();
            let default_hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                default_hook(info);
                crate::kernel_log::print();
            }));
            match self.backend {
                Backend::Passthrough => mount("bench", "virtiofs", "dax=never"),
                Backend::Ext4 | Backend::Ext4GuestKernel => {
                    let device = bench_device();
                    run("mkfs.ext4", &["-q", "-F", &device]);
                    mount(&device, "ext4", "");
                }
                Backend::FxfsGuest | Backend::FxfsGuestDax => mount("bench", "fxfs", ""),
                Backend::FxfsGuestDaxForkWarm | Backend::FxfsGuestDaxForkCold => {
                    return fork_workloads();
                }
                Backend::Ext4DaxPmem => {
                    let device = bench_device();
                    run("mkfs.ext4", &["-q", "-F", &device]);
                    mount(&device, "ext4", "dax=always");
                }
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
                metadata_workloads(runs);
                // A failed build is recorded, and the other iterations still run.
                if runs("build") && std::panic::catch_unwind(build_workload).is_err() {
                    println!("FAILED build");
                    run("rm", &["-rf", &format!("{MNT}/build")]);
                }
                // pgbench runs only when named, which keeps a run of every other group short.
                if groups.split(',').any(|g| g == "pgbench")
                    && std::panic::catch_unwind(pgbench_workload).is_err()
                {
                    println!("FAILED pgbench");
                }
                drop_caches();
            }
            println!("DONE");
        }
    }
}
