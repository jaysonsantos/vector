mod config_builder;
mod loader;
mod source;

use std::{
    collections::HashMap,
    fmt::Debug,
    fs::{File, ReadDir},
    path::{Path, PathBuf},
    sync::Mutex,
};

use config_builder::ConfigBuilderLoader;
use loader::process::Process;

use super::{
    builder::ConfigBuilder, format, validation, vars, Config, ConfigPath, Format, FormatHint,
};
use crate::signal;
use glob::glob;
use once_cell::sync::Lazy;

pub use config_builder::*;
pub use loader::*;
pub use source::*;

pub static CONFIG_PATHS: Lazy<Mutex<Vec<ConfigPath>>> = Lazy::new(Mutex::default);

pub(super) fn read_dir<P: AsRef<Path> + Debug>(path: P) -> Result<ReadDir, Vec<String>> {
    path.as_ref()
        .read_dir()
        .map_err(|err| vec![format!("Could not read config dir: {:?}, {}.", path, err)])
}

pub(super) fn component_name<P: AsRef<Path> + Debug>(path: P) -> Result<String, Vec<String>> {
    path.as_ref()
        .file_stem()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
        .ok_or_else(|| vec![format!("Couldn't get component name for file: {:?}", path)])
}

pub(super) fn open_file<P: AsRef<Path> + Debug>(path: P) -> Option<File> {
    match File::open(&path) {
        Ok(f) => Some(f),
        Err(error) => {
            if let std::io::ErrorKind::NotFound = error.kind() {
                error!(message = "Config file not found in path.", ?path);
                None
            } else {
                error!(message = "Error opening config file.", %error, ?path);
                None
            }
        }
    }
}

/// Merge the paths coming from different cli flags with different formats into
/// a unified list of paths with formats.
pub fn merge_path_lists(
    path_lists: Vec<(&[PathBuf], FormatHint)>,
) -> impl Iterator<Item = (PathBuf, FormatHint)> + '_ {
    path_lists
        .into_iter()
        .flat_map(|(paths, format)| paths.iter().cloned().map(move |path| (path, format)))
}

/// Expand a list of paths (potentially containing glob patterns) into real
/// config paths, replacing it with the default paths when empty.
pub fn process_paths(config_paths: &[ConfigPath]) -> Option<Vec<ConfigPath>> {
    let default_paths = default_config_paths();

    let starting_paths = if !config_paths.is_empty() {
        config_paths
    } else {
        &default_paths
    };

    let mut paths = Vec::new();

    for config_path in starting_paths {
        let config_pattern: &PathBuf = config_path.into();

        let matches: Vec<PathBuf> = match glob(config_pattern.to_str().expect("No ability to glob"))
        {
            Ok(glob_paths) => glob_paths.filter_map(Result::ok).collect(),
            Err(err) => {
                error!(message = "Failed to read glob pattern.", path = ?config_pattern, error = ?err);
                return None;
            }
        };

        if matches.is_empty() {
            error!(message = "Config file not found in path.", path = ?config_pattern);
            std::process::exit(exitcode::CONFIG);
        }

        match config_path {
            ConfigPath::File(_, format) => {
                for path in matches {
                    paths.push(ConfigPath::File(path, *format));
                }
            }
            ConfigPath::Dir(_) => {
                for path in matches {
                    paths.push(ConfigPath::Dir(path))
                }
            }
        }
    }

    paths.sort();
    paths.dedup();
    // Ignore poison error and let the current main thread continue running to do the cleanup.
    std::mem::drop(CONFIG_PATHS.lock().map(|mut guard| *guard = paths.clone()));

    Some(paths)
}

pub fn load_from_paths(config_paths: &[ConfigPath]) -> Result<Config, Vec<String>> {
    let (builder, load_warnings) = load_builder_from_paths(config_paths)?;
    let (config, build_warnings) = builder.build_with_warnings()?;

    for warning in load_warnings.into_iter().chain(build_warnings) {
        warn!("{}", warning);
    }

    Ok(config)
}

/// Loads a configuration from paths. If a provider is present in the builder, the config is
/// used as bootstrapping for a remote source. Otherwise, provider instantiation is skipped.
pub async fn load_from_paths_with_provider(
    config_paths: &[ConfigPath],
    signal_handler: &mut signal::SignalHandler,
) -> Result<Config, Vec<String>> {
    let (mut builder, load_warnings) = load_builder_from_paths(config_paths)?;
    validation::check_provider(&builder)?;
    signal_handler.clear();

    // If there's a provider, overwrite the existing config builder with the remote variant.
    if let Some(mut provider) = builder.provider {
        builder = provider.build(signal_handler).await?;
        debug!(message = "Provider configured.", provider = ?provider.provider_type());
    }

    let (new_config, build_warnings) = builder.build_with_warnings()?;

    for warning in load_warnings.into_iter().chain(build_warnings) {
        warn!("{}", warning);
    }

    Ok(new_config)
}

/// Iterators over `ConfigPaths`, and processes a file/dir according to a provided `Loader`.
fn loader_from_paths<T, L>(
    mut loader: L,
    config_paths: &[ConfigPath],
) -> Result<(T, Vec<String>), Vec<String>>
where
    T: serde::de::DeserializeOwned,
    L: Loader<T> + Process,
{
    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    for config_path in config_paths {
        match config_path {
            ConfigPath::File(path, format_hint) => {
                match loader.load_from_file(
                    path,
                    format_hint
                        .or_else(move || Format::from_path(&path).ok())
                        .unwrap_or_default(),
                ) {
                    Ok(warns) => warnings.extend(warns),
                    Err(errs) => errors.extend(errs),
                };
            }
            ConfigPath::Dir(path) => {
                match loader.load_from_dir(path) {
                    Ok(warns) => warnings.extend(warns),
                    Err(errs) => errors.extend(errs),
                };
            }
        }
    }

    if errors.is_empty() {
        Ok((loader.take(), warnings))
    } else {
        Err(errors)
    }
}

/// Uses `ConfigBuilderLoader` to process `ConfigPaths`, deserializing to a `ConfigBuilder`.
pub fn load_builder_from_paths(
    config_paths: &[ConfigPath],
) -> Result<(ConfigBuilder, Vec<String>), Vec<String>> {
    loader_from_paths(ConfigBuilderLoader::new(), config_paths)
}

/// Uses `SourceLoader` to process `ConfigPaths`, deserializing to a toml `SourceMap`.
pub fn load_source_from_paths(
    config_paths: &[ConfigPath],
) -> Result<(toml::value::Table, Vec<String>), Vec<String>> {
    loader_from_paths(SourceLoader::new(), config_paths)
}

pub fn load_from_str(input: &str, format: Format) -> Result<Config, Vec<String>> {
    let (builder, load_warnings) = load_from_inputs(std::iter::once((input.as_bytes(), format)))?;
    let (config, build_warnings) = builder.build_with_warnings()?;

    for warning in load_warnings.into_iter().chain(build_warnings) {
        warn!("{}", warning);
    }

    Ok(config)
}

fn load_from_inputs(
    inputs: impl IntoIterator<Item = (impl std::io::Read, Format)>,
) -> Result<(ConfigBuilder, Vec<String>), Vec<String>> {
    let mut config = Config::builder();
    let mut errors = Vec::new();
    let mut warnings = Vec::new();

    for (input, format) in inputs {
        if let Err(errs) = load(input, format).and_then(|(n, warn)| {
            warnings.extend(warn);
            config.append(n)
        }) {
            // TODO: add back paths
            errors.extend(errs.iter().map(|e| e.to_string()));
        }
    }

    if errors.is_empty() {
        Ok((config, warnings))
    } else {
        Err(errors)
    }
}

pub fn prepare_input<R: std::io::Read>(mut input: R) -> Result<(String, Vec<String>), Vec<String>> {
    let mut source_string = String::new();
    input
        .read_to_string(&mut source_string)
        .map_err(|e| vec![e.to_string()])?;

    let mut vars = std::env::vars().collect::<HashMap<_, _>>();
    if !vars.contains_key("HOSTNAME") {
        if let Ok(hostname) = crate::get_hostname() {
            vars.insert("HOSTNAME".into(), hostname);
        }
    }
    vars::interpolate(&source_string, &vars)
}

pub fn load<R: std::io::Read, T>(input: R, format: Format) -> Result<(T, Vec<String>), Vec<String>>
where
    T: serde::de::DeserializeOwned,
{
    let (with_vars, warnings) = prepare_input(input)?;

    format::deserialize(&with_vars, format).map(|builder| (builder, warnings))
}

#[cfg(not(windows))]
fn default_config_paths() -> Vec<ConfigPath> {
    vec![ConfigPath::File(
        "/etc/vector/vector.toml".into(),
        Some(Format::Toml),
    )]
}

#[cfg(windows)]
fn default_config_paths() -> Vec<ConfigPath> {
    let program_files =
        std::env::var("ProgramFiles").expect("%ProgramFiles% environment variable must be defined");
    let config_path = format!("{}\\Vector\\config\\vector.toml", program_files);
    vec![ConfigPath::File(
        PathBuf::from(config_path),
        Some(Format::Toml),
    )]
}

#[cfg(all(
    test,
    feature = "sinks-elasticsearch",
    feature = "transforms-pipelines",
    feature = "transforms-regex_parser",
    feature = "transforms-sample",
    feature = "sources-demo_logs",
    feature = "sinks-console"
))]
mod tests {
    use std::path::PathBuf;

    use super::load_builder_from_paths;
    use crate::{
        config::{ComponentKey, ConfigPath},
        transforms::pipelines::PipelinesConfig,
    };

    #[test]
    fn load_namespacing_folder() {
        let path = PathBuf::from(".")
            .join("tests")
            .join("namespacing")
            .join("success");
        let configs = vec![ConfigPath::Dir(path)];
        let (builder, warnings) = load_builder_from_paths(&configs).unwrap();
        assert!(warnings.is_empty());
        assert!(builder
            .transforms
            .contains_key(&ComponentKey::from("apache_parser")));
        assert!(builder
            .transforms
            .contains_key(&ComponentKey::from("processing")));
        assert!(builder
            .sources
            .contains_key(&ComponentKey::from("apache_logs")));
        assert!(builder
            .sinks
            .contains_key(&ComponentKey::from("es_cluster")));
        assert_eq!(builder.tests.len(), 2);
        let processing = builder
            .transforms
            .get(&ComponentKey::from("processing"))
            .unwrap();
        let output = serde_json::to_string_pretty(&processing.inner).unwrap();
        let processing: PipelinesConfig = serde_json::from_str(&output).unwrap();
        assert!(processing.metrics().as_ref().is_empty());
        let logs = processing.logs().as_ref();
        let first = logs.first().unwrap();
        assert_eq!(first.transforms().len(), 2);
    }

    #[test]
    fn load_namespacing_ignore_invalid() {
        let path = PathBuf::from(".")
            .join("tests")
            .join("namespacing")
            .join("ignore-invalid");
        let configs = vec![ConfigPath::Dir(path)];
        let (_, warns) = load_builder_from_paths(&configs).unwrap();
        assert!(warns.is_empty());
    }

    #[test]
    fn load_directory_ignores_unknown_file_formats() {
        let path = PathBuf::from(".").join("tests").join("config-dir");
        let configs = vec![ConfigPath::Dir(path)];
        let (_, warnings) = load_builder_from_paths(&configs).unwrap();
        assert!(warnings.is_empty());
    }
}
