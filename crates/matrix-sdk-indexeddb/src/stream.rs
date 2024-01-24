//! `Stream` implementations to work on large collections of values from an
//! `object_store`.

use std::{
    future::Future,
    marker::PhantomPinned,
    num::NonZeroUsize,
    pin::Pin,
    ptr::NonNull,
    task::{ready, Context, Poll},
};

use futures_util::Stream;
use indexed_db_futures::prelude::*;
use pin_project_lite::pin_project;
use wasm_bindgen::JsValue;
use web_sys::{DomException, IdbKeyRange};

pin_project! {
    /// `StreamByCursor` is an async iterator (a [`Stream`]) that traverses an
    /// [`IdbObjectStore`] by using a cursor ([`IdbCursorWithValue`]).
    ///
    /// Such async iterator is helpful when reading a lot of data from an IndexedDB
    /// object store. With [`StreamExt`], one can easily map the results from the
    /// database, chunk them, filter them etc., without having to pay the price of
    /// loading all the values in memory before manipulating them. since the
    /// `Stream::Item` implementation is a `Result<(JsValue, JsValue),
    /// DomException>`, [`TryStreamExt`] can be used too.
    /// In `(JsValue, JsValue)`, the first value represents the [cursor's key][key],
    /// the second value represents the [cursor's value][value].
    ///
    /// [`StreamExt`]: futures_utils::StreamExt
    /// [`TryStreamExt`]: futures_utils::TryStreamExt
    /// [key]: https://developer.mozilla.org/en-US/docs/Web/API/IDBCursor/key
    /// [value]: https://developer.mozilla.org/en-US/docs/Web/API/IDBCursorWithValue/value
    ///
    /// ```rust,no_run
    /// use futures_util::TryStreamExt;
    /// use indexed_db_futures::prelude::*;
    /// use matrix_sdk_indexeddb::stream::StreamByCursor;
    /// use wasm_bindgen::JsValue;
    /// use web_sys::DomException;
    ///
    /// async fn example(db: &IdbDatabase) -> Result<(), DomException> {
    ///     let transaction = db.transaction_on_one("foo")?;
    ///     let object_store = transaction.object_store("foo")?;
    ///     let stream = StreamByCursor::new(&object_store).await?;
    ///
    ///     let results = stream
    ///         .map_ok(|(_key, value)| value)
    ///         .try_chunks(2)
    ///         .try_collect::<Vec<_>>()
    ///         .await;
    ///
    ///     assert_eq!(
    ///         results,
    ///         Ok(vec![
    ///             vec![JsValue::from_f64(0.), JsValue::from_f64(1.),],
    ///             vec![JsValue::from_f64(2.), JsValue::from_f64(3.),],
    ///             vec![JsValue::from_f64(4.),]
    ///         ])
    ///     );
    ///
    ///     Ok(())
    /// }
    /// ```
    pub struct StreamByCursor<'a> {
        // The current cursor.
        cursor: Option<IdbCursorWithValue<'a, IdbObjectStore<'a>>>,

        // When asking for the next cursor, the [`IdbCursorWithValue::continue_cursor`]
        // method has to be called. It's an async method. Thus, in the `Stream`
        // implementation of `Self`, this future must be stored to be polled manually.
        continue_cursor_future: Option<Pin<Box<dyn Future<Output = Result<bool, DomException>> + 'a>>>,

        // The cursor API is designed in a way that the first item is already fetched
        // when the cursor is created. Because `Stream::poll_next` is designed the other
        // way —no item is fetched before `poll_next` is called—, the first item must be
        // stored somewhere. Here is a good place.
        first_item: Option<(JsValue, JsValue)>,
    }
}

impl<'a> StreamByCursor<'a> {
    /// Build a new `StreamByCursor`.
    ///
    /// It takes a reference to an [`IdbObjectStore`].
    ///
    /// The cursor is opened by this method.
    pub async fn new(object_store: &'a IdbObjectStore<'a>) -> Result<Self, DomException> {
        let cursor = object_store.open_cursor()?.await?;

        Ok(Self::new_with_cursor(cursor))
    }

    // Helper for `Self` and for `StreamByRenewedCursor`.
    fn new_with_cursor(cursor: Option<IdbCursorWithValue<'a, IdbObjectStore<'a>>>) -> Self {
        let first_item = cursor.as_ref().and_then(|cursor| cursor.key().zip(Some(cursor.value())));

        Self { cursor, continue_cursor_future: None, first_item }
    }
}

impl<'a> Stream for StreamByCursor<'a> {
    type Item = Result<(JsValue, JsValue), DomException>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.project();

        // Let's get the cursor.
        let Some(cursor) = &this.cursor else {
            // No cursor? It means no matching records is found.
            // There is no possible value. Let's close the `Stream`.
            return Poll::Ready(None);
        };

        // A first result is ready. Let's return it.
        // This operation happens once.
        if let Some(value) = this.first_item.take() {
            return Poll::Ready(Some(Ok(value)));
        }

        // The first value has been consumed. Let's poll for the next one?
        //
        // `continue_cursor` has not been called yet. Let's call it.
        if this.continue_cursor_future.is_none() {
            *this.continue_cursor_future = Some(Box::pin(cursor.continue_cursor()?));
        }

        // Now that `continue_cursor` has been called, we can poll its future.
        //
        // It returns a `bool`: `true` if a next value exists, `false` otherwise.
        if ready!(this
            .continue_cursor_future
            .as_mut()
            // SAFETY: The case where `continue_cursor_future` is `None` has been handled
            // hereinabove.
            .unwrap()
            .as_mut()
            .poll(cx))?
        {
            // At this point, we have a value ready to be returned. The
            // `continue_cursor_future` can be dropped.
            *this.continue_cursor_future = None;

            Poll::Ready(Some(Ok((
                cursor.key().unwrap_or_else(|| JsValue::UNDEFINED),
                cursor.value(),
            ))))
        } else {
            // It doesn't have a new value. End of the cursor. End of the stream.
            Poll::Ready(None)
        }
    }
}

pin_project! {
    /// `StreamByRenewedCursor` is an async iterator (a [`Stream`]) that traverses
    /// an [`IdbObjectStore`] by using a cursor ([`IdbCursorWithValue`]) and a range
    /// ([`IdbKeyRange`]).
    ///
    /// Why a range? What is renewed? Well, that's kind of complex. An
    /// [`IdbTransaction`] is dropped by the runtime (most of the time, the
    /// JavaScript runtime) when the transaction is considered “idle”. It's hard to
    /// estimate when a transaction will be dropped, and it can happen in
    /// circumstances out of control. One solution to this is to _renew_ the
    /// transaction every time _x_ values are fetched from it. The lower _x_ is, the
    /// higher the probability for the transaction to not be dropped is.
    ///
    /// Then. What is renewed? The [`IdbTransaction`]. How? With a _transaction
    /// builder_: a closure than generates an [`IdbTransaction`]. Why a range?
    /// Because when a transaction is renewed, the cursor must be re-positioned to the
    /// previous position.
    ///
    /// Such async iterator is helpful when reading a lot of data from an IndexedDB
    /// object store. With [`StreamExt`], one can easily map the results from the
    /// database, chunk them, filter them etc., without having to pay the price of
    /// loading all the values in memory before manipulating them. since the
    /// `Stream::Item` implementation is a `Result<(JsValue, JsValue),
    /// DomException>`, [`TryStreamExt`] can be used too.
    /// In `(JsValue, JsValue)`, the first value represents the [cursor's key][key],
    /// the second value represents the [cursor's value][value].
    ///
    /// [`StreamExt`]: futures_utils::StreamExt
    /// [`TryStreamExt`]: futures_utils::TryStreamExt
    /// [key]: https://developer.mozilla.org/en-US/docs/Web/API/IDBCursor/key
    /// [value]: https://developer.mozilla.org/en-US/docs/Web/API/IDBCursorWithValue/value
    ///
    /// ```rust,no_run
    /// use std::num::NonZeroUsize;
    ///
    /// use futures_util::TryStreamExt;
    /// use indexed_db_futures::prelude::*;
    /// use matrix_sdk_indexeddb::stream::StreamByRenewedCursor;
    /// use wasm_bindgen::JsValue;
    /// use web_sys::DomException;
    ///
    /// async fn example(db: &IdbDatabase) -> Result<(), DomException> {
    ///     let stream = StreamByRenewedCursor::new(
    ///         // Transaction builder.
    ///         || db.transaction_on_one("foo"),
    ///         // The `object_store` name.
    ///         "foo".to_owned(),
    ///         // Number of values to successfully fetch before renewing the transaction.
    ///         NonZeroUsize::new(10).unwrap(),
    ///     ).await?;
    ///
    ///     let results = stream
    ///         .map_ok(|(_key, value)| value)
    ///         .try_chunks(2)
    ///         .try_collect::<Vec<_>>()
    ///         .await;
    ///
    ///     assert_eq!(
    ///         results,
    ///         Ok(vec![
    ///             vec![JsValue::from_f64(0.), JsValue::from_f64(1.),],
    ///             vec![JsValue::from_f64(2.), JsValue::from_f64(3.),],
    ///             vec![JsValue::from_f64(4.),]
    ///         ])
    ///     );
    ///
    ///     Ok(())
    /// }
    /// ```
    pub struct StreamByRenewedCursor<'a, F> {
        // The closure that is used to generate a new [`IdbTransaction`].
        transaction_builder: F,

        // The `object_store` name.
        object_store_name: String,

        // The amount of values to fetch from the `object_store` before renewing the
        // [`IdbTransaction`]. This value will never change.
        renew_every: usize,

        // The number of values that can still be fetched before renewing the [`IdbTransaction`].
        // This value changes every time a valid value is polled.
        renew_in: usize,

        // The latest known valid key.
        // This value changes every time a valid value is polled. It's helpful to reposition the cursor
        // when an [`IdbTransaction`] is renwed.
        latest_key: JsValue,

        // Explanations regarding the next 4 fields
        //
        // The types of [`indexedb_db_futures`] are difficult to store because there is a global
        // lifetime across all the types. To get an `IdbObjectStore`, we need an `IdbTransaction`. If
        // we store `IdbObjectStore` alone, the `IdbTransaction` will be dropped, thus not having a
        // long enough lifetime. On the opposite, if we store `IdbTransaction`, the reference is moved
        // inside the `IdbObjectStore`, which forbids to move the owned `IdbTransaction`. To solve
        // that, we use self-referential fields, one for the owned value, one for the reference for the
        // owned value. It's not ideal, but at least it works!

        // The `latest_transaction`.
        #[pin]
        latest_transaction: IdbTransaction<'a>,

        // The self-reference to `Self::latest_transaction`.
        latest_transaction_ptr: NonNull<IdbTransaction<'a>>,

        // The `latest_object_store`.
        #[pin]
        latest_object_store: Option<IdbObjectStore<'a>>,

        // The self-reference to `Self::latest_object_store`.
        latest_object_store_ptr: NonNull<Option<IdbObjectStore<'a>>>,

        // The inner stream, aka [`StreamByCursor`].
        #[pin]
        inner_stream: Option<StreamByCursor<'a>>,

        // When asking for a new cursor, the [`IdbObjectStore::open_cursor_with_range`]
        // method has to be called. It's an async method. Thus, in the `Stream`
        // implementation of `Self`, this future must be stored to be polled manually.
        cursor_future: Option<Pin<Box<IdbCursorWithValueFuture<'a, IdbObjectStore<'a>>>>>,

        // The entire struct must be unmovable. Let's use a `PhantomPinned` so that it cannot implement
        // `Unpin`.
        _pin: PhantomPinned,
    }
}

impl<'a, F> StreamByRenewedCursor<'a, F>
where
    F: FnMut() -> Result<IdbTransaction<'a>, DomException>,
{
    /// Build a new `StreamByRenewdCursor`.
    ///
    /// It takes a `transaction_builder`, an `object_store_name`, and a
    /// `renew_every`. See the documentation of [`Self`] to learn more.
    pub async fn new(
        mut transaction_builder: F,
        object_store_name: String,
        renew_every: NonZeroUsize,
    ) -> Result<Pin<Box<Self>>, DomException> {
        let transaction = transaction_builder()?;
        let latest_key = JsValue::from_str("");
        let after_latest_key = IdbKeyRange::lower_bound_with_open(&latest_key, true)?;

        let this = Self {
            transaction_builder,
            object_store_name,
            renew_every: renew_every.into(),
            renew_in: renew_every.into(),
            latest_key,
            latest_transaction: transaction,
            latest_transaction_ptr: NonNull::dangling(),
            latest_object_store: None,
            latest_object_store_ptr: NonNull::dangling(),
            inner_stream: None,
            cursor_future: None,
            _pin: PhantomPinned,
        };
        let mut this = Box::pin(this);

        unsafe {
            let this = Pin::get_unchecked_mut(Pin::as_mut(&mut this));

            this.latest_transaction_ptr = NonNull::from(&this.latest_transaction);
            this.latest_object_store = Some(
                this.latest_transaction_ptr
                    .as_ref()
                    .object_store(this.object_store_name.as_str())?,
            );
            this.latest_object_store_ptr = NonNull::from(&this.latest_object_store);

            let cursor = this
                .latest_object_store_ptr
                .as_ref()
                .as_ref()
                .unwrap()
                .open_cursor_with_range(&after_latest_key)?
                .await?;
            let inner_stream = StreamByCursor::new_with_cursor(cursor);

            this.inner_stream = Some(inner_stream);
        }

        Ok(this)
    }
}

impl<'a, F> Stream for StreamByRenewedCursor<'a, F>
where
    F: FnMut() -> Result<IdbTransaction<'a>, DomException>,
{
    type Item = Result<(JsValue, JsValue), DomException>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        // `object_store` must be renewed, along with `inner_stream`.
        if *this.renew_in == 0 {
            // To renew `object_store`, we must use the `cursor_future`.
            // If it's not defined, let's build one.
            if this.cursor_future.is_none() {
                // Get and save the new `transaction`.
                let transaction = (this.transaction_builder)()?;
                this.latest_transaction.set(transaction);

                // Get and asve the new `object_store`.
                let object_store = unsafe { this.latest_transaction_ptr.as_ref() }
                    .object_store(this.object_store_name.as_str())?;
                this.latest_object_store.set(Some(object_store));

                // Get a new `cursor_future`.
                let object_store_ref = unsafe { this.latest_object_store_ptr.as_ref() };
                let after_latest_key = IdbKeyRange::lower_bound_with_open(&this.latest_key, true)?;
                let cursor_future =
                    object_store_ref.as_ref().unwrap().open_cursor_with_range(&after_latest_key)?;

                // Save the `cursor_future` to poll it later.
                *this.cursor_future = Some(Box::pin(cursor_future));
            }

            // Let's poll `cursor_future` since it's `Some(_)`!
            let cursor = ready!(this
                .cursor_future
                .as_mut()
                // SAFETY: The case where `cursor_future` is `None` has been handled
                // hereinabove.
                .unwrap()
                .as_mut()
                .poll(cx))?;

            // We have a cursor. Yeah! We can build the `StreamByCursor`.
            let inner_stream = StreamByCursor::new_with_cursor(cursor);
            // And save it.
            *this.inner_stream = Some(inner_stream);

            // Update `renew_in`.
            *this.renew_in = *this.renew_every;
        }

        // Let's poll `inner_stream`.
        let result = ready!(this
            .inner_stream
            .as_pin_mut()
            .expect("`inner_stream` cannot be `None`")
            .poll_next(cx));

        if let Some(Ok((key, _value))) = &result {
            // Decrement `renew_in` only if a result has been produced.
            *this.renew_in -= 1;
            // Save the latest key.
            *this.latest_key = key.clone();
        }

        Poll::Ready(result)
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    use futures_util::TryStreamExt;
    use indexed_db_futures::prelude::*;
    use wasm_bindgen::JsValue;
    use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
    use web_sys::DomException;

    use super::*;

    wasm_bindgen_test_configure!(run_in_browser);

    #[wasm_bindgen_test]
    async fn stream_by_cursor() -> Result<(), DomException> {
        let mut db = IdbDatabase::open("foobar")?;

        db.set_on_upgrade_needed(Some(|event: &IdbVersionChangeEvent| -> Result<(), JsValue> {
            if event.db().object_store_names().find(|n| n == "baz").is_none() {
                event.db().create_object_store("baz")?;
            }

            Ok(())
        }));

        let db = db.await?;

        {
            let transaction =
                db.transaction_on_one_with_mode("baz", IdbTransactionMode::Readwrite)?;
            let object_store = transaction.object_store("baz")?;

            for nth in 0..5 {
                object_store.add_key_val_owned(
                    JsValue::from_str(&format!("key{nth}")),
                    &JsValue::from(nth),
                )?;
            }

            transaction.await.into_result()?;
        }

        // Test one read. Nothing particular here.
        {
            let transaction = db.transaction_on_one("baz")?;
            let store = transaction.object_store("baz")?;

            let value: Option<JsValue> = store.get_owned("key3")?.await?;
            assert_eq!(value.expect("value is none").as_f64().expect("boo"), 3.);
        }

        // Stream!
        {
            let transaction = db.transaction_on_one("baz")?;
            let object_store = transaction.object_store("baz")?;
            let mut stream = StreamByCursor::new(&object_store).await?;

            assert_eq!(
                stream.try_next().await?,
                Some((JsValue::from_str("key0"), JsValue::from_f64(0.)))
            );
            assert_eq!(
                stream.try_next().await?,
                Some((JsValue::from_str("key1"), JsValue::from_f64(1.)))
            );
            assert_eq!(
                stream.try_next().await?,
                Some((JsValue::from_str("key2"), JsValue::from_f64(2.)))
            );
            assert_eq!(
                stream.try_next().await?,
                Some((JsValue::from_str("key3"), JsValue::from_f64(3.)))
            );
            assert_eq!(
                stream.try_next().await?,
                Some((JsValue::from_str("key4"), JsValue::from_f64(4.)))
            );
            assert_eq!(stream.try_next().await?, None);
        }

        // Stream with some operations.
        {
            let transaction = db.transaction_on_one("baz")?;
            let object_store = transaction.object_store("baz")?;
            let stream = StreamByCursor::new(&object_store).await?;

            let results =
                stream.map_ok(|(_key, value)| value).try_chunks(2).try_collect::<Vec<_>>().await;

            assert_eq!(
                results,
                Ok(vec![
                    vec![JsValue::from_f64(0.), JsValue::from_f64(1.),],
                    vec![JsValue::from_f64(2.), JsValue::from_f64(3.),],
                    vec![JsValue::from_f64(4.),]
                ])
            );
        }

        Ok(())
    }

    #[wasm_bindgen_test]
    async fn stream_by_renewed_cursor() -> Result<(), DomException> {
        let mut db = IdbDatabase::open("bazqux")?;

        db.set_on_upgrade_needed(Some(|event: &IdbVersionChangeEvent| -> Result<(), JsValue> {
            if event.db().object_store_names().find(|n| n == "baz").is_none() {
                event.db().create_object_store("baz")?;
            }

            Ok(())
        }));

        let db = db.await?;

        {
            let transaction =
                db.transaction_on_one_with_mode("baz", IdbTransactionMode::Readwrite)?;
            let object_store = transaction.object_store("baz")?;

            for nth in 0..5 {
                object_store.add_key_val_owned(
                    JsValue::from_str(&format!("key{nth}")),
                    &JsValue::from(nth),
                )?;
            }

            transaction.await.into_result()?;
        }

        // Test one read. Nothing particular here.
        {
            let transaction = db.transaction_on_one("baz")?;
            let store = transaction.object_store("baz")?;

            let value: Option<JsValue> = store.get_owned("key3")?.await?;
            assert_eq!(value.expect("value is none").as_f64().expect("boo"), 3.);
        }

        // Stream!
        {
            let mut number_of_renews = 0;
            let mut stream = StreamByRenewedCursor::new(
                || {
                    number_of_renews += 1;
                    Ok(db.transaction_on_one("baz")?)
                },
                "baz".to_owned(),
                // SAFETY: `unwrap` is safe because 3 is not zero.
                NonZeroUsize::new(3).unwrap(),
            )
            .await?;

            assert_eq!(
                stream.try_next().await?,
                Some((JsValue::from_str("key0"), JsValue::from_f64(0.)))
            );
            assert_eq!(
                stream.try_next().await?,
                Some((JsValue::from_str("key1"), JsValue::from_f64(1.)))
            );
            assert_eq!(
                stream.try_next().await?,
                Some((JsValue::from_str("key2"), JsValue::from_f64(2.)))
            );
            assert_eq!(
                stream.try_next().await?,
                Some((JsValue::from_str("key3"), JsValue::from_f64(3.)))
            );
            assert_eq!(
                stream.try_next().await?,
                Some((JsValue::from_str("key4"), JsValue::from_f64(4.)))
            );
            assert_eq!(stream.try_next().await?, None);

            assert_eq!(number_of_renews, 2);
        }

        // Stream with some operations.
        {
            let mut number_of_renews = 0;
            let stream = StreamByRenewedCursor::new(
                || {
                    number_of_renews += 1;
                    Ok(db.transaction_on_one("baz")?)
                },
                "baz".to_owned(),
                // SAFETY: `unwrap` is safe because 3 is not zero.
                NonZeroUsize::new(3).unwrap(),
            )
            .await?;

            let results =
                stream.map_ok(|(_key, value)| value).try_chunks(2).try_collect::<Vec<_>>().await;

            assert_eq!(
                results,
                Ok(vec![
                    vec![JsValue::from_f64(0.), JsValue::from_f64(1.),],
                    vec![JsValue::from_f64(2.), JsValue::from_f64(3.),],
                    vec![JsValue::from_f64(4.),]
                ])
            );

            assert_eq!(number_of_renews, 2);
        }

        Ok(())
    }
}
