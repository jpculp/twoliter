//! The ecr module owns the 'ecr' subcommand and controls the process of copying
//! multi-arch container images to private ECR across configured regions.

pub(crate) mod manifest;

use self::manifest::{ResolvedImage, ResolvedSource};
use crate::aws::client::build_client_config;
use crate::Args;
use aws_credential_types::provider::ProvideCredentials;
use aws_sdk_sts::operation::get_caller_identity::GetCallerIdentityError;
use aws_sdk_sts::Client as StsClient;
use aws_types::region::Region;
use clap::{ArgGroup, Parser};
use error_utils::AwsSdkError;
use indicatif::{ProgressBar, ProgressStyle};
use log::{debug, error, info, trace};
use oci_cli_wrapper::ImageTool;
use pubsys_config::InfraConfig;
use snafu::{ensure, OptionExt, ResultExt};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio_retry::strategy::{jitter, ExponentialBackoff};
use tokio_retry::RetryIf;

const MAX_ATTEMPTS: usize = 3;
const INITIAL_BACKOFF_MS: u64 = 5_000;
const MAX_BACKOFF_MS: u64 = 30_000;

/// Copies manifests of containers to ECR.
#[derive(Debug, Parser)]
#[command(group(
    ArgGroup::new("source")
        .required(true)
        .args(["manifest", "source_uri", "source_archive"])
))]
pub(crate) struct EcrArgs {
    /// Path to a manifest TOML listing multiple images to publish
    #[arg(long, conflicts_with_all = ["source_uri", "source_archive", "repository_name", "tag"])]
    manifest: Option<PathBuf>,

    /// Source image URI in a remote registry
    #[arg(long, conflicts_with = "source_archive")]
    source_uri: Option<String>,

    /// Path to a local OCI archive tarball to push
    #[arg(long, conflicts_with = "source_uri")]
    source_archive: Option<PathBuf>,

    /// ECR repository name in the destination accounts
    #[arg(long, required_unless_present = "manifest")]
    repository_name: Option<String>,

    /// Tag for the image, if omitted it is inferred from the source URI
    #[arg(long)]
    tag: Option<String>,

    /// Regions where you want containers published
    #[arg(long, value_delimiter = ',')]
    regions: Vec<String>,

    /// Enables dry-run mode (displays what would be published without pushing)
    #[arg(long)]
    dry_run: bool,

    /// Don't display progress bars
    #[arg(long)]
    no_progress: bool,
}

fn job_label(region: &str, image: &ResolvedImage) -> String {
    format!("{}/{}:{}", region, image.repository_name, image.tag)
}

fn retry_strategy() -> impl Iterator<Item = Duration> {
    ExponentialBackoff::from_millis(INITIAL_BACKOFF_MS)
        .max_delay(Duration::from_millis(MAX_BACKOFF_MS))
        .map(jitter)
        .enumerate()
        .map(|(attempt, d)| {
            if attempt > 0 {
                error!("Retrying: attempt = {}", attempt + 1);
            }
            d
        })
        .take(MAX_ATTEMPTS - 1)
}

fn ecr_registry(region: &str, account_id: &str, role_arn: Option<&str>) -> String {
    let partition = role_arn
        .and_then(|arn| arn.split(':').nth(1))
        .unwrap_or("aws");
    let suffix = match partition {
        "aws-cn" => "amazonaws.com.cn",
        "aws-iso" => "c2s.ic.gov",
        "aws-iso-b" => "sc2s.sgov.gov",
        _ => "amazonaws.com",
    };
    format!("{account_id}.dkr.ecr.{region}.{suffix}")
}

/// Common entrypoint from main()
pub(crate) async fn run(args: &Args, ecr_args: &EcrArgs) -> Result<()> {
    let infra_config = InfraConfig::from_path_or_lock(&args.infra_config_path, false)
        .context(error::ConfigSnafu)?;
    trace!("Parsed infra config: {infra_config:#?}");

    let aws = infra_config.aws.unwrap_or_default();

    let images = manifest::resolve_images(
        ecr_args.manifest.as_ref(),
        ecr_args.source_uri.as_deref(),
        ecr_args.source_archive.as_ref(),
        ecr_args.repository_name.as_deref(),
        ecr_args.tag.as_deref(),
    )
    .context(error::ManifestSnafu)?;

    let image_tool = ImageTool::from_builtin_krane();

    ensure!(
        !aws.regions.is_empty(),
        error::MissingConfigSnafu {
            missing: "aws.regions"
        }
    );
    let sts_region = Region::new(aws.regions[0].clone());

    let regions: Vec<&String> = aws
        .regions
        .iter()
        .filter(|r| ecr_args.regions.is_empty() || ecr_args.regions.contains(r))
        .collect();

    let total = regions.len() * images.len();

    let progress_bar = ProgressBar::new(total as u64);
    if ecr_args.no_progress {
        progress_bar.set_draw_target(indicatif::ProgressDrawTarget::hidden());
    }
    progress_bar.set_style(
        ProgressStyle::default_bar()
            .template("  Publishing [{bar:40.green/black}] {pos}/{len} {msg}")
            .expect("valid template")
            .progress_chars("=> "),
    );

    let mut failures: Vec<String> = Vec::new();

    if ecr_args.dry_run {
        info!("Dry-run mode enabled, no images will be published.");
    }

    for region in &regions {
        let role_arn = aws
            .region
            .get(region.as_str())
            .and_then(|r| r.role.as_deref())
            .or(aws.role.as_deref());

        let target_region = Region::new(region.to_string());
        let sdk_config = build_client_config(&target_region, &sts_region, &aws).await;

        let account_id = match role_arn.and_then(|arn| arn.split(':').nth(4)) {
            Some(id) => id.to_owned(),
            None => {
                let sts_client = StsClient::new(&sdk_config);
                let identity = sts_client
                    .get_caller_identity()
                    .send()
                    .await
                    .map_err(AwsSdkError::from)
                    .context(error::GetCallerIdentitySnafu { region: *region })?;
                identity
                    .account()
                    .context(error::MissingAccountIdSnafu { region: *region })?
                    .to_owned()
            }
        };

        let registry = ecr_registry(region, &account_id, role_arn);

        if ecr_args.dry_run {
            info!("[dry-run] {region} ({account_id}):");
            for image in &images {
                let destination_uri = format!("{registry}/{}:{}", image.repository_name, image.tag);
                match &image.source {
                    ResolvedSource::Registry(source_uri) => {
                        info!("[dry-run]   {source_uri} -> {destination_uri}");
                    }
                    ResolvedSource::OciArchive(path) => {
                        info!("[dry-run]   {} -> {destination_uri}", path.display());
                    }
                }
            }
            continue;
        }

        let credentials_provider =
            sdk_config
                .credentials_provider()
                .context(error::MissingConfigSnafu {
                    missing: "credentials provider",
                })?;

        let credentials = credentials_provider
            .provide_credentials()
            .await
            .context(error::CredentialsSnafu { region: *region })?;

        let mut env = HashMap::new();
        env.insert("AWS_ACCESS_KEY_ID", credentials.access_key_id());
        env.insert("AWS_SECRET_ACCESS_KEY", credentials.secret_access_key());
        if let Some(token) = credentials.session_token() {
            env.insert("AWS_SESSION_TOKEN", token);
        }

        progress_bar.println(format!("Publishing to {region} ({account_id})"));

        for image in &images {
            let label = job_label(region, image);

            let result = publish_image(&image_tool, image, &registry, &env).await;

            match &result {
                Ok(()) => progress_bar.println(format!("  Published {label}")),
                Err(e) => {
                    progress_bar.println(format!("  FAILED {label} - {e}"));
                    failures.push(label);
                }
            }
            progress_bar.inc(1);
        }
    }

    progress_bar.finish_and_clear();

    if !failures.is_empty() {
        return error::PublishFailedSnafu {
            count: failures.len(),
            targets: failures.join(", "),
        }
        .fail();
    }

    info!("All {} targets succeeded", total);
    Ok(())
}

async fn publish_image(
    image_tool: &ImageTool,
    image: &ResolvedImage,
    registry: &str,
    credentials: &HashMap<&str, &str>,
) -> Result<()> {
    let destination_uri = format!("{registry}/{}:{}", image.repository_name, image.tag);

    match &image.source {
        ResolvedSource::Registry(source_uri) => {
            debug!("Pulling {source_uri}");
            let pull_dir =
                tempfile::TempDir::new().context(error::PullTempDirSnafu { uri: source_uri })?;
            let layout_path = pull_dir.path().join("layout");
            image_tool
                .pull_oci_image(&layout_path, source_uri)
                .await
                .context(error::ImageToolSnafu { region: registry })?;

            debug!("Pushing {destination_uri}");
            RetryIf::start(
                retry_strategy(),
                || async {
                    image_tool
                        .push_oci_layout(&layout_path, &destination_uri, Some(credentials))
                        .await
                },
                |e: &oci_cli_wrapper::error::Error| {
                    error!("Failed to push to {registry}: {e}");
                    true
                },
            )
            .await
            .context(error::ImageToolSnafu { region: registry })?;
        }
        ResolvedSource::OciArchive(path) => {
            debug!("Pushing {} -> {destination_uri}", path.display());
            RetryIf::start(
                retry_strategy(),
                || async {
                    image_tool
                        .push_oci_archive(path, &destination_uri, Some(credentials))
                        .await
                },
                |e: &oci_cli_wrapper::error::Error| {
                    error!("Failed to push to {registry}: {e}");
                    true
                },
            )
            .await
            .context(error::ImageToolSnafu { region: registry })?;
        }
    }

    Ok(())
}

mod error {
    use snafu::Snafu;

    #[derive(Debug, Snafu)]
    #[snafu(visibility(pub(super)))]
    pub(crate) enum Error {
        #[snafu(display("Error reading config: {}", source))]
        Config { source: pubsys_config::Error },

        #[snafu(display("Failed to resolve image manifest: {}", source))]
        Manifest { source: super::manifest::Error },

        #[snafu(display("Failed to obtain credentials for {}: {}", region, source))]
        Credentials {
            region: String,
            source: aws_credential_types::provider::error::CredentialsError,
        },

        #[snafu(display("Failed to create temp directory for pulling {}: {}", uri, source))]
        PullTempDir { uri: String, source: std::io::Error },

        #[snafu(display("Failed to run image tool for {}: {}", region, source))]
        ImageTool {
            region: String,
            source: oci_cli_wrapper::error::Error,
        },

        #[snafu(display("Error getting account ID in {}: {}", region, source))]
        GetCallerIdentity {
            region: String,
            #[snafu(source(from(super::AwsSdkError<super::GetCallerIdentityError>, Box::new)))]
            source: Box<super::AwsSdkError<super::GetCallerIdentityError>>,
        },

        #[snafu(display("No account ID returned for region {}", region))]
        MissingAccountId { region: String },

        #[snafu(display("Missing config: {}", missing))]
        MissingConfig { missing: String },

        #[snafu(display("Publish failed for {} target(s): {}", count, targets))]
        PublishFailed { count: usize, targets: String },
    }
}

pub(crate) use error::Error;
type Result<T> = std::result::Result<T, error::Error>;

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_ecr_registry_aws() {
        let arn = "arn:aws:iam::111222333444:role/TestRole";
        let result = ecr_registry("us-west-2", "111222333444", Some(arn));
        assert_eq!(result, "111222333444.dkr.ecr.us-west-2.amazonaws.com");
    }

    #[test]
    fn test_ecr_registry_aws_cn() {
        let arn = "arn:aws-cn:iam::555666777888:role/TestRole";
        let result = ecr_registry("cn-north-1", "555666777888", Some(arn));
        assert_eq!(result, "555666777888.dkr.ecr.cn-north-1.amazonaws.com.cn");
    }

    #[test]
    fn test_ecr_registry_no_role() {
        let result = ecr_registry("us-west-2", "111222333444", None);
        assert_eq!(result, "111222333444.dkr.ecr.us-west-2.amazonaws.com");
    }
}
