Defer `await`s in write operations in `Transaction` until calling `Transaction::commit`.
Additionally, remove `async` modifier from functions that no longer need to be asynchronous
as a result of the change.
