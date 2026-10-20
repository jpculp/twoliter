/*!
This module provides a very simple parser for RPM spec files.

It does not attempt to expand macros or perform any meaningful validation. Its
only purpose is to extract Source and Patch declarations so they can be passed
to Cargo as files to watch for changes, and so that the patches can be replayed
onto an upstream archive before its dependencies are vendored.

*/
pub(crate) mod error;
use error::Result;

use snafu::ResultExt;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// The strip level to assume when a spec does not say otherwise. Effectively
/// every Bottlerocket spec uses `%autosetup -p1`.
const DEFAULT_PATCH_STRIP_LEVEL: u32 = 1;

pub(crate) struct SpecInfo {
    pub(crate) sources: Vec<PathBuf>,
    /// Patches in the order `rpmbuild` applies them, i.e. ascending `PatchN:`
    /// index rather than the order they happen to appear in the file.
    pub(crate) patches: Vec<PathBuf>,
    /// The `-p` value `%autosetup` was invoked with, which is the strip level
    /// every patch in `patches` is applied at.
    pub(crate) patch_strip_level: u32,
}

impl SpecInfo {
    /// Returns a list of 'Source' and 'Patch' lines found in a spec file.
    pub(crate) fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let (sources, patches, patch_strip_level) = Self::parse(path)?;
        let sources = Self::filter(&sources);
        let patches = Self::filter(&patches);
        Ok(Self {
            sources,
            patches,
            patch_strip_level,
        })
    }

    /// "Parse" a spec file, extracting values of potential interest.
    fn parse<P: AsRef<Path>>(path: P) -> Result<(Vec<String>, Vec<String>, u32)> {
        let path = path.as_ref();
        let f = File::open(path).context(error::SpecFileReadSnafu { path })?;
        let f = BufReader::new(f);

        let mut sources = Vec::new();
        // Collected as (index, value) so the list can be sorted into the order
        // that `rpmbuild` applies the patches in.
        let mut patches: Vec<(u32, String)> = Vec::new();
        let mut patch_strip_level = DEFAULT_PATCH_STRIP_LEVEL;

        for line in f.lines() {
            let line = line.context(error::SpecFileReadSnafu { path })?;

            let mut tokens = line.split_whitespace().collect::<VecDeque<&str>>();
            if let Some(t) = tokens.pop_front() {
                if t.starts_with("Source") {
                    if let Some(s) = tokens.pop_front() {
                        sources.push(s.into());
                    }
                } else if t.starts_with("Patch") {
                    if let Some(p) = tokens.pop_front() {
                        patches.push((patch_index(t), p.into()));
                    }
                } else if t == "%autosetup" {
                    if let Some(level) = strip_level(tokens) {
                        patch_strip_level = level;
                    }
                }
            }
        }

        // A stable sort keeps file order for ties, which covers the bare
        // `Patch:` tag and any tag whose index we could not make sense of.
        patches.sort_by_key(|(index, _)| *index);

        Ok((
            sources,
            patches.into_iter().map(|(_, value)| value).collect(),
            patch_strip_level,
        ))
    }

    /// Emitting a non-existent file for `rerun-if-changed` will cause Cargo
    /// to always repeat the build. Therefore we exclude "files" that do not
    /// exist or that point outside the package directory. We also exclude
    /// anything that appears to be an unexpanded macro.
    fn filter(input: &[String]) -> Vec<PathBuf> {
        input
            .iter()
            .filter(|s| !s.contains("%{"))
            .map(PathBuf::from)
            .filter(|p| p.components().count() == 1)
            .filter(|p| p.file_name().is_some())
            .collect()
    }
}

/// Extracts the numeric index from a `PatchN:` tag, treating the bare `Patch:`
/// tag and anything unparseable as index 0.
fn patch_index(tag: &str) -> u32 {
    tag.trim_end_matches(':')
        .trim_start_matches("Patch")
        .parse()
        .unwrap_or(0)
}

/// Finds the `-p<N>` strip level in a list of `%autosetup` arguments. Both the
/// attached (`-p1`) and detached (`-p 1`) forms are accepted.
fn strip_level(args: VecDeque<&str>) -> Option<u32> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if let Some(value) = arg.strip_prefix("-p") {
            if value.is_empty() {
                return args.next().and_then(|v| v.parse().ok());
            }
            return value.parse().ok();
        }
    }
    None
}

// =^..^= =^..^= =^..^= =^..^= =^..^= =^..^= =^..^= =^..^= =^..^=

#[cfg(test)]
mod test {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn spec_info(contents: &str) -> (TempDir, SpecInfo) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.spec");
        let mut f = File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        drop(f);
        let info = SpecInfo::new(&path).unwrap();
        (dir, info)
    }

    #[test]
    fn test_patches_sorted_by_index() {
        let (_dir, info) = spec_info(
            "\
Patch0010: ten.patch
Patch0002: two.patch
Patch0001: one.patch
",
        );
        assert_eq!(
            info.patches,
            vec![
                PathBuf::from("one.patch"),
                PathBuf::from("two.patch"),
                PathBuf::from("ten.patch"),
            ]
        );
    }

    #[test]
    fn test_bare_patch_tag_sorts_first_and_keeps_file_order() {
        let (_dir, info) = spec_info(
            "\
Patch1: one.patch
Patch: bare.patch
",
        );
        assert_eq!(
            info.patches,
            vec![PathBuf::from("bare.patch"), PathBuf::from("one.patch")]
        );
    }

    #[test]
    fn test_sources_and_patches_are_separated() {
        let (_dir, info) = spec_info(
            "\
Source0: foo-1.0.tar.gz
Source100: foo.service
Patch1: one.patch
",
        );
        assert_eq!(
            info.sources,
            vec![
                PathBuf::from("foo-1.0.tar.gz"),
                PathBuf::from("foo.service")
            ]
        );
        assert_eq!(info.patches, vec![PathBuf::from("one.patch")]);
    }

    #[test]
    fn test_macros_and_paths_are_filtered() {
        let (_dir, info) = spec_info(
            "\
Patch1: %{name}-fix.patch
Patch2: subdir/nested.patch
Patch3: ok.patch
",
        );
        assert_eq!(info.patches, vec![PathBuf::from("ok.patch")]);
    }

    #[test]
    fn test_default_strip_level() {
        let (_dir, info) = spec_info("%prep\n%autosetup -n foo-1.0\n");
        assert_eq!(info.patch_strip_level, 1);
    }

    #[test]
    fn test_strip_level_attached() {
        let (_dir, info) = spec_info("%prep\n%autosetup -Sgit -n foo-1.0 -p0\n");
        assert_eq!(info.patch_strip_level, 0);
    }

    #[test]
    fn test_strip_level_detached() {
        let (_dir, info) = spec_info("%prep\n%autosetup -n foo-1.0 -p 2\n");
        assert_eq!(info.patch_strip_level, 2);
    }

    #[test]
    fn test_strip_level_without_autosetup() {
        let (_dir, info) = spec_info("%prep\n%setup -q\n%patch -P 1 -p1\n");
        assert_eq!(info.patch_strip_level, 1);
    }
}
