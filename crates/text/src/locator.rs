use smallvec::SmallVec;
use std::iter;

/// An identifier for a position in a ordered collection.
///
/// Allows prepending and appending without needing to renumber existing locators
/// using `Locator::between(lhs, rhs)`.
///
/// The initial location for a collection should be `Locator::between(Locator::min(), Locator::max())`,
/// leaving room for items to be inserted before and after it.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Locator(SmallVec<[u64; 2]>);

impl Clone for Locator {
    fn clone(&self) -> Self {
        // We manually implement clone to avoid the overhead of SmallVec's clone implementation.
        // Using `from_slice` is faster than `clone` for SmallVec as we can use our `Copy` implementation of u64.
        Self {
            0: SmallVec::from_slice(&self.0),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.0.clone_from(&source.0);
    }
}

impl Locator {
    pub const fn min() -> Self {
        // SAFETY: 1 is <= 2
        Self(unsafe { SmallVec::from_const_with_len_unchecked([u64::MIN; 2], 1) })
    }

    pub const fn max() -> Self {
        // SAFETY: 1 is <= 2
        Self(unsafe { SmallVec::from_const_with_len_unchecked([u64::MAX; 2], 1) })
    }

    pub const fn min_ref() -> &'static Self {
        const { &Self::min() }
    }

    pub const fn max_ref() -> &'static Self {
        const { &Self::max() }
    }

    pub fn assign(&mut self, other: &Self) {
        self.0.resize(other.0.len(), 0);
        self.0.copy_from_slice(&other.0);
    }

    pub fn between(lhs: &Self, rhs: &Self) -> Self {
        let lhs = lhs.0.iter().copied().chain(iter::repeat(u64::MIN));
        let rhs = rhs.0.iter().copied().chain(iter::repeat(u64::MAX));
        let mut location = SmallVec::new();
        for (lhs, rhs) in lhs.zip(rhs) {
            // This shift is essential! It optimizes for the common case of sequential typing.
            let mid = lhs + ((rhs.saturating_sub(lhs)) >> 48);
            location.push(mid);
            if mid > lhs {
                break;
            }
        }
        Self(location)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for Locator {
    fn default() -> Self {
        Self::min()
    }
}

impl sum_tree::Item for Locator {
    type Summary = Locator;

    fn summary(&self, _cx: ()) -> Self::Summary {
        self.clone()
    }
}

impl sum_tree::KeyedItem for Locator {
    type Key = Locator;

    fn key(&self) -> Self::Key {
        self.clone()
    }
}

impl sum_tree::ContextLessSummary for Locator {
    fn zero() -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &Self) {
        self.assign(summary);
    }
}

