//! Sparse, exact-size sets keep a single thin owner and allocate only on insertion.
//! This intentionally retains neither capacity nor a hash table between writes.
use super::smallbox::SmallBox;
#[repr(transparent)]
pub struct ExactSet<T> {
    values: Option<SmallBox<[T]>>,
}
impl<T> Default for ExactSet<T> {
    fn default() -> Self {
        Self { values: None }
    }
}
impl<T> ExactSet<T> {
    pub fn get(&self) -> Option<&[T]> {
        self.values.as_deref()
    }
    pub fn heap_bytes(&self) -> usize {
        self.values.as_ref().map_or(0, SmallBox::allocation_size)
    }
}
impl<T: Copy + Eq> ExactSet<T> {
    /// Duplicate references preserve the allocation, which may already be archived.
    pub fn insert(&mut self, value: T) -> bool {
        let previous = self.get().unwrap_or_default();
        if previous.contains(&value) {
            return false;
        }
        self.values = Some(SmallBox::from_box(
            previous.iter().copied().chain([value]).collect(),
        ));
        true
    }
}
