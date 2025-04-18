use std::{
    path::{Path, PathBuf},
    vec,
};

use globset::Glob;
use ignore::gitignore::Gitignore;
use log::{debug, error};
use oxc_linter::{ConfigStore, ConfigStoreBuilder, LintOptions, Linter, Oxlintrc};
use rustc_hash::{FxBuildHasher, FxHashMap};
use tokio::sync::{Mutex, OnceCell, RwLock};
use tower_lsp_server::{
    UriExt,
    lsp_types::{Diagnostic, Uri},
};

use crate::{
    ConcurrentHashMap, Options, Run,
    linter::{
        config_walker::ConfigWalker, error_with_position::DiagnosticReport,
        isolated_lint_handler::IsolatedLintHandlerOptions, server_linter::ServerLinter,
    },
};

pub struct BackendWorker {
    root_uri: OnceCell<Uri>,
    server_linter: RwLock<ServerLinter>,
    diagnostics_report_map: ConcurrentHashMap<String, Vec<DiagnosticReport>>,
    options: Mutex<Options>,
    gitignore_glob: Mutex<Vec<Gitignore>>,
    nested_configs: ConcurrentHashMap<PathBuf, ConfigStore>,
}

impl BackendWorker {
    pub async fn new(root_uri: Uri, options: Options) -> Self {
        let root_uri_cell = OnceCell::new();
        root_uri_cell.set(root_uri.clone()).unwrap();

        let nested_configs = Self::init_nested_configs(root_uri.clone(), &options.clone()).await;
        let (server_linter, oxlintrc) =
            Self::init_linter_config(root_uri.clone(), &options.clone(), &nested_configs).await;

        Self {
            root_uri: root_uri_cell,
            server_linter: RwLock::new(server_linter),
            diagnostics_report_map: ConcurrentHashMap::default(),
            options: Mutex::new(options.clone()),
            gitignore_glob: Mutex::new(Self::init_ignore_glob(root_uri, oxlintrc).await),
            nested_configs,
        }
    }

    pub fn is_responsible_for_uri(&self, uri: &Uri) -> bool {
        if let Some(root_uri) = self.root_uri.get() {
            if let Some(path) = uri.to_file_path() {
                return path.starts_with(root_uri.to_file_path().unwrap());
            }
        }
        false
    }

    pub fn remove_diagnostics(&self, uri: &Uri) {
        self.diagnostics_report_map.pin().remove(&uri.to_string());
    }

    /// Searches inside root_uri recursively for the default oxlint config files
    /// and insert them inside the nested configuration
    async fn init_nested_configs(
        root_uri: Uri,
        options: &Options,
    ) -> ConcurrentHashMap<PathBuf, ConfigStore> {
        // nested config is disabled, no need to search for configs
        if options.use_nested_configs() {
            return ConcurrentHashMap::default();
        }

        let root_path = root_uri.to_file_path().expect("Failed to convert URI to file path");

        let paths = ConfigWalker::new(&root_path).paths();
        let nested_configs =
            ConcurrentHashMap::with_capacity_and_hasher(paths.capacity(), FxBuildHasher::default());

        for path in paths {
            let file_path = Path::new(&path);
            let Some(dir_path) = file_path.parent() else {
                continue;
            };

            let oxlintrc = Oxlintrc::from_file(file_path).expect("Failed to parse config file");
            let config_store_builder = ConfigStoreBuilder::from_oxlintrc(false, oxlintrc)
                .expect("Failed to create config store builder");
            let config_store = config_store_builder.build().expect("Failed to build config store");
            nested_configs.pin().insert(dir_path.to_path_buf(), config_store);
        }

        nested_configs
    }

    async fn init_linter_config(
        root_uri: Uri,
        options: &Options,
        nested_configs: &ConcurrentHashMap<PathBuf, ConfigStore>,
    ) -> (ServerLinter, Oxlintrc) {
        let root_path = root_uri.to_file_path().unwrap();
        let relative_config_path = options.config_path.clone();
        let oxlintrc = if relative_config_path.is_some() {
            let config = root_path.join(relative_config_path.unwrap());
            if config.try_exists().expect("Could not get fs metadata for config") {
                if let Ok(oxlintrc) = Oxlintrc::from_file(&config) {
                    oxlintrc
                } else {
                    error!("Failed to initialize oxlintrc config: {}", config.to_string_lossy());
                    Oxlintrc::default()
                }
            } else {
                error!(
                    "Config file not found: {}, fallback to default config",
                    config.to_string_lossy()
                );
                Oxlintrc::default()
            }
        } else {
            Oxlintrc::default()
        };

        // clone because we are returning it for ignore builder
        let config_builder =
            ConfigStoreBuilder::from_oxlintrc(false, oxlintrc.clone()).unwrap_or_default();

        // TODO(refactor): pull this into a shared function, because in oxlint we have the same functionality.
        let use_nested_config = options.use_nested_configs();

        let use_cross_module = if use_nested_config {
            nested_configs.pin().values().any(|config| config.plugins().has_import())
        } else {
            config_builder.plugins().has_import()
        };

        let config_store = config_builder.build().expect("Failed to build config store");

        let lint_options = LintOptions { fix: options.fix_kind(), ..Default::default() };

        let linter = if use_nested_config {
            let nested_configs = nested_configs.pin();
            let nested_configs_copy: FxHashMap<PathBuf, ConfigStore> = nested_configs
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<FxHashMap<_, _>>();

            Linter::new_with_nested_configs(lint_options, config_store, nested_configs_copy)
        } else {
            Linter::new(lint_options, config_store)
        };

        let server_linter = ServerLinter::new_with_linter(
            linter,
            IsolatedLintHandlerOptions { use_cross_module, root_path: root_path.to_path_buf() },
        );

        (server_linter, oxlintrc)
    }

    async fn init_ignore_glob(root_uri: Uri, oxlintrc: Oxlintrc) -> Vec<Gitignore> {
        let mut builder = globset::GlobSetBuilder::new();
        // Collecting all ignore files
        builder.add(Glob::new("**/.eslintignore").unwrap());
        builder.add(Glob::new("**/.gitignore").unwrap());

        let ignore_file_glob_set = builder.build().unwrap();

        let walk = ignore::WalkBuilder::new(root_uri.to_file_path().unwrap())
            .ignore(true)
            .hidden(false)
            .git_global(false)
            .build()
            .flatten();

        let mut gitignore_globs = vec![];
        for entry in walk {
            let ignore_file_path = entry.path();
            if !ignore_file_glob_set.is_match(ignore_file_path) {
                continue;
            }

            if let Some(ignore_file_dir) = ignore_file_path.parent() {
                let mut builder = ignore::gitignore::GitignoreBuilder::new(ignore_file_dir);
                builder.add(ignore_file_path);
                if let Ok(gitignore) = builder.build() {
                    gitignore_globs.push(gitignore);
                }
            }
        }

        if !oxlintrc.ignore_patterns.is_empty() {
            let mut builder =
                ignore::gitignore::GitignoreBuilder::new(oxlintrc.path.parent().unwrap());
            for entry in &oxlintrc.ignore_patterns {
                builder.add_line(None, entry).expect("Failed to add ignore line");
            }
            gitignore_globs.push(builder.build().unwrap());
        }

        gitignore_globs
    }

    fn needs_linter_restart(old_options: &Options, new_options: &Options) -> bool {
        old_options.config_path != new_options.config_path
            || old_options.use_nested_configs() != new_options.use_nested_configs()
            || old_options.fix_kind() != new_options.fix_kind()
    }

    pub async fn should_lint_on_run_type(&self, current_run: Run) -> bool {
        let run_level = { self.options.lock().await.run };

        run_level == current_run
    }

    pub async fn lint_file(
        &self,
        uri: &Uri,
        content: Option<String>,
    ) -> Option<Vec<DiagnosticReport>> {
        if self.is_ignored(&uri).await {
            return None;
        }

        Some(self.update_diagnostics(uri, content).await)
    }

    async fn update_diagnostics(
        &self,
        uri: &Uri,
        content: Option<String>,
    ) -> Vec<DiagnosticReport> {
        let diagnostics = self.server_linter.read().await.run_single(&uri, content);
        if let Some(diagnostics) = diagnostics {
            self.diagnostics_report_map.pin().insert(uri.to_string(), diagnostics.clone());

            return diagnostics;
        }

        vec![]
    }

    pub fn get_clear_diagnostics(&self) -> Vec<(String, Vec<Diagnostic>)> {
        self.diagnostics_report_map
            .pin()
            .keys()
            .map(|uri| (uri.clone(), vec![]))
            .collect::<Vec<_>>()
    }

    async fn is_ignored(&self, uri: &Uri) -> bool {
        let gitignore_globs = &(*self.gitignore_glob.lock().await);
        for gitignore in gitignore_globs {
            if let Some(uri_path) = uri.to_file_path() {
                if !uri_path.starts_with(gitignore.path()) {
                    continue;
                }
                if gitignore.matched_path_or_any_parents(&uri_path, uri_path.is_dir()).is_ignore() {
                    debug!("ignored: {uri:?}");
                    return true;
                }
            }
        }
        false
    }
}
