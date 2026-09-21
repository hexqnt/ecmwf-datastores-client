//! Provider-aware request planning layered over the low-level client API.

pub use model::{
    CostingOutcome, DEFAULT_MAX_ITEMS, DEFAULT_MAX_REQUESTS, GeographicArea, Plan, PlannedRequest,
    Planner, PlanningError, PlanningOutcome, PlanningRequest, PlanningResult, TimeRange,
};
pub use profile::DatasetProfile;

mod engine;
mod model;
mod profile;

#[cfg(test)]
mod tests;
