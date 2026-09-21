use std::num::NonZeroUsize;

use chrono::{NaiveDate, NaiveTime, Timelike as _};
use thiserror::Error;

use crate::{Client, CollectionId, RequestCost, Selection, error::Error as ClientError};

use super::{DatasetProfile, engine};

/// Conservative request size used when no collection-specific limit is known.
pub const DEFAULT_MAX_ITEMS: usize = 100_000;
/// Default upper bound on requests produced by one plan.
pub const DEFAULT_MAX_REQUESTS: usize = 1_000;

const MINUTES_PER_DAY: usize = 24 * 60;
const BITS_PER_WORD: usize = 64;
const MINUTE_BITMAP_WORDS: usize = MINUTES_PER_DAY.div_ceil(BITS_PER_WORD);

/// Result type returned by planning operations.
pub type PlanningResult<T> = std::result::Result<T, PlanningError>;

/// An error produced while validating or partitioning a request.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PlanningError {
    /// The request cannot be represented or safely partitioned.
    #[error("invalid planning request: {0}")]
    InvalidRequest(String),

    /// Provider costing could not be queried.
    #[error("provider costing is unavailable: {0}")]
    CostingUnavailable(#[source] ClientError),
    /// Provider costing failed after it had already required the plan to change.
    #[error("provider costing became unavailable after refining the plan: {0}")]
    CostingInterrupted(#[source] ClientError),
}

/// Whether automatic planning used provider costs or its local fallback.
#[derive(Debug)]
#[non_exhaustive]
pub enum CostingOutcome {
    /// Every final request was checked by the provider.
    Provider,

    /// Costing was unavailable and the local plan was retained.
    LocalFallback {
        /// Transport or service error that caused the fallback.
        reason: ClientError,
    },
}

/// A complete collection plan independent of output files or execution policy.
#[derive(Debug, Clone)]
pub struct Plan {
    pub(super) dataset: CollectionId,

    pub(super) profile: DatasetProfile,

    pub(super) requests: Vec<PlannedRequest>,
}

impl Plan {
    /// Returns the target collection.
    pub const fn dataset(&self) -> &CollectionId {
        &self.dataset
    }

    /// Returns the profile used to build and refine this plan.
    pub const fn profile(&self) -> DatasetProfile {
        self.profile
    }

    /// Returns planned requests in submission order.
    pub fn requests(&self) -> &[PlannedRequest] {
        &self.requests
    }

    /// Consumes the plan and returns its requests.
    pub fn into_requests(self) -> Vec<PlannedRequest> {
        self.requests
    }

    /// Returns the total local item estimate when every part is known.
    pub fn estimated_items(&self) -> Option<usize> {
        self.requests.iter().try_fold(0_usize, |total, request| {
            total.checked_add(request.estimated_items?)
        })
    }
}

/// Configures local limits and provider-aware refinement.
#[derive(Debug, Clone, Copy)]
pub struct Planner {
    pub(super) max_items: NonZeroUsize,
    pub(super) max_requests: NonZeroUsize,
}

impl Planner {
    /// Returns a planner with conservative default limits.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the maximum local item estimate allowed per request.
    pub const fn max_items(self) -> NonZeroUsize {
        self.max_items
    }
    /// Returns the maximum number of requests allowed in a plan.
    pub const fn max_requests(self) -> NonZeroUsize {
        self.max_requests
    }

    /// Builds a plan using only local profiles and estimates.
    pub fn plan_locally(&self, request: &PlanningRequest) -> PlanningResult<Plan> {
        engine::build_local_plan(request, *self)
    }

    /// Sets the maximum local item estimate per request.
    pub const fn with_max_items(mut self, max_items: NonZeroUsize) -> Self {
        self.max_items = max_items;
        self
    }
    /// Sets the maximum number of planned requests.
    pub const fn with_max_requests(mut self, max_requests: NonZeroUsize) -> Self {
        self.max_requests = max_requests;
        self
    }

    /// Plans with provider costs and falls back when costing is initially unavailable.
    ///
    /// Once a provider estimate requires the local plan to change, a later
    /// costing failure is returned as [`PlanningError::CostingInterrupted`].
    /// Falling back at that point could return a request already known to
    /// exceed a provider limit.
    pub async fn plan(
        &self,
        client: &Client,
        request: &PlanningRequest,
    ) -> PlanningResult<PlanningOutcome> {
        let local = self.plan_locally(request)?;
        match self.refine_with_costs(client, &local).await {
            Ok(plan) => Ok(PlanningOutcome {
                plan,
                costing: CostingOutcome::Provider,
            }),
            Err(PlanningError::CostingUnavailable(reason)) => Ok(PlanningOutcome {
                plan: local,
                costing: CostingOutcome::LocalFallback { reason },
            }),
            Err(error) => Err(error),
        }
    }

    /// Refines an existing local plan using provider cost estimates.
    pub async fn refine_with_costs(&self, client: &Client, plan: &Plan) -> PlanningResult<Plan> {
        engine::refine_plan(client, plan, *self).await
    }
}
impl Default for Planner {
    fn default() -> Self {
        Self {
            max_items: NonZeroUsize::new(DEFAULT_MAX_ITEMS).expect("default is non-zero"),
            max_requests: NonZeroUsize::new(DEFAULT_MAX_REQUESTS).expect("default is non-zero"),
        }
    }
}

/// Inclusive calendar range with normalized times of day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeRange {
    end: NaiveDate,

    start: NaiveDate,

    times: Vec<NaiveTime>,
}

impl TimeRange {
    /// Creates an inclusive range from typed dates and times.
    pub fn new(
        start: NaiveDate,
        end: NaiveDate,
        times: impl IntoIterator<Item = NaiveTime>,
    ) -> PlanningResult<Self> {
        if start > end {
            return Err(PlanningError::InvalidRequest(
                "range start must not be later than its end".into(),
            ));
        }
        let times = times.into_iter().collect::<Vec<_>>();
        if times
            .iter()
            .any(|time| time.second() != 0 || time.nanosecond() != 0)
        {
            return Err(PlanningError::InvalidRequest(
                "range times must have minute precision".into(),
            ));
        }
        let mut seen_minutes = [0_u64; MINUTE_BITMAP_WORDS];
        for time in &times {
            let minute = usize::try_from(time.num_seconds_from_midnight() / 60)
                .map_err(|_| PlanningError::InvalidRequest("time of day is out of range".into()))?;
            let mask = 1_u64 << (minute % BITS_PER_WORD);
            let word = &mut seen_minutes[minute / BITS_PER_WORD];
            if *word & mask != 0 {
                return Err(PlanningError::InvalidRequest(
                    "range times must not contain duplicates".into(),
                ));
            }
            *word |= mask;
        }
        Ok(Self { end, start, times })
    }

    /// Returns the last included date.
    pub const fn end(&self) -> NaiveDate {
        self.end
    }

    /// Returns the first included date.
    pub const fn start(&self) -> NaiveDate {
        self.start
    }

    /// Returns typed times of day in their configured order.
    pub fn times(&self) -> &[NaiveTime] {
        &self.times
    }

    pub(super) fn provider_times(&self) -> impl Iterator<Item = String> + '_ {
        self.times
            .iter()
            .map(|time| time.format("%H:%M").to_string())
    }
}

/// Validated geographic subset in south/north and west/east coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeographicArea {
    west: f64,

    east: f64,

    south: f64,

    north: f64,
}

impl GeographicArea {
    /// Creates a bounded area from `[min, max]` latitude and longitude pairs.
    pub fn new(latitude: [f64; 2], longitude: [f64; 2]) -> PlanningResult<Self> {
        let [south, north] = latitude;
        let [west, east] = longitude;
        if ![south, north, west, east]
            .iter()
            .all(|value| value.is_finite())
        {
            return Err(PlanningError::InvalidRequest(
                "area coordinates must be finite".into(),
            ));
        }
        if !(-90.0..=90.0).contains(&south) || !(-90.0..=90.0).contains(&north) || south > north {
            return Err(PlanningError::InvalidRequest(
                "area latitude must be [min, max] within -90..=90".into(),
            ));
        }
        if !(-180.0..=180.0).contains(&west) || !(-180.0..=180.0).contains(&east) || west > east {
            return Err(PlanningError::InvalidRequest(
                "area longitude must be [min, max] within -180..=180".into(),
            ));
        }
        Ok(Self {
            west,
            east,
            south,
            north,
        })
    }

    /// Returns coordinates in CDS north, west, south, east order.
    pub const fn cds_order(self) -> [f64; 4] {
        [self.north, self.west, self.south, self.east]
    }
}

/// One bounded selection produced by a planner.
#[derive(Debug, Clone)]
pub struct PlannedRequest {
    pub(super) selection: Selection,

    pub(super) provider_cost: Option<RequestCost>,

    pub(super) estimated_items: Option<usize>,
}

impl PlannedRequest {
    /// Returns the exact selection sent to CDS.
    pub const fn selection(&self) -> &Selection {
        &self.selection
    }

    /// Consumes the request into its selection, local estimate, and provider cost.
    pub fn into_parts(self) -> (Selection, Option<usize>, Option<RequestCost>) {
        (self.selection, self.estimated_items, self.provider_cost)
    }

    /// Returns the server-side cost estimate used to build the plan, when available.
    pub const fn provider_cost(&self) -> Option<&RequestCost> {
        self.provider_cost.as_ref()
    }

    /// Returns the estimated Cartesian item count.
    pub const fn estimated_items(&self) -> Option<usize> {
        self.estimated_items
    }
}

/// A collection request plus optional structured dimensions used by the planner.
#[derive(Debug, Clone)]
pub struct PlanningRequest {
    time: Option<TimeRange>,

    area: Option<GeographicArea>,

    dataset: CollectionId,

    profile: DatasetProfile,

    request: Selection,
}

impl PlanningRequest {
    /// Creates a request whose raw selection is preserved verbatim.
    pub fn new(dataset: CollectionId, request: Selection) -> Self {
        let profile = DatasetProfile::for_collection(&dataset);
        Self {
            dataset,
            profile,
            request,
            time: None,
            area: None,
        }
    }

    /// Returns the structured time range, when present.
    pub const fn time(&self) -> Option<&TimeRange> {
        self.time.as_ref()
    }

    /// Returns the structured geographic area, when present.
    pub const fn area(&self) -> Option<GeographicArea> {
        self.area
    }

    /// Returns the target collection.
    pub const fn dataset(&self) -> &CollectionId {
        &self.dataset
    }

    /// Returns the exact base selection.
    pub const fn request(&self) -> &Selection {
        &self.request
    }

    /// Returns the selected dataset profile.
    pub const fn profile(&self) -> DatasetProfile {
        self.profile
    }

    /// Adds a structured geographic subset.
    pub fn with_area(mut self, area: GeographicArea) -> Self {
        self.area = Some(area);
        self
    }
    /// Overrides the profile inferred from the collection identifier.
    pub fn with_profile(mut self, profile: DatasetProfile) -> Self {
        self.profile = profile;
        self
    }
    /// Adds a structured inclusive time range.
    pub fn with_time_range(mut self, time: TimeRange) -> Self {
        self.time = Some(time);
        self
    }
}

/// Result of automatic provider-aware planning.
#[derive(Debug)]
pub struct PlanningOutcome {
    plan: Plan,

    costing: CostingOutcome,
}

impl PlanningOutcome {
    /// Returns the resulting plan.
    pub const fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Returns how request costs were obtained.
    pub const fn costing(&self) -> &CostingOutcome {
        &self.costing
    }

    /// Consumes the outcome and returns the plan.
    pub fn into_plan(self) -> Plan {
        self.plan
    }
    /// Consumes the outcome into its plan and costing status.
    pub fn into_parts(self) -> (Plan, CostingOutcome) {
        (self.plan, self.costing)
    }
}
