//! Pure projection of Supervisor snapshots into compact user-visible rows and headers.

use crate::supervisor::{Lifecycle, LivenessState, ProcessSnapshot, ProjectSnapshot};
use crate::tui::{LifecycleTone, ProcessRowView};

pub(super) fn process_rows(snapshot: &ProjectSnapshot, selected: usize) -> Vec<ProcessRowView> {
    let show_profiles = snapshot.processes.iter().any(|process| {
        process
            .current_profile
            .as_deref()
            .is_some_and(|current| Some(current) != process.next_profile.as_deref())
            || process.current_run.is_some()
                && process.current_profile.is_none()
                && process.next_profile.is_some()
            || process.next_profile.as_deref() != snapshot.selected_profile.as_deref()
    });
    snapshot
        .processes
        .iter()
        .enumerate()
        .map(|(index, process)| ProcessRowView {
            name: process.name.clone(),
            status: status_label(process),
            lifecycle_tone: lifecycle_tone(process),
            profile: show_profiles
                .then(|| profile_label(process, snapshot.base_profile_name.as_str())),
            cpu: process.metrics.map(|metrics| {
                metric_precision(format_cpu(metrics.cpu_percent), metrics.best_effort)
            }),
            memory: process
                .metrics
                .map(|metrics| metric_precision(format_rss(metrics.rss_kib), metrics.best_effort)),
            selected: index == selected,
        })
        .collect()
}

fn profile_label(process: &ProcessSnapshot, base_profile_name: &str) -> String {
    let next = process.next_profile.as_deref().unwrap_or(base_profile_name);
    match process.current_run {
        Some(_)
            if process
                .current_profile
                .as_deref()
                .unwrap_or(base_profile_name)
                != next =>
        {
            format!(
                "{} → {next}",
                process
                    .current_profile
                    .as_deref()
                    .unwrap_or(base_profile_name)
            )
        }
        _ => next.to_string(),
    }
}

pub(super) fn profile_changes_pending(snapshot: &ProjectSnapshot) -> bool {
    snapshot.processes.iter().any(|process| {
        process.current_run.is_some()
            && process.current_profile.as_deref() != process.next_profile.as_deref()
    })
}

pub(super) fn process_list_title(snapshot: &ProjectSnapshot) -> String {
    if snapshot.available_profiles.is_empty() {
        return "Processes".to_string();
    }
    let selected = snapshot
        .selected_profile
        .as_deref()
        .unwrap_or(snapshot.base_profile_name.as_str());
    let pending = snapshot
        .processes
        .iter()
        .filter(|process| {
            process.current_run.is_some()
                && process.current_profile.as_deref() != process.next_profile.as_deref()
        })
        .count();
    if pending == 0 {
        format!("Processes · Profile: {selected}")
    } else {
        format!("Processes · Profile: {selected} · {pending} pending")
    }
}

/// A compact CPU column: one decimal place at most, no more precision than
/// the sample claims.
pub(super) fn format_cpu(percent: f64) -> String {
    if percent >= 10.0 {
        format!("{}%", percent.round())
    } else {
        format!("{percent:.1}%")
    }
}

/// A compact resident-memory column in powers of 1024.
pub(super) fn metric_precision(value: String, best_effort: bool) -> String {
    if best_effort {
        format!("~{value}")
    } else {
        value
    }
}

pub(super) fn format_rss(kib: u64) -> String {
    const MIB: u64 = 1024;
    const GIB: u64 = 1024 * MIB;
    match kib {
        0 => "0".to_string(),
        value if value < MIB => format!("{kib}K"),
        value if value < GIB => format!("{}M", value / MIB),
        value => format!("{:.1}G", value as f64 / GIB as f64),
    }
}

/// Project structured lifecycle state into the concise row label. The label
/// is a projection; the snapshot remains the authority.
pub(super) fn status_label(process: &ProcessSnapshot) -> String {
    if !process.enabled && process.current_run.is_none() {
        return "Disabled".to_string();
    }
    if process.lifecycle == Lifecycle::Done {
        return "Done".to_string();
    }
    // A liveness failure is a health result while the Run remains active.
    // Controlled recovery uses Stopping or RestartBackoff instead.
    if process.lifecycle == Lifecycle::Running
        && process
            .liveness
            .as_ref()
            .is_some_and(|liveness| liveness.state == LivenessState::Failing)
    {
        return "Unhealthy".to_string();
    }
    // A failure stays visible while the Process is not mid-shutdown or
    // waiting through an automatic restart delay. Those states project their
    // own reason into the label.
    if !matches!(
        process.lifecycle,
        Lifecycle::Stopping | Lifecycle::RestartBackoff
    ) && let Some(failure) = &process.failure
    {
        return format!("Failed ({})", short_reason(&failure.detail));
    }
    match process.lifecycle {
        // Done returns above; this arm keeps the match exhaustive.
        Lifecycle::Done | Lifecycle::Idle | Lifecycle::Stopped => "Stopped".to_string(),
        Lifecycle::Starting => "Starting".to_string(),
        Lifecycle::Running => "Ready".to_string(),
        Lifecycle::Waiting => match &process.blocked_reason {
            Some(reason) => format!("Waiting ({})", short_reason(reason)),
            None => "Waiting".to_string(),
        },
        Lifecycle::Stopping => {
            if let Some(failure) = &process.failure {
                format!("Stopping ({})", short_reason(&failure.detail))
            } else {
                "Stopping".to_string()
            }
        }
        Lifecycle::RestartBackoff => match &process.restart_backoff {
            Some(backoff) => format!("Restarting ({})", short_reason(&backoff.reason)),
            None => "Restarting".to_string(),
        },
    }
}

pub(super) fn lifecycle_tone(process: &ProcessSnapshot) -> LifecycleTone {
    if !process.enabled && process.current_run.is_none() {
        return LifecycleTone::Muted;
    }
    if process.lifecycle == Lifecycle::Done {
        return LifecycleTone::Success;
    }
    if process.lifecycle == Lifecycle::Running
        && process
            .liveness
            .as_ref()
            .is_some_and(|liveness| liveness.state == LivenessState::Failing)
    {
        return LifecycleTone::Error;
    }
    if !matches!(
        process.lifecycle,
        Lifecycle::Stopping | Lifecycle::RestartBackoff
    ) && process.failure.is_some()
    {
        return LifecycleTone::Error;
    }
    match process.lifecycle {
        Lifecycle::Idle | Lifecycle::Stopped => LifecycleTone::Muted,
        Lifecycle::Starting => LifecycleTone::Info,
        Lifecycle::Running | Lifecycle::Done => LifecycleTone::Success,
        Lifecycle::Waiting | Lifecycle::Stopping | Lifecycle::RestartBackoff => {
            LifecycleTone::Warning
        }
    }
}

/// A bounded, character-safe reason for one row.
fn short_reason(detail: &str) -> String {
    let mut truncated: String = detail.chars().take(40).collect();
    if detail.chars().count() > 40 {
        truncated.push('…');
    }
    truncated
}

/// Project the selected Process into the console pane's header: name, the
/// live Run identity and PID when one exists, the concise status label,
/// the Run's age and compact metrics when sampled, and the bounded
/// diagnostic (a blocked reason or failure detail) when one is present.
/// The header is a projection of the immutable Supervisor snapshot.
pub(super) fn selected_header(process: &ProcessSnapshot, now_ms: u64) -> String {
    let mut header = process.name.clone();
    if let Some(run_id) = process.current_run {
        header.push_str(&format!(" · run {run_id}"));
    }
    if let Some(pid) = process.root_pid {
        header.push_str(&format!(" · PID {pid}"));
    }
    header.push_str(&format!(" · {}", status_label(process)));
    let budget = &process.automatic_restart_budget;
    if budget.automatic_retries_used > 0 || budget.exhausted {
        header.push_str(&format!(
            " · automatic retries {}/{}",
            budget.automatic_retries_used, budget.max_restarts
        ));
    }
    if let Some(started_at_ms) = process.run_started_at_ms {
        let age_ms = now_ms.saturating_sub(started_at_ms);
        header.push_str(&format!(" · {}", format_age(age_ms)));
    }
    if let Some(metrics) = &process.metrics {
        header.push_str(&format!(
            " · {}",
            metric_precision(format_rss(metrics.rss_kib), metrics.best_effort)
        ));
        header.push_str(&format!(
            " · {} CPU",
            metric_precision(format_cpu(metrics.cpu_percent), metrics.best_effort)
        ));
    }
    if process.lifecycle == Lifecycle::Waiting {
        if let Some(reason) = &process.blocked_reason {
            header.push_str(&format!(" · {reason}"));
        }
    } else if process.lifecycle == Lifecycle::RestartBackoff {
        if let Some(backoff) = &process.restart_backoff {
            header.push_str(&format!(
                " · {} · next restart at {}ms",
                backoff.reason, backoff.next_attempt_at_ms
            ));
        }
    } else if let Some(readiness) = &process.readiness {
        if let Some(last_error) = &readiness.last_error {
            header.push_str(&format!(
                " · readiness attempt {}: {}",
                readiness.attempts,
                short_reason(last_error)
            ));
        }
    } else if process.lifecycle != Lifecycle::Stopping
        && let Some(failure) = &process.failure
    {
        header.push_str(&format!(" · {}", short_reason(&failure.detail)));
    }
    header
}

/// A compact Run age: seconds under a minute, then whole minutes.
pub(super) fn format_age(age_ms: u64) -> String {
    let seconds = age_ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        let minutes = seconds / 60;
        format!("{minutes}m{}s", seconds % 60)
    }
}
