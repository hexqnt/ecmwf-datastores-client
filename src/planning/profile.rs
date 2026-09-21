use crate::CollectionId;

/// Strategy used to translate structured temporal fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DatasetProfile {
    /// A collection for which only exact raw fields are understood.
    Other,

    /// Daily ERA5 statistics grouped by year.
    Era5Daily,

    /// Hourly disk-backed ERA5 datasets allowing multi-year selections.
    Era5Hourly,

    /// Monthly ERA5 data allowing multi-year selections.
    Era5Monthly,

    /// ERA5 Complete using MARS expressions.
    Era5Complete,

    /// Hourly ERA5-Land data restricted to one month per request.
    Era5LandHourly,
}

impl DatasetProfile {
    pub(super) const fn planning(self) -> PlanningProfile {
        match self {
            Self::Era5Hourly => PlanningProfile {
                temporal_encoding: TemporalEncoding::HourlyCalendar,
                provider_grouping: ProviderGrouping::MultipleYears,
            },
            Self::Era5LandHourly => PlanningProfile {
                temporal_encoding: TemporalEncoding::HourlyCalendar,
                provider_grouping: ProviderGrouping::Month,
            },
            Self::Era5Daily => PlanningProfile {
                temporal_encoding: TemporalEncoding::DailyCalendar,
                provider_grouping: ProviderGrouping::Year,
            },
            Self::Era5Monthly => PlanningProfile {
                temporal_encoding: TemporalEncoding::MonthlyCalendar,
                provider_grouping: ProviderGrouping::MultipleYears,
            },
            Self::Era5Complete => PlanningProfile {
                temporal_encoding: TemporalEncoding::Mars,
                provider_grouping: ProviderGrouping::Month,
            },
            Self::Other => PlanningProfile {
                temporal_encoding: TemporalEncoding::Raw,
                provider_grouping: ProviderGrouping::Raw,
            },
        }
    }

    /// Selects the built-in profile for a collection identifier.
    pub fn for_collection(id: &CollectionId) -> Self {
        match id.as_str() {
            "reanalysis-era5-pressure-levels" | "reanalysis-era5-single-levels" => Self::Era5Hourly,
            "reanalysis-era5-land" => Self::Era5LandHourly,
            "derived-era5-single-levels-daily-statistics" => Self::Era5Daily,
            "reanalysis-era5-land-monthly-means"
            | "reanalysis-era5-single-levels-monthly-means"
            | "reanalysis-era5-pressure-levels-monthly-means" => Self::Era5Monthly,
            "reanalysis-era5-complete" => Self::Era5Complete,
            _ => Self::Other,
        }
    }

    pub(super) const fn cost_split_axes(self) -> &'static [&'static str] {
        match self {
            Self::Era5Hourly | Self::Era5LandHourly => &[
                "year",
                "month",
                "day",
                "time",
                "pressure_level",
                "variable",
                "product_type",
            ],
            Self::Era5Daily => &[
                "year",
                "month",
                "day",
                "variable",
                "product_type",
                "daily_statistic",
                "frequency",
            ],
            Self::Era5Monthly => &[
                "year",
                "month",
                "time",
                "pressure_level",
                "variable",
                "product_type",
            ],
            Self::Era5Complete => &[
                "date", "time", "levelist", "param", "number", "step", "type", "stream",
            ],
            Self::Other => &[],
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TemporalEncoding {
    Raw,

    Mars,

    DailyCalendar,

    HourlyCalendar,

    MonthlyCalendar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProviderGrouping {
    Raw,

    Year,

    Month,

    MultipleYears,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PlanningProfile {
    temporal_encoding: TemporalEncoding,

    provider_grouping: ProviderGrouping,
}

impl PlanningProfile {
    pub(super) const fn temporal_encoding(self) -> TemporalEncoding {
        self.temporal_encoding
    }

    pub(super) const fn provider_grouping(self) -> ProviderGrouping {
        self.provider_grouping
    }
}
