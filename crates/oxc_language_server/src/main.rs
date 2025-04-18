use code_actions::{
    CODE_ACTION_KIND_SOURCE_FIX_ALL_OXC, apply_all_fix_code_action, apply_fix_code_action,
    ignore_this_line_code_action, ignore_this_rule_code_action,
};
use commands::LSP_COMMANDS;
use futures::future::join_all;
use log::{debug, error, info};
use oxc_linter::{ConfigStoreBuilder, FixKind, Oxlintrc};
use rustc_hash::{FxBuildHasher, FxHashMap};
use serde::{Deserialize, Serialize};
use std::{fmt::Debug, str::FromStr};
use tokio::sync::Mutex;
use tower_lsp_server::{
    Client, LanguageServer, LspService, Server, UriExt,
    jsonrpc::{Error, Result},
    lsp_types::{
        CodeActionOrCommand, CodeActionParams, CodeActionResponse, ConfigurationItem, Diagnostic,
        DidChangeConfigurationParams, DidChangeTextDocumentParams, DidChangeWatchedFilesParams,
        DidCloseTextDocumentParams, DidOpenTextDocumentParams, DidSaveTextDocumentParams,
        ExecuteCommandParams, FileChangeType, InitializeParams, InitializeResult,
        InitializedParams, Range, ServerInfo, Uri,
    },
};
use worker::BackendWorker;

use crate::capabilities::Capabilities;

mod capabilities;
mod code_actions;
mod commands;
mod linter;
mod worker;

type ConcurrentHashMap<K, V> = papaya::HashMap<K, V, FxBuildHasher>;

const OXC_CONFIG_FILE: &str = ".oxlintrc.json";

struct Backend {
    client: Client,
    workspace_workers: Mutex<Vec<BackendWorker>>,
}

#[derive(Debug, Serialize, Deserialize, Default, PartialEq, PartialOrd, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub enum Run {
    OnSave,
    #[default]
    OnType,
}
#[derive(Debug, Default, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Options {
    run: Run,
    config_path: Option<String>,
    flags: FxHashMap<String, String>,
}

impl Options {
    fn use_nested_configs(&self) -> bool {
        !self.flags.contains_key("disable_nested_config") || self.config_path.is_some()
    }

    fn fix_kind(&self) -> FixKind {
        self.flags.get("fix_kind").map_or(FixKind::SafeFix, |kind| match kind.as_str() {
            "safe_fix" => FixKind::SafeFix,
            "safe_fix_or_suggestion" => FixKind::SafeFixOrSuggestion,
            "dangerous_fix" => FixKind::DangerousFix,
            "dangerous_fix_or_suggestion" => FixKind::DangerousFixOrSuggestion,
            "none" => FixKind::None,
            "all" => FixKind::All,
            _ => {
                info!("invalid fix_kind flag `{kind}`, fallback to `safe_fix`");
                FixKind::SafeFix
            }
        })
    }
}

impl LanguageServer for Backend {
    #[expect(deprecated)] // TODO: FIXME
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let options = params.initialization_options.and_then(|mut value| {
            let settings = value.get_mut("settings")?.take();
            serde_json::from_value::<Options>(settings).ok()
        });

        // ToDo: add support for multiple workspace folders
        // maybe fallback when the client does not support it
        let root_worker = BackendWorker::new(
            params.root_uri.clone().unwrap(),
            options.clone().unwrap_or_default(),
        )
        .await;

        *self.workspace_workers.lock().await = vec![root_worker];

        if let Some(value) = options {
            info!("initialize: {value:?}");
            info!("language server version: {:?}", env!("CARGO_PKG_VERSION"));
        }

        Ok(InitializeResult {
            server_info: Some(ServerInfo { name: "oxc".into(), version: None }),
            offset_encoding: None,
            capabilities: Capabilities::from(params.capabilities).into(),
        })
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        // ToDo: check which workers needs which changes with self.client.configuration
        // needs client capability
        let changed_options =
            if let Ok(options) = serde_json::from_value::<Options>(params.settings) {
                options
            } else {
                // Fallback if some client didn't took changed configuration in params of `workspace/configuration`
                let Some(options) = self
                    .client
                    .configuration(vec![ConfigurationItem {
                        scope_uri: None,
                        section: Some("oxc_language_server".into()),
                    }])
                    .await
                    .ok()
                    .and_then(|mut config| config.first_mut().map(serde_json::Value::take))
                    .and_then(|value| serde_json::from_value::<Options>(value).ok())
                else {
                    error!("Can't fetch `oxc_language_server` configuration");
                    return;
                };
                options
            };

        let current_option = &self.options.lock().await.clone();

        debug!(
            "
        configuration changed:
        incoming: {changed_options:?}
        current: {current_option:?}
        "
        );

        *self.options.lock().await = changed_options.clone();

        if changed_options.use_nested_configs() != current_option.use_nested_configs() {
            self.nested_configs.pin().clear();
            self.init_nested_configs().await;
        }

        if Self::needs_linter_restart(current_option, &changed_options) {
            self.init_linter_config().await;
            self.revalidate_open_files().await;
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // ToDo: check which workers needs which changes
        debug!("watched file did change");
        if self.options.lock().await.use_nested_configs() {
            let nested_configs = self.nested_configs.pin();

            params.changes.iter().for_each(|x| {
                let Some(file_path) = x.uri.to_file_path() else {
                    info!("Unable to convert {:?} to a file path", x.uri);
                    return;
                };
                let Some(file_name) = file_path.file_name() else {
                    info!("Unable to retrieve file name from {file_path:?}");
                    return;
                };

                if file_name != OXC_CONFIG_FILE {
                    return;
                }

                let Some(dir_path) = file_path.parent() else {
                    info!("Unable to retrieve parent from {file_path:?}");
                    return;
                };

                // spellchecker:off -- "typ" is accurate
                if x.typ == FileChangeType::CREATED || x.typ == FileChangeType::CHANGED {
                    // spellchecker:on
                    let oxlintrc =
                        Oxlintrc::from_file(&file_path).expect("Failed to parse config file");
                    let config_store_builder = ConfigStoreBuilder::from_oxlintrc(false, oxlintrc)
                        .expect("Failed to create config store builder");
                    let config_store =
                        config_store_builder.build().expect("Failed to build config store");
                    nested_configs.insert(dir_path.to_path_buf(), config_store);
                // spellchecker:off -- "typ" is accurate
                } else if x.typ == FileChangeType::DELETED {
                    // spellchecker:on
                    nested_configs.remove(&dir_path.to_path_buf());
                }
            });
        }

        self.init_linter_config().await;
        self.revalidate_open_files().await;
    }

    async fn initialized(&self, _params: InitializedParams) {
        debug!("oxc initialized.");
    }

    async fn shutdown(&self) -> Result<()> {
        self.clear_all_diagnostics().await;
        Ok(())
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        debug!("oxc server did save");
        let uri = &params.text_document.uri;
        let worker = self.get_responsible_worker(uri).await;
        if !worker.should_lint_on_run_type(Run::OnSave).await {
            return;
        }
        if let Some(diagnostics) = worker.lint_file(uri, None).await {
            self.client
                .publish_diagnostics(
                    uri.clone(),
                    diagnostics.clone().into_iter().map(|d| d.diagnostic).collect(),
                    None,
                )
                .await;
        }
    }

    /// When the document changed, it may not be written to disk, so we should
    /// get the file context from the language client
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = &params.text_document.uri;
        let worker = self.get_responsible_worker(uri).await;
        if !worker.should_lint_on_run_type(Run::OnType).await {
            return;
        }
        let content = params.content_changes.first().map(|c| c.text.clone());
        if let Some(diagnostics) = worker.lint_file(uri, content).await {
            self.client
                .publish_diagnostics(
                    uri.clone(),
                    diagnostics.clone().into_iter().map(|d| d.diagnostic).collect(),
                    Some(params.text_document.version),
                )
                .await;
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = &params.text_document.uri;
        let worker = self.get_responsible_worker(uri).await;
        let content = params.text_document.text;
        if let Some(diagnostics) = worker.lint_file(uri, Some(content)).await {
            self.client
                .publish_diagnostics(
                    uri.clone(),
                    diagnostics.clone().into_iter().map(|d| d.diagnostic).collect(),
                    Some(params.text_document.version),
                )
                .await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let worker = self.get_responsible_worker(&params.text_document.uri).await;
        worker.remove_diagnostics(&params.text_document.uri);
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let uri = &params.text_document.uri;
        let worker = self.get_responsible_worker(&params.text_document.uri).await;

        let report_map = self.diagnostics_report_map.pin();
        let Some(value) = report_map.get(&uri.to_string()) else {
            return Ok(None);
        };

        let reports = value.iter().filter(|r| {
            r.diagnostic.range == params.range || range_overlaps(params.range, r.diagnostic.range)
        });

        let is_source_fix_all_oxc = params
            .context
            .only
            .is_some_and(|only| only.contains(&CODE_ACTION_KIND_SOURCE_FIX_ALL_OXC));

        if is_source_fix_all_oxc {
            return Ok(apply_all_fix_code_action(reports, uri)
                .map(|code_actions| vec![CodeActionOrCommand::CodeAction(code_actions)]));
        }

        let mut code_actions_vec: Vec<CodeActionOrCommand> = vec![];

        for report in reports {
            if let Some(fix_action) = apply_fix_code_action(report, uri) {
                code_actions_vec.push(CodeActionOrCommand::CodeAction(fix_action));
            }

            code_actions_vec
                .push(CodeActionOrCommand::CodeAction(ignore_this_line_code_action(report, uri)));

            code_actions_vec
                .push(CodeActionOrCommand::CodeAction(ignore_this_rule_code_action(report, uri)));
        }

        if code_actions_vec.is_empty() {
            return Ok(None);
        }

        Ok(Some(code_actions_vec))
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<serde_json::Value>> {
        let command = LSP_COMMANDS.iter().find(|c| c.command_id() == params.command);
        match command {
            Some(c) => c.execute(self, params.arguments).await,
            None => Err(Error::invalid_request()),
        }
    }
}

impl Backend {
    #[inline(always)]
    pub async fn get_responsible_worker<'a>(&'a self, uri: &Uri) -> &'a BackendWorker {
        let workers = self.workspace_workers.lock().await;
        workers
            .iter()
            .find(|worker| worker.is_responsible_for_uri(uri))
            .expect("No responsible worker found")
    }

    // clears all diagnostics for workspace folders
    async fn clear_all_diagnostics(&self) {
        let mut cleared_diagnostics = vec![];
        for worker in self.workspace_workers.lock().await.iter() {
            cleared_diagnostics.extend(worker.get_clear_diagnostics());
        }
        self.publish_all_diagnostics(&cleared_diagnostics).await;
    }

    async fn publish_all_diagnostics(&self, result: &Vec<(String, Vec<Diagnostic>)>) {
        join_all(result.iter().map(|(path, diagnostics)| {
            self.client.publish_diagnostics(Uri::from_str(path).unwrap(), diagnostics.clone(), None)
        }))
        .await;
    }

    fn needs_linter_restart(old_options: &Options, new_options: &Options) -> bool {
        old_options.config_path != new_options.config_path
            || old_options.use_nested_configs() != new_options.use_nested_configs()
            || old_options.fix_kind() != new_options.fix_kind()
    }
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) =
        LspService::build(|client| Backend { client, workspace_workers: Mutex::new(vec![]) })
            .finish();

    Server::new(stdin, stdout, socket).serve(service).await;
}

fn range_overlaps(a: Range, b: Range) -> bool {
    a.start <= b.end && a.end >= b.start
}
