//! Raising the soft limit of open descriptors before running a subcommand.
//!
//! A reader keeps open the `c<N>.bin` of every column it reads in every active part
//! (contract §6.3), so `SELECT *` over the 10 parts of ClickBench `hits` needs about
//! 10 × 105 descriptors, above the usual soft limit of 1024. `veta` raises its soft limit
//! to the hard one at startup. `setrlimit` would need `unsafe`, which contract §2 only
//! allows in hot loops of `veta-codec` and `veta-exec`, so the raise is done by
//! re-executing the binary once through `/bin/sh -c 'ulimit -S -n <hard>; exec ...'`.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Set in the environment of the re-executed process so it does not try again.
pub const REEXEC_MARK: &str = "VETA_NOFILE_RAISED";

const SHELL_SCRIPT: &str = "n=$1; shift; ulimit -S -n \"$n\" 2>/dev/null; exec \"$0\" \"$@\"";

/// Soft and hard limit of open files from the text of `/proc/self/limits`; `None` when
/// the line is missing or a value is not a number (`unlimited`).
pub fn parse_nofile(limits: &str) -> Option<(u64, u64)> {
    let line = limits.lines().find(|l| l.starts_with("Max open files"))?;
    let mut values = line["Max open files".len()..].split_whitespace();
    let soft = values.next()?.parse().ok()?;
    let hard = values.next()?.parse().ok()?;
    Some((soft, hard))
}

/// The soft limit to ask for, or `None` when it already equals the hard one.
pub fn raise_target(limits: &str) -> Option<u64> {
    let (soft, hard) = parse_nofile(limits)?;
    (soft < hard).then_some(hard)
}

/// Re-executes the current binary with the soft limit of open files raised to the hard
/// one. Returns (and the process goes on with its current limit) when the limit is
/// already at the hard one, when this process is the re-executed one, or when anything
/// needed for the re-execution is missing.
pub fn raise_open_files_limit() {
    if std::env::var_os(REEXEC_MARK).is_some() {
        return;
    }
    let Ok(limits) = std::fs::read_to_string("/proc/self/limits") else {
        return;
    };
    let Some(target) = raise_target(&limits) else {
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // `exec` only returns on failure; then the current limit is kept.
    let _ = Command::new("/bin/sh")
        .arg("-c")
        .arg(SHELL_SCRIPT)
        .arg(exe)
        .arg(target.to_string())
        .args(args)
        .env(REEXEC_MARK, "1")
        .exec();
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: &str = "Limit                     Soft Limit           Hard Limit           Units     \n\
                          Max cpu time              unlimited            unlimited            seconds   \n\
                          Max open files            1024                 524288               files     \n\
                          Max locked memory         8388608              8388608              bytes     \n";

    #[test]
    fn parses_the_open_files_line() {
        assert_eq!(parse_nofile(LIMITS), Some((1024, 524288)));
        assert_eq!(raise_target(LIMITS), Some(524288));
    }

    #[test]
    fn no_target_when_soft_equals_hard_or_line_is_unusable() {
        let equal = LIMITS.replace("1024                 524288", "524288               524288");
        assert_eq!(raise_target(&equal), None);
        let unlimited = LIMITS.replace("524288   ", "unlimited");
        assert_eq!(parse_nofile(&unlimited), None);
        assert_eq!(
            raise_target("Max cpu time unlimited unlimited seconds\n"),
            None
        );
        assert_eq!(raise_target(""), None);
    }
}
