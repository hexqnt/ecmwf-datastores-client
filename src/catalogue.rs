use std::{collections::HashMap, num::NonZeroU32};

use chrono::{DateTime, Utc};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Map, Value};
use url::Url;

use crate::{
    error::{Error, Result},
    http::{HttpClient, PageLink, PagePayload, Paged, RequestTemplate, TypedResponse},
    id::{CollectionId, LicenceId},
    serde_helpers, utils,
};

/// A spatial extent with its coordinate count encoded in the type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BoundingBox {
    /// Lower and upper corners in a two-dimensional coordinate system.
    TwoDimensional([f64; 4]),

    /// Lower and upper corners in a coordinate system with a vertical axis.
    ThreeDimensional([f64; 6]),
}

impl BoundingBox {
    /// Returns the coordinates in service-defined axis order.
    pub fn as_slice(&self) -> &[f64] {
        match self {
            Self::TwoDimensional(coordinates) => coordinates,
            Self::ThreeDimensional(coordinates) => coordinates,
        }
    }

    /// Returns the number of coordinate axes represented by this box.
    pub const fn dimensions(&self) -> usize {
        match self {
            Self::TwoDimensional(_) => 2,
            Self::ThreeDimensional(_) => 3,
        }
    }
}

impl<'de> Deserialize<'de> for BoundingBox {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct BoundingBoxVisitor;

        impl<'de> serde::de::Visitor<'de> for BoundingBoxVisitor {
            type Value = BoundingBox;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a bounding box containing four or six coordinates")
            }

            fn visit_seq<A>(self, mut sequence: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                use serde::de::Error as _;

                let mut coordinates = [0.0; 6];
                let mut length = 0;
                while let Some(coordinate) = sequence.next_element()? {
                    if length == coordinates.len() {
                        return Err(A::Error::invalid_length(length + 1, &self));
                    }
                    coordinates[length] = coordinate;
                    length += 1;
                }
                let [first, second, third, fourth, fifth, sixth] = coordinates;
                match length {
                    4 => Ok(BoundingBox::TwoDimensional([first, second, third, fourth])),
                    6 => Ok(BoundingBox::ThreeDimensional([
                        first, second, third, fourth, fifth, sixth,
                    ])),
                    _ => Err(A::Error::invalid_length(length, &self)),
                }
            }
        }

        deserializer.deserialize_seq(BoundingBoxVisitor)
    }
}

/// Sort orders supported by the catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CollectionSort {
    /// Sort by collection identifier.
    Id,

    /// Sort by collection title.
    Title,

    /// Sort by last update time.
    Update,

    /// Sort by search relevance.
    Relevance,
}

impl CollectionSort {
    /// Returns the string representation used by the API.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Relevance => "relevance",
            Self::Title => "title",
            Self::Update => "update",
        }
    }
}

/// Hypermedia link object returned by the catalogue.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct Link {
    /// Link relation, such as `self`, `next`, or `results`.
    pub rel: Option<String>,

    /// Absolute or response-relative target URL.
    pub href: String,

    /// Optional human-readable link title.
    #[serde(default)]
    pub title: Option<String>,

    /// Optional media type of the target resource.
    #[serde(default)]
    pub r#type: Option<String>,
}

/// Licence metadata entry.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct Licence {
    /// Stable licence identifier.
    pub id: LicenceId,

    /// Additional server-provided licence metadata.
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,

    /// Current licence revision, when supplied by the server.
    #[serde(default)]
    pub revision: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct LicencePayload {
    #[serde(default)]
    licences: Vec<Licence>,
}

/// Client for catalogue-related API endpoints.
#[derive(Clone)]
pub struct CatalogueApi {
    http: HttpClient,

    base_url: Url,
}

impl CatalogueApi {
    /// Creates a new catalogue API client rooted at the given base URL.
    pub fn new(mut base_url: Url, http: HttpClient) -> Self {
        utils::ensure_trailing_slash(&mut base_url);
        Self { http, base_url }
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.base_url.join(path).map_err(Error::from)
    }

    /// Fetches the collection's request form definition.
    pub async fn get_form(&self, collection_id: &CollectionId) -> Result<Vec<Map<String, Value>>> {
        let url = self.endpoint(&format!("collections/{collection_id}/form.json"))?;
        self.http
            .execute_typed(&RequestTemplate::new(Method::GET, url))
            .await
    }
    /// Lists licences available for the current user.
    pub async fn get_licences(&self, scope: Option<&str>) -> Result<Vec<Licence>> {
        let mut template =
            RequestTemplate::new(Method::GET, self.endpoint("vocabularies/licences")?);
        if let Some(scope) = scope {
            template = template.with_query_pair("scope", scope.to_string());
        }
        let payload: LicencePayload = self.http.execute_typed(&template).await?;
        Ok(payload.licences)
    }
    /// Fetches a single collection by identifier.
    pub async fn get_collection(&self, collection_id: &CollectionId) -> Result<Collection> {
        let url = self.endpoint(&format!("collections/{collection_id}"))?;
        let template = RequestTemplate::new(Method::GET, url);
        self.http.execute_typed(&template).await
    }
    /// Fetches the collection's published parameter constraints.
    pub async fn get_constraints(
        &self,
        collection_id: &CollectionId,
    ) -> Result<Vec<Map<String, Value>>> {
        let url = self.endpoint(&format!("collections/{collection_id}/constraints.json"))?;
        self.http
            .execute_typed(&RequestTemplate::new(Method::GET, url))
            .await
    }

    /// Collects all collections across pages.
    pub async fn all_collections(&self, request: &CollectionsRequest) -> Result<Vec<Collection>> {
        self.list_collections(request).await?.collect_all().await
    }

    /// Retrieves a single page of collections.
    pub async fn list_collections(&self, request: &CollectionsRequest) -> Result<CollectionsPage> {
        let mut template = RequestTemplate::new(Method::GET, self.endpoint("datasets")?);
        if request.search_stats {
            template = template.with_query_pair("search_stats", "true");
        }
        if let Some(limit) = request.limit {
            template = template.with_query_pair("limit", limit.to_string());
        }
        if let Some(sort_by) = request.sort_by {
            template = template.with_query_pair("sortby", sort_by.as_str());
        }
        if let Some(query) = &request.query {
            template = template.with_query_pair("q", query.clone());
        }
        for keyword in &request.keywords {
            template = template.with_query_pair("kw", keyword.clone());
        }

        let response = self
            .http
            .execute_typed_response::<CollectionsPayload>(&template)
            .await?;
        Ok(CollectionsPage::new(response, self.http.clone()))
    }

    /// Logs any broadcast messages returned by the API.
    pub async fn broadcast_messages(&self) -> Result<()> {
        let url = self.endpoint("messages")?;
        let template = RequestTemplate::new(Method::GET, url).with_log_messages(false);
        let response = self.http.execute(&template).await?;
        response.log_messages();
        Ok(())
    }
}

/// Geographic extent expressed as bounding boxes.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct SpatialExtent {
    /// Two- or three-dimensional bounding boxes.
    #[serde(default)]
    pub bbox: Vec<BoundingBox>,
}

/// Time intervals covered by a collection.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct TemporalExtent {
    /// Start/end time intervals.
    #[serde(default)]
    pub interval: Vec<TemporalInterval>,
}

/// One closed or open temporal interval in a collection extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemporalInterval {
    /// Inclusive end of the interval, or `None` for an open upper bound.
    pub end: Option<DateTime<Utc>>,

    /// Inclusive start of the interval, or `None` for an open lower bound.
    pub start: Option<DateTime<Utc>>,
}

impl<'de> Deserialize<'de> for TemporalInterval {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let [start, end] = <[Option<serde_helpers::ParsedDateTime>; 2]>::deserialize(deserializer)?;
        Ok(Self {
            start: start.map(|value| value.0),
            end: end.map(|value| value.0),
        })
    }
}

/// Catalogue collection metadata.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct Collection {
    /// Stable collection identifier used for retrieval requests.
    pub id: CollectionId,

    /// Human-readable collection title.
    #[serde(default)]
    pub title: Option<String>,

    /// Hypermedia links associated with the collection.
    #[serde(default)]
    pub links: Vec<Link>,

    /// Additional server fields not represented by typed members.
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,

    /// Spatial and temporal coverage metadata.
    #[serde(default)]
    pub extent: Option<CollectionExtent>,

    /// Last update time reported by the catalogue.
    #[serde(
        default,
        deserialize_with = "serde_helpers::deserialize_option_datetime"
    )]
    pub updated: Option<DateTime<Utc>>,

    /// Search and classification keywords.
    #[serde(default)]
    pub keywords: Vec<String>,

    /// Initial publication time reported by the catalogue.
    #[serde(
        default,
        deserialize_with = "serde_helpers::deserialize_option_datetime"
    )]
    pub published: Option<DateTime<Utc>>,

    /// Human-readable collection description.
    #[serde(default)]
    pub description: Option<String>,
}

impl Collection {
    /// Returns the last update timestamp if provided.
    pub fn updated_at(&self) -> Option<DateTime<Utc>> {
        self.updated
    }

    /// Returns the publication timestamp if provided.
    pub fn published_at(&self) -> Option<DateTime<Utc>> {
        self.published
    }

    /// Extracts the end of the temporal extent.
    pub fn end_datetime(&self) -> Option<DateTime<Utc>> {
        self.extent
            .as_ref()?
            .temporal
            .as_ref()?
            .interval
            .first()?
            .end
    }

    /// Extracts the beginning of the temporal extent.
    pub fn begin_datetime(&self) -> Option<DateTime<Utc>> {
        self.extent
            .as_ref()?
            .temporal
            .as_ref()?
            .interval
            .first()?
            .start
    }
}

/// Spatial and temporal extent metadata.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct CollectionExtent {
    /// Geographic coverage.
    #[serde(default)]
    pub spatial: Option<SpatialExtent>,

    /// Time coverage.
    #[serde(default)]
    pub temporal: Option<TemporalExtent>,
}

/// Faceted search statistics returned by the catalogue.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct CollectionSearchStats {
    /// Counts grouped by keyword category.
    #[serde(default, rename = "kw")]
    pub keyword_facets: Vec<CollectionSearchFacet>,
}

/// Counts for one catalogue keyword category.
#[derive(Debug, Clone, Deserialize)]
#[non_exhaustive]
pub struct CollectionSearchFacet {
    /// Number of matching collections for each keyword.
    pub groups: HashMap<String, u64>,

    /// Human-readable keyword category.
    pub category: String,
}

/// Paginated wrapper around catalogue collections.
#[derive(Debug, Clone)]
pub struct CollectionsPage {
    inner: Paged<Collection>,

    search_stats: Option<CollectionSearchStats>,

    number_matched: Option<u64>,
    number_returned: Option<u64>,
}

impl CollectionsPage {
    fn new(response: TypedResponse<CollectionsPayload>, http: HttpClient) -> Self {
        let CollectionsPayload {
            collections,
            links,
            search,
            number_matched,
            number_returned,
        } = response.body;
        Self {
            inner: Paged::new(collections, links, response.url, http),
            search_stats: search,
            number_matched,
            number_returned,
        }
    }

    /// Returns a borrowed slice of collections on this page.
    pub fn collections(&self) -> &[Collection] {
        self.inner.items()
    }

    /// Returns search facets when requested and supplied by the service.
    pub fn search_stats(&self) -> Option<&CollectionSearchStats> {
        self.search_stats.as_ref()
    }

    /// Returns the total number of collections matching the query, when supplied.
    pub fn number_matched(&self) -> Option<u64> {
        self.number_matched
    }
    /// Returns the number of collections on this page, when supplied.
    pub fn number_returned(&self) -> Option<u64> {
        self.number_returned
    }

    /// Consumes the page and returns the owned list of collections.
    pub fn into_collections(self) -> Vec<Collection> {
        self.inner.into_items()
    }

    /// Retrieves the next page if available.
    pub async fn next(&self) -> Result<Option<Self>> {
        self.follow("next").await
    }

    /// Retrieves the previous page if available.
    pub async fn prev(&self) -> Result<Option<Self>> {
        self.follow("prev").await
    }

    async fn follow(&self, rel: &str) -> Result<Option<Self>> {
        let Some(response) = self.inner.fetch::<CollectionsPayload>(rel).await? else {
            return Ok(None);
        };
        Ok(Some(Self::new(response, self.inner.http().clone())))
    }

    /// Collects all remaining collections across pages.
    pub async fn collect_all(self) -> Result<Vec<Collection>> {
        self.inner.collect_all::<CollectionsPayload>().await
    }
}

/// Filters and options used when listing collections.
#[derive(Debug, Clone, Default)]
pub struct CollectionsRequest {
    /// Maximum non-zero number of collections returned on one page.
    pub limit: Option<NonZeroU32>,

    /// Full-text search query.
    pub query: Option<String>,

    /// Requested result ordering.
    pub sort_by: Option<CollectionSort>,

    /// Catalogue keyword filters.
    pub keywords: Vec<String>,

    /// Whether the service should include search statistics.
    pub search_stats: bool,
}

impl CollectionsRequest {
    /// Sets the maximum number of collections returned on one page.
    pub fn with_limit(mut self, limit: NonZeroU32) -> Self {
        self.limit = Some(limit);
        self
    }
    /// Adds a full-text search query.
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = Some(query.into());
        self
    }
    /// Adds a keyword filter.
    pub fn with_keyword(mut self, keyword: impl Into<String>) -> Self {
        self.keywords.push(keyword.into());
        self
    }
}

#[derive(Debug, Deserialize)]
struct CollectionsPayload {
    #[serde(default)]
    links: Vec<PageLink>,

    #[serde(default)]
    search: Option<CollectionSearchStats>,

    collections: Vec<Collection>,

    #[serde(default, rename = "numberMatched")]
    number_matched: Option<u64>,
    #[serde(default, rename = "numberReturned")]
    number_returned: Option<u64>,
}

impl PagePayload<Collection> for CollectionsPayload {
    fn into_page(self) -> (Vec<Collection>, Vec<PageLink>) {
        (self.collections, self.links)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_collection_from_sanitized_live_fixture() {
        let fixture: Value =
            serde_json::from_str(include_str!("../tests/fixtures/cds-contract.json")).unwrap();
        let collection: Collection = serde_json::from_value(fixture["collection"].clone()).unwrap();

        assert_eq!(collection.id.as_str(), "reanalysis-era5-single-levels");
        assert!(collection.begin_datetime().is_some());
        assert_eq!(
            collection.extent.unwrap().spatial.unwrap().bbox[0].dimensions(),
            2
        );
    }

    #[test]
    fn rejects_invalid_extent_shapes_and_timestamps() {
        assert!(serde_json::from_str::<BoundingBox>("[0, 1, 2, 3, 4]").is_err());
        assert!(serde_json::from_str::<TemporalInterval>(r#"["not-a-date", null]"#).is_err());
        assert!(serde_json::from_str::<TemporalInterval>(r"[null]").is_err());
    }
}
