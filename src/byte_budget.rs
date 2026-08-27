use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// An atomic byte limit shared by the two ends of one bounded queue.
#[derive(Clone)]
pub(crate) struct ByteBudget {
    pending: Arc<AtomicUsize>,
    limit: usize,
}

/// A tentative byte reservation.
///
/// Dropping this value releases the bytes. Call [`ByteReservation::commit`]
/// only after the queue accepts the item. The queue consumer must then call
/// [`ByteBudget::release`] at its existing completion boundary.
pub(crate) struct ByteReservation {
    budget: ByteBudget,
    amount: usize,
    pending_before: usize,
    release_on_drop: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ByteBudgetExceeded {
    pub(crate) pending_bytes: usize,
}

impl ByteBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            pending: Arc::new(AtomicUsize::new(0)),
            limit,
        }
    }

    pub(crate) fn reserve(&self, amount: usize) -> Result<ByteReservation, ByteBudgetExceeded> {
        let mut current = self.pending.load(Ordering::Acquire);
        loop {
            let Some(next) = current
                .checked_add(amount)
                .filter(|next| *next <= self.limit)
            else {
                return Err(ByteBudgetExceeded {
                    pending_bytes: current,
                });
            };
            match self.pending.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(ByteReservation {
                        budget: self.clone(),
                        amount,
                        pending_before: current,
                        release_on_drop: true,
                    });
                }
                Err(actual) => current = actual,
            }
        }
    }

    pub(crate) fn release(&self, amount: usize) {
        self.pending.fetch_sub(amount, Ordering::AcqRel);
    }

    pub(crate) fn pending_bytes(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }
}

impl ByteReservation {
    pub(crate) fn pending_before(&self) -> usize {
        self.pending_before
    }

    /// Keep the reservation charged after successful queue admission.
    pub(crate) fn commit(mut self) {
        self.release_on_drop = false;
    }
}

impl Drop for ByteReservation {
    fn drop(&mut self) {
        if self.release_on_drop {
            self.budget.release(self.amount);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    #[test]
    fn reservation_is_all_or_nothing_at_the_exact_limit() {
        let budget = ByteBudget::new(4);
        let reservation = budget.reserve(4).unwrap();

        assert_eq!(budget.pending_bytes(), 4);
        assert_eq!(
            budget.reserve(1).err(),
            Some(ByteBudgetExceeded { pending_bytes: 4 })
        );
        assert_eq!(budget.pending_bytes(), 4);

        drop(reservation);
        assert_eq!(budget.pending_bytes(), 0);
    }

    #[test]
    fn checked_add_rejects_overflow_without_changing_the_budget() {
        let budget = ByteBudget::new(usize::MAX);
        let reservation = budget.reserve(usize::MAX).unwrap();

        assert_eq!(
            budget.reserve(1).err(),
            Some(ByteBudgetExceeded {
                pending_bytes: usize::MAX,
            })
        );
        assert_eq!(budget.pending_bytes(), usize::MAX);

        drop(reservation);
        assert_eq!(budget.pending_bytes(), 0);
    }

    #[test]
    fn concurrent_reservations_never_exceed_the_limit() {
        const WORKERS: usize = 8;
        let budget = ByteBudget::new(8);
        let start = Arc::new(Barrier::new(WORKERS + 1));
        let release = Arc::new(Barrier::new(WORKERS + 1));
        let (results, received_results) = std::sync::mpsc::channel();
        let mut workers = Vec::new();

        for _ in 0..WORKERS {
            let budget = budget.clone();
            let start = Arc::clone(&start);
            let release = Arc::clone(&release);
            let results = results.clone();
            workers.push(std::thread::spawn(move || {
                start.wait();
                let reservation = budget.reserve(2).ok();
                results.send(reservation.is_some()).unwrap();
                release.wait();
            }));
        }
        drop(results);

        start.wait();
        let admitted = received_results
            .iter()
            .take(WORKERS)
            .map(usize::from)
            .sum::<usize>();
        assert_eq!(admitted, 4);
        assert_eq!(budget.pending_bytes(), 8);
        release.wait();

        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(budget.pending_bytes(), 0);
    }
}
