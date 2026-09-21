# Performance measurements

The benchmark suite separates deterministic CPU work from local I/O. It never
contacts an ECMWF service.

## Benchmarks

The `perf/run` helper keeps target names, exact Criterion IDs, build profiles,
and output locations in one place. List the available scenarios without
building or initializing their fixtures:

```sh
perf/run list
```

Run all benchmarks, one target, or one scenario:

```sh
perf/run bench
perf/run bench planner
perf/run bench planner daily_six_years
```

For an explicit before/after comparison, save a named baseline before editing
and compare against it afterwards:

```sh
perf/run bench planner --save-baseline before
perf/run bench planner --baseline before
```

The planner benchmark reports calendar days processed per second. The download benchmark uses an unauthenticated loopback HTTP asset and writes it through the public atomic download API; temporary-directory setup is outside the measured section. The assembly benchmark creates deterministic source fixtures once; per-iteration copies are also outside the measured section. It creates only the selected fixture, so profiling one format does not include setup for the other. NetCDF throughput counts data-variable bytes; GRIB throughput counts source-file bytes.

Download and assembly results describe the current host and filesystem. Compare
runs on the same otherwise-idle machine. Do not compare tmpfs results with a
disk-backed temporary directory. The OS page cache is intentionally left warm;
cold-cache experiments should be run separately and recorded as such.

## CPU profiling

Install `cargo-flamegraph` and profile an individual scenario. The helper uses
an exact benchmark ID and writes the SVG below `target/profiling`. The profiling
profile preserves the optimized release settings while retaining symbols.

```sh
perf/run flamegraph planner daily_six_years
perf/run flamegraph assembly netcdf_6x8_timesteps
```

Set `PROFILE_SECONDS` to change the default ten-second capture:

```sh
PROFILE_SECONDS=30 perf/run flamegraph planner mars_ten_years
```

For hardware counters on Linux, `stat` prints five repeated measurements and
`record` writes a named data file below `target/profiling`. The event list and
sampling frequency can be overridden with `PERF_STAT_EVENTS` and
`PERF_RECORD_FREQUENCY`.

```sh
perf/run stat planner mars_ten_years
perf/run record planner mars_ten_years
perf report -i target/profiling/planner-mars_ten_years.data
```

The profiling commands build with `--profile profiling` and run Criterion with
`--profile-time`, keeping the process in the benchmark body and avoiding
statistical analysis and report generation in profiler samples. `perf` may
require adjusted kernel permissions (`perf_event_paranoid`) on Linux.

Record the command, CPU model, Rust version, filesystem, scenario, and Git commit
with every retained result. Optimize only after a repeatable benchmark and a
profile identify the same expensive path.
