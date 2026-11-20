//! Saving a file in the background in the order its changes were made.
//!
//! Each change hands a snapshot to a background thread. They can finish in any order,
//! so a save older than the one already written is skipped: a slow save never puts back
//! what a newer one replaced.

use std::sync::{Arc, Mutex, PoisonError};

/// The saves of one file.
pub struct Saves<T> {
    /// The newest save written so far.
    written: Arc<Mutex<u64>>,
    made: u64,
    latest: Option<Save<T>>,
}

impl<T> Default for Saves<T> {
    fn default() -> Self {
        Self {
            written: Arc::default(),
            made: 0,
            latest: None,
        }
    }
}

impl<T: Clone> Saves<T> {
    /// A save of `snapshot`, newer than every one made before.
    pub fn save(&mut self, snapshot: T) -> Save<T> {
        self.made += 1;
        let save = Save {
            order: self.made,
            written: self.written.clone(),
            snapshot,
        };
        self.latest = Some(save.clone());
        save
    }

    /// The newest save made, again: when quitting, to be sure it is written. It does
    /// nothing if it already was.
    pub fn latest(&self) -> Option<Save<T>> {
        self.latest.clone()
    }
}

#[derive(Clone)]
pub struct Save<T> {
    order: u64,
    written: Arc<Mutex<u64>>,
    snapshot: T,
}

impl<T> Save<T> {
    /// Writes the snapshot with `write`, unless a newer save already has; then it
    /// returns the default. Blocks, so it runs off the UI thread.
    pub fn run<R: Default>(self, write: impl FnOnce(&T) -> anyhow::Result<R>) -> anyhow::Result<R> {
        let mut written = self.written.lock().unwrap_or_else(PoisonError::into_inner);
        if self.order <= *written {
            return Ok(R::default());
        }
        let result = write(&self.snapshot)?;
        *written = self.order;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::Saves;

    fn run(save: super::Save<&'static str>, file: &mut Vec<&'static str>) {
        save.run(|text| {
            file.push(text);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn an_older_save_finishing_last_is_skipped() {
        let mut saves = Saves::default();
        let (older, newer) = (saves.save("old"), saves.save("new"));
        let mut file = Vec::new();
        run(newer, &mut file);
        run(older, &mut file);
        assert_eq!(file, ["new"]);
    }

    #[test]
    fn saves_in_order_are_all_written_and_the_latest_only_once() {
        let mut saves = Saves::default();
        let mut file = Vec::new();
        run(saves.save("a"), &mut file);
        let b = saves.save("b");
        assert!(saves.latest().is_some());
        run(b, &mut file);
        run(saves.latest().unwrap(), &mut file);
        assert_eq!(file, ["a", "b"]);
    }

    #[test]
    fn a_failed_save_can_be_tried_again() {
        let mut saves = Saves::default();
        let save = saves.save("a");
        assert!(
            save.clone()
                .run::<()>(|_| anyhow::bail!("disk full"))
                .is_err()
        );
        let mut file = Vec::new();
        run(save, &mut file);
        assert_eq!(file, ["a"]);
    }
}
