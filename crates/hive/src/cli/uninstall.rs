//! `hive uninstall`: remove this installation and its plugin registrations.

pub(crate) fn uninstall_cmd(force: bool, purge: bool) -> ! {
    let result = std::env::current_exe()
        .map_err(anyhow::Error::from)
        .and_then(|target| crate::uninstall::run(&target, force, purge));
    let success = match result {
        Ok(success) => success,
        Err(error) => {
            eprintln!("uninstall: {error}");
            false
        }
    };
    std::process::exit(i32::from(!success));
}
