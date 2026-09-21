use std::{num::NonZeroUsize, time::Duration};

use chrono::{NaiveDate, NaiveTime};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
};

use crate::{Client, CollectionId, RetryPolicy, Selection};

use super::*;

fn date(value: &str) -> NaiveDate {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
}

fn selection(value: Value) -> Selection {
    Selection::try_from(value).unwrap()
}

fn collection(value: &str) -> CollectionId {
    CollectionId::parse(value).unwrap()
}

fn time(value: &str) -> NaiveTime {
    NaiveTime::parse_from_str(value, "%H:%M").unwrap()
}

#[test]
fn time_ranges_reject_subminute_precision() {
    for value in ["12:00:01", "12:00:00.000000001"] {
        let time = NaiveTime::parse_from_str(value, "%H:%M:%S%.f").unwrap();
        assert!(TimeRange::new(date("2024-01-01"), date("2024-01-01"), [time]).is_err());
    }
}

#[test]
fn rejects_empty_mars_axes() {
    for key in ["param", "levelist", "time"] {
        let request = PlanningRequest::new(
            collection("reanalysis-era5-complete"),
            selection(json!({ key: [] })),
        )
        .with_time_range(TimeRange::new(date("2024-01-01"), date("2024-01-02"), []).unwrap());
        assert!(Planner::new().plan_locally(&request).is_err());
    }
}

#[test]
fn rejects_overflowing_mars_cardinality() {
    let request = PlanningRequest::new(
        collection("reanalysis-era5-complete"),
        selection(json!({ "step": format!("0/to/{}", usize::MAX) })),
    );
    assert!(Planner::new().plan_locally(&request).is_err());
}

#[test]
fn mars_date_estimates_accept_both_supported_formats() {
    for expression in ["2024-01-01/to/2024-01-03", "20240101/to/20240103"] {
        let request = PlanningRequest::new(
            collection("reanalysis-era5-complete"),
            selection(json!({ "date": expression, "param": "129" })),
        );
        let plan = Planner::new().plan_locally(&request).unwrap();
        assert_eq!(plan.estimated_items(), Some(3));
    }
}

#[test]
fn mars_ranges_respect_local_field_limits() {
    let request = PlanningRequest::new(
        collection("reanalysis-era5-complete"),
        selection(json!({
            "param": "129/130",
            "levelist": "500/850",
            "levtype": "pl",
            "type": "an",
            "stream": "oper"
        })),
    )
    .with_time_range(
        TimeRange::new(
            date("2024-01-01"),
            date("2024-01-03"),
            [time("00:00"), time("06:00"), time("12:00"), time("18:00")],
        )
        .unwrap(),
    );
    let planner = Planner::new().with_max_items(NonZeroUsize::new(16).unwrap());

    let plan = planner.plan_locally(&request).unwrap();

    assert_eq!(plan.requests().len(), 3);
    assert!(
        plan.requests()
            .iter()
            .all(|part| part.estimated_items() == Some(16))
    );
    assert_eq!(
        plan.requests()[0].selection().as_map()["date"],
        "2024-01-01"
    );
}

#[test]
fn mars_ranges_keep_month_boundaries_with_partial_chunks() {
    let request = PlanningRequest::new(
        collection("reanalysis-era5-complete"),
        selection(json!({ "param": "129" })),
    )
    .with_time_range(TimeRange::new(date("2024-01-30"), date("2024-02-03"), []).unwrap());
    let plan = Planner::new()
        .with_max_items(NonZeroUsize::new(2).unwrap())
        .plan_locally(&request)
        .unwrap();

    let dates = plan
        .requests()
        .iter()
        .map(|part| part.selection().as_map()["date"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        dates,
        [
            "2024-01-30/to/2024-01-31",
            "2024-02-01/to/2024-02-02",
            "2024-02-03",
        ]
    );
    assert_eq!(plan.estimated_items(), Some(5));
}

#[test]
fn splits_short_months_with_a_single_item_limit() {
    let request = PlanningRequest::new(
        collection("reanalysis-era5-single-levels"),
        selection(json!({ "variable": ["2m_temperature"] })),
    )
    .with_time_range(
        TimeRange::new(date("2023-02-01"), date("2023-02-28"), [time("00:00")]).unwrap(),
    );
    let plan = Planner::new()
        .with_max_items(NonZeroUsize::MIN)
        .with_max_requests(NonZeroUsize::new(28).unwrap())
        .plan_locally(&request)
        .unwrap();

    assert_eq!(plan.estimated_items(), Some(28));
    assert_eq!(plan.requests().len(), 28);
    assert!(
        plan.requests()
            .iter()
            .all(|part| part.estimated_items() == Some(1))
    );
}

#[test]
fn provider_profiles_choose_their_largest_safe_seed_grouping() {
    let daily = PlanningRequest::new(
        collection("derived-era5-single-levels-daily-statistics"),
        selection(json!({
            "product_type": "reanalysis",
            "variable": ["2m_temperature"],
            "daily_statistic": "daily_mean",
            "frequency": "1_hourly"
        })),
    )
    .with_time_range(TimeRange::new(date("2021-01-01"), date("2026-09-30"), []).unwrap());
    let land = PlanningRequest::new(
        collection("reanalysis-era5-land"),
        selection(json!({ "variable": ["2m_temperature"] })),
    )
    .with_time_range(
        TimeRange::new(date("2024-01-01"), date("2024-02-29"), [time("00:00")]).unwrap(),
    );

    let daily_plan = Planner::new().plan_locally(&daily).unwrap();
    let land_plan = Planner::new().plan_locally(&land).unwrap();

    assert_eq!(daily_plan.requests().len(), 6);
    assert_eq!(daily_plan.estimated_items(), Some(2_099));
    assert_eq!(land_plan.requests().len(), 2);
}

#[test]
fn locally_splits_calendar_ranges_without_crossing_partial_months() {
    let request = PlanningRequest::new(
        collection("reanalysis-era5-pressure-levels"),
        selection(json!({
            "variable": ["temperature"],
            "pressure_level": ["500"]
        })),
    )
    .with_time_range(
        TimeRange::new(
            date("2024-01-30"),
            date("2024-02-02"),
            [time("00:00"), time("12:00")],
        )
        .unwrap(),
    );
    let planner = Planner::new().with_max_items(NonZeroUsize::new(4).unwrap());

    let plan = planner.plan_locally(&request).unwrap();

    assert_eq!(plan.requests().len(), 2);
    assert_eq!(
        plan.requests()[0].selection().as_map()["month"],
        json!(["01"])
    );
    assert_eq!(
        plan.requests()[0].selection().as_map()["day"],
        json!(["30", "31"])
    );
    assert_eq!(plan.requests()[0].estimated_items(), Some(4));
}

#[test]
fn calendar_estimates_count_only_real_dates() {
    let request = PlanningRequest::new(
        collection("reanalysis-era5-single-levels"),
        selection(json!({
            "year": ["2023", "2024"],
            "month": ["02"],
            "day": ["28", "29", "30"],
            "time": ["00:00"],
            "variable": ["2m_temperature"]
        })),
    );

    let plan = Planner::new().plan_locally(&request).unwrap();

    assert_eq!(plan.estimated_items(), Some(3));
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
}

async fn read_json_request(stream: &mut tokio::net::TcpStream) -> Value {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut buffer).await.unwrap();
        assert_ne!(read, 0);
        request.extend_from_slice(&buffer[..read]);
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length: ")
                .and_then(|value| value.parse::<usize>().ok())
        })
        .unwrap();
    while request.len() < header_end + content_length {
        let read = stream.read(&mut buffer).await.unwrap();
        request.extend_from_slice(&buffer[..read]);
    }
    serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap()
}

#[tokio::test]
async fn provider_costs_drive_balanced_proportional_splits() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let body = read_json_request(&mut stream).await;
            let years = body["inputs"]["year"].as_array().unwrap().len();
            let cost = years * 40 + 10;
            respond(
                &mut stream,
                "200 OK",
                &format!(
                    "{{\"id\":\"size\",\"cost\":{cost},\"limit\":100,\"cost_bar_steps\":[50,70]}}"
                ),
            )
            .await;
        }
    });
    let client = Client::new(endpoint.parse().unwrap(), None).unwrap();
    let request = PlanningRequest::new(
        collection("reanalysis-era5-single-levels"),
        selection(json!({
            "product_type": ["reanalysis"],
            "variable": ["2m_temperature"]
        })),
    )
    .with_time_range(
        TimeRange::new(date("2025-01-01"), date("2027-12-31"), [time("00:00")]).unwrap(),
    );

    let outcome = Planner::new().plan(&client, &request).await.unwrap();

    assert!(matches!(outcome.costing(), CostingOutcome::Provider));
    assert_eq!(outcome.plan().requests().len(), 3);
    for request in outcome.plan().requests() {
        let cost = request.provider_cost().expect("provider cost is present");
        approx::assert_abs_diff_eq!(cost.cost(), 50.0);
    }
    server.await.unwrap();
}

#[tokio::test]
async fn refinement_enforces_the_current_planner_request_limit() {
    let request = PlanningRequest::new(
        collection("reanalysis-era5-land"),
        selection(json!({ "variable": ["2m_temperature"] })),
    )
    .with_time_range(
        TimeRange::new(date("2024-01-01"), date("2024-02-29"), [time("00:00")]).unwrap(),
    );
    let plan = Planner::new().plan_locally(&request).unwrap();
    assert_eq!(plan.requests().len(), 2);

    let client = Client::new("http://127.0.0.1:1/api/".parse().unwrap(), None).unwrap();
    let planner = Planner::new().with_max_requests(NonZeroUsize::MIN);

    let result = planner.refine_with_costs(&client, &plan).await;
    assert!(
        matches!(&result, Err(PlanningError::InvalidRequest(message)) if message.contains("configured limit")),
        "unexpected refinement result: {result:?}"
    );
}
#[tokio::test]
async fn automatic_planning_does_not_restore_a_plan_rejected_by_costing() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read_json_request(&mut stream).await;
        respond(
            &mut stream,
            "200 OK",
            r#"{"id":"size","cost":200,"limit":100,"cost_bar_steps":[50,70]}"#,
        )
        .await;

        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read_json_request(&mut stream).await;
        respond(
            &mut stream,
            "503 Service Unavailable",
            r#"{"message":"offline"}"#,
        )
        .await;
    });
    let client = Client::builder(endpoint.parse().unwrap())
        .retry_policy(
            RetryPolicy::new(
                NonZeroUsize::new(1).unwrap(),
                Duration::ZERO,
                Duration::ZERO,
            )
            .unwrap(),
        )
        .build()
        .unwrap();
    let request = PlanningRequest::new(
        collection("reanalysis-era5-single-levels"),
        selection(json!({
            "product_type": ["reanalysis"],
            "variable": ["2m_temperature"],
            "year": ["2024", "2025"]
        })),
    );

    let result = Planner::new().plan(&client, &request).await;
    assert!(
        matches!(result, Err(PlanningError::CostingInterrupted(_))),
        "unexpected planning result: {result:?}"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn automatic_planning_retains_local_plan_when_costing_is_unavailable() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/api/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read_json_request(&mut stream).await;
        respond(
            &mut stream,
            "503 Service Unavailable",
            "{\"message\":\"offline\"}",
        )
        .await;
    });
    let client = Client::new(endpoint.parse().unwrap(), None).unwrap();
    let request = PlanningRequest::new(
        collection("future-dataset"),
        selection(json!({ "variable": ["temperature"] })),
    );

    let outcome = Planner::new().plan(&client, &request).await.unwrap();

    assert!(matches!(
        outcome.costing(),
        CostingOutcome::LocalFallback { .. }
    ));
    assert_eq!(outcome.plan().requests().len(), 1);
    server.await.unwrap();
}
