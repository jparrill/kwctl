extern crate anyhow;
extern crate clap;
extern crate directories;
extern crate policy_evaluator;
extern crate pretty_bytes;
#[macro_use]
extern crate prettytable;
extern crate serde_yaml;

use anyhow::{anyhow, Result};
use clap::ArgMatches;
use directories::UserDirs;
use lazy_static::lazy_static;
use std::{
    collections::HashMap,
    convert::TryFrom,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    str::FromStr,
};

use tokio::task::spawn_blocking;
use verify::VerificationAnnotations;

use tracing::{debug, info, warn};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{
    filter::{EnvFilter, LevelFilter},
    fmt,
};

use policy_evaluator::policy_evaluator::PolicyExecutionMode;
use policy_evaluator::policy_fetcher::{
    registry::config::{read_docker_config_json_file, DockerConfig},
    registry::Registry,
    sigstore,
    sources::{read_sources_file, Certificate, Sources},
    store::DEFAULT_ROOT,
    verify::{
        config::{read_verification_file, LatestVerificationConfig, Signature, Subject},
        FulcioAndRekorData,
    },
    PullDestination,
};

use crate::utils::new_policy_execution_mode_from_str;

mod annotate;
mod backend;
mod cli;
mod completions;
mod inspect;
mod policies;
mod pull;
mod push;
mod rm;
mod run;
mod scaffold;
mod utils;
mod verify;

pub(crate) const KWCTL_VERIFICATION_CONFIG: &str = "verification-config.yml";

lazy_static! {
    pub(crate) static ref KWCTL_DEFAULT_VERIFICATION_CONFIG_PATH: String = {
        DEFAULT_ROOT
            .config_dir()
            .join(KWCTL_VERIFICATION_CONFIG)
            .display()
            .to_string()
    };
}

#[tokio::main]
async fn main() -> Result<()> {
    let matches = cli::build_cli().get_matches();

    // setup logging
    let level_filter = if matches.contains_id("verbose") {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };
    let filter_layer = EnvFilter::from_default_env()
        .add_directive(level_filter.into())
        .add_directive("cranelift_codegen=off".parse().unwrap()) // this crate generates lots of tracing events we don't care about
        .add_directive("cranelift_wasm=off".parse().unwrap()) // this crate generates lots of tracing events we don't care about
        .add_directive("hyper=off".parse().unwrap()) // this crate generates lots of tracing events we don't care about
        .add_directive("regalloc=off".parse().unwrap()) // this crate generates lots of tracing events we don't care about
        .add_directive("wasmtime_cache=off".parse().unwrap()); // wasmtime_cache messages are not critical and just confuse users
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt::layer().with_writer(std::io::stderr))
        .init();

    match matches.subcommand_name() {
        Some("policies") => policies::list(),
        Some("pull") => {
            if let Some(matches) = matches.subcommand_matches("pull") {
                let uri = matches.get_one::<String>("uri").unwrap();
                let destination = matches
                    .get_one::<String>("output-path")
                    .map(|output| PathBuf::from_str(output).unwrap());
                let destination = match destination {
                    Some(destination) => PullDestination::LocalFile(destination),
                    None => PullDestination::MainStore,
                };

                let (sources, docker_config) = remote_server_options(matches)?;

                let verification_options = verification_options(matches)?;
                let mut verified_manifest_digest: Option<String> = None;
                if verification_options.is_some() {
                    let fulcio_and_rekor_data = build_fulcio_and_rekor_data(matches).await?;
                    // verify policy prior to pulling if keys listed, and keep the
                    // verified manifest digest:
                    verified_manifest_digest = Some(
                        verify::verify(
                            uri,
                            docker_config.as_ref(),
                            sources.as_ref(),
                            verification_options.as_ref().unwrap(),
                            fulcio_and_rekor_data.as_ref(),
                        )
                        .await
                        .map_err(|e| anyhow!("Policy {} cannot be validated\n{:?}", uri, e))?,
                    );
                }

                let policy =
                    pull::pull(uri, docker_config.as_ref(), sources.as_ref(), destination).await?;

                if verification_options.is_some() {
                    let fulcio_and_rekor_data = build_fulcio_and_rekor_data(matches).await?;
                    verify::verify_local_checksum(
                        &policy,
                        docker_config.as_ref(),
                        sources.as_ref(),
                        &verified_manifest_digest.unwrap(),
                        fulcio_and_rekor_data.as_ref(),
                    )
                    .await?
                }
            };
            Ok(())
        }
        Some("verify") => {
            if let Some(matches) = matches.subcommand_matches("verify") {
                let uri = matches.get_one::<String>("uri").unwrap();
                let (sources, docker_config) = remote_server_options(matches)?;
                let verification_options = verification_options(matches)?
                    .ok_or_else(|| anyhow!("could not retrieve sigstore options"))?;
                let fulcio_and_rekor_data = build_fulcio_and_rekor_data(matches).await?;
                verify::verify(
                    uri,
                    docker_config.as_ref(),
                    sources.as_ref(),
                    &verification_options,
                    fulcio_and_rekor_data.as_ref(),
                )
                .await
                .map_err(|e| anyhow!("Policy {} cannot be validated\n{:?}", uri, e))?;
            };
            Ok(())
        }
        Some("push") => {
            if let Some(matches) = matches.subcommand_matches("push") {
                let (sources, docker_config) = remote_server_options(matches)?;
                let wasm_uri =
                    crate::utils::map_path_to_uri(matches.get_one::<String>("policy").unwrap())?;
                let wasm_path = crate::utils::wasm_path(wasm_uri.as_str())?;
                let uri = matches
                    .get_one::<String>("uri")
                    .map(|u| {
                        if u.starts_with("registry://") {
                            u.clone()
                        } else {
                            format!("registry://{}", u)
                        }
                    })
                    .unwrap();

                debug!(
                    policy = wasm_path.to_string_lossy().to_string().as_str(),
                    destination = uri.as_str(),
                    "policy push"
                );

                let force = matches.contains_id("force");

                let immutable_ref = push::push(
                    wasm_path,
                    &uri,
                    docker_config.as_ref(),
                    sources.as_ref(),
                    force,
                )
                .await?;

                match matches.get_one::<String>("output").map(|s| s.as_str()) {
                    Some("json") => {
                        let mut response: HashMap<&str, String> = HashMap::new();
                        response.insert("immutable_ref", immutable_ref);
                        serde_json::to_writer(std::io::stdout(), &response)?
                    }
                    _ => {
                        println!("Policy successfully pushed: {}", immutable_ref);
                    }
                }
            };
            Ok(())
        }
        Some("rm") => {
            if let Some(matches) = matches.subcommand_matches("rm") {
                let uri = matches.get_one::<String>("uri").unwrap();
                rm::rm(uri)?;
            }
            Ok(())
        }
        Some("run") => {
            if let Some(matches) = matches.subcommand_matches("run") {
                let uri = matches.get_one::<String>("uri").unwrap();
                let request = match matches
                    .get_one::<String>("request-path")
                    .map(|s| s.as_str())
                    .unwrap()
                {
                    "-" => {
                        let mut buffer = String::new();
                        io::stdin()
                            .read_to_string(&mut buffer)
                            .map_err(|e| anyhow!("Error reading request from stdin: {}", e))?;
                        buffer
                    }
                    request_path => fs::read_to_string(request_path).map_err(|e| {
                        anyhow!(
                            "Error opening request file {}; {}",
                            matches.get_one::<String>("request-path").unwrap(),
                            e
                        )
                    })?,
                };
                if matches.contains_id("settings-path") && matches.contains_id("settings-json") {
                    return Err(anyhow!(
                        "'settings-path' and 'settings-json' cannot be used at the same time"
                    ));
                }
                let settings = if matches.contains_id("settings-path") {
                    matches
                        .get_one::<String>("settings-path")
                        .map(|settings| -> Result<String> {
                            fs::read_to_string(settings).map_err(|e| {
                                anyhow!("Error reading settings from {}: {}", settings, e)
                            })
                        })
                        .transpose()?
                } else if matches.contains_id("settings-json") {
                    Some(matches.get_one::<String>("settings-json").unwrap().clone())
                } else {
                    None
                };
                let (sources, docker_config) = remote_server_options(matches)
                    .map_err(|e| anyhow!("Error getting remote server options: {}", e))?;
                let execution_mode: Option<PolicyExecutionMode> =
                    if let Some(mode_name) = matches.get_one::<String>("execution-mode") {
                        Some(new_policy_execution_mode_from_str(mode_name)?)
                    } else {
                        None
                    };

                let verification_options = verification_options(matches)?;
                let mut verified_manifest_digest: Option<String> = None;
                let fulcio_and_rekor_data = build_fulcio_and_rekor_data(matches).await?;
                if verification_options.is_some() {
                    // verify policy prior to pulling if keys listed, and keep the
                    // verified manifest digest:
                    verified_manifest_digest = Some(
                        verify::verify(
                            uri,
                            docker_config.as_ref(),
                            sources.as_ref(),
                            verification_options.as_ref().unwrap(),
                            fulcio_and_rekor_data.as_ref(),
                        )
                        .await
                        .map_err(|e| anyhow!("Policy {} cannot be validated\n{:?}", uri, e))?,
                    );
                }

                let enable_wasmtime_cache = !matches.contains_id("disable-wasmtime-cache");

                run::pull_and_run(
                    uri,
                    execution_mode,
                    docker_config.as_ref(),
                    sources.as_ref(),
                    &request,
                    settings,
                    &verified_manifest_digest,
                    fulcio_and_rekor_data.as_ref(),
                    enable_wasmtime_cache,
                )
                .await?;
            }
            Ok(())
        }
        Some("annotate") => {
            if let Some(matches) = matches.subcommand_matches("annotate") {
                let wasm_path = matches
                    .get_one::<String>("wasm-path")
                    .map(|output| PathBuf::from_str(output).unwrap())
                    .unwrap();
                let metadata_file = matches
                    .get_one::<String>("metadata-path")
                    .map(|output| PathBuf::from_str(output).unwrap())
                    .unwrap();
                let destination = matches
                    .get_one::<String>("output-path")
                    .map(|output| PathBuf::from_str(output).unwrap())
                    .unwrap();
                annotate::write_annotation(wasm_path, metadata_file, destination)?;
            }
            Ok(())
        }
        Some("inspect") => {
            if let Some(matches) = matches.subcommand_matches("inspect") {
                let uri = matches.get_one::<String>("uri").unwrap();
                let output = inspect::OutputType::try_from(
                    matches.get_one::<String>("output").map(|s| s.as_str()),
                )?;
                let (sources, docker_config) = remote_server_options(matches)?;

                inspect::inspect(uri, output, sources, docker_config).await?;
            };
            Ok(())
        }
        Some("scaffold") => {
            if let Some(matches) = matches.subcommand_matches("scaffold") {
                if let Some(_matches) = matches.subcommand_matches("verification-config") {
                    println!("{}", scaffold::verification_config()?);
                }
            }
            if let Some(matches) = matches.subcommand_matches("scaffold") {
                if let Some(matches) = matches.subcommand_matches("manifest") {
                    let uri = matches.get_one::<String>("uri").unwrap();
                    let resource_type = matches.get_one::<String>("type").unwrap();
                    if matches.contains_id("settings-path") && matches.contains_id("settings-json")
                    {
                        return Err(anyhow!(
                            "'settings-path' and 'settings-json' cannot be used at the same time"
                        ));
                    }
                    let settings = if matches.contains_id("settings-path") {
                        matches
                            .get_one::<String>("settings-path")
                            .map(|settings| -> Result<String> {
                                fs::read_to_string(settings).map_err(|e| {
                                    anyhow!("Error reading settings from {}: {}", settings, e)
                                })
                            })
                            .transpose()?
                    } else if matches.contains_id("settings-json") {
                        Some(matches.get_one::<String>("settings-json").unwrap().clone())
                    } else {
                        None
                    };
                    let policy_title = matches.get_one::<String>("title").cloned();

                    scaffold::manifest(uri, resource_type, settings, policy_title)?;
                };
            }
            Ok(())
        }
        Some("completions") => {
            if let Some(matches) = matches.subcommand_matches("completions") {
                completions::completions(matches.get_one::<String>("shell").unwrap())?;
            }
            Ok(())
        }
        Some("digest") => {
            if let Some(matches) = matches.subcommand_matches("digest") {
                let uri = matches.get_one::<String>("uri").unwrap();
                let (sources, docker_config) = remote_server_options(matches)?;
                let registry = Registry::new(docker_config.as_ref());
                let digest = registry.manifest_digest(uri, sources.as_ref()).await?;
                println!("{}@{}", uri, digest);
            }
            Ok(())
        }
        Some(command) => Err(anyhow!("unknown subcommand: {}", command)),
        None => {
            // NOTE: this should not happen due to
            // SubcommandRequiredElseHelp setting
            unreachable!();
        }
    }
}

fn remote_server_options(matches: &ArgMatches) -> Result<(Option<Sources>, Option<DockerConfig>)> {
    let sources = if let Some(sources_path) = matches.get_one::<String>("sources-path") {
        Some(read_sources_file(Path::new(&sources_path))?)
    } else {
        let sources_path = DEFAULT_ROOT.config_dir().join("sources.yaml");
        if Path::exists(&sources_path) {
            Some(read_sources_file(&sources_path)?)
        } else {
            None
        }
    };

    let docker_config = if let Some(docker_config_json_path) =
        matches.get_one::<String>("docker-config-json-path")
    {
        Some(read_docker_config_json_file(Path::new(
            docker_config_json_path,
        ))?)
    } else if let Some(user_dir) = UserDirs::new() {
        let config_json_path = user_dir.home_dir().join(".docker").join("config.json");
        if Path::exists(&config_json_path) {
            Some(read_docker_config_json_file(&config_json_path)?)
        } else {
            None
        }
    } else {
        None
    };
    Ok((sources, docker_config))
}

fn verification_options(matches: &ArgMatches) -> Result<Option<LatestVerificationConfig>> {
    if let Some(verification_config) = build_verification_options_from_flags(matches)? {
        // flags present, built configmap from them:
        if matches.contains_id("verification-config-path") {
            return Err(anyhow!(
                "verification-config-path cannot be used in conjunction with other verification flags"
            ));
        }
        return Ok(Some(verification_config));
    }
    if let Some(verification_config_path) = matches.get_one::<String>("verification-config-path") {
        // config flag present, read it:
        return Ok(Some(read_verification_file(Path::new(
            &verification_config_path,
        ))?));
    } else {
        let verification_config_path = DEFAULT_ROOT.config_dir().join(KWCTL_VERIFICATION_CONFIG);
        if Path::exists(&verification_config_path) {
            // default config flag present, read it:
            info!(path = ?verification_config_path, "Default verification config present, using it");
            Ok(Some(read_verification_file(&verification_config_path)?))
        } else {
            Ok(None)
        }
    }
}

// Takes clap flags and builds a Some(LatestVerificationConfig) containing all
// passed pub keys and annotations in LatestVerificationConfig.AllOf.
// If no verification flags where used, it returns a None.
fn build_verification_options_from_flags(
    matches: &ArgMatches,
) -> Result<Option<LatestVerificationConfig>> {
    let key_files: Option<Vec<String>> = matches
        .get_many::<String>("verification-key")
        .map(|items| items.into_iter().map(|i| i.to_string()).collect());

    let annotations: Option<VerificationAnnotations> =
        match matches.get_many::<String>("verification-annotation") {
            None => None,
            Some(items) => {
                let mut values: HashMap<String, String> = HashMap::new();
                for item in items {
                    let tmp: Vec<_> = item.splitn(2, '=').collect();
                    if tmp.len() == 2 {
                        values.insert(String::from(tmp[0]), String::from(tmp[1]));
                    }
                }
                if values.is_empty() {
                    None
                } else {
                    Some(values)
                }
            }
        };

    let cert_email: Option<String> = matches
        .get_many::<String>("cert-email")
        .map(|items| items.into_iter().map(|i| i.to_string()).collect());
    let cert_oidc_issuer: Option<String> = matches
        .get_many::<String>("cert-oidc-issuer")
        .map(|items| items.into_iter().map(|i| i.to_string()).collect());

    let github_owner: Option<String> = matches
        .get_many::<String>("github-owner")
        .map(|items| items.into_iter().map(|i| i.to_string()).collect());
    let github_repo: Option<String> = matches
        .get_many::<String>("github-repo")
        .map(|items| items.into_iter().map(|i| i.to_string()).collect());

    if key_files.is_none()
        && annotations.is_none()
        && cert_email.is_none()
        && cert_oidc_issuer.is_none()
        && github_owner.is_none()
        && github_repo.is_none()
    {
        // no verification flags were used, don't create a LatestVerificationConfig
        return Ok(None);
    }

    if key_files.is_none()
        && cert_email.is_none()
        && cert_oidc_issuer.is_none()
        && github_owner.is_none()
        && annotations.is_some()
    {
        return Err(anyhow!(
            "Intending to verify annotations, but no verification keys, OIDC issuer or GitHub owner were passed"
        ));
    }

    if github_repo.is_some() && github_owner.is_none() {
        return Err(anyhow!(
            "Intending to verify GitHub actions signature, but the repository owner is missing."
        ));
    }

    let mut signatures: Vec<Signature> = Vec::new();

    if (cert_email.is_some() && cert_oidc_issuer.is_none())
        || (cert_email.is_none() && cert_oidc_issuer.is_some())
    {
        return Err(anyhow!(
            "Intending to verify OIDC issuer, but no email or issuer were provided. You must pass the email and OIDC issuer to be validated together "
        ));
    } else if cert_email.is_some() && cert_oidc_issuer.is_some() {
        let sig = Signature::GenericIssuer {
            issuer: cert_oidc_issuer.unwrap(),
            subject: Subject::Equal(cert_email.unwrap()),
            annotations: annotations.clone(),
        };
        signatures.push(sig)
    }

    if let Some(repo_owner) = github_owner {
        let sig = Signature::GithubAction {
            owner: repo_owner,
            repo: github_repo,
            annotations: annotations.clone(),
        };
        signatures.push(sig)
    }

    for key_path in key_files.iter().flatten() {
        let sig = Signature::PubKey {
            owner: None,
            key: fs::read_to_string(key_path)
                .map_err(|e| anyhow!("could not read file {}: {:?}", key_path, e))?
                .to_string(),
            annotations: annotations.clone(),
        };
        signatures.push(sig);
    }
    let signatures_all_of: Option<Vec<Signature>> = if signatures.is_empty() {
        None
    } else {
        Some(signatures)
    };
    let verification_config = LatestVerificationConfig {
        all_of: signatures_all_of,
        any_of: None,
    };
    Ok(Some(verification_config))
}

async fn build_fulcio_and_rekor_data(matches: &ArgMatches) -> Result<Option<FulcioAndRekorData>> {
    if matches.contains_id("fulcio-cert-path") || matches.contains_id("rekor-public-key-path") {
        let mut fulcio_certs: Vec<Certificate> = vec![];
        if let Some(items) = matches.get_many::<String>("fulcio-cert-path") {
            for item in items {
                let data = fs::read(item)?;
                let cert = Certificate::Pem(data);
                fulcio_certs.push(cert);
            }
        };

        let rekor_public_key = if let Some(rekor_public_key_path) =
            matches.get_one::<String>("rekor-public-key-path")
        {
            Some(fs::read_to_string(rekor_public_key_path)?)
        } else {
            None
        };

        if fulcio_certs.is_empty() || rekor_public_key.is_none() {
            return Err(anyhow!(
                "both a fulcio certificate and a rekor public key are required"
            ));
        }

        Ok(Some(FulcioAndRekorData::FromCustomData {
            fulcio_certs,
            rekor_public_key,
        }))
    } else {
        let checkout_path = DEFAULT_ROOT.config_dir().join("fulcio_and_rekor_data");
        if !Path::exists(&checkout_path) {
            fs::create_dir_all(checkout_path.clone())?
        }

        let repo =
            spawn_blocking(move || sigstore::tuf::SigstoreRepository::fetch(Some(&checkout_path)))
                .await?;
        match repo {
            Ok(repo) => Ok(Some(FulcioAndRekorData::FromTufRepository { repo })),
            Err(e) => {
                warn!("Cannot fetch TUF repository: {:?}", e);
                // policy-fetcher will print the needed follow-up warning messages
                Ok(None)
            }
        }
    }
}
