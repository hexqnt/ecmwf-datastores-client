# ecmwf-datastores CLI

[🇺🇸 English](./README.md) · [🇷🇺 Русский](./README.ru.md)

A command-line tool for reproducible ECMWF data retrieval. It plans and resumes jobs, downloads and assembles results, and writes timing reports.

Structured `[time]` and `[bbox]` planning is supported for:

- `reanalysis-era5-pressure-levels`
- `reanalysis-era5-single-levels`
- `reanalysis-era5-land`
- `derived-era5-single-levels-daily-statistics`
- `reanalysis-era5-complete`
- ERA5-Land, single-level, and pressure-level monthly means

Other collections can use exact CDS fields in `[request]`, without structured `[time]` or `[bbox]` sections.

## Installation

```sh
cargo install --git https://github.com/hexqnt/ecmwf-datastores-client.git \
  --package ecmwf-datastores-cli
```

From a checkout:

```sh
cargo install --path crates/ecmwf-datastores-cli
```

Both commands install the `ecmwf-datastores` executable.

## Configuration

Start with the [example configurations](./examples/). Dates and times must be quoted. Bounding boxes use `lat` and `lon` as `[min, max]` pairs.

Use `[time]` for an inclusive date range. Put the remaining CDS form fields in `[request]`; do not duplicate spatial or temporal fields added by the planner.

The planner splits known axes using provider limits and costs when available, or local estimates otherwise. Defaults are `max_items = 100000` and `max_requests = 1000`; override them in `[split]`. Unknown multi-value fields and raw ranges are preserved but cannot be included in the item estimate.

Credentials follow the library discovery order. Use `--credentials PATH` to select a file.

## Commands

```console
ecmwf-datastores plan request.toml
ecmwf-datastores constraints request.toml
ecmwf-datastores retrieve request.toml --report report.json
ecmwf-datastores resume JOB_ID output.nc
```

- `plan` prints the resulting requests without submitting them.
- `constraints` prints server-provided allowed values; success does not guarantee submission.
- `retrieve` submits, downloads, and assembles the plan.
- `resume` continues one known job.

Progress is written to standard error. Control it with `--progress auto|always|never`. Existing output files require `--overwrite`.

## Execution and assembly

Retrieval runs a bounded pipeline, with two active server jobs and one download by default. Override it when needed:

```toml
[execution]
max_active_jobs = 2
max_concurrent_downloads = 1
```

Multipart GRIB results are concatenated in plan order. NetCDF variables are merged by named one-dimensional coordinates; conflicting overlaps fail. Assembly requires space for all parts and the final file. Parts are retained after a failure and can be assembled manually:

```console
ecmwf-datastores assemble result.nc .result.part-1.nc .result.part-2.nc
```

Multipart requests cannot assemble explicit archives; use `download_format = "unarchived"` when available. NetCDF support is enabled by the default `netcdf-assembly` feature. Build with `--no-default-features` for a GRIB-only CLI.

## Reports and recovery

`--report PATH` writes a JSON report and a `PATH.events.jsonl` journal. Job IDs are synced immediately after submission, allowing interrupted jobs to be resumed. The final report includes request details, byte counts, timings, throughput, and any retained parts. `submission_unknown` means the server may have accepted a request without returning a job ID.

## Importing CDS-generated code

Save the snippet from **Show API request code**, then run:

```console
ecmwf-datastores import-python cds-request.py --output request.toml
```

The importer reads Python literals without executing the source. The resulting TOML can be planned, checked, or retrieved normally.
