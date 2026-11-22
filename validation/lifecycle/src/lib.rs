//! Offline verifier for synthetic lifecycle receipts.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Receipt {
    Stop { session: u32, request: u32 },
    Confirm { session: u32, request: u32 },
    Drain { session: u32 },
    Seal { session: u32 },
    ExitObserved { session: u32, code: u32 },
    ExitTimeout { session: u32 },
    AckMissing { session: u32 },
    AckDegraded { session: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitWitness {
    Pending,
    Observed(u32),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Applied,
    Duplicate,
    OtherSession,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub receipt: Receipt,
    pub outcome: Outcome,
}

#[derive(Debug)]
pub struct Verifier {
    session: u32,
    stop: Option<u32>,
    confirmed: bool,
    drained: bool,
    sealed: bool,
    exit: ExitWitness,
    trace: Vec<Entry>,
}

impl Verifier {
    pub fn new(session: u32) -> Self {
        Self {
            session,
            stop: None,
            confirmed: false,
            drained: false,
            sealed: false,
            exit: ExitWitness::Pending,
            trace: Vec::new(),
        }
    }

    pub fn accept(&mut self, receipt: Receipt) -> Outcome {
        let session = match receipt {
            Receipt::Stop { session, .. }
            | Receipt::Confirm { session, .. }
            | Receipt::Drain { session }
            | Receipt::Seal { session }
            | Receipt::ExitObserved { session, .. }
            | Receipt::ExitTimeout { session }
            | Receipt::AckMissing { session }
            | Receipt::AckDegraded { session } => session,
        };
        let outcome = if session != self.session {
            Outcome::OtherSession
        } else {
            match receipt {
                Receipt::Stop { request, .. } => match self.stop {
                    None => {
                        self.stop = Some(request);
                        Outcome::Applied
                    }
                    Some(existing) if existing == request => Outcome::Duplicate,
                    Some(_) => Outcome::Rejected,
                },
                Receipt::Confirm { request, .. } if self.stop == Some(request) => {
                    if self.confirmed {
                        Outcome::Duplicate
                    } else {
                        self.confirmed = true;
                        Outcome::Applied
                    }
                }
                Receipt::Drain { .. } if self.confirmed => {
                    if self.drained {
                        Outcome::Duplicate
                    } else {
                        self.drained = true;
                        Outcome::Applied
                    }
                }
                Receipt::Seal { .. } if self.drained => {
                    if self.sealed {
                        Outcome::Duplicate
                    } else {
                        self.sealed = true;
                        Outcome::Applied
                    }
                }
                Receipt::ExitObserved { code, .. } if self.exit == ExitWitness::Pending => {
                    self.exit = ExitWitness::Observed(code);
                    Outcome::Applied
                }
                Receipt::ExitTimeout { .. } if self.exit == ExitWitness::Pending => {
                    self.exit = ExitWitness::Unknown;
                    Outcome::Applied
                }
                Receipt::AckMissing { .. } | Receipt::AckDegraded { .. } => Outcome::Applied,
                _ => Outcome::Rejected,
            }
        };
        self.trace.push(Entry { receipt, outcome });
        outcome
    }

    pub fn sealed(&self) -> bool {
        self.sealed
    }
    pub fn exit(&self) -> ExitWitness {
        self.exit
    }
    pub fn trace(&self) -> &[Entry] {
        &self.trace
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const S: u32 = 1;
    fn stop() -> Receipt {
        Receipt::Stop {
            session: S,
            request: 7,
        }
    }
    fn confirm() -> Receipt {
        Receipt::Confirm {
            session: S,
            request: 7,
        }
    }
    fn drain() -> Receipt {
        Receipt::Drain { session: S }
    }
    fn seal() -> Receipt {
        Receipt::Seal { session: S }
    }
    fn sealed() -> Verifier {
        let mut v = Verifier::new(S);
        for r in [stop(), confirm(), drain(), seal()] {
            assert_eq!(v.accept(r), Outcome::Applied);
        }
        v
    }
    #[test]
    fn ordered_receipts_and_independent_exit() {
        let mut v = sealed();
        assert!(v.sealed());
        assert_eq!(v.exit(), ExitWitness::Pending);
        assert_eq!(
            v.accept(Receipt::ExitObserved {
                session: S,
                code: 7
            }),
            Outcome::Applied
        );
        assert_eq!(v.exit(), ExitWitness::Observed(7));
        assert_eq!(
            v.accept(Receipt::ExitTimeout { session: S }),
            Outcome::Rejected
        );
        assert_eq!(v.exit(), ExitWitness::Observed(7));
    }
    #[test]
    fn timeout_is_unknown_not_exit() {
        let mut v = sealed();
        assert_eq!(
            v.accept(Receipt::ExitTimeout { session: S }),
            Outcome::Applied
        );
        assert_eq!(v.exit(), ExitWitness::Unknown);
    }
    #[test]
    fn missing_or_mismatched_confirmation_cannot_complete() {
        let mut v = Verifier::new(S);
        v.accept(stop());
        assert_eq!(
            v.accept(Receipt::AckMissing { session: S }),
            Outcome::Applied
        );
        assert_eq!(
            v.accept(Receipt::Confirm {
                session: S,
                request: 8
            }),
            Outcome::Rejected
        );
        assert_eq!(v.accept(drain()), Outcome::Rejected);
        assert_eq!(v.accept(seal()), Outcome::Rejected);
        assert!(!v.sealed());
        assert_eq!(v.trace().len(), 5);
    }
    #[test]
    fn repeated_stop_has_one_effect() {
        let mut v = Verifier::new(S);
        assert_eq!(v.accept(stop()), Outcome::Applied);
        assert_eq!(v.accept(stop()), Outcome::Duplicate);
        assert_eq!(
            v.accept(Receipt::Stop {
                session: S,
                request: 8
            }),
            Outcome::Rejected
        );
        for r in [confirm(), drain(), seal()] {
            assert_eq!(v.accept(r), Outcome::Applied);
        }
        assert!(v.sealed());
    }
    #[test]
    fn out_of_order_receipts_do_not_advance() {
        let mut v = Verifier::new(S);
        for r in [confirm(), drain(), seal()] {
            assert_eq!(v.accept(r), Outcome::Rejected);
        }
        v.accept(stop());
        assert_eq!(v.accept(seal()), Outcome::Rejected);
        v.accept(confirm());
        assert_eq!(v.accept(seal()), Outcome::Rejected);
        assert!(!v.sealed());
        assert_eq!(v.accept(drain()), Outcome::Applied);
        assert_eq!(v.accept(seal()), Outcome::Applied);
    }
    #[test]
    fn degraded_ack_is_traceable_but_not_confirmation() {
        let mut v = Verifier::new(S);
        v.accept(stop());
        let degraded = Receipt::AckDegraded { session: S };
        assert_eq!(v.accept(degraded), Outcome::Applied);
        assert_eq!(v.accept(drain()), Outcome::Rejected);
        assert_eq!(
            v.trace()[1],
            Entry {
                receipt: degraded,
                outcome: Outcome::Applied
            }
        );
        assert_eq!(v.accept(confirm()), Outcome::Applied);
        assert_eq!(v.accept(drain()), Outcome::Applied);
        assert_eq!(v.accept(seal()), Outcome::Applied);
    }
    #[test]
    fn other_session_cannot_advance_own_state() {
        let mut v = Verifier::new(S);
        assert_eq!(
            v.accept(Receipt::Stop {
                session: 2,
                request: 7
            }),
            Outcome::OtherSession
        );
        assert_eq!(v.accept(confirm()), Outcome::Rejected);
        assert!(!v.sealed());
        assert_eq!(v.trace()[0].outcome, Outcome::OtherSession);
    }
}
