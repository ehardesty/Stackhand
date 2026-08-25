use libghostty_vt::paste;
use std::sync::mpsc::{Receiver, TryRecvError};

/// Maximum size of one paste accepted by the prototype.
///
/// This limit is smaller than the bounded PTY input queue. The encoded form of
/// one accepted paste therefore fits within the queue limit; a paste that is
/// too large is rejected before it reaches the terminal owner.
pub const PASTE_LIMIT_BYTES: usize = 64 * 1_024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasteCompletion {
    Delivered,
    Failed(String),
}

pub struct PasteRequest {
    id: u64,
    completion: Receiver<Result<(), String>>,
    finished: bool,
}

impl PasteRequest {
    pub(crate) fn new(id: u64, completion: Receiver<Result<(), String>>) -> Self {
        Self {
            id,
            completion,
            finished: false,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn poll(&mut self) -> Option<PasteCompletion> {
        if self.finished {
            return None;
        }
        match self.completion.try_recv() {
            Ok(Ok(())) => {
                self.finished = true;
                Some(PasteCompletion::Delivered)
            }
            Ok(Err(error)) => {
                self.finished = true;
                Some(PasteCompletion::Failed(error))
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.finished = true;
                Some(PasteCompletion::Failed(
                    "terminal owner stopped before paste delivery completed".to_string(),
                ))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteRejection {
    Unsafe,
    /// The Run is shutting down; input is no longer admitted.
    Stopping,
    TooLarge {
        bytes: usize,
        limit: usize,
    },
    Busy {
        attempted_bytes: usize,
        pending_bytes: usize,
        limit_bytes: usize,
    },
}

impl std::fmt::Display for PasteRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsafe => formatter.write_str(
                "paste was rejected by Ghostty safety validation (it contains a newline or bracketed-paste terminator)",
            ),
            Self::Stopping => formatter.write_str(
                "paste was rejected because the Run is shutting down",
            ),
            Self::TooLarge { bytes, limit } => write!(
                formatter,
                "paste was rejected because it is {bytes} bytes; the prototype limit is {limit} bytes",
            ),
            Self::Busy {
                attempted_bytes,
                pending_bytes,
                limit_bytes,
            } => write!(
                formatter,
                "paste was rejected before admission: {attempted_bytes} bytes requested with {pending_bytes} of {limit_bytes} command bytes pending",
            ),
        }
    }
}

impl std::error::Error for PasteRejection {}

pub fn validate(data: &str) -> Result<(), PasteRejection> {
    let bytes = data.len();
    if bytes > PASTE_LIMIT_BYTES {
        return Err(PasteRejection::TooLarge {
            bytes,
            limit: PASTE_LIMIT_BYTES,
        });
    }
    if !paste::is_safe(data) {
        return Err(PasteRejection::Unsafe);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn accepts_ghostty_safe_text_within_limit() {
        assert_eq!(validate("paste me"), Ok(()));
    }

    #[test]
    fn rejects_newline_using_ghostty_validation() {
        assert_eq!(validate("first\nsecond"), Err(PasteRejection::Unsafe));
    }

    #[test]
    fn rejects_bracketed_paste_terminator_using_ghostty_validation() {
        assert_eq!(
            validate("prefix\x1b[201~suffix"),
            Err(PasteRejection::Unsafe)
        );
    }

    #[test]
    fn rejects_paste_before_it_can_be_partly_delivered() {
        let data = "x".repeat(PASTE_LIMIT_BYTES + 1);
        assert_eq!(
            validate(&data),
            Err(PasteRejection::TooLarge {
                bytes: PASTE_LIMIT_BYTES + 1,
                limit: PASTE_LIMIT_BYTES,
            })
        );
    }

    #[test]
    fn request_token_reports_delivery_and_owner_disconnect() {
        let (delivered_tx, delivered_rx) = mpsc::channel();
        let mut delivered = PasteRequest::new(7, delivered_rx);
        delivered_tx.send(Ok(())).unwrap();
        assert_eq!(delivered.poll(), Some(PasteCompletion::Delivered));

        let (failed_tx, failed_rx) = mpsc::channel();
        let mut failed = PasteRequest::new(8, failed_rx);
        drop(failed_tx);
        assert!(matches!(failed.poll(), Some(PasteCompletion::Failed(_))));
    }
}
