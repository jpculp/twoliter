use serde::Deserialize;
use snafu::ResultExt;
use std::fs;
use std::path::{Path, PathBuf};

/// A manifest file listing multiple images to publish.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    #[serde(rename = "image")]
    pub(crate) images: Vec<ImageEntry>,
}

/// A single image entry in the manifest.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImageEntry {
    pub(crate) source_uri: Option<String>,
    pub(crate) source_archive: Option<PathBuf>,
    pub(crate) repository_name: String,
    pub(crate) tag: Option<String>,
}

/// A validated, ready-to-use image specification.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedImage {
    pub(crate) source: ResolvedSource,
    pub(crate) repository_name: String,
    pub(crate) tag: String,
}

#[derive(Debug, Clone)]
pub(crate) enum ResolvedSource {
    Registry(String),
    OciArchive(PathBuf),
}

impl Manifest {
    pub(crate) fn from_path<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path).context(error::ReadFileSnafu { path })?;
        toml::from_str(&content).context(error::ParseTomlSnafu { path })
    }
}

pub(crate) fn resolve_images(
    manifest_path: Option<&PathBuf>,
    source_uri: Option<&str>,
    source_archive: Option<&PathBuf>,
    repository_name: Option<&str>,
    tag: Option<&str>,
) -> Result<Vec<ResolvedImage>> {
    if let Some(path) = manifest_path {
        let m = Manifest::from_path(path)?;
        m.images
            .iter()
            .enumerate()
            .map(|(i, entry)| resolve_entry(entry, i))
            .collect()
    } else {
        let entry = ImageEntry {
            source_uri: source_uri.map(|s| s.to_owned()),
            source_archive: source_archive.map(|p| p.to_owned()),
            repository_name: repository_name.unwrap_or_default().to_owned(),
            tag: tag.map(|t| t.to_owned()),
        };
        Ok(vec![resolve_entry(&entry, 0)?])
    }
}

fn resolve_entry(entry: &ImageEntry, index: usize) -> Result<ResolvedImage> {
    let source = match (&entry.source_uri, &entry.source_archive) {
        (Some(uri), None) => ResolvedSource::Registry(uri.clone()),
        (None, Some(path)) => ResolvedSource::OciArchive(path.clone()),
        (Some(_), Some(_)) => {
            return error::AmbiguousSourceSnafu { index }.fail();
        }
        (None, None) => {
            return error::MissingSourceSnafu { index }.fail();
        }
    };

    let tag = match &entry.tag {
        Some(t) => t.clone(),
        None => infer_tag(&source, index)?,
    };

    Ok(ResolvedImage {
        source,
        repository_name: entry.repository_name.clone(),
        tag,
    })
}

fn infer_tag(source: &ResolvedSource, index: usize) -> Result<String> {
    match source {
        ResolvedSource::Registry(uri) => {
            if uri.contains('@') {
                return error::CannotInferTagFromDigestSnafu {
                    uri: uri.clone(),
                    index,
                }
                .fail();
            }
            let path_part = uri.rsplit('/').next().unwrap_or(uri);
            match path_part.rsplit_once(':') {
                Some((_, tag)) if !tag.is_empty() => Ok(tag.to_owned()),
                _ => error::CannotInferTagSnafu {
                    uri: uri.clone(),
                    index,
                }
                .fail(),
            }
        }
        ResolvedSource::OciArchive(path) => error::TagRequiredForOciArchiveSnafu {
            path: path.clone(),
            index,
        }
        .fail(),
    }
}

mod error {
    use snafu::Snafu;
    use std::path::PathBuf;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(super)))]
    pub(crate) enum Error {
        #[snafu(display("Failed to read '{}': {}", path.display(), source))]
        ReadFile {
            path: PathBuf,
            source: std::io::Error,
        },

        #[snafu(display("Invalid TOML in '{}': {}", path.display(), source))]
        ParseToml {
            path: PathBuf,
            source: toml::de::Error,
        },

        #[snafu(display("Image entry {}: both source_uri and source_archive specified", index))]
        AmbiguousSource { index: usize },

        #[snafu(display(
            "Image entry {}: neither source_uri nor source_archive specified",
            index
        ))]
        MissingSource { index: usize },

        #[snafu(display(
            "Image entry {}: cannot infer tag from digest reference '{}'; specify tag explicitly",
            index,
            uri
        ))]
        CannotInferTagFromDigest { uri: String, index: usize },

        #[snafu(display(
            "Image entry {}: cannot infer tag from URI '{}'; specify tag explicitly",
            index,
            uri
        ))]
        CannotInferTag { uri: String, index: usize },

        #[snafu(display(
            "Image entry {}: tag is required when source is an archive '{}'; specify tag explicitly",
            index, path.display()
        ))]
        TagRequiredForOciArchive { path: PathBuf, index: usize },
    }
}

pub(crate) use error::Error;
pub(crate) type Result<T> = std::result::Result<T, error::Error>;

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_resolve_images_from_uri() {
        let images = resolve_images(
            None,
            Some("111222333444.dkr.ecr.us-west-2.amazonaws.com/my-repo:v1.0.0"),
            None,
            Some("my-repo"),
            None,
        )
        .unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].repository_name, "my-repo");
        assert_eq!(images[0].tag, "v1.0.0");
        assert!(matches!(images[0].source, ResolvedSource::Registry(_)));
    }

    #[test]
    fn test_resolve_images_from_archive() {
        let images = resolve_images(
            None,
            None,
            Some(&PathBuf::from("/tmp/my-image.tar")),
            Some("my-repo"),
            Some("v2.0.0"),
        )
        .unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].repository_name, "my-repo");
        assert_eq!(images[0].tag, "v2.0.0");
        assert!(matches!(images[0].source, ResolvedSource::OciArchive(_)));
    }

    #[test]
    fn test_resolve_images_archive_requires_tag() {
        let result = resolve_images(
            None,
            None,
            Some(&PathBuf::from("/tmp/my-image.tar")),
            Some("my-repo"),
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_images_no_source_fails() {
        let result = resolve_images(None, None, None, Some("my-repo"), Some("v1"));
        assert!(result.is_err());
    }

    #[test]
    fn test_infer_tag_from_uri() {
        let source = ResolvedSource::Registry(
            "111222333444.dkr.ecr.us-west-2.amazonaws.com/my-repo:v1.2.3".to_owned(),
        );
        let tag = infer_tag(&source, 0).unwrap();
        assert_eq!(tag, "v1.2.3");
    }

    #[test]
    fn test_infer_tag_from_digest_fails() {
        let source = ResolvedSource::Registry(
            "111222333444.dkr.ecr.us-west-2.amazonaws.com/my-repo@sha256:abc123".to_owned(),
        );
        assert!(infer_tag(&source, 0).is_err());
    }

    #[test]
    fn test_manifest_from_toml() {
        let content = r#"
[[image]]
source_uri = "public.ecr.aws/bottlerocket/bottlerocket-admin:v0.20.0"
repository_name = "bottlerocket-admin"

[[image]]
source_uri = "public.ecr.aws/bottlerocket/bottlerocket-control:v0.6.1"
repository_name = "bottlerocket-control"
tag = "latest"
"#;
        let manifest: Manifest = toml::from_str(content).unwrap();
        assert_eq!(manifest.images.len(), 2);
        assert_eq!(manifest.images[0].repository_name, "bottlerocket-admin");
        assert_eq!(manifest.images[0].tag, None);
        assert_eq!(manifest.images[1].repository_name, "bottlerocket-control");
        assert_eq!(manifest.images[1].tag, Some("latest".to_owned()));
    }
}
