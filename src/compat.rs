// https://github.com/rust-lang/rust/issues/68125
pub trait FoldFirst {
    #[inline]
    fn fold_first_compat<F>(mut self, f: F) -> Option<Self::Item>
    where
        Self: Sized + Iterator,
        F: FnMut(Self::Item, Self::Item) -> Self::Item,
    {
        let first = self.next()?;
        Some(self.fold(first, f))
    }
}

impl<T, I> FoldFirst for I
where
    T: Sized,
    I: Iterator<Item = T>
{
}
