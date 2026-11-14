//! Esquie runs one installation or update job at a time: both write the same
//! executable and integration, so they must never interleave.
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Job {
    Installation,
    Update,
}

#[derive(Default)]
pub struct Jobs(Mutex<Option<Job>>);

/// Held for the whole job, on whichever thread does the work. Dropping it
/// frees the slot; drop it before reporting completion to the UI, so the next
/// job can start as soon as the user sees this one finish.
#[must_use]
pub struct Guard(Arc<Jobs>);

impl Jobs {
    fn slot(&self) -> MutexGuard<'_, Option<Job>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Claim the slot, or name the job that already holds it.
    pub fn begin(self: &Arc<Self>, job: Job) -> Result<Guard, Job> {
        let mut slot = self.slot();
        match *slot {
            Some(running) => Err(running),
            None => {
                *slot = Some(job);
                Ok(Guard(self.clone()))
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        *self.0.slot() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_job_at_a_time() {
        let jobs = Arc::new(Jobs::default());
        let update = jobs.begin(Job::Update).unwrap();
        assert_eq!(jobs.begin(Job::Update).err(), Some(Job::Update));
        assert_eq!(jobs.begin(Job::Installation).err(), Some(Job::Update));
        drop(update);
        let install = jobs.begin(Job::Installation).unwrap();
        // Released from another thread, as the worker threads do.
        std::thread::spawn(move || drop(install)).join().unwrap();
        assert!(jobs.begin(Job::Update).is_ok());
    }
}
