use std::{hint::black_box, num::NonZeroUsize};

use chrono::{NaiveDate, NaiveTime};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use ecmwf_datastores_client::planning::{Planner, PlanningRequest, TimeRange};
use ecmwf_datastores_client::{CollectionId, Selection};
use serde_json::json;

struct Scenario {
    name: &'static str,
    planner: Planner,
    request: PlanningRequest,
    days: u64,
}

fn date(value: &str) -> NaiveDate {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").expect("benchmark date is valid")
}

fn time(value: &str) -> NaiveTime {
    NaiveTime::parse_from_str(value, "%H:%M").expect("benchmark time is valid")
}

fn request(dataset: &str, selection: serde_json::Value) -> PlanningRequest {
    PlanningRequest::new(
        CollectionId::parse(dataset).expect("benchmark collection ID is valid"),
        Selection::try_from(selection).expect("benchmark selection is an object"),
    )
}

#[allow(clippy::too_many_lines)] // Keeping the scenario matrix together makes comparisons explicit.
fn scenarios() -> [Scenario; 5] {
    let hourly_times = (0..24)
        .map(|hour| time(&format!("{hour:02}:00")))
        .collect::<Vec<_>>();
    [
        Scenario {
            name: "hourly_one_day",
            planner: Planner::new(),
            request: request(
                "reanalysis-era5-single-levels",
                json!({"variable": ["2m_temperature"]}),
            )
            .with_time_range(
                TimeRange::new(
                    date("2025-01-01"),
                    date("2025-01-01"),
                    hourly_times.iter().copied(),
                )
                .expect("benchmark range is valid"),
            ),
            days: 1,
        },
        Scenario {
            name: "daily_six_years",
            planner: Planner::new(),
            request: request(
                "derived-era5-single-levels-daily-statistics",
                json!({
                    "product_type": "reanalysis",
                    "variable": ["2m_temperature"],
                    "daily_statistic": "daily_mean",
                    "frequency": "1_hourly"
                }),
            )
            .with_time_range(
                TimeRange::new(date("2021-01-01"), date("2026-12-31"), [])
                    .expect("benchmark range is valid"),
            ),
            days: 2_191,
        },
        Scenario {
            name: "hourly_many_axes",
            planner: Planner::new()
                .with_max_items(NonZeroUsize::new(10_000).expect("benchmark limit is non-zero")),
            request: request(
                "reanalysis-era5-pressure-levels",
                json!({
                    "product_type": ["reanalysis"],
                    "variable": ["temperature", "geopotential", "relative_humidity"],
                    "pressure_level": ["1000", "925", "850", "700", "500", "300"]
                }),
            )
            .with_time_range(
                TimeRange::new(
                    date("2020-01-01"),
                    date("2025-12-31"),
                    hourly_times.iter().copied(),
                )
                .expect("benchmark range is valid"),
            ),
            days: 2_192,
        },
        Scenario {
            name: "calendar_maximum_fragmentation",
            planner: Planner::new()
                .with_max_items(NonZeroUsize::MIN)
                .with_max_requests(NonZeroUsize::new(1_000).expect("benchmark limit is non-zero")),
            request: request(
                "reanalysis-era5-single-levels",
                json!({"variable": ["2m_temperature"]}),
            )
            .with_time_range(
                TimeRange::new(date("2024-01-01"), date("2024-12-31"), [time("00:00")])
                    .expect("benchmark range is valid"),
            ),
            days: 366,
        },
        Scenario {
            name: "mars_ten_years",
            planner: Planner::new()
                .with_max_items(NonZeroUsize::new(2_000).expect("benchmark limit is non-zero")),
            request: request(
                "reanalysis-era5-complete",
                json!({
                    "param": "129/130/131/132",
                    "levelist": "1000/850/700/500",
                    "levtype": "pl",
                    "type": "an",
                    "stream": "oper"
                }),
            )
            .with_time_range(
                TimeRange::new(
                    date("2016-01-01"),
                    date("2025-12-31"),
                    [time("00:00"), time("06:00"), time("12:00"), time("18:00")],
                )
                .expect("benchmark range is valid"),
            ),
            days: 3_653,
        },
    ]
}

fn benchmark_planner(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("planner/local");
    for scenario in scenarios() {
        group.throughput(Throughput::Elements(scenario.days));
        group.bench_with_input(
            BenchmarkId::from_parameter(scenario.name),
            &scenario,
            |bencher, scenario| {
                bencher.iter(|| {
                    black_box(
                        scenario
                            .planner
                            .plan_locally(black_box(&scenario.request))
                            .expect("benchmark request can be planned"),
                    )
                });
            },
        );
    }
    group.finish();
}

criterion_group!(benches, benchmark_planner);
criterion_main!(benches);
