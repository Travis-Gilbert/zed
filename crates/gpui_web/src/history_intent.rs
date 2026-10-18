//! One browser history command may describe several native undo records.
//!
//! WebKit emits all of a command's beforeinput records, then its matching
//! input records. Dispatch the document command once while accounting for
//! every native record. Draining the batch admits another command immediately,
//! even in the same JS job. A deferred expiry only retires an incomplete batch
//! whose cancelable beforeinput never received its input counterpart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistoryDirection {
    Undo,
    Redo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Before,
    Input,
}
#[derive(Clone, Copy, Debug)]
struct Batch {
    direction: HistoryDirection,
    remaining: u32,
    phase: Phase,
    generation: u64,
}
#[derive(Default)]
pub(crate) struct HistoryIntentGate {
    batch: Option<Batch>,
    generation: u64,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HistoryAdmission {
    pub dispatch: bool,
    pub expire: Option<u64>,
}
impl HistoryIntentGate {
    pub(crate) fn before(&mut self, direction: HistoryDirection) -> HistoryAdmission {
        if let Some(batch) = self.batch.as_mut()
            && batch.direction == direction
            && batch.phase == Phase::Before
        {
            batch.remaining = batch.remaining.saturating_add(1);
            return HistoryAdmission {
                dispatch: false,
                expire: None,
            };
        }
        self.generation = self.generation.saturating_add(1);
        self.batch = Some(Batch {
            direction,
            remaining: 1,
            phase: Phase::Before,
            generation: self.generation,
        });
        HistoryAdmission {
            dispatch: true,
            expire: Some(self.generation),
        }
    }
    pub(crate) fn input(&mut self, direction: HistoryDirection) -> bool {
        if let Some(batch) = self.batch.as_mut()
            && batch.direction == direction
        {
            batch.remaining -= 1;
            batch.phase = Phase::Input;
            if batch.remaining == 0 {
                self.batch = None;
            }
            return false;
        }
        // A browser/extension may omit beforeinput entirely. Its input event
        // is still a command and must not import the native history's text.
        true
    }
    pub(crate) fn expire(&mut self, generation: u64) {
        if self
            .batch
            .as_ref()
            .is_some_and(|batch| batch.generation == generation)
        {
            self.batch = None;
        }
    }
    pub(crate) fn clear(&mut self) {
        self.batch = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn command(gate: &mut HistoryIntentGate, direction: HistoryDirection, records: usize) -> usize {
        let before = (0..records)
            .filter(|_| gate.before(direction).dispatch)
            .count();
        before + (0..records).filter(|_| gate.input(direction)).count()
    }
    #[test]
    fn recorded_webkit_six_before_six_input_is_one_document_command() {
        let mut gate = HistoryIntentGate::default();
        assert_eq!(command(&mut gate, HistoryDirection::Undo, 6), 1);
        assert_eq!(command(&mut gate, HistoryDirection::Redo, 6), 1);
    }
    #[test]
    fn separate_commands_in_one_job_are_not_collapsed() {
        let mut gate = HistoryIntentGate::default();
        assert_eq!(command(&mut gate, HistoryDirection::Undo, 6), 1);
        assert_eq!(command(&mut gate, HistoryDirection::Undo, 6), 1);
        assert_eq!(command(&mut gate, HistoryDirection::Redo, 1), 1);
    }
    #[test]
    fn canceled_beforeinput_expires_without_retiring_a_successor() {
        let mut gate = HistoryIntentGate::default();
        let canceled = gate.before(HistoryDirection::Undo);
        gate.expire(canceled.expire.unwrap());
        let next = gate.before(HistoryDirection::Undo);
        assert!(next.dispatch);
        gate.expire(canceled.expire.unwrap());
        assert!(!gate.input(HistoryDirection::Undo));
        assert_eq!(command(&mut gate, HistoryDirection::Undo, 1), 1);
    }
    #[test]
    fn input_only_fallback_is_a_command_and_directions_are_distinct() {
        let mut gate = HistoryIntentGate::default();
        assert!(gate.input(HistoryDirection::Undo));
        assert!(gate.input(HistoryDirection::Redo));
        assert!(gate.before(HistoryDirection::Undo).dispatch);
        assert!(gate.before(HistoryDirection::Redo).dispatch);
        assert!(!gate.input(HistoryDirection::Redo));
        gate.clear();
        assert_eq!(command(&mut gate, HistoryDirection::Undo, 1), 1);
    }
}
