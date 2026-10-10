use std::collections::BTreeMap;
use std::default::Default;
use std::io;

// Iterator that sorts enumerated elements
// by their index
//
// This is useful for sorting elements
// that were processed by a pool of workers
// and not necessarily returned in order.
//
// Note: `early_items` is unbounded, which
// shouldn't be a problem if pool of workers
// picks elements in order - the distortions
// should be minimal anyway.
pub struct SortingIterator<T, I> {
    early_items: BTreeMap<u64, T>,
    iter: I,
    next_i: u64,
}

impl<T, I> SortingIterator<T, I> {
    pub fn new(iter: I) -> Self {
        SortingIterator {
            iter,
            early_items: Default::default(),
            next_i: 0,
        }
    }
}

impl<T, I> Iterator for SortingIterator<T, I>
where
    I: Iterator<Item = (u64, T)>,
{
    type Item = io::Result<T>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.early_items.remove(&self.next_i) {
                self.next_i += 1;
                return Some(Ok(item));
            }

            if let Some((i, item)) = self.iter.next() {
                if i == self.next_i {
                    self.next_i += 1;
                    return Some(Ok(item));
                } else {
                    if i < self.next_i || self.early_items.contains_key(&i) {
                        return Some(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "duplicate chunk completion",
                        )));
                    }
                    self.early_items.insert(i, item);
                }
            } else {
                if !self.early_items.is_empty() {
                    self.early_items.clear();
                    return Some(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "chunk completion is missing",
                    )));
                }
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SortingIterator;
    #[test]
    fn out_of_order_completions_preserve_input_order() {
        let items =
            SortingIterator::new([(2, "c"), (0, "a"), (1, "b")].into_iter());
        assert_eq!(
            items.collect::<std::io::Result<Vec<_>>>().unwrap(),
            ["a", "b", "c"]
        );
    }
    #[test]
    fn a_missing_completion_is_an_error() {
        let mut items = SortingIterator::new([(1, "b")].into_iter());
        assert_eq!(
            items.next().unwrap().unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }
}
