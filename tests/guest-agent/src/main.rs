use anyhow::Context;
use std::env::args;
use test_cases::{TestCase, test_cases};

fn run_guest_agent(test_name: &str) -> anyhow::Result<()> {
    let tests = test_cases();
    let test_case = tests
        .into_iter()
        .find(|t| t.name() == test_name)
        .context("No such test!")?;
    let TestCase { test, .. } = test_case;
    test.in_guest();

    #[cfg(target_os = "freebsd")]
    {
        use test_cases::freebsd_guest;
        freebsd_guest::halt_vm();
    }

    #[allow(unreachable_code)]
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let mut cli_args = args();
    let _exec_name = cli_args.next();
    let test_name = cli_args.next().context("Missing test name argument")?;
    // The guest's init, under a VMM without libkrun's own (fxdax's Firecracker), which runs the
    // test and resets the VM, whatever becomes of the test.
    #[cfg(target_os = "linux")]
    if std::process::id() == 1 {
        let init = test_cases::guest_init::set_up();
        if let Ok(Err(err)) = std::panic::catch_unwind(|| run_guest_agent(&test_name)) {
            eprintln!("{err:#}");
        }
        init.reset();
    }
    run_guest_agent(&test_name)?;
    Ok(())
}
