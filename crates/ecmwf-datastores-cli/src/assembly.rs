use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read as _},
    path::{Path, PathBuf},
};

#[cfg(feature = "netcdf-assembly")]
use std::{collections::HashMap, ops::Range};

use anyhow::{Context, Result, bail, ensure};
use ecmwf_datastores_client::Selection;
#[cfg(feature = "netcdf-assembly")]
use netcdf::{
    AttributeValue, Extent, NcTypeDescriptor, Variable, VariableMut,
    types::{FloatType, IntType, NcVariableType},
};
use tempfile::Builder;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Grib,

    Netcdf,
}

#[cfg(feature = "netcdf-assembly")]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum CoordinateValue {
    U8(u8),

    I8(i8),

    U16(u16),

    U32(u32),

    U64(u64),

    I16(i16),

    I32(i32),

    I64(i64),

    F32(u32),

    F64(u64),

    Char(i8),

    String(String),
}

#[cfg(feature = "netcdf-assembly")]
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct NcChar(i8);

// SAFETY: `NcChar` has the exact one-byte representation required by NC_CHAR.
#[cfg(feature = "netcdf-assembly")]
unsafe impl NcTypeDescriptor for NcChar {
    fn type_descriptor() -> NcVariableType {
        NcVariableType::Char
    }
}

#[cfg(feature = "netcdf-assembly")]
struct ChunkGrid {
    shape: Vec<usize>,

    starts: Vec<usize>,

    finished: bool,

    chunk_shape: Vec<usize>,
}

#[cfg(feature = "netcdf-assembly")]
impl ChunkGrid {
    const MAX_BUFFER_BYTES: usize = 8 * 1024 * 1024;

    fn for_type<T>(shape: Vec<usize>) -> Self {
        let mut capacity = Self::MAX_BUFFER_BYTES / std::mem::size_of::<T>();
        let mut chunk_shape = vec![1; shape.len()];
        for (chunk, length) in chunk_shape.iter_mut().zip(&shape).rev() {
            *chunk = (*length).clamp(1, capacity.max(1));
            capacity /= *chunk;
        }
        let finished = shape.contains(&0);
        let starts = vec![0; shape.len()];
        Self {
            shape,
            starts,
            finished,
            chunk_shape,
        }
    }

    fn next_extents(
        &mut self,
        destination_ranges: &[Range<usize>],
    ) -> Option<(Vec<Extent>, Vec<Extent>)> {
        if self.finished {
            return None;
        }
        let counts = self
            .starts
            .iter()
            .zip(&self.shape)
            .zip(&self.chunk_shape)
            .map(|((&start, &length), &chunk)| chunk.min(length - start))
            .collect::<Vec<_>>();
        let source = self
            .starts
            .iter()
            .zip(&counts)
            .map(|(&start, &count)| Extent::from(start..start + count))
            .collect();
        let destination = self
            .starts
            .iter()
            .zip(&counts)
            .zip(destination_ranges)
            .map(|((&start, &count), destination)| {
                let start = destination.start + start;
                Extent::from(start..start + count)
            })
            .collect();

        let Some(dimension) = (0..self.starts.len())
            .rev()
            .find(|&dimension| self.starts[dimension] + counts[dimension] < self.shape[dimension])
        else {
            self.finished = true;
            return Some((source, destination));
        };
        self.starts[dimension] += counts[dimension];
        self.starts[dimension + 1..].fill(0);
        Some((source, destination))
    }
}

#[cfg(feature = "netcdf-assembly")]
#[derive(Debug)]
struct VariablePlan {
    name: String,

    dimensions: Vec<String>,

    attributes: Vec<(String, AttributeValue)>,

    variable_type: NcVariableType,
}

#[cfg(feature = "netcdf-assembly")]
#[derive(Debug)]
struct DimensionPlan {
    len: usize,

    name: String,

    ranges: Vec<Option<Range<usize>>>,
}

#[cfg(feature = "netcdf-assembly")]
#[derive(Debug)]
struct DimensionMetadata {
    len: usize,

    name: String,

    coordinate: Option<CoordinateMetadata>,
}

#[cfg(feature = "netcdf-assembly")]
#[derive(Debug)]
struct NetcdfPartMetadata {
    variables: Vec<VariablePlan>,

    attributes: Vec<(String, AttributeValue)>,

    dimensions: Vec<DimensionMetadata>,
}

#[cfg(feature = "netcdf-assembly")]
#[derive(Debug)]
struct CoordinateMetadata {
    values: Vec<CoordinateValue>,

    attributes: Vec<(String, AttributeValue)>,

    variable_type: NcVariableType,
}

#[cfg(feature = "netcdf-assembly")]
#[derive(Default)]
struct CombinedCoordinates {
    values: Vec<CoordinateValue>,

    positions: HashMap<CoordinateValue, usize>,
}

#[cfg(feature = "netcdf-assembly")]
impl CombinedCoordinates {
    fn insert(&mut self, name: &str, values: &[CoordinateValue]) -> Result<Range<usize>> {
        let Some(first) = values.first() else {
            return Ok(0..0);
        };
        if let Some(&start) = self.positions.get(first) {
            let end = start + values.len();
            ensure!(
                self.values.get(start..end) == Some(values),
                "coordinate `{name}` overlaps existing values non-contiguously"
            );
            return Ok(start..end);
        }

        ensure!(
            values
                .iter()
                .all(|value| !self.positions.contains_key(value)),
            "coordinate `{name}` overlaps existing values non-contiguously"
        );
        let start = self.values.len();
        for (offset, value) in values.iter().cloned().enumerate() {
            ensure!(
                self.positions.insert(value, start + offset).is_none(),
                "coordinate `{name}` contains duplicate values"
            );
        }
        self.values.extend_from_slice(values);
        Ok(start..self.values.len())
    }
}

#[cfg(feature = "netcdf-assembly")]
fn copy_typed<T: NcTypeDescriptor + Copy>(
    source: &Variable<'_>,
    destination: &mut VariableMut<'_>,
    destination_ranges: &[Range<usize>],
) -> Result<()> {
    let shape = source
        .dimensions()
        .iter()
        .map(netcdf::Dimension::len)
        .collect::<Vec<_>>();
    ensure!(
        shape.len() == destination_ranges.len(),
        "NetCDF variable `{}` has an invalid destination rank",
        source.name()
    );
    let mut chunks = ChunkGrid::for_type::<T>(shape);
    while let Some((source_extents, destination_extents)) = chunks.next_extents(destination_ranges)
    {
        let values = source.get_values::<T, _>(source_extents)?;
        destination.put_values(&values, destination_extents)?;
    }
    Ok(())
}

#[cfg(feature = "netcdf-assembly")]
fn copy_strings(
    source: &Variable<'_>,
    destination: &mut VariableMut<'_>,
    destination_ranges: &[Range<usize>],
) -> Result<()> {
    let shape = source
        .dimensions()
        .iter()
        .map(netcdf::Dimension::len)
        .collect::<Vec<_>>();
    ensure!(
        shape.len() == destination_ranges.len(),
        "NetCDF variable `{}` has an invalid destination rank",
        source.name()
    );
    if shape.contains(&0) {
        return Ok(());
    }
    let mut indices = vec![0; shape.len()];
    loop {
        let value = source.get_string(indices.as_slice())?;
        let destination_indices = indices
            .iter()
            .zip(destination_ranges)
            .map(|(&index, range)| range.start + index)
            .collect::<Vec<_>>();
        destination.put_string(&value, destination_indices)?;

        if !advance_indices(&mut indices, &shape) {
            break;
        }
    }
    Ok(())
}

#[cfg(feature = "netcdf-assembly")]
fn copy_variable(
    source: &Variable<'_>,
    destination: &mut VariableMut<'_>,
    destination_ranges: &[Range<usize>],
) -> Result<()> {
    macro_rules! copy_values {
        ($type:ty) => {{
            copy_typed::<$type>(source, destination, destination_ranges)?;
        }};
    }
    match source.vartype() {
        NcVariableType::Int(IntType::U8) => copy_values!(u8),
        NcVariableType::Int(IntType::U16) => copy_values!(u16),
        NcVariableType::Int(IntType::U32) => copy_values!(u32),
        NcVariableType::Int(IntType::U64) => copy_values!(u64),
        NcVariableType::Int(IntType::I8) => copy_values!(i8),
        NcVariableType::Int(IntType::I16) => copy_values!(i16),
        NcVariableType::Int(IntType::I32) => copy_values!(i32),
        NcVariableType::Int(IntType::I64) => copy_values!(i64),
        NcVariableType::Float(FloatType::F32) => copy_values!(f32),
        NcVariableType::Float(FloatType::F64) => copy_values!(f64),
        NcVariableType::Char => copy_values!(NcChar),
        NcVariableType::String => copy_strings(source, destination, destination_ranges)?,
        other => bail!(
            "NetCDF variable `{}` uses an unsupported type {other:?}",
            source.name()
        ),
    }
    Ok(())
}

#[cfg(feature = "netcdf-assembly")]
fn merge_netcdf(parts: &[PathBuf], output: &Path) -> Result<()> {
    let files = parts
        .iter()
        .map(|path| {
            netcdf::open(path)
                .with_context(|| format!("failed to open NetCDF part {}", path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    let part_plans = files
        .iter()
        .zip(parts)
        .map(|(file, path)| read_netcdf_metadata(file, path))
        .collect::<Result<Vec<_>>>()?;

    let dimensions = plan_dimensions(&part_plans)?;
    let variables = plan_variables(&part_plans)?;
    let mut output = netcdf::create(output).context("failed to create assembled NetCDF file")?;

    if let Some(first) = part_plans.first() {
        for (name, value) in &first.attributes {
            output.add_attribute(name, value.clone())?;
        }
    }
    for dimension in &dimensions {
        output.add_dimension(&dimension.name, dimension.len)?;
    }
    for variable in &variables {
        let names = variable
            .dimensions
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let mut destination =
            output.add_variable_with_type(&variable.name, &names, &variable.variable_type)?;
        for (name, value) in &variable.attributes {
            destination.put_attribute(name, value.clone())?;
        }
    }

    let mut written_ranges: HashMap<String, Vec<Vec<Range<usize>>>> = HashMap::new();
    for (file_index, file) in files.iter().enumerate() {
        for source in file.variables() {
            let name = source.name();
            let ranges = source
                .dimensions()
                .iter()
                .map(|dimension| {
                    let name = dimension.name();
                    let range = dimensions
                        .iter()
                        .find(|candidate| candidate.name == name)
                        .and_then(|candidate| candidate.ranges[file_index].clone())
                        .with_context(|| format!("dimension `{name}` has no assembly range"))?;
                    Ok(range)
                })
                .collect::<Result<Vec<_>>>()?;

            let previous = written_ranges.entry(name.clone()).or_default();
            if let Some(existing) = previous
                .iter()
                .find(|existing| ranges_overlap(existing, &ranges))
            {
                ensure!(
                    existing.as_slice() == ranges.as_slice(),
                    "NetCDF variable `{name}` has partially overlapping output regions"
                );
                let destination = output
                    .variable(&name)
                    .with_context(|| format!("assembled NetCDF variable `{name}` is missing"))?;
                ensure!(
                    variables_equal(&source, &destination, &ranges)?,
                    "NetCDF variable `{name}` has conflicting values in an overlapping region"
                );
                continue;
            }
            let mut destination = output
                .variable_mut(&name)
                .with_context(|| format!("assembled NetCDF variable `{name}` is missing"))?;
            copy_variable(&source, &mut destination, &ranges)?;
            previous.push(ranges);
        }
    }
    output.close()?;
    Ok(())
}

#[cfg(not(feature = "netcdf-assembly"))]
fn merge_netcdf(_parts: &[PathBuf], _output: &Path) -> Result<()> {
    bail!(
        "NetCDF assembly is disabled; rebuild ecmwf-datastores-cli with the `netcdf-assembly` feature"
    )
}

fn detect_format(parts: &[PathBuf]) -> Result<Format> {
    let mut detected = None;
    for part in parts {
        let mut header = [0_u8; 8];
        File::open(part)
            .with_context(|| format!("failed to open downloaded part {}", part.display()))?
            .read_exact(&mut header)
            .with_context(|| format!("downloaded part {} is too short", part.display()))?;
        let format = if header.starts_with(b"GRIB") {
            Format::Grib
        } else if header.starts_with(b"CDF\x01")
            || header.starts_with(b"CDF\x02")
            || header.starts_with(b"CDF\x05")
            || header == *b"\x89HDF\r\n\x1a\n"
        {
            Format::Netcdf
        } else {
            bail!(
                "downloaded part {} is neither an unarchived GRIB nor a NetCDF file",
                part.display()
            );
        };
        if let Some(previous) = detected {
            ensure!(
                previous == format,
                "downloaded parts contain a mixture of GRIB and NetCDF files"
            );
        }
        detected = Some(format);
    }
    detected.context("cannot assemble an empty download plan")
}

#[cfg(feature = "netcdf-assembly")]
fn ranges_overlap(left: &[Range<usize>], right: &[Range<usize>]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.start < right.end && right.start < left.end)
}

#[cfg(feature = "netcdf-assembly")]
fn plan_dimension(parts: &[NetcdfPartMetadata], name: String) -> Result<DimensionPlan> {
    let occurrences = parts
        .iter()
        .enumerate()
        .filter_map(|(index, part)| {
            part.dimensions
                .iter()
                .find(|dimension| dimension.name == name)
                .map(|dimension| (index, dimension))
        })
        .collect::<Vec<_>>();
    let coordinates = occurrences
        .iter()
        .map(|(_, dimension)| dimension.coordinate.as_ref())
        .collect::<Vec<_>>();
    let coordinate_based = coordinates.iter().all(Option::is_some);
    ensure!(
        coordinate_based || coordinates.iter().all(Option::is_none),
        "dimension `{name}` has a coordinate variable in only some NetCDF parts"
    );

    let mut ranges = vec![None; parts.len()];
    if !coordinate_based {
        let len = occurrences[0].1.len;
        ensure!(
            occurrences
                .iter()
                .all(|(_, candidate)| candidate.len == len),
            "dimension `{name}` changes size but has no coordinate variable"
        );
        for (index, _) in occurrences {
            ranges[index] = Some(0..len);
        }
        return Ok(DimensionPlan { len, name, ranges });
    }

    let first_coordinate = coordinates[0].expect("coordinate variable exists");
    let expected_type = &first_coordinate.variable_type;
    let expected_attributes = &first_coordinate.attributes;
    let mut combined = CombinedCoordinates::default();
    for ((index, dimension), coordinate) in occurrences.into_iter().zip(coordinates) {
        let coordinate = coordinate.expect("coordinate variable exists");
        ensure!(
            &coordinate.variable_type == expected_type,
            "coordinate variable `{name}` has an incompatible schema"
        );
        ensure!(
            attributes_equal(&coordinate.attributes, expected_attributes),
            "coordinate variable `{name}` has incompatible metadata across parts"
        );
        ensure!(
            coordinate.values.len() == dimension.len,
            "coordinate variable `{name}` has an invalid length"
        );
        ranges[index] = Some(combined.insert(&name, &coordinate.values)?);
    }
    Ok(DimensionPlan {
        name,
        len: combined.values.len(),
        ranges,
    })
}

#[cfg(feature = "netcdf-assembly")]
fn plan_variables(parts: &[NetcdfPartMetadata]) -> Result<Vec<&VariablePlan>> {
    let mut plans: Vec<&VariablePlan> = Vec::new();
    for part in parts {
        for variable in &part.variables {
            if let Some(existing) = plans.iter().find(|plan| plan.name == variable.name) {
                ensure!(
                    existing.dimensions == variable.dimensions
                        && existing.variable_type == variable.variable_type
                        && attributes_equal(&existing.attributes, &variable.attributes),
                    "NetCDF variable `{}` has an incompatible schema across parts",
                    variable.name
                );
                continue;
            }
            plans.push(variable);
        }
    }
    Ok(plans)
}

#[cfg(feature = "netcdf-assembly")]
fn plan_dimensions(parts: &[NetcdfPartMetadata]) -> Result<Vec<DimensionPlan>> {
    let mut names = Vec::new();
    for part in parts {
        for dimension in &part.dimensions {
            if !names.contains(&dimension.name) {
                names.push(dimension.name.clone());
            }
        }
    }

    names
        .into_iter()
        .map(|name| plan_dimension(parts, name))
        .collect()
}

#[cfg(feature = "netcdf-assembly")]
fn variables_equal(
    source: &Variable<'_>,
    destination: &Variable<'_>,
    destination_ranges: &[Range<usize>],
) -> Result<bool> {
    macro_rules! compare_values {
        ($type:ty) => {
            variable_values_equal::<$type>(source, destination, destination_ranges, PartialEq::eq)
        };
    }
    match source.vartype() {
        NcVariableType::Int(IntType::U8) => compare_values!(u8),
        NcVariableType::Int(IntType::U16) => compare_values!(u16),
        NcVariableType::Int(IntType::U32) => compare_values!(u32),
        NcVariableType::Int(IntType::U64) => compare_values!(u64),
        NcVariableType::Int(IntType::I8) => compare_values!(i8),
        NcVariableType::Int(IntType::I16) => compare_values!(i16),
        NcVariableType::Int(IntType::I32) => compare_values!(i32),
        NcVariableType::Int(IntType::I64) => compare_values!(i64),
        NcVariableType::Float(FloatType::F32) => {
            variable_values_equal::<f32>(source, destination, destination_ranges, |left, right| {
                left.to_bits() == right.to_bits()
            })
        }
        NcVariableType::Float(FloatType::F64) => {
            variable_values_equal::<f64>(source, destination, destination_ranges, |left, right| {
                left.to_bits() == right.to_bits()
            })
        }
        NcVariableType::Char => compare_values!(NcChar),
        NcVariableType::String => string_values_equal(source, destination, destination_ranges),
        other => bail!(
            "NetCDF variable `{}` uses an unsupported type {other:?}",
            source.name()
        ),
    }
}

#[cfg(feature = "netcdf-assembly")]
fn advance_indices(indices: &mut [usize], shape: &[usize]) -> bool {
    let Some(dimension) = (0..indices.len())
        .rev()
        .find(|&dimension| indices[dimension] + 1 < shape[dimension])
    else {
        return false;
    };
    indices[dimension] += 1;
    indices[dimension + 1..].fill(0);
    true
}

fn concatenate_grib(parts: &[PathBuf], output: &Path) -> Result<()> {
    let mut output = OpenOptions::new().write(true).truncate(true).open(output)?;
    for part in parts {
        let mut input = File::open(part)?;
        io::copy(&mut input, &mut output)
            .with_context(|| format!("failed to append GRIB part {}", part.display()))?;
    }
    Ok(())
}

#[cfg(feature = "netcdf-assembly")]
fn attributes_equal(left: &[(String, AttributeValue)], right: &[(String, AttributeValue)]) -> bool {
    left.len() == right.len()
        && left.iter().all(|(name, value)| {
            right.iter().any(|(candidate_name, candidate_value)| {
                name == candidate_name && attribute_values_equal(value, candidate_value)
            })
        })
}

fn assemble_blocking(parts: &[PathBuf], target: &Path, overwrite: bool) -> Result<PathBuf> {
    let format = detect_format(parts)?;
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create assembly directory {}", parent.display()))?;
    let temporary = Builder::new()
        .prefix(".ecmwf-assembly-")
        .tempfile_in(parent)
        .with_context(|| format!("failed to create assembly file in {}", parent.display()))?;
    let temporary = temporary.into_temp_path();

    match format {
        Format::Grib => concatenate_grib(parts, &temporary)?,
        Format::Netcdf => merge_netcdf(parts, &temporary)?,
    }
    OpenOptions::new()
        .write(true)
        .open(&temporary)
        .context("failed to reopen temporary assembled output")?
        .sync_all()
        .context("failed to sync temporary assembled output")?;

    if overwrite {
        temporary.persist(target)
    } else {
        temporary.persist_noclobber(target)
    }
    .map_err(|error| error.error)
    .with_context(|| format!("failed to save assembled output to {}", target.display()))?;

    for part in parts {
        if let Err(error) = fs::remove_file(part) {
            eprintln!(
                "warning: failed to remove assembled part {}: {error}",
                part.display()
            );
        }
    }
    Ok(target.to_path_buf())
}

#[cfg(feature = "netcdf-assembly")]
fn coordinate_values(variable: &Variable<'_>) -> Result<Vec<CoordinateValue>> {
    let values = match variable.vartype() {
        NcVariableType::Int(IntType::U8) => variable
            .get_values::<u8, _>(..)?
            .into_iter()
            .map(CoordinateValue::U8)
            .collect(),
        NcVariableType::Int(IntType::U16) => variable
            .get_values::<u16, _>(..)?
            .into_iter()
            .map(CoordinateValue::U16)
            .collect(),
        NcVariableType::Int(IntType::U32) => variable
            .get_values::<u32, _>(..)?
            .into_iter()
            .map(CoordinateValue::U32)
            .collect(),
        NcVariableType::Int(IntType::U64) => variable
            .get_values::<u64, _>(..)?
            .into_iter()
            .map(CoordinateValue::U64)
            .collect(),
        NcVariableType::Int(IntType::I8) => variable
            .get_values::<i8, _>(..)?
            .into_iter()
            .map(CoordinateValue::I8)
            .collect(),
        NcVariableType::Int(IntType::I16) => variable
            .get_values::<i16, _>(..)?
            .into_iter()
            .map(CoordinateValue::I16)
            .collect(),
        NcVariableType::Int(IntType::I32) => variable
            .get_values::<i32, _>(..)?
            .into_iter()
            .map(CoordinateValue::I32)
            .collect(),
        NcVariableType::Int(IntType::I64) => variable
            .get_values::<i64, _>(..)?
            .into_iter()
            .map(CoordinateValue::I64)
            .collect(),
        NcVariableType::Float(FloatType::F32) => variable
            .get_values::<f32, _>(..)?
            .into_iter()
            .map(|value| CoordinateValue::F32(value.to_bits()))
            .collect(),
        NcVariableType::Float(FloatType::F64) => variable
            .get_values::<f64, _>(..)?
            .into_iter()
            .map(|value| CoordinateValue::F64(value.to_bits()))
            .collect(),
        NcVariableType::Char => variable
            .get_values::<NcChar, _>(..)?
            .into_iter()
            .map(|value| CoordinateValue::Char(value.0))
            .collect(),
        NcVariableType::String => (0..variable.len())
            .map(|index| variable.get_string(index).map(CoordinateValue::String))
            .collect::<netcdf::Result<Vec<_>>>()?,
        other => bail!(
            "coordinate variable `{}` uses an unsupported type {other:?}",
            variable.name()
        ),
    };
    Ok(values)
}

#[cfg(feature = "netcdf-assembly")]
fn string_values_equal(
    source: &Variable<'_>,
    destination: &Variable<'_>,
    destination_ranges: &[Range<usize>],
) -> Result<bool> {
    let shape = source
        .dimensions()
        .iter()
        .map(netcdf::Dimension::len)
        .collect::<Vec<_>>();
    ensure!(
        shape.len() == destination_ranges.len(),
        "invalid comparison rank"
    );
    if shape.contains(&0) {
        return Ok(true);
    }
    let mut indices = vec![0; shape.len()];
    loop {
        let destination_indices = indices
            .iter()
            .zip(destination_ranges)
            .map(|(&index, range)| range.start + index)
            .collect::<Vec<_>>();
        if source.get_string(indices.as_slice())?
            != destination.get_string(destination_indices.as_slice())?
        {
            return Ok(false);
        }
        if !advance_indices(&mut indices, &shape) {
            break;
        }
    }
    Ok(true)
}

#[cfg(feature = "netcdf-assembly")]
fn read_netcdf_metadata(file: &netcdf::File, path: &Path) -> Result<NetcdfPartMetadata> {
    ensure!(
        file.groups()?.next().is_none(),
        "NetCDF groups are not supported while assembling {}",
        path.display()
    );
    let attributes = file
        .attributes()
        .map(|attribute| Ok((attribute.name().to_owned(), attribute.value()?)))
        .collect::<Result<Vec<_>>>()?;
    let dimensions = file
        .dimensions()
        .map(|dimension| {
            let name = dimension.name();
            let coordinate = file
                .variable(&name)
                .map(|variable| -> Result<CoordinateMetadata> {
                    let coordinate_dimensions = variable
                        .dimensions()
                        .iter()
                        .map(netcdf::Dimension::name)
                        .collect::<Vec<_>>();
                    ensure!(
                        coordinate_dimensions.as_slice() == [name.as_str()],
                        "coordinate variable `{name}` has an incompatible schema"
                    );
                    Ok(CoordinateMetadata {
                        variable_type: variable.vartype(),
                        attributes: variable_attributes(&variable)?,
                        values: coordinate_values(&variable)?,
                    })
                })
                .transpose()?;
            Ok(DimensionMetadata {
                name,
                len: dimension.len(),
                coordinate,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let variables = file
        .variables()
        .map(|variable| {
            let variable_type = variable.vartype();
            ensure!(
                matches!(
                    variable_type,
                    NcVariableType::Int(_)
                        | NcVariableType::Float(_)
                        | NcVariableType::Char
                        | NcVariableType::String
                ),
                "NetCDF variable `{}` uses an unsupported type {variable_type:?}",
                variable.name()
            );
            Ok(VariablePlan {
                name: variable.name(),
                dimensions: variable
                    .dimensions()
                    .iter()
                    .map(netcdf::Dimension::name)
                    .collect(),
                variable_type,
                attributes: variable_attributes(&variable)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(NetcdfPartMetadata {
        variables,
        attributes,
        dimensions,
    })
}

#[cfg(feature = "netcdf-assembly")]
fn variable_attributes(variable: &Variable<'_>) -> Result<Vec<(String, AttributeValue)>> {
    variable
        .attributes()
        .map(|attribute| Ok((attribute.name().to_owned(), attribute.value()?)))
        .collect()
}

#[cfg(feature = "netcdf-assembly")]
fn variable_values_equal<T: NcTypeDescriptor + Copy>(
    source: &Variable<'_>,
    destination: &Variable<'_>,
    destination_ranges: &[Range<usize>],
    equal: impl Fn(&T, &T) -> bool,
) -> Result<bool> {
    let shape = source
        .dimensions()
        .iter()
        .map(netcdf::Dimension::len)
        .collect::<Vec<_>>();
    ensure!(
        shape.len() == destination_ranges.len(),
        "invalid comparison rank"
    );
    let mut chunks = ChunkGrid::for_type::<T>(shape);
    while let Some((source_extents, destination_extents)) = chunks.next_extents(destination_ranges)
    {
        let left = source.get_values::<T, _>(source_extents)?;
        let right = destination.get_values::<T, _>(destination_extents)?;
        if !left
            .iter()
            .zip(&right)
            .all(|(left, right)| equal(left, right))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(feature = "netcdf-assembly")]
fn attribute_values_equal(left: &AttributeValue, right: &AttributeValue) -> bool {
    match (left, right) {
        (AttributeValue::Float(left), AttributeValue::Float(right)) => {
            left.to_bits() == right.to_bits()
        }
        (AttributeValue::Double(left), AttributeValue::Double(right)) => {
            left.to_bits() == right.to_bits()
        }
        (AttributeValue::Floats(left), AttributeValue::Floats(right)) => left
            .iter()
            .map(|value| value.to_bits())
            .eq(right.iter().map(|value| value.to_bits())),
        (AttributeValue::Doubles(left), AttributeValue::Doubles(right)) => left
            .iter()
            .map(|value| value.to_bits())
            .eq(right.iter().map(|value| value.to_bits())),
        _ => left == right,
    }
}

/// Ensures that a multipart request asks the provider for unarchived parts.
///
/// # Errors
///
/// Returns an error when `download_format` is present and is not `unarchived`.
pub fn validate_multipart_request(request: &Selection) -> Result<()> {
    let Some(value) = request.as_map().get("download_format") else {
        return Ok(());
    };
    ensure!(
        value.as_str() == Some("unarchived"),
        "multipart output requires `download_format = \"unarchived\"`"
    );
    Ok(())
}

/// Assembles ordered `GRIB` or `NetCDF` parts into one output file.
///
/// The work runs on Tokio's blocking pool. Successfully assembled input parts
/// are removed after the output has been committed.
///
/// # Errors
///
/// Returns an error when the inputs are incompatible, an input or output cannot
/// be read or written, or an existing target may not be replaced.
pub async fn assemble(parts: Vec<PathBuf>, target: PathBuf, overwrite: bool) -> Result<PathBuf> {
    if parts.len() == 1 {
        return parts.into_iter().next().context("one part is present");
    }
    tokio::task::spawn_blocking(move || assemble_blocking(&parts, &target, overwrite))
        .await
        .context("output assembly task failed")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "netcdf-assembly")]
    fn empty_netcdf_dimensions_produce_no_chunks() {
        for shape in [vec![0], vec![0, 2], vec![2, 0], vec![2, 0, 3]] {
            let ranges = shape.iter().map(|&length| 0..length).collect::<Vec<_>>();
            let mut chunks = ChunkGrid::for_type::<f32>(shape);
            assert!(chunks.next_extents(&ranges).is_none());
        }
    }

    #[test]
    #[cfg(feature = "netcdf-assembly")]
    fn scalar_netcdf_variable_produces_one_chunk() {
        let mut chunks = ChunkGrid::for_type::<f32>(vec![]);
        let (source, destination) = chunks.next_extents(&[]).unwrap();
        assert!(source.is_empty());
        assert!(destination.is_empty());
        assert!(chunks.next_extents(&[]).is_none());
    }

    #[test]
    fn concatenates_grib_parts_and_removes_them() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join(".result.part-1.grib");
        let second = directory.path().join(".result.part-2.grib");
        let target = directory.path().join("result.grib");
        fs::write(&first, b"GRIBaaaa").unwrap();
        fs::write(&second, b"GRIBbbbb").unwrap();

        assemble_blocking(&[first.clone(), second.clone()], &target, false).unwrap();

        assert_eq!(fs::read(target).unwrap(), b"GRIBaaaaGRIBbbbb");
        assert!(!first.exists());
        assert!(!second.exists());
    }

    #[test]
    fn creates_assembly_output_directory() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.grib");
        let second = directory.path().join("second.grib");
        let target = directory.path().join("nested/output.grib");
        fs::write(&first, b"GRIBaaaa").unwrap();
        fs::write(&second, b"GRIBbbbb").unwrap();

        assemble_blocking(&[first, second], &target, false).unwrap();

        assert_eq!(fs::read(target).unwrap(), b"GRIBaaaaGRIBbbbb");
    }

    #[test]
    fn existing_output_preserves_grib_parts() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join(".result.part-1.grib");
        let second = directory.path().join(".result.part-2.grib");
        let target = directory.path().join("result.grib");
        fs::write(&first, b"GRIBaaaa").unwrap();
        fs::write(&second, b"GRIBbbbb").unwrap();
        fs::write(&target, b"existing").unwrap();

        assert!(assemble_blocking(&[first.clone(), second.clone()], &target, false).is_err());

        assert_eq!(fs::read(target).unwrap(), b"existing");
        assert!(first.exists());
        assert!(second.exists());
    }

    #[test]
    #[cfg(feature = "netcdf-assembly")]
    fn merges_netcdf_parts_by_coordinate() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join(".result.part-1.nc");
        let second = directory.path().join(".result.part-2.nc");
        let target = directory.path().join("result.nc");
        write_netcdf_part(&first, &[0, 1], "temperature", &[10.0, 11.0, 12.0, 13.0]);
        write_netcdf_part(&second, &[2, 3], "temperature", &[20.0, 21.0, 22.0, 23.0]);

        assemble_blocking(&[first, second], &target, false).unwrap();

        let output = netcdf::open(target).unwrap();
        assert_eq!(output.dimension_len("time"), Some(4));
        assert_eq!(
            output
                .variable("time")
                .unwrap()
                .get_values::<i32, _>(..)
                .unwrap(),
            [0, 1, 2, 3]
        );
        assert_eq!(
            output
                .variable("temperature")
                .unwrap()
                .get_values::<f32, _>(..)
                .unwrap(),
            [10.0, 11.0, 12.0, 13.0, 20.0, 21.0, 22.0, 23.0]
        );
        assert_eq!(
            output
                .variable("temperature")
                .unwrap()
                .attribute_value("_FillValue")
                .unwrap()
                .unwrap(),
            AttributeValue::Float(-9999.0)
        );
        assert_eq!(
            output
                .variable("origin")
                .unwrap()
                .get_values::<NcChar, _>(..)
                .unwrap()
                .into_iter()
                .map(|value| value.0.cast_unsigned())
                .collect::<Vec<_>>(),
            b"CDS"
        );
        assert_eq!(
            output
                .variable("experiment")
                .unwrap()
                .get_string(())
                .unwrap(),
            "reanalysis"
        );
    }

    #[test]
    #[cfg(feature = "netcdf-assembly")]
    fn merges_disjoint_netcdf_variables() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join(".result.part-1.nc");
        let second = directory.path().join(".result.part-2.nc");
        let target = directory.path().join("result.nc");
        write_netcdf_part(&first, &[0], "temperature", &[10.0, 11.0]);
        write_netcdf_part(&second, &[0], "precipitation", &[1.0, 2.0]);

        assemble_blocking(&[first, second], &target, false).unwrap();

        let output = netcdf::open(target).unwrap();
        assert_eq!(
            output
                .variable("temperature")
                .unwrap()
                .get_values::<f32, _>(..)
                .unwrap(),
            [10.0, 11.0]
        );
        assert_eq!(
            output
                .variable("precipitation")
                .unwrap()
                .get_values::<f32, _>(..)
                .unwrap(),
            [1.0, 2.0]
        );
    }

    #[test]
    #[cfg(feature = "netcdf-assembly")]
    fn conflicting_overlapping_netcdf_values_preserve_parts() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join(".result.part-1.nc");
        let second = directory.path().join(".result.part-2.nc");
        let target = directory.path().join("result.nc");
        write_overlapping_netcdf_part(&first, 0, &[10.0, 11.0]);
        write_overlapping_netcdf_part(&second, 1, &[20.0, 21.0]);

        let error =
            assemble_blocking(&[first.clone(), second.clone()], &target, false).unwrap_err();

        assert!(error.to_string().contains("conflicting values"));
        assert!(!target.exists());
        assert!(first.exists());
        assert!(second.exists());
    }

    #[cfg(feature = "netcdf-assembly")]
    fn write_overlapping_netcdf_part(path: &Path, valid_time: i64, values: &[f32]) {
        let mut file = netcdf::create(path).unwrap();
        file.add_dimension("latitude", 2).unwrap();
        file.add_variable::<f32>("latitude", &["latitude"])
            .unwrap()
            .put_values(&[50.0, 40.0], ..)
            .unwrap();
        file.add_variable::<i64>("valid_time", &[])
            .unwrap()
            .put_value(valid_time, ())
            .unwrap();
        file.add_variable::<f32>("temperature", &["latitude"])
            .unwrap()
            .put_values(values, ..)
            .unwrap();
        file.close().unwrap();
    }

    #[cfg(feature = "netcdf-assembly")]
    fn write_netcdf_part(path: &Path, times: &[i32], name: &str, values: &[f32]) {
        let mut file = netcdf::create(path).unwrap();
        file.add_dimension("time", times.len()).unwrap();
        file.add_dimension("latitude", 2).unwrap();
        file.add_dimension("name_strlen", 3).unwrap();
        file.add_variable::<i32>("time", &["time"])
            .unwrap()
            .put_values(times, ..)
            .unwrap();
        file.add_variable::<f32>("latitude", &["latitude"])
            .unwrap()
            .put_values(&[50.0, 40.0], ..)
            .unwrap();
        let mut temperature = file
            .add_variable::<f32>(name, &["time", "latitude"])
            .unwrap();
        temperature
            .put_attribute("_FillValue", -9999.0_f32)
            .unwrap();
        temperature.put_values(values, ..).unwrap();
        file.add_variable::<NcChar>("origin", &["name_strlen"])
            .unwrap()
            .put_values(
                &[
                    NcChar(b'C'.cast_signed()),
                    NcChar(b'D'.cast_signed()),
                    NcChar(b'S'.cast_signed()),
                ],
                ..,
            )
            .unwrap();
        file.add_variable_with_type("experiment", &[], &NcVariableType::String)
            .unwrap()
            .put_string("reanalysis", ())
            .unwrap();
        file.close().unwrap();
    }
}
