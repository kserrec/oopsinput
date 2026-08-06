//! Bounded child processes. Every external helper answers to a hard
//! deadline (SPEC §9-1); this is the one wait-or-kill loop both helper
//! runners share, so "no path outlives the timeout" has a single proof.

use std::process::Child;
use std::time::Instant;

/// Wait for `child` until `deadline`, polling briefly. Returns true only on
/// a successful exit before the deadline; a child that overruns, fails, or
/// cannot be waited on is killed and reaped — never left a zombie.
pub(crate) fn wait_or_kill(child: &mut Child, deadline: Instant) -> bool {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}
