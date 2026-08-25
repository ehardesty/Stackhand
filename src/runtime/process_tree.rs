//! Private Unix Process Tree containment behind the Run owner.
//!
//! Each Run starts with its root process as a process-group (and session)
//! leader: portable-pty calls `setsid()` in every spawned PTY child, and
//! pipe mode uses `CommandExt::process_group(0)`. Semantic signals therefore
//! target the owned group `-pgid` instead of only the root PID, and
//! membership checks enumerate processes whose process group matches the
//! owned one.
//!
//! Containment limits (documented, not a security boundary):
//! - A descendant that calls `setsid()` or `setpgid()` escapes the owned
//!   group. Its exit cannot be confirmed and it will not receive group
//!   signals.
//! - PID reuse between spawn and signal is mitigated by checking that the
//!   root still exists and still leads the expected group where the platform
//!   permits; complete protection is impossible on Unix.
//! - Membership enumeration is best effort: it observes the live process
//!   table, so a member that exits during enumeration may appear or vanish
//!   racily.

use std::collections::BTreeSet;

use anyhow::{Context, Result};

/// Why a semantic signal could not be delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalError {
    /// The owned Process Group no longer exists. This is an idempotent
    /// exit race, not a failure.
    NotFound,
    /// The Process Group exists but at least one member cannot be
    /// signaled. Escalation against this numeric PGID must stop: the group
    /// identity may no longer be owned by this Run.
    Ownership(String),
    /// Delivery failed for another reason; retained as a diagnostic.
    Failed(String),
}

impl SignalError {
    pub fn detail(self) -> String {
        match self {
            Self::NotFound => "the owned Process Group is already gone".to_string(),
            Self::Ownership(detail) | Self::Failed(detail) => detail,
        }
    }
}

/// The owned Process Tree identity for one Run: the root PID doubles as the
/// owned process-group ID.
#[derive(Clone, Copy, Debug)]
pub struct UnixProcessTree {
    root_pid: u32,
}

impl UnixProcessTree {
    pub fn from_root(root_pid: u32) -> Self {
        Self { root_pid }
    }

    pub fn root_pid(&self) -> u32 {
        self.root_pid
    }

    /// Deliver one semantic signal to the whole owned Process Group.
    ///
    /// There is deliberately no fallback to the positive root PID after a
    /// failed group signal. `ESRCH` proves the group is empty, and `EPERM`
    /// proves the group exists but is not safely ours anymore. Both cases
    /// forbid a blind positive-PID signal.
    ///
    /// Platform note (observed on Darwin arm64): a group whose only member
    /// is an unreaped zombie session leader answers group signals with
    /// EPERM. Callers must therefore observe root exit with
    /// [`Self::root_exit_pending`] and skip group signaling when only the
    /// zombie root remains; see [`Self::signal_root_unreaped`].
    pub fn signal(&self, semantic: SemanticSignal) -> std::result::Result<(), SignalError> {
        let signal = match semantic {
            SemanticSignal::Interrupt => libc::SIGINT,
            SemanticSignal::Terminate => libc::SIGTERM,
            SemanticSignal::Kill => libc::SIGKILL,
        };
        let group = -(self.root_pid as libc::pid_t);

        // SAFETY: signaling one owned process group identified by the Run's
        // root PID; the call itself cannot fail unsafely.
        let result = unsafe { libc::kill(group, signal) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ESRCH) => Err(SignalError::NotFound),
            Some(libc::EPERM) => Err(SignalError::Ownership(format!(
                "{semantic:?} was refused for process group {}: a member cannot be signaled",
                self.root_pid
            ))),
            _ => Err(SignalError::Failed(format!(
                "{semantic:?} failed for process group {}: {error}",
                self.root_pid
            ))),
        }
    }

    /// Deliver one semantic signal to the unreaped root process itself.
    ///
    /// Safe only while the root has not been reaped: the kernel keeps a
    /// direct child's PID reserved until reaping, so this cannot hit a
    /// reused PID. After reaping, no signal may target that PID again.
    pub fn signal_root_unreaped(
        &self,
        semantic: SemanticSignal,
    ) -> std::result::Result<(), SignalError> {
        let signal = match semantic {
            SemanticSignal::Interrupt => libc::SIGINT,
            SemanticSignal::Terminate => libc::SIGTERM,
            SemanticSignal::Kill => libc::SIGKILL,
        };
        let pid = self.root_pid as libc::pid_t;
        // SAFETY: signaling one owned, unreaped direct child process; the
        // call itself cannot fail unsafely.
        let result = unsafe { libc::kill(pid, signal) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Err(SignalError::NotFound)
        } else {
            Err(SignalError::Failed(format!(
                "{semantic:?} could not reach root {}: {error}",
                self.root_pid
            )))
        }
    }

    /// Whether the root child has exited but is not yet reaped.
    ///
    /// Uses waitid with WNOWAIT: the exit is observed without reaping, so
    /// Process Group identity stays intact for later decisions.
    pub fn root_exit_pending(pid: u32) -> bool {
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let result = libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            );
            if result == 0 {
                // POSIX: with WNOHANG, a still-running child also returns 0
                // and leaves si_pid set to 0. Only a non-zero si_pid means
                // the exit event was observed.
                return info.si_pid() != 0;
            }
            // The child is gone entirely (already reaped elsewhere): treat
            // it as exited so callers do not keep signaling a dead group.
            matches!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            )
        }
    }

    /// Best-effort list of live members: every visible process whose process
    /// group matches the owned group, including the (possibly zombie) root.
    pub fn remaining_members(&self) -> Result<BTreeSet<u32>> {
        let wanted = self.root_pid;
        let mut members = BTreeSet::new();
        for (pid, pgid) in enumerate_pid_pgids()? {
            if pgid == wanted {
                members.insert(pid);
            }
        }
        Ok(members)
    }

    /// Live Process Tree members other than the root itself. The unreaped
    /// root stays a group member until it is reaped, so containment checks
    /// that decide whether to escalate must exclude it.
    pub fn remaining_members_excluding_root(&self) -> Result<BTreeSet<u32>> {
        let root = self.root_pid;
        Ok(self
            .remaining_members()?
            .into_iter()
            .filter(|pid| *pid != root)
            .collect())
    }

    /// Confirm whether the given PIDs are gone. Returns the PIDs that are
    /// still present or whose state could not be observed. Only `ESRCH`
    /// proves absence; `EPERM` from the liveness probe means "present but
    /// unconfirmed".
    #[allow(dead_code)] // Consumed by Run callers and Milestone 0B cleanup evidence.
    pub fn confirm_gone(pids: &[u32]) -> Vec<u32> {
        pids.iter()
            .copied()
            .filter(|pid| probe_pid(*pid) != MemberState::Gone)
            .collect()
    }
}

/// Liveness of one observed process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Part of the containment vocabulary used by callers and Milestone 0B tests.
pub enum MemberState {
    Present,
    Gone,
    /// The process exists but cannot be inspected by this user.
    Unconfirmed,
}

fn probe_pid(pid: u32) -> MemberState {
    let raw = pid as libc::pid_t;
    // SAFETY: probing liveness with signal 0; cannot fail unsafely.
    let result = unsafe { libc::kill(raw, 0) };
    if result == 0 {
        return MemberState::Present;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => MemberState::Gone,
        _ => MemberState::Unconfirmed,
    }
}

/// Semantic shutdown actions. Operating-system mappings stay inside this
/// adapter: interrupt → SIGINT, terminate → SIGTERM, kill → SIGKILL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticSignal {
    Interrupt,
    Terminate,
    Kill,
}

#[cfg(target_os = "linux")]
fn enumerate_pid_pgids() -> Result<Vec<(u32, u32)>> {
    let mut pairs = Vec::new();
    let entries = std::fs::read_dir("/proc").context("could not read /proc")?;
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue; // The process may have exited during enumeration.
        };
        // The comm field may contain spaces and parentheses; take the text
        // after the final ')' character.
        let Some(rest) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        // Fields after ')' start at ppid (field 4 overall); pgrp is field 5.
        let Some(pgrp) = fields.get(2).and_then(|value| value.parse().ok()) else {
            continue;
        };
        pairs.push((pid, pgrp));
    }
    Ok(pairs)
}

#[cfg(target_os = "macos")]
fn enumerate_pid_pgids() -> Result<Vec<(u32, u32)>> {
    let output = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,pgid="])
        .output()
        .context("could not run ps to enumerate process groups")?;
    if !output.status.success() {
        return Err(anyhow::anyhow!("ps failed while enumerating groups"));
    }
    let mut pairs = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(pgid)) = (fields.next(), fields.next()) else {
            continue;
        };
        if let (Ok(pid), Ok(pgid)) = (pid.parse(), pgid.parse()) {
            pairs.push((pid, pgid));
        }
    }
    Ok(pairs)
}

#[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
fn enumerate_pid_pgids() -> Result<Vec<(u32, u32)>> {
    Err(anyhow::anyhow!(
        "process-group enumeration is not implemented on this platform"
    ))
}
