use std::sync::mpsc::{Receiver, TryRecvError};

/// Maximum size of one paste accepted by the prototype.
///
/// This limit is smaller than the bounded PTY input queue. The encoded form of
/// one accepted paste therefore fits within the queue limit; a paste that is
/// too large is rejected before it reaches the terminal owner.
pub const PASTE_LIMIT_BYTES: usize = 64 * 1_024;
pub(crate) const PASTE_FRAMING_BYTES: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasteCompletion {
    Delivered,
    Refused,
    Failed(String),
}

pub struct PasteRequest {
    id: u64,
    completion: Receiver<PasteCompletion>,
    finished: bool,
}

impl PasteRequest {
    pub(crate) fn new(id: u64, completion: Receiver<PasteCompletion>) -> Self {
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
            Ok(outcome) => {
                self.finished = true;
                Some(outcome)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.finished = true;
                Some(PasteCompletion::Failed(
                    "paste completion channel closed before delivery was confirmed".to_string(),
                ))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteRejection {
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
            Self::Stopping => {
                formatter.write_str("paste was rejected because the Run is shutting down")
            }
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

pub(crate) fn admit_size(data: &str) -> Result<(), PasteRejection> {
    let bytes = data.len();
    if bytes > PASTE_LIMIT_BYTES {
        return Err(PasteRejection::TooLarge {
            bytes,
            limit: PASTE_LIMIT_BYTES,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn rejects_paste_before_it_can_be_partly_delivered() {
        let data = "x".repeat(PASTE_LIMIT_BYTES + 1);
        assert_eq!(
            admit_size(&data),
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
        delivered_tx.send(PasteCompletion::Delivered).unwrap();
        assert_eq!(delivered.poll(), Some(PasteCompletion::Delivered));

        let (failed_tx, failed_rx) = mpsc::channel();
        let mut failed = PasteRequest::new(8, failed_rx);
        drop(failed_tx);
        assert!(matches!(failed.poll(), Some(PasteCompletion::Failed(_))));
    }
}
