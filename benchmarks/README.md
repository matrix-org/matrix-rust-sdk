# Benchmarks for the rust-sdk crypto layer

This directory contains various benchmarks that test critical functionality in
the crypto layer in the rust-sdk.

We're using [Criterion] for the benchmarks, the full documentation for Criterion
can be found [here](https://bheisler.github.io/criterion.rs/book/criterion_rs.html).

## Running the benchmarks

The benchmark can be run by using the `bench` command of `cargo`:

```bash
$ cargo bench
```

This will work from the workspace directory of the rust-sdk.

To lower compile times, you might be interested in using the `profiling` profile, that's optimized
for a fair tradeoff between compile times and runtime performance:

```bash
$ cargo bench --profile profiling
```

If you want to pass options to the benchmark [you'll need to specify the name of
the benchmark](https://bheisler.github.io/criterion.rs/book/faq.html#cargo-bench-gives-unrecognized-option-errors-for-valid-command-line-options):

```bash
$ cargo bench --bench crypto_bench -- # Your options go here
```

If you want to run only a specific benchmark, pass the name of the
benchmark as an argument:

```bash
$ cargo bench --bench crypto_bench "Room key sharing/"
```

After the benchmarks are done, a HTML report can be found in `target/criterion/report/index.html`.

### Using a baseline for the benchmark

The benchmarks will by default compare the results to the previous run of the
benchmark. If you are improving the performance of a specific feature and run
the benchmark many times, it may be useful to store a baseline to compare
against instead.

The `--save-baseline` switch can be used to create a baseline for the benchmark.

```bash
$ cargo bench --bench crypto_bench -- --save-baseline libolm
```

After you make your changes you can use the baseline to compare the results like
so:

```bash
$ cargo bench --bench crypto_bench -- --baseline libolm
```

### Generating Flame Graphs for the benchmarks

The benchmarks support profiling and generating [Flame Graphs] while they run in
profiling mode using [pprof].

Profiling usually requires root permissions, to avoid the need for root
permissions you can adjust the value of `perf_event_paranoid`, e.g. the most
permisive value is `-1`:

```bash
$ echo -1 | sudo tee /proc/sys/kernel/perf_event_paranoid
```

To generate flame graphs feature, enable the profiling mode using the
`--profile-time` command line flag:

```bash
$ cargo bench --bench crypto_bench -- --profile-time=5
```

After the benchmarks are done, a flame graph for each individual benchmark can be
found in `target/criterion/<name-of-benchmark>/profile/flamegraph.svg`.

[pprof]: https://docs.rs/pprof/0.5.0/pprof/index.html#
[Criterion]: https://docs.rs/criterion/0.3.5/criterion/
[Flame Graphs]: https://www.brendangregg.com/flamegraphs.html
