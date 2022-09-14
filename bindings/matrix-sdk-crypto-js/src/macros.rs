/// We have the following pattern quite often in our code:
///
/// ```rust
/// struct Foo {
///     inner: Bar,
/// }
///
/// impl From<Bar> for Foo {
///     fn from(inner: Bar) -> Self {
///         Self { inner }
///     }
/// }
/// ```
///
/// Because I feel lazy, let's do a macro to write this:
///
/// ```rust
/// struct Foo {
///     inner: Bar,
/// }
///
/// impl_from_to_inner!(Bar => Foo);
/// ```
#[macro_export]
macro_rules! impl_from_to_inner {
    ($from:ty => $to:ty) => {
        impl From<$from> for $to {
            fn from(inner: $from) -> Self {
                Self { inner }
            }
        }
    };
}
