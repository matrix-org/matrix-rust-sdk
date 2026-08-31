Dropping `CrossProcessLock` no longer releases the lock if a `CrossProcessGuard`
is still alive.

An important problem has been found with the `CrossProcessLock`. We noticed
that when a `CrossProcessLock` is dropped, all `CrossProcessLockGuard`s report
0 holder, and thus, the lock is released, but it's wrong. As long as guards are
alive, the lock **MUST NOT** be released. Dropping all the guards releases the
lock, but dropping the `CrossProcessLock` —which is not the lock in itself— must
not drop the acquired lock.
There is no public changes, but the incorrect behaviour is now fixed.
