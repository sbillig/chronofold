//! # Chronofold
//!
//! Chronofold is a conflict-free replicated data structure (a.k.a. *CRDT*) for
//! versioned text.
//!
//! This crate aims to offer a fast implementation with an easy-to-use
//! `Vec`-like API. It should be near impossible to shoot yourself in the foot
//! and end up with corrupted or lost data.
//!
//! **Note:** We are not there yet! While this implementation should be
//! correct, it is not yet optimized for speed and memory usage. The API might
//! see some changes as we continue to explore different use cases.
//!
//! This implementation is based on ideas published in the paper ["Chronofold:
//! a data structure for versioned text"][paper] by Victor Grishchenko and
//! Mikhail Patrakeev. If you look for a formal introduction to what a
//! chronofold is, reading that excellent paper is highly recommended!
//!
//! [paper]: https://arxiv.org/abs/2002.09511
//!
//! # Example usage
//!
//! ```rust
//! use chronofold::{Chronofold, LogIndex, Op};
//!
//! type AuthorId = &'static str;
//!
//! // Alice creates a chronofold on her machine, makes some initial changes
//! // and sends a copy to Bob.
//! let mut cfold_a = Chronofold::<AuthorId, char>::default();
//! cfold_a.session("alice").extend("Hello chronfold!".chars());
//! let mut cfold_b = cfold_a.clone();
//!
//! // Alice adds some more text, ...
//! let ops_a: Vec<Op<AuthorId, char>> = {
//!     let mut session = cfold_a.session("alice");
//!     session.splice(
//!         LogIndex(15)..LogIndex(15),
//!         " - a data structure for versioned text".chars(),
//!     );
//!     session.iter_ops().collect()
//! };
//!
//! // ... while Bob fixes a typo.
//! let ops_b: Vec<Op<AuthorId, char>> = {
//!     let mut session = cfold_b.session("bob");
//!     session.insert_after(Some(LogIndex(10)), 'o');
//!     session.iter_ops().collect()
//! };
//!
//! // Now their respective states have diverged.
//! assert_eq!(
//!     "Hello chronfold - a data structure for versioned text!",
//!     format!("{}", cfold_a),
//! );
//! assert_eq!("Hello chronofold!", format!("{}", cfold_b));
//!
//! // As soon as both have seen all ops, their states have converged.
//! for op in ops_a {
//!     cfold_b.apply(op).unwrap();
//! }
//! for op in ops_b {
//!     cfold_a.apply(op).unwrap();
//! }
//! let final_text = "Hello chronofold - a data structure for versioned text!";
//! assert_eq!(final_text, format!("{}", cfold_a));
//! assert_eq!(final_text, format!("{}", cfold_b));
//! ```

// As we only have a handful of public items, we've decided to re-export
// everything in the crate root and keep our internal module structure
// private. This keeps things simple for our users and gives us more
// flexibility in restructuring the crate.
mod distributed;
mod error;
mod index;
mod iter;
mod session;
mod version;
pub use crate::distributed::*;
pub use crate::error::*;
pub use crate::index::*;
pub use crate::iter::*;
pub use crate::session::*;
pub use crate::version::*;

use std::fmt;

#[cfg(feature = "serde")]
#[macro_use]
extern crate serde;

/// An entry in the chronofold's log.
#[derive(PartialEq, Eq, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Change<T> {
    Insert(T),
    Delete,
}

/// A conflict-free replicated data structure for versioned sequences.
///
/// # Terminology
///
/// A chronofold can be regarded either as a log of changes or as a sequence of
/// elements. These two viewpoints require distinct terminology:
///
/// - A *log index* is a 0-based index in the log of changes. This indices are
///   stable (i.e. they stay the same after edits), but are subjective for
///   each author.
/// - An *element* is a visible (not yet deleted) value of type `T`.
/// - *Log order* refers to the chronological order in which changes were
///   added to the log. This order is subjective for each author.
/// - *Causal order* refers to the order of the linked list.
///
/// # Editing
///
/// You can edit a chronofold in two ways: Either by applying [`Op`]s, or by
/// creating a [`Session`] which has a `Vec`-like API.
///
/// # Indexing
///
/// Like [`Vec`], the `Chronofold` type allows to access values by index,
/// because it implements the [`Index`] trait. The same rules apply:
/// out-of-bound indexes cause panics, and you can use `get` to check whether
/// the index exists.
///
/// [`Vec`]: https://doc.rust-lang.org/std/vec/struct.Vec.html
/// [`Index`]: https://doc.rust-lang.org/std/ops/trait.Index.html
#[derive(PartialEq, Eq, Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Chronofold<A: Author, T> {
    log: Vec<Change<T>>,
    root: Option<LogIndex>,
    version: Version<A>,
    // TODO: Use sparse arrays for the following secondary logs and exclude the
    // trivial cases to save memory.
    next_indices: Vec<Option<LogIndex>>,
    timestamps: Vec<Timestamp<A>>,
    references: Vec<Option<LogIndex>>,
    deleted: Vec<bool>,
}

impl<A: Author, T: fmt::Debug> Chronofold<A, T> {
    /// Constructs a new, empty chronofold.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an editing session for a single author.
    pub fn session(&mut self, author: A) -> Session<'_, A, T> {
        Session::new(author, self)
    }

    pub(crate) fn next_log_index(&self) -> LogIndex {
        LogIndex(self.log.len())
    }

    /// Applies an op to the chronofold.
    pub fn apply(&mut self, op: Op<A, T>) -> Result<(), ChronofoldError<A, T>> {
        // Check if an op with the same id was applied already.
        // TODO: Consider adding an `apply_unchecked` variant to skip this
        // check.
        if self.log_index(&op.id).is_some() {
            return Err(ChronofoldError::ExistingTimestamp(op));
        }

        // Convert the reference timestamp, as all our internal functions work
        // with log indices.
        match op.reference {
            Some(t) => match self.log_index(&t) {
                Some(reference) => self
                    .apply_change(op.id, Some(reference), op.change)
                    .map(|_| ()),
                None => Err(ChronofoldError::UnknownReference(op)),
            },
            None => self.apply_change(op.id, None, op.change).map(|_| ()),
        }
    }

    pub(crate) fn apply_change(
        &mut self,
        id: Timestamp<A>,
        reference: Option<LogIndex>,
        change: Change<T>,
    ) -> Result<LogIndex, ChronofoldError<A, T>> {
        // Find the predecessor to `op`.
        let predecessor = if let Some(idx) = self
            .iter_log_indices_causal_range(..)
            .filter(|i| self.references[i.0] == reference)
            .filter(|i| self.timestamps[i.0] > id)
            .last()
        {
            self.iter_subtree(idx).last()
        } else {
            reference
        };

        // Set the predecessors next index to our new change's index while
        // keeping it's previous next index for ourselves.
        let new_index = LogIndex(self.log.len());
        let next_index;
        if let Some(idx) = predecessor {
            next_index = self.next_indices[idx.0];
            self.next_indices[idx.0] = Some(new_index);
        } else {
            next_index = self.root;
            self.root = Some(new_index);
        }

        // If `op` is a removal, mark the referenced change as deleted.
        if let (Some(idx), Change::Delete) = (reference, &change) {
            self.deleted[idx.0] = true;
        }

        // Append to the chronofold's log and secondary logs.
        self.log.push(change);
        self.next_indices.push(next_index);
        self.timestamps.push(id);
        self.references.push(reference);
        self.deleted.push(false);

        // Increment version.
        self.version.inc(&id);

        Ok(new_index)
    }

    /// Returns `true` if the chronofold contains no elements.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the number of elements in the chronofold.
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    /// Returns a reference to a change in the chronofold's log.
    ///
    /// If `index` is out of bounds, `None` is returned.
    pub fn get(&self, index: LogIndex) -> Option<&Change<T>> {
        self.log.get(index.0)
    }

    fn log_index(&self, timestamp: &Timestamp<A>) -> Option<LogIndex> {
        for i in (timestamp.0).0..self.log.len() {
            if self.timestamps[i] == *timestamp {
                return Some(LogIndex(i));
            }
        }
        None
    }
}

impl<A: Author, T> Default for Chronofold<A, T> {
    fn default() -> Self {
        Self {
            log: Vec::default(),
            root: None,
            version: Version::default(),
            next_indices: Vec::default(),
            timestamps: Vec::default(),
            references: Vec::default(),
            deleted: Vec::default(),
        }
    }
}

impl<A: Author, T: fmt::Display> fmt::Display for Chronofold<A, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            self.iter_elements()
                .fold("".to_owned(), |s, t| s + &t.to_string())
        )
    }
}
