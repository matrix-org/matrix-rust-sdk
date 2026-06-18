Fix a bug where cloning a `CrossProcessLockGuard` does not increase the
`num_holders`, protecting against an _impossible to release_ lock, or a
_prematurely released_ lock. Why? Because when the second to last holder drops
the lock, it makes the `num_holders` to overflow and to wrap around. It would
never make it to release because the number of holders became `u32::MAX`.
Alternatively, the counter could reach zero prematurely, and then the lock was
prematurely released while holders exist.

Another bug existed where the `num_holders` was off by 1 with
`CrossProcessLockConfig::SingleProcess`, and is now fixed.
