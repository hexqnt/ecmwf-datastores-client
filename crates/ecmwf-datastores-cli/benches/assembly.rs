use std::{fs, hint::black_box, path::PathBuf};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ecmwf_datastores_cli::assembly;
use tempfile::TempDir;

const MIB: usize = 1024 * 1024;

struct Fixture {
    _directory: TempDir,
    parts: Vec<PathBuf>,
}

struct WorkingSet {
    _directory: TempDir,
    output: PathBuf,
    parts: Vec<PathBuf>,
}

impl Fixture {
    fn working_set(&self) -> WorkingSet {
        let directory = tempfile::tempdir().expect("benchmark directory can be created");
        let parts = self
            .parts
            .iter()
            .enumerate()
            .map(|(index, source)| {
                let destination = directory.path().join(format!("part-{index:04}"));
                fs::copy(source, &destination).expect("benchmark part can be copied");
                destination
            })
            .collect();
        let output = directory.path().join("assembled-output");
        WorkingSet {
            _directory: directory,
            output,
            parts,
        }
    }
}

fn grib_fixture(part_count: usize, part_size: usize) -> Fixture {
    let directory = tempfile::tempdir().expect("benchmark directory can be created");
    let mut contents = vec![0x5a; part_size];
    contents[..4].copy_from_slice(b"GRIB");
    let parts = (0..part_count)
        .map(|index| {
            let path = directory.path().join(format!("source-{index:04}.grib"));
            fs::write(&path, &contents).expect("benchmark GRIB part can be written");
            path
        })
        .collect::<Vec<_>>();
    Fixture {
        _directory: directory,
        parts,
    }
}

fn netcdf_fixture(part_count: usize, times_per_part: usize) -> Fixture {
    const LATITUDES: usize = 181;
    const LONGITUDES: usize = 360;

    let directory = tempfile::tempdir().expect("benchmark directory can be created");
    let latitudes = (0..LATITUDES)
        .map(|index| 90.0 - f32::from(u16::try_from(index).expect("latitude index fits in u16")))
        .collect::<Vec<_>>();
    let longitudes = (0..LONGITUDES)
        .map(|index| f32::from(u16::try_from(index).expect("longitude index fits in u16")))
        .collect::<Vec<_>>();
    let value_count = times_per_part * LATITUDES * LONGITUDES;
    let values = (0..value_count)
        .map(|index| f32::from(u16::try_from(index % 10_000).expect("data value fits in u16")))
        .collect::<Vec<_>>();
    let parts = (0..part_count)
        .map(|part_index| {
            let path = directory.path().join(format!("source-{part_index:04}.nc"));
            let times = (0..times_per_part)
                .map(|offset| {
                    i64::try_from(part_index * times_per_part + offset)
                        .expect("benchmark time fits in i64")
                })
                .collect::<Vec<_>>();
            write_netcdf_part(&path, &times, &latitudes, &longitudes, &values);
            path
        })
        .collect::<Vec<_>>();
    Fixture {
        _directory: directory,
        parts,
    }
}

fn write_netcdf_part(
    path: &std::path::Path,
    times: &[i64],
    latitudes: &[f32],
    longitudes: &[f32],
    values: &[f32],
) {
    let mut file = netcdf::create(path).expect("benchmark NetCDF part can be created");
    file.add_dimension("valid_time", times.len())
        .expect("time dimension can be added");
    file.add_dimension("latitude", latitudes.len())
        .expect("latitude dimension can be added");
    file.add_dimension("longitude", longitudes.len())
        .expect("longitude dimension can be added");
    file.add_variable::<i64>("valid_time", &["valid_time"])
        .expect("time coordinate can be added")
        .put_values(times, ..)
        .expect("time coordinate can be written");
    file.add_variable::<f32>("latitude", &["latitude"])
        .expect("latitude coordinate can be added")
        .put_values(latitudes, ..)
        .expect("latitude coordinate can be written");
    file.add_variable::<f32>("longitude", &["longitude"])
        .expect("longitude coordinate can be added")
        .put_values(longitudes, ..)
        .expect("longitude coordinate can be written");
    file.add_variable::<f32>("temperature", &["valid_time", "latitude", "longitude"])
        .expect("data variable can be added")
        .put_values(values, ..)
        .expect("data variable can be written");
    file.close().expect("benchmark NetCDF part can be closed");
}

fn bench_fixture(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    runtime: &tokio::runtime::Runtime,
    name: &str,
    bytes: u64,
    make_fixture: impl Fn() -> Fixture,
) {
    group.throughput(Throughput::Bytes(bytes));
    let mut fixture = None;
    group.bench_function(BenchmarkId::from_parameter(name), |bencher| {
        let fixture = fixture.get_or_insert_with(&make_fixture);
        bencher.iter_batched(
            || fixture.working_set(),
            |working| {
                let saved = runtime
                    .block_on(assembly::assemble(working.parts, working.output, false))
                    .expect("benchmark assembly succeeds");
                black_box(saved);
            },
            BatchSize::PerIteration,
        );
    });
}

fn benchmark_assembly(criterion: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("benchmark runtime builds");
    let mut group = criterion.benchmark_group("assembly");
    group.sample_size(10);
    bench_fixture(
        &mut group,
        &runtime,
        "grib_8x4_mib",
        32 * MIB as u64,
        || grib_fixture(8, 4 * MIB),
    );
    bench_fixture(
        &mut group,
        &runtime,
        "netcdf_6x8_timesteps",
        (6 * 8 * 181 * 360 * size_of::<f32>()) as u64,
        || netcdf_fixture(6, 8),
    );
    group.finish();
}

criterion_group!(benches, benchmark_assembly);
criterion_main!(benches);
