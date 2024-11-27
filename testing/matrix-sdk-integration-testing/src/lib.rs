#![cfg(test)]
#![allow(unexpected_cfgs)] // Triggered by the init_tracing_for_tests!() invocation.

matrix_sdk_test::init_tracing_for_tests!();

mod helpers;
mod tests;
