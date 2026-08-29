//! Token budget — non-silent enforcement.

use crate::workflow::WorkflowError;

pub struct Budget {
    total: Option<u64>,
    spent: u64,
}

impl Budget {
    pub fn new(total: Option<u64>) -> Self {
        Self { total, spent: 0 }
    }

    pub fn add(&mut self, n: u64) -> Result<(), WorkflowError> {
        if let Some(total) = self.total {
            if self.spent + n > total {
                return Err(WorkflowError::BudgetExceeded {
                    spent: self.spent + n,
                    total,
                });
            }
        }
        self.spent += n;
        Ok(())
    }

    pub fn spent(&self) -> u64 {
        self.spent
    }

    pub fn remaining(&self) -> u64 {
        match self.total {
            Some(total) => total.saturating_sub(self.spent),
            None => u64::MAX,
        }
    }

    pub fn limit(&self) -> Option<u64> {
        self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_exceeds() {
        let mut b = Budget::new(None);
        assert!(b.add(1_000_000).is_ok());
        assert_eq!(b.spent(), 1_000_000);
        assert_eq!(b.remaining(), u64::MAX);
    }

    #[test]
    fn limited_exceeds_after_total() {
        let mut b = Budget::new(Some(8));
        assert!(b.add(5).is_ok());
        assert!(b.add(5).is_err()); // 10 > 8
        assert_eq!(b.spent(), 5);
        assert_eq!(b.remaining(), 3);
    }
}
