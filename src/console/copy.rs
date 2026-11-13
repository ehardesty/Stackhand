use super::ConsoleInteraction;
use crate::host::clipboard::{COPY_LIMIT_BYTES, CopyOutcome};
use crate::runtime::TerminalHandle;
use crate::terminal::CopyRequest;
use crate::tui::ConsoleWarning;

pub(super) enum PendingCopy {
    Terminal(CopyRequest),
    Logs(String),
}

impl ConsoleInteraction {
    pub fn poll_requests(&mut self, mut copy: impl FnMut(String) -> CopyOutcome) -> bool {
        let changed = self.poll_paste_requests();
        let Some(request) = self.copy_request.take() else {
            return changed;
        };
        let result = match request {
            PendingCopy::Logs(text) => Ok(Some(text)),
            PendingCopy::Terminal(request) => {
                let Some(result) = request.poll() else {
                    self.copy_request = Some(PendingCopy::Terminal(request));
                    return changed;
                };
                result
            }
        };
        self.view.warning = copy_warning(result, &mut copy);
        true
    }

    pub fn copy_logs(&mut self, text: String) {
        self.view.warning = if text.is_empty() {
            Some(ConsoleWarning::NoLogsToCopy)
        } else if self.copy_request.is_some() {
            Some(ConsoleWarning::CopyBusy)
        } else if text.len() > COPY_LIMIT_BYTES {
            Some(ConsoleWarning::CopyTooLarge)
        } else {
            self.copy_request = Some(PendingCopy::Logs(text));
            None
        };
    }

    pub(super) fn request_terminal_copy(&mut self, session: &TerminalHandle<'_>) {
        if self.copy_request.is_some() {
            self.view.warning = Some(ConsoleWarning::CopyBusy);
        } else {
            self.copy_request = Some(PendingCopy::Terminal(session.request_copy()));
            self.view.warning = None;
        }
    }
}

pub(super) fn copy_warning(
    result: Result<Option<String>, String>,
    copy: impl FnOnce(String) -> CopyOutcome,
) -> Option<ConsoleWarning> {
    match result {
        Ok(Some(text)) if !text.is_empty() => match copy(text) {
            CopyOutcome::Submitted => None,
            CopyOutcome::Empty => Some(ConsoleWarning::NothingSelected),
            CopyOutcome::Failed => Some(ConsoleWarning::ClipboardFailed),
            CopyOutcome::TooLarge => Some(ConsoleWarning::CopyTooLarge),
            CopyOutcome::Disabled => Some(ConsoleWarning::ClipboardDisabled),
        },
        Ok(_) => Some(ConsoleWarning::NothingSelected),
        Err(_) => Some(ConsoleWarning::ClipboardFailed),
    }
}
