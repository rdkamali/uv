use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use tracing::{instrument, warn};

use axi_client::{FlatDistributions, OwnedArchive, SimpleMetadata, SimpleMetadatum};
use axi_normalize::PackageName;
use axi_traits::NoBinary;
use axi_warnings::warn_user_once;
use distribution_filename::DistFilename;
use distribution_types::{Dist, IndexUrl, PrioritizedDistribution, ResolvableDist};
use pep440_rs::Version;
use platform_tags::Tags;
use pypi_types::{Hashes, Yanked};

use crate::python_requirement::PythonRequirement;

/// A map from versions to distributions.
#[derive(Debug, Default, Clone)]
pub struct VersionMap(BTreeMap<Version, PrioritizedDistribution>);

impl VersionMap {
    /// Initialize a [`VersionMap`] from the given metadata.
    #[instrument(skip_all, fields(package_name))]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_metadata(
        raw_metadata: OwnedArchive<SimpleMetadata>,
        package_name: &PackageName,
        index: &IndexUrl,
        tags: &Tags,
        python_requirement: &PythonRequirement,
        exclude_newer: Option<&DateTime<Utc>>,
        mut flat_index: Option<FlatDistributions>,
        no_binary: &NoBinary,
    ) -> Self {
        // NOTE: We should experiment with refactoring the code
        // below to work on rkyv::Archived<SimpleMetadata>. More
        // specifically, we may want to adjust VersionMap itself to
        // contain an Archived<SimpleMetadata> of some kind, that in
        // turn is used in the resolver. The idea here is to avoid
        // eagerly deserializing all of the metadata for a package
        // up-front.
        let metadata = OwnedArchive::deserialize(&raw_metadata);

        let mut version_map = BTreeMap::new();

        // Check if binaries are allowed for this package
        let no_binary = match no_binary {
            NoBinary::None => false,
            NoBinary::All => true,
            NoBinary::Packages(packages) => packages.contains(package_name),
        };

        // Collect compatible distributions.
        for SimpleMetadatum { version, files } in metadata {
            // If we have packages of the same name from find links, give them
            // priority, otherwise start with an empty priority dist.
            let mut priority_dist = flat_index
                .as_mut()
                .and_then(|flat_index| flat_index.remove(&version))
                .unwrap_or_default();
            for (filename, file) in files.all() {
                // Support resolving as if it were an earlier timestamp, at least as long files have
                // upload time information.
                if let Some(exclude_newer) = exclude_newer {
                    match file.upload_time_utc_ms.as_ref() {
                        Some(&upload_time) if upload_time >= exclude_newer.timestamp_millis() => {
                            continue;
                        }
                        None => {
                            warn_user_once!(
                                "{} is missing an upload date, but user provided: {exclude_newer}",
                                file.filename,
                            );
                            continue;
                        }
                        _ => {}
                    }
                }

                let yanked = if let Some(ref yanked) = file.yanked {
                    yanked.clone()
                } else {
                    Yanked::default()
                };

                // Prioritize amongst all available files.
                let requires_python = file.requires_python.clone();
                let hash = file.hashes.clone();
                match filename {
                    DistFilename::WheelFilename(filename) => {
                        // If pre-built binaries are disabled, skip this wheel
                        if no_binary {
                            continue;
                        }

                        // To be compatible, the wheel must both have compatible tags _and_ have a
                        // compatible Python requirement.
                        let priority = filename.compatibility(tags).filter(|_| {
                            file.requires_python
                                .as_ref()
                                .map_or(true, |requires_python| {
                                    requires_python.contains(python_requirement.target())
                                })
                        });
                        let dist = Dist::from_registry(
                            DistFilename::WheelFilename(filename),
                            file,
                            index.clone(),
                        );
                        priority_dist.insert_built(
                            dist,
                            requires_python,
                            yanked,
                            Some(hash),
                            priority,
                        );
                    }
                    DistFilename::SourceDistFilename(filename) => {
                        let dist = Dist::from_registry(
                            DistFilename::SourceDistFilename(filename),
                            file,
                            index.clone(),
                        );
                        priority_dist.insert_source(dist, requires_python, yanked, Some(hash));
                    }
                }
            }
            version_map.insert(version, priority_dist);
        }
        // Add any left over packages from the version map that we didn't visit
        // above via `SimpleMetadata`.
        if let Some(flat_index) = flat_index {
            version_map.extend(flat_index.into_iter());
        }
        Self(version_map)
    }

    /// Return the [`DistFile`] for the given version, if any.
    pub(crate) fn get(&self, version: &Version) -> Option<ResolvableDist> {
        self.0.get(version).and_then(PrioritizedDistribution::get)
    }

    /// Return an iterator over the versions and distributions.
    pub(crate) fn iter(&self) -> impl DoubleEndedIterator<Item = (&Version, ResolvableDist)> {
        self.0
            .iter()
            .filter_map(|(version, dist)| Some((version, dist.get()?)))
    }

    /// Return the [`Hashes`] for the given version, if any.
    pub(crate) fn hashes(&self, version: &Version) -> Vec<Hashes> {
        self.0
            .get(version)
            .map(|file| file.hashes().to_vec())
            .unwrap_or_default()
    }
}

impl From<FlatDistributions> for VersionMap {
    fn from(flat_index: FlatDistributions) -> Self {
        Self(flat_index.into())
    }
}
