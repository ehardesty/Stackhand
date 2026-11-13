use super::{ConsoleWarning, Essential, Hint, VisibleAction};

pub(super) fn append_warning(hints: &mut Vec<Hint>, warning: ConsoleWarning) {
    let (message, follow_ups): (&str, &[(&str, VisibleAction)]) = match warning {
        ConsoleWarning::PasteRejected => (
            "WARNING: paste rejected; focus an input-enabled console first",
            &[],
        ),
        ConsoleWarning::PasteTooLarge => {
            ("WARNING: paste exceeds the 64 KiB limit; nothing sent", &[])
        }
        ConsoleWarning::PasteRefused => (
            "WARNING: paste not sent: the focused process could treat it as commands (line break without bracketed paste, or a paste terminator)",
            &[],
        ),
        ConsoleWarning::InputRejected => (
            "WARNING: terminal input was rejected; Run is stopping or its queue is full",
            &[],
        ),
        ConsoleWarning::InputBackpressure => (
            "WARNING: child input queue is saturated; delivery is bounded",
            &[],
        ),
        ConsoleWarning::OutputTruncated => (
            "Earlier Logs expired at the history limit; recent output continues",
            &[],
        ),
        ConsoleWarning::PasteDeliveryFailed => (
            "WARNING: an admitted paste may not have fully reached the child",
            &[],
        ),
        ConsoleWarning::CopyTooLarge => (
            "WARNING: copy exceeds 64 KiB; selection remains available",
            &[],
        ),
        ConsoleWarning::ClipboardDisabled => {
            ("WARNING: clipboard is disabled by STACKHAND_CLIPBOARD", &[])
        }
        ConsoleWarning::CopyBusy => ("WARNING: a copy request is already pending", &[]),
        ConsoleWarning::ClipboardFailed => (
            "WARNING: clipboard write failed; selection remains available",
            &[],
        ),
        ConsoleWarning::NothingSelected => (
            "WARNING: no terminal text is selected",
            &[("v: start selection", VisibleAction::ToggleSelection)],
        ),
        ConsoleWarning::NoLogsToCopy => ("WARNING: no Logs text is available to copy", &[]),
        ConsoleWarning::InputDisabled => (
            "WARNING: input is not enabled for this Process",
            &[("Ctrl-A: Processes", VisibleAction::FocusProcesses)],
        ),
        ConsoleWarning::LogsCommandOnly => (
            "WARNING: Logs is read-only",
            &[
                ("Ctrl-F /: search", VisibleAction::SearchEdit),
                ("Esc: Processes", VisibleAction::FocusProcesses),
            ],
        ),
        ConsoleWarning::SelectionUnavailable => (
            "WARNING: terminal selection is available only in Terminal view",
            &[],
        ),
        ConsoleWarning::LinkOpenFailed => ("WARNING: could not open the port in a browser", &[]),
    };
    hints.push(Hint::text(message, Essential));
    hints.extend(
        follow_ups
            .iter()
            .map(|(label, action)| Hint::action(*label, *action, Essential)),
    );
}
