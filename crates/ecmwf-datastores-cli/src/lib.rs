//! Configuration, planning, and import support for the command-line client.
#![warn(missing_docs)]

/// Multipart `GRIB` and `NetCDF` output assembly.
pub mod assembly;
/// Typed retrieval configuration and TOML parsing.
pub mod config;
/// Request expansion, size estimation, and partitioning.
pub mod plan;
/// Safe import of the Python literals emitted by the CDS web form.
pub mod python;
