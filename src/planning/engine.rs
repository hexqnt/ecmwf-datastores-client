use std::{collections::VecDeque, num::TryFromIntError};

use chrono::{Datelike, Months, NaiveDate};
use serde_json::{Map, Value, json};

use crate::{Client, CollectionId, RequestCost, Selection};

use super::profile::{ProviderGrouping, TemporalEncoding};
use super::{
    DatasetProfile, Plan, PlannedRequest, Planner, PlanningError, PlanningRequest, PlanningResult,
    TimeRange,
};

type RequestMap = Map<String, Value>;
type CostedMap = (RequestMap, Option<RequestCost>);

type Result<T> = PlanningResult<T>;

#[derive(Default)]
struct CalendarEstimateScratch {
    days: Vec<i64>,

    months: Vec<i64>,

    years: Vec<i64>,
}

macro_rules! invalid {
    ($($argument:tt)*) => {
        return Err(PlanningError::InvalidRequest(format!($($argument)*)))
    };
}

#[derive(Debug)]
struct MonthlyAxes {
    years: Vec<String>,

    months: Vec<String>,
}

#[derive(Debug)]
struct CalendarAxes {
    days: Vec<String>,

    years: Vec<String>,

    months: Vec<String>,
}

impl CalendarAxes {
    fn is_full_year(&self) -> bool {
        self.months.len() == 12 && self.days.len() == 31
    }
}

trait PlanningContext<T> {
    fn or_invalid(self, message: &'static str) -> Result<T>;
    fn or_invalid_with(self, message: impl FnOnce() -> String) -> Result<T>;
}

impl<T> PlanningContext<T> for Option<T> {
    fn or_invalid(self, message: &'static str) -> Result<T> {
        self.ok_or_else(|| PlanningError::InvalidRequest(message.into()))
    }
    fn or_invalid_with(self, message: impl FnOnce() -> String) -> Result<T> {
        self.ok_or_else(|| PlanningError::InvalidRequest(message()))
    }
}

impl<T, E> PlanningContext<T> for std::result::Result<T, E> {
    fn or_invalid(self, message: &'static str) -> Result<T> {
        self.map_err(|_| PlanningError::InvalidRequest(message.into()))
    }
    fn or_invalid_with(self, message: impl FnOnce() -> String) -> Result<T> {
        self.map_err(|_| PlanningError::InvalidRequest(message()))
    }
}

impl From<TryFromIntError> for PlanningError {
    fn from(_: TryFromIntError) -> Self {
        Self::InvalidRequest("numeric value is outside the supported range".into())
    }
}

fn apply_area(spec: &PlanningRequest, request: &mut Map<String, Value>) -> Result<()> {
    let Some(area) = spec.area() else {
        return Ok(());
    };
    ensure_absent(request, &["area"])?;
    let [north, west, south, east] = area.cds_order();
    if spec.profile() == DatasetProfile::Era5Complete {
        request.insert(
            "area".into(),
            Value::String(format!(
                "{}/{}/{}/{}",
                format_coord(north),
                format_coord(west),
                format_coord(south),
                format_coord(east)
            )),
        );
    } else {
        request.insert("area".into(), json!([north, west, south, east]));
    }
    Ok(())
}

fn finish_plan(spec: &PlanningRequest, planner: Planner, maps: Vec<CostedMap>) -> Result<Plan> {
    finish_maps(spec.dataset().clone(), spec.profile(), planner, maps)
}

fn finish_maps(
    dataset: CollectionId,
    profile: DatasetProfile,
    planner: Planner,
    maps: Vec<CostedMap>,
) -> Result<Plan> {
    ensure_request_limit(maps.len(), planner)?;
    let mut scratch = CalendarEstimateScratch::default();
    let plan = maps
        .into_iter()
        .map(|(map, provider_cost)| {
            let estimated_items = if profile == DatasetProfile::Era5Complete {
                estimate_mars_items(&map)?
            } else {
                estimate_calendar_items(profile, &map, &mut scratch)?
            };
            Ok(PlannedRequest {
                estimated_items,
                selection: Selection::from(map),
                provider_cost,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if let Some(items) = plan
        .iter()
        .filter_map(|request| request.estimated_items)
        .find(|&items| items > planner.max_items.get())
    {
        invalid!(
            "request is estimated at {} items but cannot be safely split below the configured limit of {}; use [time] for ERA5 ranges or reduce the raw request",
            items,
            planner.max_items
        );
    }
    Ok(Plan {
        dataset,
        profile,
        requests: plan,
    })
}

fn parse_numeric_axis(value: Option<&Value>, target: &mut Vec<i64>) -> bool {
    target.clear();
    let Some(values) = value.and_then(Value::as_array) else {
        return false;
    };
    for value in values {
        let parsed = match value {
            Value::Number(number) => number.as_i64(),
            Value::String(value) => value.parse().ok(),
            _ => None,
        };
        let Some(parsed) = parsed else {
            target.clear();
            return false;
        };
        target.push(parsed);
    }
    true
}

fn extend_bounded<T>(
    target: &mut Vec<T>,
    mut additional: Vec<T>,
    max_requests: usize,
) -> Result<()> {
    if additional.len() > max_requests.saturating_sub(target.len()) {
        invalid!("plan exceeds the configured limit of {max_requests} requests");
    }
    target.append(&mut additional);
    Ok(())
}

fn dates_by_month(start: NaiveDate, end: NaiveDate) -> Result<Vec<Vec<NaiveDate>>> {
    let mut groups: Vec<Vec<NaiveDate>> = Vec::new();
    let mut date = start;
    loop {
        if groups
            .last()
            .is_none_or(|group| group[0].year() != date.year() || group[0].month() != date.month())
        {
            groups.push(Vec::new());
        }
        groups
            .last_mut()
            .expect("a group was just inserted")
            .push(date);
        if date == end {
            break;
        }
        date = date
            .succ_opt()
            .ok_or_else(|| PlanningError::InvalidRequest("date range overflows".into()))?;
    }
    Ok(groups)
}

fn months_by_year(start: NaiveDate, end: NaiveDate) -> Result<Vec<Vec<NaiveDate>>> {
    let mut groups: Vec<Vec<NaiveDate>> = Vec::new();
    let mut month = start.with_day(1).expect("day one is always valid");
    let last = end.with_day(1).expect("day one is always valid");
    loop {
        if groups
            .last()
            .is_none_or(|group| group[0].year() != month.year())
        {
            groups.push(Vec::new());
        }
        groups
            .last_mut()
            .expect("a group was just inserted")
            .push(month);
        if month == last {
            break;
        }
        month = month
            .checked_add_months(Months::new(1))
            .ok_or_else(|| PlanningError::InvalidRequest("month range overflows".into()))?;
    }
    Ok(groups)
}

fn format_coord(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}
fn format_mars_range(start: NaiveDate, end: NaiveDate) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}/to/{end}")
    }
}

fn balanced_ranges(
    item_count: usize,
    suggested_parts: usize,
) -> impl Iterator<Item = std::ops::Range<usize>> {
    let part_count = suggested_parts.clamp(2, item_count);
    (0..part_count).scan((0, item_count), move |(start, remaining), index| {
        let remaining_parts = part_count - index;
        let part_len = remaining.div_ceil(remaining_parts);
        let range = *start..*start + part_len;
        *start += part_len;
        *remaining -= part_len;
        Some(range)
    })
}

fn parse_mars_date(value: &str) -> Result<NaiveDate> {
    value
        .parse::<NaiveDate>()
        .or_else(|_| NaiveDate::parse_from_str(value, "%Y%m%d"))
        .or_invalid_with(|| format!("unsupported MARS date `{value}`; use YYYY-MM-DD or YYYYMMDD"))
}

fn build_maps(
    spec: &PlanningRequest,
    planner: Planner,
    base: Map<String, Value>,
) -> Result<Vec<Map<String, Value>>> {
    let max_requests = planner.max_requests.get();

    let maps = match (spec.profile(), spec.time()) {
        (
            DatasetProfile::Era5Hourly
            | DatasetProfile::Era5LandHourly
            | DatasetProfile::Era5Daily
            | DatasetProfile::Era5Monthly,
            None,
        ) => split_calendar_map(base, spec.profile(), planner.max_items.get(), max_requests)?,
        (_, None) => vec![base],
        (
            profile @ (DatasetProfile::Era5Hourly
            | DatasetProfile::Era5LandHourly
            | DatasetProfile::Era5Daily),
            Some(time),
        ) => build_day_maps(&base, time, profile, planner)?,
        (DatasetProfile::Era5Monthly, Some(time)) => build_month_maps(&base, time, planner)?,
        (DatasetProfile::Era5Complete, Some(time)) => {
            ensure_absent(&base, &["date"])?;
            if !time.times().is_empty() {
                ensure_absent(&base, &["time"])?;
            }
            split_mars_requests(&base, time, planner.max_items.get(), max_requests)?
        }
        (DatasetProfile::Other, Some(_)) => {
            unreachable!("unsupported structured ranges are rejected while parsing")
        }
    };
    if maps.len() > max_requests {
        invalid!("plan exceeds the configured limit of {max_requests} requests");
    }

    Ok(maps)
}

fn build_day_maps(
    base: &Map<String, Value>,
    time: &TimeRange,
    profile: DatasetProfile,
    planner: Planner,
) -> Result<Vec<Map<String, Value>>> {
    let max_requests = planner.max_requests.get();
    ensure_absent(base, &["year", "month", "day", "time"])?;
    ensure_months_bounded(time.start(), time.end(), max_requests)?;
    let mut maps = Vec::new();
    for dates in dates_by_month(time.start(), time.end())? {
        let mut request = base.clone();
        let first = dates[0];
        request.insert("year".into(), json!([format!("{:04}", first.year())]));
        request.insert("month".into(), json!([format!("{:02}", first.month())]));
        request.insert(
            "day".into(),
            json!(
                dates
                    .iter()
                    .map(|date| format!("{:02}", date.day()))
                    .collect::<Vec<_>>()
            ),
        );
        if matches!(
            profile,
            DatasetProfile::Era5Hourly | DatasetProfile::Era5LandHourly
        ) {
            request.insert(
                "time".into(),
                json!(time.provider_times().collect::<Vec<_>>()),
            );
        }
        extend_bounded(
            &mut maps,
            split_calendar_map(request, profile, planner.max_items.get(), max_requests)?,
            max_requests,
        )?;
    }
    Ok(maps)
}

pub(super) fn build_local_plan(spec: &PlanningRequest, planner: Planner) -> Result<Plan> {
    validate_request(spec)?;
    let mut base = spec.request().as_map().clone();
    apply_area(spec, &mut base)?;
    let maps = build_candidate_maps(spec, planner, base)?;

    finish_plan(
        spec,
        planner,
        maps.into_iter().map(|map| (map, None)).collect(),
    )
}

fn build_month_maps(
    base: &Map<String, Value>,
    time: &TimeRange,
    planner: Planner,
) -> Result<Vec<Map<String, Value>>> {
    let max_requests = planner.max_requests.get();
    ensure_absent(base, &["year", "month", "day"])?;
    ensure_years_bounded(time.start(), time.end(), max_requests)?;
    if !time.times().is_empty() {
        ensure_absent(base, &["time"])?;
    }
    let mut maps = Vec::new();
    for months in months_by_year(time.start(), time.end())? {
        let mut request = base.clone();
        request.insert("year".into(), json!([format!("{:04}", months[0].year())]));
        request.insert(
            "month".into(),
            json!(
                months
                    .iter()
                    .map(|date| format!("{:02}", date.month()))
                    .collect::<Vec<_>>()
            ),
        );
        if !time.times().is_empty() {
            request.insert(
                "time".into(),
                json!(time.provider_times().collect::<Vec<_>>()),
            );
        }
        extend_bounded(
            &mut maps,
            split_calendar_map(
                request,
                DatasetProfile::Era5Monthly,
                planner.max_items.get(),
                max_requests,
            )?,
            max_requests,
        )?;
    }
    Ok(maps)
}

fn build_candidate_maps(
    spec: &PlanningRequest,
    planner: Planner,
    base: Map<String, Value>,
) -> Result<Vec<Map<String, Value>>> {
    let Some(time) = spec.time() else {
        return build_maps(spec, planner, base);
    };
    let planning = spec.profile().planning();
    match (planning.temporal_encoding(), planning.provider_grouping()) {
        (
            encoding @ (TemporalEncoding::HourlyCalendar | TemporalEncoding::DailyCalendar),
            grouping @ (ProviderGrouping::MultipleYears | ProviderGrouping::Year),
        ) => {
            ensure_absent(&base, &["year", "month", "day", "time"])?;
            let multiple_years = grouping == ProviderGrouping::MultipleYears;
            let axes = calendar_rectangles(time.start(), time.end(), multiple_years)?;
            let mut maps = Vec::new();
            for axes in axes {
                let mut request = base.clone();
                request.insert("year".into(), json!(axes.years));
                request.insert("month".into(), json!(axes.months));
                request.insert("day".into(), json!(axes.days));
                if encoding == TemporalEncoding::HourlyCalendar {
                    request.insert(
                        "time".into(),
                        json!(time.provider_times().collect::<Vec<_>>()),
                    );
                }
                extend_bounded(
                    &mut maps,
                    split_calendar_map(
                        request,
                        spec.profile(),
                        planner.max_items.get(),
                        planner.max_requests.get(),
                    )?,
                    planner.max_requests.get(),
                )?;
            }
            Ok(maps)
        }
        (TemporalEncoding::MonthlyCalendar, ProviderGrouping::MultipleYears) => {
            ensure_absent(&base, &["year", "month", "day"])?;
            if !time.times().is_empty() {
                ensure_absent(&base, &["time"])?;
            }
            let mut maps = Vec::new();
            for axes in monthly_rectangles(time.start(), time.end())? {
                let mut request = base.clone();
                request.insert("year".into(), json!(axes.years));
                request.insert("month".into(), json!(axes.months));
                if !time.times().is_empty() {
                    request.insert(
                        "time".into(),
                        json!(time.provider_times().collect::<Vec<_>>()),
                    );
                }
                extend_bounded(
                    &mut maps,
                    split_calendar_map(
                        request,
                        spec.profile(),
                        planner.max_items.get(),
                        planner.max_requests.get(),
                    )?,
                    planner.max_requests.get(),
                )?;
            }
            Ok(maps)
        }
        _ => build_maps(spec, planner, base),
    }
}

fn validate_request(spec: &PlanningRequest) -> Result<()> {
    let profile = spec.profile();
    if spec.time().is_some() && profile == DatasetProfile::Other {
        invalid!(
            "structured time ranges are not supported for collection `{}`",
            spec.dataset()
        );
    }
    if spec.area().is_some() && profile == DatasetProfile::Other {
        invalid!(
            "structured areas are not supported for collection `{}`",
            spec.dataset()
        );
    }
    if profile.planning().temporal_encoding() == TemporalEncoding::HourlyCalendar
        && spec.time().is_some_and(|time| time.times().is_empty())
    {
        invalid!("hourly ERA5 ranges must contain at least one time of day");
    }
    if profile == DatasetProfile::Era5Daily
        && spec.time().is_some_and(|time| !time.times().is_empty())
    {
        invalid!("daily ERA5 ranges do not accept times of day");
    }
    Ok(())
}

fn split_cost_map(
    profile: DatasetProfile,
    request: Map<String, Value>,
    suggested_parts: usize,
) -> Result<Option<Vec<RequestMap>>> {
    for key in profile.cost_split_axes() {
        let Some(value) = request.get(*key) else {
            continue;
        };
        let parts = match value {
            Value::Array(values) if values.len() > 1 => Some(
                balanced_ranges(values.len(), suggested_parts)
                    .map(|range| Value::Array(values[range].to_vec()))
                    .collect(),
            ),
            Value::String(expression) if *key == "date" => {
                split_mars_date(expression, suggested_parts)?
            }
            _ => None,
        };
        if let Some(mut parts) = parts {
            let last = parts.pop().expect("a split always has at least two parts");
            let mut requests = Vec::with_capacity(parts.len() + 1);
            requests.extend(parts.into_iter().map(|value| {
                let mut part = request.clone();
                part.insert((*key).into(), value);
                part
            }));
            let mut last_request = request;
            last_request.insert((*key).into(), last);
            requests.push(last_request);
            return Ok(Some(requests));
        }
    }
    Ok(None)
}

fn split_mars_date(expression: &str, suggested_parts: usize) -> Result<Option<Vec<Value>>> {
    let parts = expression.split('/').collect::<Vec<_>>();
    if parts.len() != 3 || parts[1] != "to" {
        return Ok(None);
    }
    let start = parse_mars_date(parts[0])?;
    let end = parse_mars_date(parts[2])?;
    let day_count = (end - start).num_days() + 1;
    if day_count < 2 {
        return Ok(None);
    }
    let values = balanced_ranges(usize::try_from(day_count)?, suggested_parts)
        .map(|range| {
            let part_start = start
                .checked_add_days(chrono::Days::new(u64::try_from(range.start)?))
                .or_invalid("MARS date range overflows")?;
            let part_end = start
                .checked_add_days(chrono::Days::new(u64::try_from(range.end - 1)?))
                .or_invalid("MARS date range overflows")?;
            Ok(Value::String(format_mars_range(part_start, part_end)))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(values))
}

fn split_calendar_map(
    request: Map<String, Value>,
    profile: DatasetProfile,
    max_items: usize,
    max_requests: usize,
) -> Result<Vec<Map<String, Value>>> {
    let mut scratch = CalendarEstimateScratch::default();
    let Some(estimated) = estimate_calendar_items(profile, &request, &mut scratch)? else {
        return Ok(vec![request]);
    };
    split_estimated_calendar_map(
        request,
        profile,
        max_items,
        max_requests,
        estimated,
        &mut scratch,
    )
}

fn split_estimated_calendar_map(
    request: Map<String, Value>,
    profile: DatasetProfile,
    max_items: usize,
    max_requests: usize,
    estimated: usize,
    scratch: &mut CalendarEstimateScratch,
) -> Result<Vec<Map<String, Value>>> {
    if estimated <= max_items {
        return Ok(vec![request]);
    }
    let Some(key) = largest_splittable_axis(profile, &request) else {
        invalid!(
            "request is estimated at {estimated} items but cannot be split below the configured limit of {max_items}"
        );
    };
    let mut base = request;
    let values = base.remove(&key).expect("selected axis exists");
    let Value::Array(values) = values else {
        unreachable!("selected axis is an array")
    };
    // Calendar axes include nonexistent dates, so their contribution is not uniform.
    let other_items = estimated.div_ceil(values.len());
    let chunk_size = if other_items <= max_items {
        (max_items / other_items).max(1)
    } else {
        values.len().div_ceil(2)
    };
    let chunks = values.len().div_ceil(chunk_size);
    let mut result = Vec::with_capacity(chunks.min(max_requests));
    let mut values = values.into_iter();
    for index in 0..chunks {
        let chunk = values.by_ref().take(chunk_size).collect::<Vec<_>>();
        let mut part = if index + 1 == chunks {
            std::mem::take(&mut base)
        } else {
            base.clone()
        };
        part.insert(key.clone(), Value::Array(chunk));
        let additional = match estimate_calendar_items(profile, &part, scratch)? {
            Some(0) => Vec::new(),
            Some(estimated) => split_estimated_calendar_map(
                part,
                profile,
                max_items,
                max_requests,
                estimated,
                scratch,
            )?,
            None => vec![part],
        };
        extend_bounded(&mut result, additional, max_requests)?;
    }
    Ok(result)
}

fn split_mars_requests(
    base: &Map<String, Value>,
    time: &TimeRange,
    max_items: usize,
    max_requests: usize,
) -> Result<Vec<Map<String, Value>>> {
    if has_unknown_mars_array(base) {
        invalid!("cannot split a MARS request with an unknown multi-value array");
    }
    let times = if time.times().is_empty() {
        base.get("time").map_or(Ok(1), value_cardinality)?
    } else {
        time.times().len()
    };
    let factor = mars_factor(base)?
        .checked_mul(times)
        .or_invalid("MARS item count overflows usize")?;
    if factor > max_items {
        invalid!(
            "one day is estimated at {factor} MARS items, above the configured limit of {max_items}; reduce parameters or levels"
        );
    }
    let dates_per_request = max_items / factor;
    ensure_months_bounded(time.start(), time.end(), max_requests)?;
    let mut requests = Vec::new();
    let provider_time = (!time.times().is_empty())
        .then(|| Value::String(time.provider_times().collect::<Vec<_>>().join("/")));
    let mut start = time.start();
    loop {
        let month_end = start
            .with_day(1)
            .expect("day one is always valid")
            .checked_add_months(Months::new(1))
            .and_then(|next_month| next_month.pred_opt())
            .map_or_else(|| time.end(), |last_day| last_day.min(time.end()));
        loop {
            if requests.len() == max_requests {
                invalid!("plan exceeds the configured limit of {max_requests} requests");
            }
            let remaining = usize::try_from((month_end - start).num_days())? + 1;
            let chunk_days = remaining.min(dates_per_request);
            let end = start
                .checked_add_days(chrono::Days::new(u64::try_from(chunk_days - 1)?))
                .or_invalid("date range overflows")?;
            let mut request = base.clone();
            request.insert("date".into(), Value::String(format_mars_range(start, end)));
            if let Some(value) = &provider_time {
                request.insert("time".into(), value.clone());
            }
            requests.push(request);
            if end == time.end() {
                return Ok(requests);
            }
            start = end.succ_opt().or_invalid("date range overflows")?;
            if end == month_end {
                break;
            }
        }
    }
}

fn is_non_item_key(key: &str) -> bool {
    matches!(
        key,
        "area" | "grid" | "data_format" | "download_format" | "format" | "target"
    )
}

fn is_calendar_axis(profile: DatasetProfile, key: &str) -> bool {
    match profile {
        DatasetProfile::Era5Hourly | DatasetProfile::Era5LandHourly => matches!(
            key,
            "year" | "month" | "day" | "time" | "variable" | "pressure_level" | "product_type"
        ),
        DatasetProfile::Era5Daily => matches!(
            key,
            "year"
                | "month"
                | "day"
                | "variable"
                | "product_type"
                | "daily_statistic"
                | "frequency"
        ),
        DatasetProfile::Era5Monthly => matches!(
            key,
            "year" | "month" | "time" | "variable" | "pressure_level" | "product_type"
        ),
        DatasetProfile::Era5Complete | DatasetProfile::Other => false,
    }
}

fn is_simple_axis_value(value: &Value) -> bool {
    matches!(value, Value::Bool(_) | Value::Number(_))
        || matches!(value, Value::String(text) if !has_expression_separator(text))
}

fn has_expression_separator(value: &str) -> bool {
    value.bytes().any(|byte| matches!(byte, b'/' | b','))
}

fn value_cardinality(value: &Value) -> Result<usize> {
    match value {
        Value::Array(values) if values.is_empty() => {
            invalid!("MARS axes must contain at least one value");
        }
        Value::Array(values) => Ok(values.len()),
        Value::String(value) => mars_expression_cardinality(value),
        _ => Ok(1),
    }
}

fn mars_factor(request: &Map<String, Value>) -> Result<usize> {
    const AXES: &[&str] = &["param", "levelist", "type", "stream", "number", "step"];
    AXES.iter()
        .filter_map(|key| request.get(*key))
        .map(value_cardinality)
        .try_fold(1_usize, |total, count| {
            total
                .checked_mul(count?)
                .or_invalid("MARS item count overflows usize")
        })
}

fn mars_ordinal(value: &str) -> Option<usize> {
    if let Some((hour, minute)) = value.split_once(':') {
        return hour
            .parse::<usize>()
            .ok()?
            .checked_mul(60)?
            .checked_add(minute.parse::<usize>().ok()?);
    }
    value.parse().ok()
}

fn mars_date_cardinality(value: &Value) -> Result<usize> {
    let expression = value.as_str().or_invalid("MARS date must be a string")?;
    let parts = expression.split('/').collect::<Vec<_>>();
    if matches!(parts.len(), 3 | 5) && parts[1] == "to" {
        let start = parse_mars_date(parts[0])?;
        let end = parse_mars_date(parts[2])?;
        let step = if parts.len() == 5 && parts[3] == "by" {
            parts[4]
                .parse::<i64>()
                .or_invalid("invalid MARS date step")?
        } else if parts.len() == 3 {
            1
        } else {
            invalid!("unsupported MARS date expression `{expression}`");
        };
        if step <= 0 || end < start {
            invalid!("invalid MARS date range `{expression}`");
        }
        return Ok(usize::try_from((end - start).num_days() / step + 1)?);
    }
    if parts.iter().any(|part| *part == "to" || *part == "by") {
        invalid!("unsupported MARS date expression `{expression}`");
    }
    for part in &parts {
        parse_mars_date(part)?;
    }
    Ok(parts.len())
}

fn mars_expression_cardinality(value: &str) -> Result<usize> {
    let parts = value.split('/').collect::<Vec<_>>();
    let Some(to_index) = parts.iter().position(|part| *part == "to") else {
        if parts.iter().any(|part| part.is_empty() || *part == "by") {
            invalid!("unsupported MARS expression `{value}`");
        }
        return Ok(parts.len());
    };
    if to_index != 1 || !matches!(parts.len(), 3 | 5) || (parts.len() == 5 && parts[3] != "by") {
        invalid!("unsupported MARS expression `{value}`");
    }
    let start = mars_ordinal(parts[0]).or_invalid("invalid MARS range start")?;
    let end = mars_ordinal(parts[2]).or_invalid("invalid MARS range end")?;
    let step = if parts.len() == 5 {
        let step = mars_ordinal(parts[4]).or_invalid("invalid MARS range step")?;
        if parts[0].contains(':') && parts[2].contains(':') && !parts[4].contains(':') {
            step.checked_mul(60)
                .or_invalid("MARS range step overflows")?
        } else {
            step
        }
    } else {
        1
    };
    if end < start || step == 0 {
        invalid!("invalid MARS range `{value}`");
    }
    ((end - start) / step)
        .checked_add(1)
        .or_invalid("MARS item count overflows usize")
}

fn monthly_rectangles(start: NaiveDate, end: NaiveDate) -> Result<Vec<MonthlyAxes>> {
    let rectangles: Vec<MonthlyAxes> = months_by_year(start, end)?
        .into_iter()
        .map(|months| MonthlyAxes {
            years: vec![format!("{:04}", months[0].year())],
            months: months
                .iter()
                .map(|month| format!("{:02}", month.month()))
                .collect(),
        })
        .collect();
    let mut merged: Vec<MonthlyAxes> = Vec::with_capacity(rectangles.len());
    for rectangle in rectangles {
        if rectangle.months.len() == 12
            && let Some(previous) = merged.last_mut()
            && previous.months.len() == 12
        {
            previous.years.extend(rectangle.years);
        } else {
            merged.push(rectangle);
        }
    }
    Ok(merged)
}

fn ensure_absent(request: &Map<String, Value>, keys: &[&str]) -> Result<()> {
    if let Some(key) = keys.iter().find(|key| request.contains_key(**key)) {
        invalid!("`{key}` cannot be present in [request] when its structured section is used");
    }
    Ok(())
}

fn ensure_request_limit(requests: usize, planner: Planner) -> Result<()> {
    if requests > planner.max_requests.get() {
        invalid!(
            "plan contains {requests} requests but the configured limit is {}",
            planner.max_requests
        );
    }
    Ok(())
}

fn ensure_years_bounded(start: NaiveDate, end: NaiveDate, max_requests: usize) -> Result<()> {
    let years = i64::from(end.year()) - i64::from(start.year()) + 1;
    if u64::try_from(years)? > u64::try_from(max_requests)? {
        invalid!("plan exceeds the configured limit of {max_requests} requests");
    }
    Ok(())
}

fn ensure_months_bounded(start: NaiveDate, end: NaiveDate, max_requests: usize) -> Result<()> {
    let months = i64::from(end.year() - start.year()) * 12 + i64::from(end.month())
        - i64::from(start.month())
        + 1;
    if u64::try_from(months)? > u64::try_from(max_requests)? {
        invalid!("plan exceeds the configured limit of {max_requests} requests");
    }
    Ok(())
}

fn estimate_mars_items(request: &Map<String, Value>) -> Result<Option<usize>> {
    if has_unknown_mars_array(request) {
        return Ok(None);
    }
    let dates = request.get("date").map_or(Ok(1), mars_date_cardinality)?;
    let times = request.get("time").map_or(Ok(1), value_cardinality)?;
    let factor = mars_factor(request)?;
    Ok(Some(
        factor
            .checked_mul(dates)
            .and_then(|count| count.checked_mul(times))
            .or_invalid("MARS item count overflows usize")?,
    ))
}

fn estimate_calendar_items(
    profile: DatasetProfile,
    request: &Map<String, Value>,
    scratch: &mut CalendarEstimateScratch,
) -> Result<Option<usize>> {
    if profile == DatasetProfile::Other {
        return Ok(None);
    }
    let temporal = calendar_temporal_cardinality(profile, request, scratch)?;
    let mut total = temporal.unwrap_or(1);
    for (key, value) in request {
        if is_calendar_axis(profile, key) {
            if temporal.is_some() && matches!(key.as_str(), "year" | "month" | "day") {
                continue;
            }
            match value {
                Value::Array(values) if values.iter().all(is_simple_axis_value) => {
                    total = total
                        .checked_mul(values.len())
                        .or_invalid("calendar item count overflows usize")?;
                }
                Value::String(value) if has_expression_separator(value) => return Ok(None),
                Value::Array(_) | Value::Object(_) => return Ok(None),
                _ => {}
            }
        } else if matches!(value, Value::Array(values) if values.len() > 1) && !is_non_item_key(key)
        {
            return Ok(None);
        }
    }
    Ok(Some(total))
}

// Both operands are validated as finite and non-negative. Rust's saturating
// float-to-integer cast is useful here: an unrepresentable estimate becomes
// `usize::MAX` and is capped by `max_requests` before any allocation is made.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn suggested_part_count(cost: &RequestCost) -> usize {
    if cost.cost() > cost.preferred_limit() {
        (cost.cost() / cost.preferred_limit()).ceil() as usize
    } else {
        2
    }
    .max(2)
}

fn has_unknown_mars_array(request: &Map<String, Value>) -> bool {
    request.iter().any(|(key, value)| {
        matches!(value, Value::Array(values) if values.len() > 1)
            && !matches!(
                key.as_str(),
                "param" | "levelist" | "type" | "stream" | "number" | "step" | "time" | "date"
            )
            && !is_non_item_key(key)
    })
}

fn largest_splittable_axis(
    profile: DatasetProfile,
    request: &Map<String, Value>,
) -> Option<String> {
    request
        .iter()
        .filter(|(key, _)| is_calendar_axis(profile, key))
        .filter_map(|(key, value)| match value {
            Value::Array(values) if values.len() > 1 => Some((key, values.len())),
            _ => None,
        })
        .max_by_key(|(_, len)| *len)
        .map(|(key, _)| key.clone())
}

fn calendar_rectangles(
    start: NaiveDate,
    end: NaiveDate,
    multiple_years: bool,
) -> Result<Vec<CalendarAxes>> {
    let mut rectangles: Vec<CalendarAxes> = Vec::new();
    let mut cursor = start;
    loop {
        let first_of_month = cursor.with_day(1).expect("day one is always valid");
        let next_month = first_of_month
            .checked_add_months(Months::new(1))
            .or_invalid("month range overflows")?;
        let last_of_month = next_month.pred_opt().expect("a month is never empty");
        let segment_end = end.min(last_of_month);
        let is_full_month = cursor == first_of_month && segment_end == last_of_month;
        let year = format!("{:04}", cursor.year());
        let month = format!("{:02}", cursor.month());
        if is_full_month
            && let Some(previous) = rectangles.last_mut()
            && previous.years.as_slice() == [year.as_str()]
            && previous.days.len() == 31
        {
            previous.months.push(month);
        } else {
            rectangles.push(CalendarAxes {
                years: vec![year],
                months: vec![month],
                days: if is_full_month {
                    (1..=31).map(|day| format!("{day:02}")).collect()
                } else {
                    (cursor.day()..=segment_end.day())
                        .map(|day| format!("{day:02}"))
                        .collect()
                },
            });
        }
        if segment_end == end {
            break;
        }
        cursor = segment_end.succ_opt().or_invalid("date range overflows")?;
    }
    if !multiple_years {
        return Ok(rectangles);
    }
    let mut merged: Vec<CalendarAxes> = Vec::with_capacity(rectangles.len());
    for rectangle in rectangles {
        if rectangle.is_full_year()
            && let Some(previous) = merged.last_mut()
            && previous.is_full_year()
        {
            previous.years.extend(rectangle.years);
        } else {
            merged.push(rectangle);
        }
    }
    Ok(merged)
}

fn calendar_temporal_cardinality(
    profile: DatasetProfile,
    request: &Map<String, Value>,
    scratch: &mut CalendarEstimateScratch,
) -> Result<Option<usize>> {
    if !parse_numeric_axis(request.get("year"), &mut scratch.years) {
        return Ok(None);
    }
    if !parse_numeric_axis(request.get("month"), &mut scratch.months) {
        return Ok(None);
    }
    if profile == DatasetProfile::Era5Monthly {
        return scratch
            .years
            .len()
            .checked_mul(scratch.months.len())
            .map(Some)
            .or_invalid("calendar item count overflows usize");
    }
    if !parse_numeric_axis(request.get("day"), &mut scratch.days) {
        return Ok(None);
    }
    let years = &scratch.years;
    let months = &scratch.months;
    let days = &scratch.days;
    if years.iter().any(|&year| i32::try_from(year).is_err())
        || months.iter().any(|&month| u32::try_from(month).is_err())
        || days.iter().any(|&day| u32::try_from(day).is_err())
    {
        return Ok(None);
    }
    let mut count = 0_usize;
    for &year in years {
        let year = i32::try_from(year).expect("calendar year was validated");
        for &month in months {
            let month = u32::try_from(month).expect("calendar month was validated");
            let Some(last_day) = calendar_days_in_month(year, month) else {
                continue;
            };
            let valid_days = days
                .iter()
                .filter(|&&day| (1..=i64::from(last_day)).contains(&day))
                .count();
            count = count
                .checked_add(valid_days)
                .or_invalid("calendar item count overflows usize")?;
        }
    }
    Ok(Some(count))
}

fn calendar_days_in_month(year: i32, month: u32) -> Option<u32> {
    NaiveDate::from_ymd_opt(year, month, 1)?;
    Some(match month {
        4 | 6 | 9 | 11 => 30,
        2 if NaiveDate::from_ymd_opt(year, 2, 29).is_some() => 29,
        2 => 28,
        _ => 31,
    })
}

pub(super) async fn refine_plan(
    client: &Client,
    plan: &Plan,
    planner: Planner,
) -> PlanningResult<Plan> {
    ensure_request_limit(plan.requests.len(), planner)?;
    let profile = plan.profile;
    let maps = plan
        .requests
        .iter()
        .map(|request| request.selection.as_map().clone())
        .collect();
    let costed = split_by_provider_cost(&plan.dataset, profile, planner, client, maps).await?;
    finish_maps(plan.dataset.clone(), profile, planner, costed)
}

async fn split_by_provider_cost(
    dataset: &CollectionId,
    profile: DatasetProfile,
    planner: Planner,
    client: &Client,
    maps: Vec<Map<String, Value>>,
) -> PlanningResult<Vec<CostedMap>> {
    let mut pending = VecDeque::from(maps);
    let mut accepted = Vec::new();
    let mut plan_changed = false;
    while let Some(map) = pending.pop_front() {
        let selection = Selection::from(map);
        let cost = client
            .estimate_cost(dataset, &selection)
            .await
            .map_err(|error| {
                if plan_changed {
                    PlanningError::CostingInterrupted(error)
                } else {
                    PlanningError::CostingUnavailable(error)
                }
            })?;
        let map = selection.into_map();
        let target = cost.preferred_limit();
        let needs_split = !cost.is_valid() || cost.cost() > target;
        if needs_split {
            let available_parts = planner
                .max_requests
                .get()
                .saturating_sub(accepted.len() + pending.len());
            let suggested_parts = suggested_part_count(&cost).min(available_parts.max(2));
            if let Some(parts) = split_cost_map(profile, map.clone(), suggested_parts)? {
                if parts.len() > available_parts {
                    return Err(PlanningError::InvalidRequest(format!(
                        "provider-aware plan exceeds the configured limit of {} requests",
                        planner.max_requests
                    )));
                }
                for part in parts.into_iter().rev() {
                    pending.push_front(part);
                }
                plan_changed = true;
                continue;
            }
        }
        if !cost.is_valid() {
            return Err(PlanningError::InvalidRequest(format!(
                "provider rejected an indivisible request: {}",
                cost.invalid_reason().unwrap_or("unknown reason")
            )));
        }
        if cost.cost() > cost.limit() {
            return Err(PlanningError::InvalidRequest(format!(
                "provider cost {} exceeds the limit {} and the request cannot be split further",
                cost.cost(),
                cost.limit()
            )));
        }
        accepted.push((map, Some(cost)));
    }
    Ok(accepted)
}
