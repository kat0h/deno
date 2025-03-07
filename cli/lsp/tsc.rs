// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use super::analysis::CodeActionData;
use super::code_lens;
use super::config;
use super::documents::AssetOrDocument;
use super::documents::DocumentsFilter;
use super::language_server;
use super::language_server::StateSnapshot;
use super::performance::Performance;
use super::refactor::RefactorCodeActionData;
use super::refactor::ALL_KNOWN_REFACTOR_ACTION_KINDS;
use super::refactor::EXTRACT_CONSTANT;
use super::refactor::EXTRACT_INTERFACE;
use super::refactor::EXTRACT_TYPE;
use super::semantic_tokens;
use super::semantic_tokens::SemanticTokensBuilder;
use super::text::LineIndex;
use super::urls::LspClientUrl;
use super::urls::LspUrlMap;
use super::urls::INVALID_SPECIFIER;

use crate::args::FmtOptionsConfig;
use crate::args::TsConfig;
use crate::cache::HttpCache;
use crate::lsp::cache::CacheMetadata;
use crate::lsp::documents::Documents;
use crate::lsp::logging::lsp_warn;
use crate::tsc;
use crate::tsc::ResolveArgs;
use crate::util::path::relative_specifier;
use crate::util::path::specifier_to_file_path;

use dashmap::DashMap;
use deno_ast::MediaType;
use deno_core::anyhow::anyhow;
use deno_core::error::custom_error;
use deno_core::error::AnyError;
use deno_core::futures::FutureExt;
use deno_core::located_script_name;
use deno_core::op2;
use deno_core::parking_lot::Mutex;
use deno_core::resolve_url;
use deno_core::serde::de;
use deno_core::serde::Deserialize;
use deno_core::serde::Serialize;
use deno_core::serde_json;
use deno_core::serde_json::json;
use deno_core::serde_json::Value;
use deno_core::serde_v8;
use deno_core::v8;
use deno_core::JsRuntime;
use deno_core::ModuleSpecifier;
use deno_core::OpState;
use deno_core::PollEventLoopOptions;
use deno_core::RuntimeOptions;
use deno_runtime::inspector_server::InspectorServer;
use deno_runtime::tokio_util::create_basic_runtime;
use lazy_regex::lazy_regex;
use log::error;
use once_cell::sync::Lazy;
use regex::Captures;
use regex::Regex;
use serde_repr::Deserialize_repr;
use serde_repr::Serialize_repr;
use std::cmp;
use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::ops::Range;
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use text_size::TextRange;
use text_size::TextSize;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tower_lsp::jsonrpc::Error as LspError;
use tower_lsp::jsonrpc::Result as LspResult;
use tower_lsp::lsp_types as lsp;

static BRACKET_ACCESSOR_RE: Lazy<Regex> =
  lazy_regex!(r#"^\[['"](.+)[\['"]\]$"#);
static CAPTION_RE: Lazy<Regex> =
  lazy_regex!(r"<caption>(.*?)</caption>\s*\r?\n((?:\s|\S)*)");
static CODEBLOCK_RE: Lazy<Regex> = lazy_regex!(r"^\s*[~`]{3}");
static EMAIL_MATCH_RE: Lazy<Regex> = lazy_regex!(r"(.+)\s<([-.\w]+@[-.\w]+)>");
static HTTP_RE: Lazy<Regex> = lazy_regex!(r#"(?i)^https?:"#);
static JSDOC_LINKS_RE: Lazy<Regex> = lazy_regex!(
  r"(?i)\{@(link|linkplain|linkcode) (https?://[^ |}]+?)(?:[| ]([^{}\n]+?))?\}"
);
static PART_KIND_MODIFIER_RE: Lazy<Regex> = lazy_regex!(r",|\s+");
static PART_RE: Lazy<Regex> = lazy_regex!(r"^(\S+)\s*-?\s*");
static SCOPE_RE: Lazy<Regex> = lazy_regex!(r"scope_(\d)");

const FILE_EXTENSION_KIND_MODIFIERS: &[&str] =
  &[".d.ts", ".ts", ".tsx", ".js", ".jsx", ".json"];

type Request = (
  TscRequest,
  Arc<StateSnapshot>,
  oneshot::Sender<Result<Value, AnyError>>,
  CancellationToken,
);

#[derive(Debug, Clone, Copy, Serialize_repr)]
#[repr(u8)]
pub enum IndentStyle {
  #[allow(dead_code)]
  None = 0,
  Block = 1,
  #[allow(dead_code)]
  Smart = 2,
}

/// Relevant subset of https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6658.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormatCodeSettings {
  base_indent_size: Option<u8>,
  indent_size: Option<u8>,
  tab_size: Option<u8>,
  new_line_character: Option<String>,
  convert_tabs_to_spaces: Option<bool>,
  indent_style: Option<IndentStyle>,
  trim_trailing_whitespace: Option<bool>,
  insert_space_after_comma_delimiter: Option<bool>,
  insert_space_after_semicolon_in_for_statements: Option<bool>,
  insert_space_before_and_after_binary_operators: Option<bool>,
  insert_space_after_constructor: Option<bool>,
  insert_space_after_keywords_in_control_flow_statements: Option<bool>,
  insert_space_after_function_keyword_for_anonymous_functions: Option<bool>,
  insert_space_after_opening_and_before_closing_nonempty_parenthesis:
    Option<bool>,
  insert_space_after_opening_and_before_closing_nonempty_brackets: Option<bool>,
  insert_space_after_opening_and_before_closing_nonempty_braces: Option<bool>,
  insert_space_after_opening_and_before_closing_template_string_braces:
    Option<bool>,
  insert_space_after_opening_and_before_closing_jsx_expression_braces:
    Option<bool>,
  insert_space_after_type_assertion: Option<bool>,
  insert_space_before_function_parenthesis: Option<bool>,
  place_open_brace_on_new_line_for_functions: Option<bool>,
  place_open_brace_on_new_line_for_control_blocks: Option<bool>,
  insert_space_before_type_annotation: Option<bool>,
  indent_multi_line_object_literal_beginning_on_blank_line: Option<bool>,
  semicolons: Option<SemicolonPreference>,
  indent_switch_case: Option<bool>,
}

impl From<&FmtOptionsConfig> for FormatCodeSettings {
  fn from(config: &FmtOptionsConfig) -> Self {
    FormatCodeSettings {
      base_indent_size: Some(0),
      indent_size: Some(config.indent_width.unwrap_or(2)),
      tab_size: Some(config.indent_width.unwrap_or(2)),
      new_line_character: Some("\n".to_string()),
      convert_tabs_to_spaces: Some(!config.use_tabs.unwrap_or(false)),
      indent_style: Some(IndentStyle::Block),
      trim_trailing_whitespace: Some(false),
      insert_space_after_comma_delimiter: Some(true),
      insert_space_after_semicolon_in_for_statements: Some(true),
      insert_space_before_and_after_binary_operators: Some(true),
      insert_space_after_constructor: Some(false),
      insert_space_after_keywords_in_control_flow_statements: Some(true),
      insert_space_after_function_keyword_for_anonymous_functions: Some(true),
      insert_space_after_opening_and_before_closing_nonempty_parenthesis: Some(
        false,
      ),
      insert_space_after_opening_and_before_closing_nonempty_brackets: Some(
        false,
      ),
      insert_space_after_opening_and_before_closing_nonempty_braces: Some(true),
      insert_space_after_opening_and_before_closing_template_string_braces:
        Some(false),
      insert_space_after_opening_and_before_closing_jsx_expression_braces: Some(
        false,
      ),
      insert_space_after_type_assertion: Some(false),
      insert_space_before_function_parenthesis: Some(false),
      place_open_brace_on_new_line_for_functions: Some(false),
      place_open_brace_on_new_line_for_control_blocks: Some(false),
      insert_space_before_type_annotation: Some(false),
      indent_multi_line_object_literal_beginning_on_blank_line: Some(false),
      semicolons: match config.semi_colons {
        Some(false) => Some(SemicolonPreference::Remove),
        _ => Some(SemicolonPreference::Insert),
      },
      indent_switch_case: Some(true),
    }
  }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SemicolonPreference {
  Insert,
  Remove,
}

fn normalize_diagnostic(
  diagnostic: &mut crate::tsc::Diagnostic,
  specifier_map: &TscSpecifierMap,
) -> Result<(), AnyError> {
  if let Some(file_name) = &mut diagnostic.file_name {
    *file_name = specifier_map.normalize(&file_name)?.to_string();
  }
  for ri in diagnostic.related_information.iter_mut().flatten() {
    normalize_diagnostic(ri, specifier_map)?;
  }
  Ok(())
}

pub struct TsServer {
  performance: Arc<Performance>,
  cache: Arc<dyn HttpCache>,
  sender: mpsc::UnboundedSender<Request>,
  receiver: Mutex<Option<mpsc::UnboundedReceiver<Request>>>,
  specifier_map: Arc<TscSpecifierMap>,
  project_version: Arc<AtomicUsize>,
  inspector_server: Mutex<Option<Arc<InspectorServer>>>,
}

impl std::fmt::Debug for TsServer {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("TsServer")
      .field("performance", &self.performance)
      .field("cache", &self.cache)
      .field("sender", &self.sender)
      .field("receiver", &self.receiver)
      .field("specifier_map", &self.specifier_map)
      .field("project_version", &self.project_version)
      .field("inspector_server", &self.inspector_server.lock().is_some())
      .finish()
  }
}

impl TsServer {
  pub fn new(performance: Arc<Performance>, cache: Arc<dyn HttpCache>) -> Self {
    let (tx, request_rx) = mpsc::unbounded_channel::<Request>();
    Self {
      performance,
      cache,
      sender: tx,
      receiver: Mutex::new(Some(request_rx)),
      specifier_map: Arc::new(TscSpecifierMap::new()),
      project_version: Arc::new(AtomicUsize::new(1)),
      inspector_server: Mutex::new(None),
    }
  }

  pub fn start(&self, inspector_server_addr: Option<String>) {
    let maybe_inspector_server = inspector_server_addr.and_then(|addr| {
      let addr: SocketAddr = match addr.parse() {
        Ok(addr) => addr,
        Err(err) => {
          lsp_warn!("Invalid inspector server address \"{}\": {}", &addr, err);
          return None;
        }
      };
      Some(Arc::new(InspectorServer::new(addr, "deno-lsp-tsc")))
    });
    *self.inspector_server.lock() = maybe_inspector_server.clone();
    // TODO(bartlomieju): why is the join_handle ignored here? Should we store it
    // on the `TsServer` struct.
    let receiver = self.receiver.lock().take().unwrap();
    let performance = self.performance.clone();
    let cache = self.cache.clone();
    let specifier_map = self.specifier_map.clone();
    let project_version = self.project_version.clone();
    let _join_handle = thread::spawn(move || {
      run_tsc_thread(
        receiver,
        performance.clone(),
        cache.clone(),
        specifier_map.clone(),
        project_version,
        maybe_inspector_server,
      )
    });
  }

  pub async fn get_diagnostics(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifiers: Vec<ModuleSpecifier>,
    token: CancellationToken,
  ) -> Result<HashMap<String, Vec<crate::tsc::Diagnostic>>, AnyError> {
    let req = TscRequest {
      method: "$getDiagnostics",
      args: json!([specifiers
        .into_iter()
        .map(|s| self.specifier_map.denormalize(&s))
        .collect::<Vec<String>>(),]),
    };
    let raw_diagnostics = self.request_with_cancellation::<HashMap<String, Vec<crate::tsc::Diagnostic>>>(snapshot, req, token).await?;
    let mut diagnostics_map = HashMap::with_capacity(raw_diagnostics.len());
    for (mut specifier, mut diagnostics) in raw_diagnostics {
      specifier = self.specifier_map.normalize(&specifier)?.to_string();
      for diagnostic in &mut diagnostics {
        normalize_diagnostic(diagnostic, &self.specifier_map)?;
      }
      diagnostics_map.insert(specifier, diagnostics);
    }
    Ok(diagnostics_map)
  }

  pub async fn find_references(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    position: u32,
  ) -> Result<Option<Vec<ReferencedSymbol>>, LspError> {
    let req = TscRequest {
      method: "findReferences",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6230
      args: json!([self.specifier_map.denormalize(&specifier), position]),
    };
    self
      .request::<Option<Vec<ReferencedSymbol>>>(snapshot, req)
      .await
      .and_then(|mut symbols| {
        for symbol in symbols.iter_mut().flatten() {
          symbol.normalize(&self.specifier_map)?;
        }
        Ok(symbols)
      })
      .map_err(|err| {
        log::error!("Unable to get references from TypeScript: {}", err);
        LspError::internal_error()
      })
  }

  pub async fn get_navigation_tree(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
  ) -> Result<NavigationTree, AnyError> {
    let req = TscRequest {
      method: "getNavigationTree",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6235
      args: json!([self.specifier_map.denormalize(&specifier)]),
    };
    self.request(snapshot, req).await
  }

  pub async fn configure(
    &self,
    snapshot: Arc<StateSnapshot>,
    tsconfig: TsConfig,
  ) -> Result<bool, AnyError> {
    let req = TscRequest {
      method: "$configure",
      args: json!([tsconfig]),
    };
    self.request(snapshot, req).await
  }

  pub fn increment_project_version(&self) {
    self.project_version.fetch_add(1, Ordering::Relaxed);
  }

  pub async fn get_supported_code_fixes(
    &self,
    snapshot: Arc<StateSnapshot>,
  ) -> Result<Vec<String>, LspError> {
    let req = TscRequest {
      method: "$getSupportedCodeFixes",
      args: json!([]),
    };
    self.request(snapshot, req).await.map_err(|err| {
      log::error!("Unable to get fixable diagnostics: {}", err);
      LspError::internal_error()
    })
  }

  pub async fn get_quick_info(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    position: u32,
  ) -> Result<Option<QuickInfo>, LspError> {
    let req = TscRequest {
      method: "getQuickInfoAtPosition",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6214
      args: json!([self.specifier_map.denormalize(&specifier), position]),
    };
    self.request(snapshot, req).await.map_err(|err| {
      log::error!("Unable to get quick info: {}", err);
      LspError::internal_error()
    })
  }

  pub async fn get_code_fixes(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    range: Range<u32>,
    codes: Vec<String>,
    format_code_settings: FormatCodeSettings,
    preferences: UserPreferences,
  ) -> Vec<CodeFixAction> {
    let req = TscRequest {
      method: "getCodeFixesAtPosition",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6257
      args: json!([
        self.specifier_map.denormalize(&specifier),
        range.start,
        range.end,
        codes,
        format_code_settings,
        preferences,
      ]),
    };
    let result = self
      .request::<Vec<CodeFixAction>>(snapshot, req)
      .await
      .and_then(|mut actions| {
        for action in &mut actions {
          action.normalize(&self.specifier_map)?;
        }
        Ok(actions)
      });
    match result {
      Ok(items) => items,
      Err(err) => {
        // sometimes tsc reports errors when retrieving code actions
        // because they don't reflect the current state of the document
        // so we will log them to the output, but we won't send an error
        // message back to the client.
        log::error!("Error getting actions from TypeScript: {}", err);
        Vec::new()
      }
    }
  }

  pub async fn get_applicable_refactors(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    range: Range<u32>,
    preferences: Option<UserPreferences>,
    only: String,
  ) -> Result<Vec<ApplicableRefactorInfo>, LspError> {
    let req = TscRequest {
      method: "getApplicableRefactors",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6274
      args: json!([
        self.specifier_map.denormalize(&specifier),
        { "pos": range.start, "end": range.end },
        preferences.unwrap_or_default(),
        json!(null),
        only,
      ]),
    };
    self.request(snapshot, req).await.map_err(|err| {
      log::error!("Failed to request to tsserver {}", err);
      LspError::invalid_request()
    })
  }

  pub async fn get_combined_code_fix(
    &self,
    snapshot: Arc<StateSnapshot>,
    code_action_data: &CodeActionData,
    format_code_settings: FormatCodeSettings,
    preferences: UserPreferences,
  ) -> Result<CombinedCodeActions, LspError> {
    let req = TscRequest {
      method: "getCombinedCodeFix",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6258
      args: json!([
        {
          "type": "file",
          "fileName": self.specifier_map.denormalize(&code_action_data.specifier),
        },
        &code_action_data.fix_id,
        format_code_settings,
        preferences,
      ]),
    };
    self
      .request::<CombinedCodeActions>(snapshot, req)
      .await
      .and_then(|mut actions| {
        actions.normalize(&self.specifier_map)?;
        Ok(actions)
      })
      .map_err(|err| {
        log::error!("Unable to get combined fix from TypeScript: {}", err);
        LspError::internal_error()
      })
  }

  #[allow(clippy::too_many_arguments)]
  pub async fn get_edits_for_refactor(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    format_code_settings: FormatCodeSettings,
    range: Range<u32>,
    refactor_name: String,
    action_name: String,
    preferences: Option<UserPreferences>,
  ) -> Result<RefactorEditInfo, LspError> {
    let req = TscRequest {
      method: "getEditsForRefactor",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6275
      args: json!([
        self.specifier_map.denormalize(&specifier),
        format_code_settings,
        { "pos": range.start, "end": range.end },
        refactor_name,
        action_name,
        preferences,
      ]),
    };
    self
      .request::<RefactorEditInfo>(snapshot, req)
      .await
      .and_then(|mut info| {
        info.normalize(&self.specifier_map)?;
        Ok(info)
      })
      .map_err(|err| {
        log::error!("Failed to request to tsserver {}", err);
        LspError::invalid_request()
      })
  }

  pub async fn get_edits_for_file_rename(
    &self,
    snapshot: Arc<StateSnapshot>,
    old_specifier: ModuleSpecifier,
    new_specifier: ModuleSpecifier,
    format_code_settings: FormatCodeSettings,
    user_preferences: UserPreferences,
  ) -> Result<Vec<FileTextChanges>, LspError> {
    let req = TscRequest {
      method: "getEditsForFileRename",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6281
      args: json!([
        self.specifier_map.denormalize(&old_specifier),
        self.specifier_map.denormalize(&new_specifier),
        format_code_settings,
        user_preferences,
      ]),
    };
    self
      .request::<Vec<FileTextChanges>>(snapshot, req)
      .await
      .and_then(|mut changes| {
        for changes in &mut changes {
          changes.normalize(&self.specifier_map)?;
        }
        Ok(changes)
      })
      .map_err(|err| {
        log::error!("Failed to request to tsserver {}", err);
        LspError::invalid_request()
      })
  }

  pub async fn get_document_highlights(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    position: u32,
    files_to_search: Vec<ModuleSpecifier>,
  ) -> Result<Option<Vec<DocumentHighlights>>, LspError> {
    let req = TscRequest {
      method: "getDocumentHighlights",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6231
      args: json!([
        self.specifier_map.denormalize(&specifier),
        position,
        files_to_search
          .into_iter()
          .map(|s| self.specifier_map.denormalize(&s))
          .collect::<Vec<_>>(),
      ]),
    };
    self.request(snapshot, req).await.map_err(|err| {
      log::error!("Unable to get document highlights from TypeScript: {}", err);
      LspError::internal_error()
    })
  }

  pub async fn get_definition(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    position: u32,
  ) -> Result<Option<DefinitionInfoAndBoundSpan>, LspError> {
    let req = TscRequest {
      method: "getDefinitionAndBoundSpan",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6226
      args: json!([self.specifier_map.denormalize(&specifier), position]),
    };
    self
      .request::<Option<DefinitionInfoAndBoundSpan>>(snapshot, req)
      .await
      .and_then(|mut info| {
        if let Some(info) = &mut info {
          info.normalize(&self.specifier_map)?;
        }
        Ok(info)
      })
      .map_err(|err| {
        log::error!("Unable to get definition from TypeScript: {}", err);
        LspError::internal_error()
      })
  }

  pub async fn get_type_definition(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    position: u32,
  ) -> Result<Option<Vec<DefinitionInfo>>, LspError> {
    let req = TscRequest {
      method: "getTypeDefinitionAtPosition",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6227
      args: json!([self.specifier_map.denormalize(&specifier), position]),
    };
    self
      .request::<Option<Vec<DefinitionInfo>>>(snapshot, req)
      .await
      .and_then(|mut infos| {
        for info in infos.iter_mut().flatten() {
          info.normalize(&self.specifier_map)?;
        }
        Ok(infos)
      })
      .map_err(|err| {
        log::error!("Unable to get type definition from TypeScript: {}", err);
        LspError::internal_error()
      })
  }

  pub async fn get_completions(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    position: u32,
    options: GetCompletionsAtPositionOptions,
    format_code_settings: FormatCodeSettings,
  ) -> Option<CompletionInfo> {
    let req = TscRequest {
      method: "getCompletionsAtPosition",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6193
      args: json!([
        self.specifier_map.denormalize(&specifier),
        position,
        options,
        format_code_settings,
      ]),
    };
    match self.request(snapshot, req).await {
      Ok(maybe_info) => maybe_info,
      Err(err) => {
        log::error!("Unable to get completion info from TypeScript: {:#}", err);
        None
      }
    }
  }

  pub async fn get_completion_details(
    &self,
    snapshot: Arc<StateSnapshot>,
    args: GetCompletionDetailsArgs,
  ) -> Result<Option<CompletionEntryDetails>, AnyError> {
    let req = TscRequest {
      method: "getCompletionEntryDetails",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6205
      args: json!([
        self.specifier_map.denormalize(&args.specifier),
        args.position,
        args.name,
        args.format_code_settings.unwrap_or_default(),
        args.source,
        args.preferences,
        args.data,
      ]),
    };
    self
      .request::<Option<CompletionEntryDetails>>(snapshot, req)
      .await
      .and_then(|mut details| {
        if let Some(details) = &mut details {
          details.normalize(&self.specifier_map)?;
        }
        Ok(details)
      })
  }

  pub async fn get_implementations(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    position: u32,
  ) -> Result<Option<Vec<ImplementationLocation>>, LspError> {
    let req = TscRequest {
      method: "getImplementationAtPosition",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6228
      args: json!([self.specifier_map.denormalize(&specifier), position]),
    };
    self
      .request::<Option<Vec<ImplementationLocation>>>(snapshot, req)
      .await
      .and_then(|mut locations| {
        for location in locations.iter_mut().flatten() {
          location.normalize(&self.specifier_map)?;
        }
        Ok(locations)
      })
      .map_err(|err| {
        log::error!("Failed to request to tsserver {}", err);
        LspError::invalid_request()
      })
  }

  pub async fn get_outlining_spans(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
  ) -> Result<Vec<OutliningSpan>, LspError> {
    let req = TscRequest {
      method: "getOutliningSpans",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6240
      args: json!([self.specifier_map.denormalize(&specifier)]),
    };
    self.request(snapshot, req).await.map_err(|err| {
      log::error!("Failed to request to tsserver {}", err);
      LspError::invalid_request()
    })
  }

  pub async fn provide_call_hierarchy_incoming_calls(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    position: u32,
  ) -> Result<Vec<CallHierarchyIncomingCall>, LspError> {
    let req = TscRequest {
      method: "provideCallHierarchyIncomingCalls",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6237
      args: json!([self.specifier_map.denormalize(&specifier), position]),
    };
    self
      .request::<Vec<CallHierarchyIncomingCall>>(snapshot, req)
      .await
      .and_then(|mut calls| {
        for call in &mut calls {
          call.normalize(&self.specifier_map)?;
        }
        Ok(calls)
      })
      .map_err(|err| {
        log::error!("Failed to request to tsserver {}", err);
        LspError::invalid_request()
      })
  }

  pub async fn provide_call_hierarchy_outgoing_calls(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    position: u32,
  ) -> Result<Vec<CallHierarchyOutgoingCall>, LspError> {
    let req = TscRequest {
      method: "provideCallHierarchyOutgoingCalls",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6238
      args: json!([self.specifier_map.denormalize(&specifier), position]),
    };
    self
      .request::<Vec<CallHierarchyOutgoingCall>>(snapshot, req)
      .await
      .and_then(|mut calls| {
        for call in &mut calls {
          call.normalize(&self.specifier_map)?;
        }
        Ok(calls)
      })
      .map_err(|err| {
        log::error!("Failed to request to tsserver {}", err);
        LspError::invalid_request()
      })
  }

  pub async fn prepare_call_hierarchy(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    position: u32,
  ) -> Result<Option<OneOrMany<CallHierarchyItem>>, LspError> {
    let req = TscRequest {
      method: "prepareCallHierarchy",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6236
      args: json!([self.specifier_map.denormalize(&specifier), position]),
    };
    self
      .request::<Option<OneOrMany<CallHierarchyItem>>>(snapshot, req)
      .await
      .and_then(|mut items| {
        match &mut items {
          Some(OneOrMany::One(item)) => {
            item.normalize(&self.specifier_map)?;
          }
          Some(OneOrMany::Many(items)) => {
            for item in items {
              item.normalize(&self.specifier_map)?;
            }
          }
          None => {}
        }
        Ok(items)
      })
      .map_err(|err| {
        log::error!("Failed to request to tsserver {}", err);
        LspError::invalid_request()
      })
  }

  pub async fn find_rename_locations(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    position: u32,
  ) -> Result<Option<Vec<RenameLocation>>, LspError> {
    let req = TscRequest {
      method: "findRenameLocations",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6221
      args: json!([
        self.specifier_map.denormalize(&specifier),
        position,
        false,
        false,
        false,
      ]),
    };
    self
      .request::<Option<Vec<RenameLocation>>>(snapshot, req)
      .await
      .and_then(|mut locations| {
        for location in locations.iter_mut().flatten() {
          location.normalize(&self.specifier_map)?;
        }
        Ok(locations)
      })
      .map_err(|err| {
        log::error!("Failed to request to tsserver {}", err);
        LspError::invalid_request()
      })
  }

  pub async fn get_smart_selection_range(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    position: u32,
  ) -> Result<SelectionRange, LspError> {
    let req = TscRequest {
      method: "getSmartSelectionRange",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6224
      args: json!([self.specifier_map.denormalize(&specifier), position]),
    };
    self.request(snapshot, req).await.map_err(|err| {
      log::error!("Failed to request to tsserver {}", err);
      LspError::invalid_request()
    })
  }

  pub async fn get_encoded_semantic_classifications(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    range: Range<u32>,
  ) -> Result<Classifications, LspError> {
    let req = TscRequest {
      method: "getEncodedSemanticClassifications",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6183
      args: json!([
        self.specifier_map.denormalize(&specifier),
        TextSpan {
          start: range.start,
          length: range.end - range.start,
        },
        "2020",
      ]),
    };
    self.request(snapshot, req).await.map_err(|err| {
      log::error!("Failed to request to tsserver {}", err);
      LspError::invalid_request()
    })
  }

  pub async fn get_signature_help_items(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    position: u32,
    options: SignatureHelpItemsOptions,
  ) -> Result<Option<SignatureHelpItems>, LspError> {
    let req = TscRequest {
      method: "getSignatureHelpItems",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6217
      args: json!([
        self.specifier_map.denormalize(&specifier),
        position,
        options,
      ]),
    };
    self.request(snapshot, req).await.map_err(|err| {
      log::error!("Failed to request to tsserver: {}", err);
      LspError::invalid_request()
    })
  }

  pub async fn get_navigate_to_items(
    &self,
    snapshot: Arc<StateSnapshot>,
    args: GetNavigateToItemsArgs,
  ) -> Result<Vec<NavigateToItem>, LspError> {
    let req = TscRequest {
      method: "getNavigateToItems",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6233
      args: json!([
        args.search,
        args.max_result_count,
        args.file.map(|f| match resolve_url(&f) {
          Ok(s) => self.specifier_map.denormalize(&s),
          Err(_) => f,
        }),
      ]),
    };
    self
      .request::<Vec<NavigateToItem>>(snapshot, req)
      .await
      .and_then(|mut items| {
        for items in &mut items {
          items.normalize(&self.specifier_map)?;
        }
        Ok(items)
      })
      .map_err(|err| {
        log::error!("Failed request to tsserver: {}", err);
        LspError::invalid_request()
      })
  }

  pub async fn provide_inlay_hints(
    &self,
    snapshot: Arc<StateSnapshot>,
    specifier: ModuleSpecifier,
    text_span: TextSpan,
    user_preferences: UserPreferences,
  ) -> Result<Option<Vec<InlayHint>>, LspError> {
    let req = TscRequest {
      method: "provideInlayHints",
      // https://github.com/denoland/deno/blob/v1.37.1/cli/tsc/dts/typescript.d.ts#L6239
      args: json!([
        self.specifier_map.denormalize(&specifier),
        text_span,
        user_preferences,
      ]),
    };
    self.request(snapshot, req).await.map_err(|err| {
      log::error!("Unable to get inlay hints: {}", err);
      LspError::internal_error()
    })
  }

  pub async fn restart(&self, snapshot: Arc<StateSnapshot>) {
    let req = TscRequest {
      method: "$restart",
      args: json!([]),
    };
    self.request::<bool>(snapshot, req).await.unwrap();
  }

  async fn request<R>(
    &self,
    snapshot: Arc<StateSnapshot>,
    req: TscRequest,
  ) -> Result<R, AnyError>
  where
    R: de::DeserializeOwned,
  {
    let mark = self.performance.mark(format!("tsc.request.{}", req.method));
    let r = self
      .request_with_cancellation(snapshot, req, Default::default())
      .await;
    self.performance.measure(mark);
    r
  }

  async fn request_with_cancellation<R>(
    &self,
    snapshot: Arc<StateSnapshot>,
    req: TscRequest,
    token: CancellationToken,
  ) -> Result<R, AnyError>
  where
    R: de::DeserializeOwned,
  {
    // When an LSP request is cancelled by the client, the future this is being
    // executed under and any local variables here will be dropped at the next
    // await point. To pass on that cancellation to the TS thread, we make this
    // wrapper which cancels the request's token on drop.
    struct DroppableToken(CancellationToken);
    impl Drop for DroppableToken {
      fn drop(&mut self) {
        self.0.cancel();
      }
    }
    let token = token.child_token();
    let droppable_token = DroppableToken(token.clone());
    let (tx, rx) = oneshot::channel::<Result<Value, AnyError>>();
    if self.sender.send((req, snapshot, tx, token)).is_err() {
      return Err(anyhow!("failed to send request to tsc thread"));
    }
    let value = rx.await??;
    drop(droppable_token);
    Ok(serde_json::from_value::<R>(value)?)
  }
}

#[derive(Debug, Clone)]
struct AssetDocumentInner {
  specifier: ModuleSpecifier,
  text: Arc<str>,
  line_index: Arc<LineIndex>,
  maybe_navigation_tree: Option<Arc<NavigationTree>>,
}

/// An lsp representation of an asset in memory, that has either been retrieved
/// from static assets built into Rust, or static assets built into tsc.
#[derive(Debug, Clone)]
pub struct AssetDocument(Arc<AssetDocumentInner>);

impl AssetDocument {
  pub fn new(specifier: ModuleSpecifier, text: impl AsRef<str>) -> Self {
    let text = text.as_ref();
    Self(Arc::new(AssetDocumentInner {
      specifier,
      text: text.into(),
      line_index: Arc::new(LineIndex::new(text)),
      maybe_navigation_tree: None,
    }))
  }

  pub fn specifier(&self) -> &ModuleSpecifier {
    &self.0.specifier
  }

  pub fn with_navigation_tree(
    &self,
    tree: Arc<NavigationTree>,
  ) -> AssetDocument {
    AssetDocument(Arc::new(AssetDocumentInner {
      maybe_navigation_tree: Some(tree),
      ..(*self.0).clone()
    }))
  }

  pub fn text(&self) -> Arc<str> {
    self.0.text.clone()
  }

  pub fn line_index(&self) -> Arc<LineIndex> {
    self.0.line_index.clone()
  }

  pub fn maybe_navigation_tree(&self) -> Option<Arc<NavigationTree>> {
    self.0.maybe_navigation_tree.clone()
  }
}

type AssetsMap = HashMap<ModuleSpecifier, AssetDocument>;

fn new_assets_map() -> Arc<Mutex<AssetsMap>> {
  let assets = tsc::LAZILY_LOADED_STATIC_ASSETS
    .iter()
    .map(|(k, v)| {
      let url_str = format!("asset:///{k}");
      let specifier = resolve_url(&url_str).unwrap();
      let asset = AssetDocument::new(specifier.clone(), v);
      (specifier, asset)
    })
    .collect::<AssetsMap>();
  Arc::new(Mutex::new(assets))
}

/// Snapshot of Assets.
#[derive(Debug, Clone)]
pub struct AssetsSnapshot(Arc<Mutex<AssetsMap>>);

impl Default for AssetsSnapshot {
  fn default() -> Self {
    Self(new_assets_map())
  }
}

impl AssetsSnapshot {
  pub fn contains_key(&self, k: &ModuleSpecifier) -> bool {
    self.0.lock().contains_key(k)
  }

  pub fn get(&self, k: &ModuleSpecifier) -> Option<AssetDocument> {
    self.0.lock().get(k).cloned()
  }
}

/// Assets are never updated and so we can safely use this struct across
/// multiple threads without needing to worry about race conditions.
#[derive(Debug, Clone)]
pub struct Assets {
  ts_server: Arc<TsServer>,
  assets: Arc<Mutex<AssetsMap>>,
}

impl Assets {
  pub fn new(ts_server: Arc<TsServer>) -> Self {
    Self {
      ts_server,
      assets: new_assets_map(),
    }
  }

  /// Initializes with the assets in the isolate.
  pub async fn initialize(&self, state_snapshot: Arc<StateSnapshot>) {
    let assets = get_isolate_assets(&self.ts_server, state_snapshot).await;
    let mut assets_map = self.assets.lock();
    for asset in assets {
      if !assets_map.contains_key(asset.specifier()) {
        assets_map.insert(asset.specifier().clone(), asset);
      }
    }
  }

  pub fn snapshot(&self) -> AssetsSnapshot {
    // it's ok to not make a complete copy for snapshotting purposes
    // because assets are static
    AssetsSnapshot(self.assets.clone())
  }

  pub fn get(&self, specifier: &ModuleSpecifier) -> Option<AssetDocument> {
    self.assets.lock().get(specifier).cloned()
  }

  pub fn cache_navigation_tree(
    &self,
    specifier: &ModuleSpecifier,
    navigation_tree: Arc<NavigationTree>,
  ) -> Result<(), AnyError> {
    let mut assets = self.assets.lock();
    let doc = assets
      .get_mut(specifier)
      .ok_or_else(|| anyhow!("Missing asset."))?;
    *doc = doc.with_navigation_tree(navigation_tree);
    Ok(())
  }
}

/// Get all the assets stored in the tsc isolate.
async fn get_isolate_assets(
  ts_server: &TsServer,
  state_snapshot: Arc<StateSnapshot>,
) -> Vec<AssetDocument> {
  let req = TscRequest {
    method: "$getAssets",
    args: json!([]),
  };
  let res: Value = ts_server.request(state_snapshot, req).await.unwrap();
  let response_assets = match res {
    Value::Array(value) => value,
    _ => unreachable!(),
  };
  let mut assets = Vec::with_capacity(response_assets.len());

  for asset in response_assets {
    let mut obj = match asset {
      Value::Object(obj) => obj,
      _ => unreachable!(),
    };
    let specifier_str = obj.get("specifier").unwrap().as_str().unwrap();
    let specifier = ModuleSpecifier::parse(specifier_str).unwrap();
    let text = match obj.remove("text").unwrap() {
      Value::String(text) => text,
      _ => unreachable!(),
    };
    assets.push(AssetDocument::new(specifier, text));
  }

  assets
}

fn get_tag_body_text(
  tag: &JsDocTagInfo,
  language_server: &language_server::Inner,
) -> Option<String> {
  tag.text.as_ref().map(|display_parts| {
    // TODO(@kitsonk) check logic in vscode about handling this API change in
    // tsserver
    let text = display_parts_to_string(display_parts, language_server);
    match tag.name.as_str() {
      "example" => {
        if CAPTION_RE.is_match(&text) {
          CAPTION_RE
            .replace(&text, |c: &Captures| {
              format!("{}\n\n{}", &c[1], make_codeblock(&c[2]))
            })
            .to_string()
        } else {
          make_codeblock(&text)
        }
      }
      "author" => EMAIL_MATCH_RE
        .replace(&text, |c: &Captures| format!("{} {}", &c[1], &c[2]))
        .to_string(),
      "default" => make_codeblock(&text),
      _ => replace_links(&text),
    }
  })
}

fn get_tag_documentation(
  tag: &JsDocTagInfo,
  language_server: &language_server::Inner,
) -> String {
  match tag.name.as_str() {
    "augments" | "extends" | "param" | "template" => {
      if let Some(display_parts) = &tag.text {
        // TODO(@kitsonk) check logic in vscode about handling this API change
        // in tsserver
        let text = display_parts_to_string(display_parts, language_server);
        let body: Vec<&str> = PART_RE.split(&text).collect();
        if body.len() == 3 {
          let param = body[1];
          let doc = body[2];
          let label = format!("*@{}* `{}`", tag.name, param);
          if doc.is_empty() {
            return label;
          }
          if doc.contains('\n') {
            return format!("{}  \n{}", label, replace_links(doc));
          } else {
            return format!("{} - {}", label, replace_links(doc));
          }
        }
      }
    }
    _ => (),
  }
  let label = format!("*@{}*", tag.name);
  let maybe_text = get_tag_body_text(tag, language_server);
  if let Some(text) = maybe_text {
    if text.contains('\n') {
      format!("{label}  \n{text}")
    } else {
      format!("{label} - {text}")
    }
  } else {
    label
  }
}

fn make_codeblock(text: &str) -> String {
  if CODEBLOCK_RE.is_match(text) {
    text.to_string()
  } else {
    format!("```\n{text}\n```")
  }
}

/// Replace JSDoc like links (`{@link http://example.com}`) with markdown links
fn replace_links<S: AsRef<str>>(text: S) -> String {
  JSDOC_LINKS_RE
    .replace_all(text.as_ref(), |c: &Captures| match &c[1] {
      "linkcode" => format!(
        "[`{}`]({})",
        if c.get(3).is_none() {
          &c[2]
        } else {
          c[3].trim()
        },
        &c[2]
      ),
      _ => format!(
        "[{}]({})",
        if c.get(3).is_none() {
          &c[2]
        } else {
          c[3].trim()
        },
        &c[2]
      ),
    })
    .to_string()
}

fn parse_kind_modifier(kind_modifiers: &str) -> HashSet<&str> {
  PART_KIND_MODIFIER_RE.split(kind_modifiers).collect()
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
  One(T),
  Many(Vec<T>),
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub enum ScriptElementKind {
  #[serde(rename = "")]
  Unknown,
  #[serde(rename = "warning")]
  Warning,
  #[serde(rename = "keyword")]
  Keyword,
  #[serde(rename = "script")]
  ScriptElement,
  #[serde(rename = "module")]
  ModuleElement,
  #[serde(rename = "class")]
  ClassElement,
  #[serde(rename = "local class")]
  LocalClassElement,
  #[serde(rename = "interface")]
  InterfaceElement,
  #[serde(rename = "type")]
  TypeElement,
  #[serde(rename = "enum")]
  EnumElement,
  #[serde(rename = "enum member")]
  EnumMemberElement,
  #[serde(rename = "var")]
  VariableElement,
  #[serde(rename = "local var")]
  LocalVariableElement,
  #[serde(rename = "function")]
  FunctionElement,
  #[serde(rename = "local function")]
  LocalFunctionElement,
  #[serde(rename = "method")]
  MemberFunctionElement,
  #[serde(rename = "getter")]
  MemberGetAccessorElement,
  #[serde(rename = "setter")]
  MemberSetAccessorElement,
  #[serde(rename = "property")]
  MemberVariableElement,
  #[serde(rename = "constructor")]
  ConstructorImplementationElement,
  #[serde(rename = "call")]
  CallSignatureElement,
  #[serde(rename = "index")]
  IndexSignatureElement,
  #[serde(rename = "construct")]
  ConstructSignatureElement,
  #[serde(rename = "parameter")]
  ParameterElement,
  #[serde(rename = "type parameter")]
  TypeParameterElement,
  #[serde(rename = "primitive type")]
  PrimitiveType,
  #[serde(rename = "label")]
  Label,
  #[serde(rename = "alias")]
  Alias,
  #[serde(rename = "const")]
  ConstElement,
  #[serde(rename = "let")]
  LetElement,
  #[serde(rename = "directory")]
  Directory,
  #[serde(rename = "external module name")]
  ExternalModuleName,
  #[serde(rename = "JSX attribute")]
  JsxAttribute,
  #[serde(rename = "string")]
  String,
  #[serde(rename = "link")]
  Link,
  #[serde(rename = "link name")]
  LinkName,
  #[serde(rename = "link text")]
  LinkText,
}

impl Default for ScriptElementKind {
  fn default() -> Self {
    Self::Unknown
  }
}

/// This mirrors the method `convertKind` in `completions.ts` in vscode
impl From<ScriptElementKind> for lsp::CompletionItemKind {
  fn from(kind: ScriptElementKind) -> Self {
    match kind {
      ScriptElementKind::PrimitiveType | ScriptElementKind::Keyword => {
        lsp::CompletionItemKind::KEYWORD
      }
      ScriptElementKind::ConstElement
      | ScriptElementKind::LetElement
      | ScriptElementKind::VariableElement
      | ScriptElementKind::LocalVariableElement
      | ScriptElementKind::Alias
      | ScriptElementKind::ParameterElement => {
        lsp::CompletionItemKind::VARIABLE
      }
      ScriptElementKind::MemberVariableElement
      | ScriptElementKind::MemberGetAccessorElement
      | ScriptElementKind::MemberSetAccessorElement => {
        lsp::CompletionItemKind::FIELD
      }
      ScriptElementKind::FunctionElement
      | ScriptElementKind::LocalFunctionElement => {
        lsp::CompletionItemKind::FUNCTION
      }
      ScriptElementKind::MemberFunctionElement
      | ScriptElementKind::ConstructSignatureElement
      | ScriptElementKind::CallSignatureElement
      | ScriptElementKind::IndexSignatureElement => {
        lsp::CompletionItemKind::METHOD
      }
      ScriptElementKind::EnumElement => lsp::CompletionItemKind::ENUM,
      ScriptElementKind::EnumMemberElement => {
        lsp::CompletionItemKind::ENUM_MEMBER
      }
      ScriptElementKind::ModuleElement
      | ScriptElementKind::ExternalModuleName => {
        lsp::CompletionItemKind::MODULE
      }
      ScriptElementKind::ClassElement | ScriptElementKind::TypeElement => {
        lsp::CompletionItemKind::CLASS
      }
      ScriptElementKind::InterfaceElement => lsp::CompletionItemKind::INTERFACE,
      ScriptElementKind::Warning => lsp::CompletionItemKind::TEXT,
      ScriptElementKind::ScriptElement => lsp::CompletionItemKind::FILE,
      ScriptElementKind::Directory => lsp::CompletionItemKind::FOLDER,
      ScriptElementKind::String => lsp::CompletionItemKind::CONSTANT,
      _ => lsp::CompletionItemKind::PROPERTY,
    }
  }
}

/// This mirrors `fromProtocolScriptElementKind` in vscode
impl From<ScriptElementKind> for lsp::SymbolKind {
  fn from(kind: ScriptElementKind) -> Self {
    match kind {
      ScriptElementKind::ModuleElement => Self::MODULE,
      // this is only present in `getSymbolKind` in `workspaceSymbols` in
      // vscode, but seems strange it isn't consistent.
      ScriptElementKind::TypeElement => Self::CLASS,
      ScriptElementKind::ClassElement => Self::CLASS,
      ScriptElementKind::EnumElement => Self::ENUM,
      ScriptElementKind::EnumMemberElement => Self::ENUM_MEMBER,
      ScriptElementKind::InterfaceElement => Self::INTERFACE,
      ScriptElementKind::IndexSignatureElement => Self::METHOD,
      ScriptElementKind::CallSignatureElement => Self::METHOD,
      ScriptElementKind::MemberFunctionElement => Self::METHOD,
      // workspaceSymbols in vscode treats them as fields, which does seem more
      // semantically correct while `fromProtocolScriptElementKind` treats them
      // as properties.
      ScriptElementKind::MemberVariableElement => Self::FIELD,
      ScriptElementKind::MemberGetAccessorElement => Self::FIELD,
      ScriptElementKind::MemberSetAccessorElement => Self::FIELD,
      ScriptElementKind::VariableElement => Self::VARIABLE,
      ScriptElementKind::LetElement => Self::VARIABLE,
      ScriptElementKind::ConstElement => Self::VARIABLE,
      ScriptElementKind::LocalVariableElement => Self::VARIABLE,
      ScriptElementKind::Alias => Self::VARIABLE,
      ScriptElementKind::FunctionElement => Self::FUNCTION,
      ScriptElementKind::LocalFunctionElement => Self::FUNCTION,
      ScriptElementKind::ConstructSignatureElement => Self::CONSTRUCTOR,
      ScriptElementKind::ConstructorImplementationElement => Self::CONSTRUCTOR,
      ScriptElementKind::TypeParameterElement => Self::TYPE_PARAMETER,
      ScriptElementKind::String => Self::STRING,
      _ => Self::VARIABLE,
    }
  }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TextSpan {
  pub start: u32,
  pub length: u32,
}

impl TextSpan {
  pub fn from_range(
    range: &lsp::Range,
    line_index: Arc<LineIndex>,
  ) -> Result<Self, AnyError> {
    let start = line_index.offset_tsc(range.start)?;
    let length = line_index.offset_tsc(range.end)? - start;
    Ok(Self { start, length })
  }

  pub fn to_range(&self, line_index: Arc<LineIndex>) -> lsp::Range {
    lsp::Range {
      start: line_index.position_tsc(self.start.into()),
      end: line_index.position_tsc(TextSize::from(self.start + self.length)),
    }
  }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SymbolDisplayPart {
  text: String,
  kind: String,
  // This is only on `JSDocLinkDisplayPart` which extends `SymbolDisplayPart`
  // but is only used as an upcast of a `SymbolDisplayPart` and not explicitly
  // returned by any API, so it is safe to add it as an optional value.
  #[serde(skip_serializing_if = "Option::is_none")]
  target: Option<DocumentSpan>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JsDocTagInfo {
  name: String,
  text: Option<Vec<SymbolDisplayPart>>,
}

// Note: the tsc protocol contains fields that are part of the protocol but
// not currently used.  They are commented out in the structures so it is clear
// that they exist.

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuickInfo {
  // kind: ScriptElementKind,
  // kind_modifiers: String,
  text_span: TextSpan,
  display_parts: Option<Vec<SymbolDisplayPart>>,
  documentation: Option<Vec<SymbolDisplayPart>>,
  tags: Option<Vec<JsDocTagInfo>>,
}

#[derive(Default)]
struct Link {
  name: Option<String>,
  target: Option<DocumentSpan>,
  text: Option<String>,
  linkcode: bool,
}

/// Takes `SymbolDisplayPart` items and converts them into a string, handling
/// any `{@link Symbol}` and `{@linkcode Symbol}` JSDoc tags and linking them
/// to the their source location.
fn display_parts_to_string(
  parts: &[SymbolDisplayPart],
  language_server: &language_server::Inner,
) -> String {
  let mut out = Vec::<String>::new();

  let mut current_link: Option<Link> = None;
  for part in parts {
    match part.kind.as_str() {
      "link" => {
        if let Some(link) = current_link.as_mut() {
          if let Some(target) = &link.target {
            if let Some(specifier) = target.to_target(language_server) {
              let link_text = link.text.clone().unwrap_or_else(|| {
                link
                  .name
                  .clone()
                  .map(|ref n| n.replace('`', "\\`"))
                  .unwrap_or_else(|| "".to_string())
              });
              let link_str = if link.linkcode {
                format!("[`{link_text}`]({specifier})")
              } else {
                format!("[{link_text}]({specifier})")
              };
              out.push(link_str);
            }
          } else {
            let maybe_text = link.text.clone().or_else(|| link.name.clone());
            if let Some(text) = maybe_text {
              if HTTP_RE.is_match(&text) {
                let parts: Vec<&str> = text.split(' ').collect();
                if parts.len() == 1 {
                  out.push(parts[0].to_string());
                } else {
                  let link_text = parts[1..].join(" ").replace('`', "\\`");
                  let link_str = if link.linkcode {
                    format!("[`{}`]({})", link_text, parts[0])
                  } else {
                    format!("[{}]({})", link_text, parts[0])
                  };
                  out.push(link_str);
                }
              } else {
                out.push(text.replace('`', "\\`"));
              }
            }
          }
          current_link = None;
        } else {
          current_link = Some(Link {
            linkcode: part.text.as_str() == "{@linkcode ",
            ..Default::default()
          });
        }
      }
      "linkName" => {
        if let Some(link) = current_link.as_mut() {
          link.name = Some(part.text.clone());
          link.target = part.target.clone();
        }
      }
      "linkText" => {
        if let Some(link) = current_link.as_mut() {
          link.name = Some(part.text.clone());
        }
      }
      _ => out.push(part.text.clone()),
    }
  }

  replace_links(out.join(""))
}

impl QuickInfo {
  pub fn to_hover(
    &self,
    line_index: Arc<LineIndex>,
    language_server: &language_server::Inner,
  ) -> lsp::Hover {
    let mut parts = Vec::<lsp::MarkedString>::new();
    if let Some(display_string) = self
      .display_parts
      .clone()
      .map(|p| display_parts_to_string(&p, language_server))
    {
      parts.push(lsp::MarkedString::from_language_code(
        "typescript".to_string(),
        display_string,
      ));
    }
    if let Some(documentation) = self
      .documentation
      .clone()
      .map(|p| display_parts_to_string(&p, language_server))
    {
      parts.push(lsp::MarkedString::from_markdown(documentation));
    }
    if let Some(tags) = &self.tags {
      let tags_preview = tags
        .iter()
        .map(|tag_info| get_tag_documentation(tag_info, language_server))
        .collect::<Vec<String>>()
        .join("  \n\n");
      if !tags_preview.is_empty() {
        parts.push(lsp::MarkedString::from_markdown(format!(
          "\n\n{tags_preview}"
        )));
      }
    }
    lsp::Hover {
      contents: lsp::HoverContents::Array(parts),
      range: Some(self.text_span.to_range(line_index)),
    }
  }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSpan {
  text_span: TextSpan,
  pub file_name: String,
  original_text_span: Option<TextSpan>,
  // original_file_name: Option<String>,
  context_span: Option<TextSpan>,
  original_context_span: Option<TextSpan>,
}

impl DocumentSpan {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    self.file_name = specifier_map.normalize(&self.file_name)?.to_string();
    Ok(())
  }
}

impl DocumentSpan {
  pub fn to_link(
    &self,
    line_index: Arc<LineIndex>,
    language_server: &language_server::Inner,
  ) -> Option<lsp::LocationLink> {
    let target_specifier = resolve_url(&self.file_name).ok()?;
    let target_asset_or_doc =
      language_server.get_maybe_asset_or_document(&target_specifier)?;
    let target_line_index = target_asset_or_doc.line_index();
    let target_uri = language_server
      .url_map
      .normalize_specifier(&target_specifier)
      .ok()?;
    let (target_range, target_selection_range) =
      if let Some(context_span) = &self.context_span {
        (
          context_span.to_range(target_line_index.clone()),
          self.text_span.to_range(target_line_index),
        )
      } else {
        (
          self.text_span.to_range(target_line_index.clone()),
          self.text_span.to_range(target_line_index),
        )
      };
    let origin_selection_range =
      if let Some(original_context_span) = &self.original_context_span {
        Some(original_context_span.to_range(line_index))
      } else {
        self
          .original_text_span
          .as_ref()
          .map(|original_text_span| original_text_span.to_range(line_index))
      };
    let link = lsp::LocationLink {
      origin_selection_range,
      target_uri: target_uri.into_url(),
      target_range,
      target_selection_range,
    };
    Some(link)
  }

  /// Convert the `DocumentSpan` into a specifier that can be sent to the client
  /// to link to the target document span. Used for converting JSDoc symbol
  /// links to markdown links.
  fn to_target(
    &self,
    language_server: &language_server::Inner,
  ) -> Option<ModuleSpecifier> {
    let specifier = resolve_url(&self.file_name).ok()?;
    let asset_or_doc =
      language_server.get_maybe_asset_or_document(&specifier)?;
    let line_index = asset_or_doc.line_index();
    let range = self.text_span.to_range(line_index);
    let mut target = language_server
      .url_map
      .normalize_specifier(&specifier)
      .ok()?
      .into_url();
    target.set_fragment(Some(&format!(
      "L{},{}",
      range.start.line + 1,
      range.start.character + 1
    )));

    Some(target)
  }
}

#[derive(Debug, Clone, Deserialize)]
pub enum MatchKind {
  #[serde(rename = "exact")]
  Exact,
  #[serde(rename = "prefix")]
  Prefix,
  #[serde(rename = "substring")]
  Substring,
  #[serde(rename = "camelCase")]
  CamelCase,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigateToItem {
  name: String,
  kind: ScriptElementKind,
  kind_modifiers: String,
  // match_kind: MatchKind,
  // is_case_sensitive: bool,
  file_name: String,
  text_span: TextSpan,
  container_name: Option<String>,
  // container_kind: ScriptElementKind,
}

impl NavigateToItem {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    self.file_name = specifier_map.normalize(&self.file_name)?.to_string();
    Ok(())
  }
}

impl NavigateToItem {
  pub fn to_symbol_information(
    &self,
    language_server: &language_server::Inner,
  ) -> Option<lsp::SymbolInformation> {
    let specifier = resolve_url(&self.file_name).ok()?;
    let asset_or_doc =
      language_server.get_asset_or_document(&specifier).ok()?;
    let line_index = asset_or_doc.line_index();
    let uri = language_server
      .url_map
      .normalize_specifier(&specifier)
      .ok()?;
    let range = self.text_span.to_range(line_index);
    let location = lsp::Location {
      uri: uri.into_url(),
      range,
    };

    let mut tags: Option<Vec<lsp::SymbolTag>> = None;
    let kind_modifiers = parse_kind_modifier(&self.kind_modifiers);
    if kind_modifiers.contains("deprecated") {
      tags = Some(vec![lsp::SymbolTag::DEPRECATED]);
    }

    // The field `deprecated` is deprecated but SymbolInformation does not have
    // a default, therefore we have to supply the deprecated deprecated
    // field. It is like a bad version of Inception.
    #[allow(deprecated)]
    Some(lsp::SymbolInformation {
      name: self.name.clone(),
      kind: self.kind.clone().into(),
      tags,
      deprecated: None,
      location,
      container_name: self.container_name.clone(),
    })
  }
}

#[derive(Debug, Clone, Deserialize)]
pub enum InlayHintKind {
  Type,
  Parameter,
  Enum,
}

impl InlayHintKind {
  pub fn to_lsp(&self) -> Option<lsp::InlayHintKind> {
    match self {
      Self::Enum => None,
      Self::Parameter => Some(lsp::InlayHintKind::PARAMETER),
      Self::Type => Some(lsp::InlayHintKind::TYPE),
    }
  }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlayHint {
  pub text: String,
  pub position: u32,
  pub kind: InlayHintKind,
  pub whitespace_before: Option<bool>,
  pub whitespace_after: Option<bool>,
}

impl InlayHint {
  pub fn to_lsp(&self, line_index: Arc<LineIndex>) -> lsp::InlayHint {
    lsp::InlayHint {
      position: line_index.position_tsc(self.position.into()),
      label: lsp::InlayHintLabel::String(self.text.clone()),
      kind: self.kind.to_lsp(),
      padding_left: self.whitespace_before,
      padding_right: self.whitespace_after,
      text_edits: None,
      tooltip: None,
      data: None,
    }
  }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigationTree {
  pub text: String,
  pub kind: ScriptElementKind,
  pub kind_modifiers: String,
  pub spans: Vec<TextSpan>,
  pub name_span: Option<TextSpan>,
  pub child_items: Option<Vec<NavigationTree>>,
}

impl NavigationTree {
  pub fn to_code_lens(
    &self,
    line_index: Arc<LineIndex>,
    specifier: &ModuleSpecifier,
    source: &code_lens::CodeLensSource,
  ) -> lsp::CodeLens {
    let range = if let Some(name_span) = &self.name_span {
      name_span.to_range(line_index)
    } else if !self.spans.is_empty() {
      let span = &self.spans[0];
      span.to_range(line_index)
    } else {
      lsp::Range::default()
    };
    lsp::CodeLens {
      range,
      command: None,
      data: Some(json!({
        "specifier": specifier,
        "source": source
      })),
    }
  }

  pub fn collect_document_symbols(
    &self,
    line_index: Arc<LineIndex>,
    document_symbols: &mut Vec<lsp::DocumentSymbol>,
  ) -> bool {
    let mut should_include = self.should_include_entry();
    if !should_include
      && self
        .child_items
        .as_ref()
        .map(|v| v.is_empty())
        .unwrap_or(true)
    {
      return false;
    }

    let children = self
      .child_items
      .as_deref()
      .unwrap_or(&[] as &[NavigationTree]);
    for span in self.spans.iter() {
      let range = TextRange::at(span.start.into(), span.length.into());
      let mut symbol_children = Vec::<lsp::DocumentSymbol>::new();
      for child in children.iter() {
        let should_traverse_child = child
          .spans
          .iter()
          .map(|child_span| {
            TextRange::at(child_span.start.into(), child_span.length.into())
          })
          .any(|child_range| range.intersect(child_range).is_some());
        if should_traverse_child {
          let included_child = child
            .collect_document_symbols(line_index.clone(), &mut symbol_children);
          should_include = should_include || included_child;
        }
      }

      if should_include {
        let mut selection_span = span;
        if let Some(name_span) = self.name_span.as_ref() {
          let name_range =
            TextRange::at(name_span.start.into(), name_span.length.into());
          if range.contains_range(name_range) {
            selection_span = name_span;
          }
        }

        let name = match self.kind {
          ScriptElementKind::MemberGetAccessorElement => {
            format!("(get) {}", self.text)
          }
          ScriptElementKind::MemberSetAccessorElement => {
            format!("(set) {}", self.text)
          }
          _ => self.text.clone(),
        };

        let mut tags: Option<Vec<lsp::SymbolTag>> = None;
        let kind_modifiers = parse_kind_modifier(&self.kind_modifiers);
        if kind_modifiers.contains("deprecated") {
          tags = Some(vec![lsp::SymbolTag::DEPRECATED]);
        }

        let children = if !symbol_children.is_empty() {
          Some(symbol_children)
        } else {
          None
        };

        // The field `deprecated` is deprecated but DocumentSymbol does not have
        // a default, therefore we have to supply the deprecated deprecated
        // field. It is like a bad version of Inception.
        #[allow(deprecated)]
        document_symbols.push(lsp::DocumentSymbol {
          name,
          kind: self.kind.clone().into(),
          range: span.to_range(line_index.clone()),
          selection_range: selection_span.to_range(line_index.clone()),
          tags,
          children,
          detail: None,
          deprecated: None,
        })
      }
    }

    should_include
  }

  fn should_include_entry(&self) -> bool {
    if let ScriptElementKind::Alias = self.kind {
      return false;
    }

    !self.text.is_empty() && self.text != "<function>" && self.text != "<class>"
  }

  pub fn walk<F>(&self, callback: &F)
  where
    F: Fn(&NavigationTree, Option<&NavigationTree>),
  {
    callback(self, None);
    if let Some(child_items) = &self.child_items {
      for child in child_items {
        child.walk_child(callback, self);
      }
    }
  }

  fn walk_child<F>(&self, callback: &F, parent: &NavigationTree)
  where
    F: Fn(&NavigationTree, Option<&NavigationTree>),
  {
    callback(self, Some(parent));
    if let Some(child_items) = &self.child_items {
      for child in child_items {
        child.walk_child(callback, self);
      }
    }
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImplementationLocation {
  #[serde(flatten)]
  pub document_span: DocumentSpan,
  // ImplementationLocation props
  // kind: ScriptElementKind,
  // display_parts: Vec<SymbolDisplayPart>,
}

impl ImplementationLocation {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    self.document_span.normalize(specifier_map)?;
    Ok(())
  }

  pub fn to_location(
    &self,
    line_index: Arc<LineIndex>,
    language_server: &language_server::Inner,
  ) -> lsp::Location {
    let specifier = resolve_url(&self.document_span.file_name)
      .unwrap_or_else(|_| ModuleSpecifier::parse("deno://invalid").unwrap());
    let uri = language_server
      .url_map
      .normalize_specifier(&specifier)
      .unwrap_or_else(|_| {
        LspClientUrl::new(ModuleSpecifier::parse("deno://invalid").unwrap())
      });
    lsp::Location {
      uri: uri.into_url(),
      range: self.document_span.text_span.to_range(line_index),
    }
  }

  pub fn to_link(
    &self,
    line_index: Arc<LineIndex>,
    language_server: &language_server::Inner,
  ) -> Option<lsp::LocationLink> {
    self.document_span.to_link(line_index, language_server)
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameLocation {
  #[serde(flatten)]
  document_span: DocumentSpan,
  // RenameLocation props
  // prefix_text: Option<String>,
  // suffix_text: Option<String>,
}

impl RenameLocation {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    self.document_span.normalize(specifier_map)?;
    Ok(())
  }
}

pub struct RenameLocations {
  pub locations: Vec<RenameLocation>,
}

impl RenameLocations {
  pub async fn into_workspace_edit(
    self,
    new_name: &str,
    language_server: &language_server::Inner,
  ) -> Result<lsp::WorkspaceEdit, AnyError> {
    let mut text_document_edit_map: HashMap<
      LspClientUrl,
      lsp::TextDocumentEdit,
    > = HashMap::new();
    for location in self.locations.iter() {
      let specifier = resolve_url(&location.document_span.file_name)?;
      let uri = language_server.url_map.normalize_specifier(&specifier)?;
      let asset_or_doc = language_server.get_asset_or_document(&specifier)?;

      // ensure TextDocumentEdit for `location.file_name`.
      if text_document_edit_map.get(&uri).is_none() {
        text_document_edit_map.insert(
          uri.clone(),
          lsp::TextDocumentEdit {
            text_document: lsp::OptionalVersionedTextDocumentIdentifier {
              uri: uri.as_url().clone(),
              version: asset_or_doc.document_lsp_version(),
            },
            edits:
              Vec::<lsp::OneOf<lsp::TextEdit, lsp::AnnotatedTextEdit>>::new(),
          },
        );
      }

      // push TextEdit for ensured `TextDocumentEdit.edits`.
      let document_edit = text_document_edit_map.get_mut(&uri).unwrap();
      document_edit.edits.push(lsp::OneOf::Left(lsp::TextEdit {
        range: location
          .document_span
          .text_span
          .to_range(asset_or_doc.line_index()),
        new_text: new_name.to_string(),
      }));
    }

    Ok(lsp::WorkspaceEdit {
      change_annotations: None,
      changes: None,
      document_changes: Some(lsp::DocumentChanges::Edits(
        text_document_edit_map.values().cloned().collect(),
      )),
    })
  }
}

#[derive(Debug, Deserialize)]
pub enum HighlightSpanKind {
  #[serde(rename = "none")]
  None,
  #[serde(rename = "definition")]
  Definition,
  #[serde(rename = "reference")]
  Reference,
  #[serde(rename = "writtenReference")]
  WrittenReference,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HighlightSpan {
  // file_name: Option<String>,
  // is_in_string: Option<bool>,
  text_span: TextSpan,
  // context_span: Option<TextSpan>,
  kind: HighlightSpanKind,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionInfo {
  // kind: ScriptElementKind,
  // name: String,
  // container_kind: Option<ScriptElementKind>,
  // container_name: Option<String>,
  #[serde(flatten)]
  pub document_span: DocumentSpan,
}

impl DefinitionInfo {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    self.document_span.normalize(specifier_map)?;
    Ok(())
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DefinitionInfoAndBoundSpan {
  pub definitions: Option<Vec<DefinitionInfo>>,
  // text_span: TextSpan,
}

impl DefinitionInfoAndBoundSpan {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    for definition in self.definitions.iter_mut().flatten() {
      definition.normalize(specifier_map)?;
    }
    Ok(())
  }

  pub async fn to_definition(
    &self,
    line_index: Arc<LineIndex>,
    language_server: &language_server::Inner,
  ) -> Option<lsp::GotoDefinitionResponse> {
    if let Some(definitions) = &self.definitions {
      let mut location_links = Vec::<lsp::LocationLink>::new();
      for di in definitions {
        if let Some(link) = di
          .document_span
          .to_link(line_index.clone(), language_server)
        {
          location_links.push(link);
        }
      }
      Some(lsp::GotoDefinitionResponse::Link(location_links))
    } else {
      None
    }
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentHighlights {
  // file_name: String,
  highlight_spans: Vec<HighlightSpan>,
}

impl DocumentHighlights {
  pub fn to_highlight(
    &self,
    line_index: Arc<LineIndex>,
  ) -> Vec<lsp::DocumentHighlight> {
    self
      .highlight_spans
      .iter()
      .map(|hs| lsp::DocumentHighlight {
        range: hs.text_span.to_range(line_index.clone()),
        kind: match hs.kind {
          HighlightSpanKind::WrittenReference => {
            Some(lsp::DocumentHighlightKind::WRITE)
          }
          _ => Some(lsp::DocumentHighlightKind::READ),
        },
      })
      .collect()
  }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TextChange {
  pub span: TextSpan,
  pub new_text: String,
}

impl TextChange {
  pub fn as_text_edit(&self, line_index: Arc<LineIndex>) -> lsp::TextEdit {
    lsp::TextEdit {
      range: self.span.to_range(line_index),
      new_text: self.new_text.clone(),
    }
  }

  pub fn as_text_or_annotated_text_edit(
    &self,
    line_index: Arc<LineIndex>,
  ) -> lsp::OneOf<lsp::TextEdit, lsp::AnnotatedTextEdit> {
    lsp::OneOf::Left(lsp::TextEdit {
      range: self.span.to_range(line_index),
      new_text: self.new_text.clone(),
    })
  }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileTextChanges {
  pub file_name: String,
  pub text_changes: Vec<TextChange>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub is_new_file: Option<bool>,
}

impl FileTextChanges {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    self.file_name = specifier_map.normalize(&self.file_name)?.to_string();
    Ok(())
  }

  pub fn to_text_document_edit(
    &self,
    language_server: &language_server::Inner,
  ) -> Result<lsp::TextDocumentEdit, AnyError> {
    let specifier = resolve_url(&self.file_name)?;
    let asset_or_doc = language_server.get_asset_or_document(&specifier)?;
    let edits = self
      .text_changes
      .iter()
      .map(|tc| tc.as_text_or_annotated_text_edit(asset_or_doc.line_index()))
      .collect();
    Ok(lsp::TextDocumentEdit {
      text_document: lsp::OptionalVersionedTextDocumentIdentifier {
        uri: specifier,
        version: asset_or_doc.document_lsp_version(),
      },
      edits,
    })
  }

  pub fn to_text_document_change_ops(
    &self,
    language_server: &language_server::Inner,
  ) -> Result<Vec<lsp::DocumentChangeOperation>, AnyError> {
    let mut ops = Vec::<lsp::DocumentChangeOperation>::new();
    let specifier = resolve_url(&self.file_name)?;
    let maybe_asset_or_document = if !self.is_new_file.unwrap_or(false) {
      let asset_or_doc = language_server.get_asset_or_document(&specifier)?;
      Some(asset_or_doc)
    } else {
      None
    };
    let line_index = maybe_asset_or_document
      .as_ref()
      .map(|d| d.line_index())
      .unwrap_or_else(|| Arc::new(LineIndex::new("")));

    if self.is_new_file.unwrap_or(false) {
      ops.push(lsp::DocumentChangeOperation::Op(lsp::ResourceOp::Create(
        lsp::CreateFile {
          uri: specifier.clone(),
          options: Some(lsp::CreateFileOptions {
            ignore_if_exists: Some(true),
            overwrite: None,
          }),
          annotation_id: None,
        },
      )));
    }

    let edits = self
      .text_changes
      .iter()
      .map(|tc| tc.as_text_or_annotated_text_edit(line_index.clone()))
      .collect();
    ops.push(lsp::DocumentChangeOperation::Edit(lsp::TextDocumentEdit {
      text_document: lsp::OptionalVersionedTextDocumentIdentifier {
        uri: specifier,
        version: maybe_asset_or_document.and_then(|d| d.document_lsp_version()),
      },
      edits,
    }));

    Ok(ops)
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Classifications {
  spans: Vec<u32>,
}

impl Classifications {
  pub fn to_semantic_tokens(
    &self,
    asset_or_doc: &AssetOrDocument,
    line_index: Arc<LineIndex>,
  ) -> LspResult<lsp::SemanticTokens> {
    let token_count = self.spans.len() / 3;
    let mut builder = SemanticTokensBuilder::new();
    for i in 0..token_count {
      let src_offset = 3 * i;
      let offset = self.spans[src_offset];
      let length = self.spans[src_offset + 1];
      let ts_classification = self.spans[src_offset + 2];

      let token_type =
        Classifications::get_token_type_from_classification(ts_classification);
      let token_modifiers =
        Classifications::get_token_modifier_from_classification(
          ts_classification,
        );

      let start_pos = line_index.position_tsc(offset.into());
      let end_pos = line_index.position_tsc(TextSize::from(offset + length));

      if start_pos.line == end_pos.line
        && start_pos.character <= end_pos.character
      {
        builder.push(
          start_pos.line,
          start_pos.character,
          end_pos.character - start_pos.character,
          token_type,
          token_modifiers,
        );
      } else {
        log::error!(
          "unexpected positions\nspecifier: {}\nopen: {}\nstart_pos: {:?}\nend_pos: {:?}",
          asset_or_doc.specifier(),
          asset_or_doc.is_open(),
          start_pos,
          end_pos
        );
        return Err(LspError::internal_error());
      }
    }
    Ok(builder.build(None))
  }

  fn get_token_type_from_classification(ts_classification: u32) -> u32 {
    assert!(ts_classification > semantic_tokens::MODIFIER_MASK);
    (ts_classification >> semantic_tokens::TYPE_OFFSET) - 1
  }

  fn get_token_modifier_from_classification(ts_classification: u32) -> u32 {
    ts_classification & semantic_tokens::MODIFIER_MASK
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefactorActionInfo {
  name: String,
  description: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  not_applicable_reason: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  kind: Option<String>,
}

impl RefactorActionInfo {
  pub fn get_action_kind(&self) -> lsp::CodeActionKind {
    if let Some(kind) = &self.kind {
      kind.clone().into()
    } else {
      let maybe_match = ALL_KNOWN_REFACTOR_ACTION_KINDS
        .iter()
        .find(|action| action.matches(&self.name));
      maybe_match
        .map(|action| action.kind.clone())
        .unwrap_or(lsp::CodeActionKind::REFACTOR)
    }
  }

  pub fn is_preferred(&self, all_actions: &[RefactorActionInfo]) -> bool {
    if EXTRACT_CONSTANT.matches(&self.name) {
      let get_scope = |name: &str| -> Option<u32> {
        if let Some(captures) = SCOPE_RE.captures(name) {
          captures[1].parse::<u32>().ok()
        } else {
          None
        }
      };

      return if let Some(scope) = get_scope(&self.name) {
        all_actions
          .iter()
          .filter(|other| {
            !std::ptr::eq(&self, other) && EXTRACT_CONSTANT.matches(&other.name)
          })
          .all(|other| {
            if let Some(other_scope) = get_scope(&other.name) {
              scope < other_scope
            } else {
              true
            }
          })
      } else {
        false
      };
    }
    if EXTRACT_TYPE.matches(&self.name) || EXTRACT_INTERFACE.matches(&self.name)
    {
      return true;
    }
    false
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicableRefactorInfo {
  name: String,
  // description: String,
  // #[serde(skip_serializing_if = "Option::is_none")]
  // inlineable: Option<bool>,
  actions: Vec<RefactorActionInfo>,
}

impl ApplicableRefactorInfo {
  pub fn to_code_actions(
    &self,
    specifier: &ModuleSpecifier,
    range: &lsp::Range,
  ) -> Vec<lsp::CodeAction> {
    let mut code_actions = Vec::<lsp::CodeAction>::new();
    // All typescript refactoring actions are inlineable
    for action in self.actions.iter() {
      code_actions
        .push(self.as_inline_code_action(action, specifier, range, &self.name));
    }
    code_actions
  }

  fn as_inline_code_action(
    &self,
    action: &RefactorActionInfo,
    specifier: &ModuleSpecifier,
    range: &lsp::Range,
    refactor_name: &str,
  ) -> lsp::CodeAction {
    let disabled = action.not_applicable_reason.as_ref().map(|reason| {
      lsp::CodeActionDisabled {
        reason: reason.clone(),
      }
    });

    lsp::CodeAction {
      title: action.description.to_string(),
      kind: Some(action.get_action_kind()),
      is_preferred: Some(action.is_preferred(&self.actions)),
      disabled,
      data: Some(
        serde_json::to_value(RefactorCodeActionData {
          specifier: specifier.clone(),
          range: *range,
          refactor_name: refactor_name.to_owned(),
          action_name: action.name.clone(),
        })
        .unwrap(),
      ),
      ..Default::default()
    }
  }
}

pub fn file_text_changes_to_workspace_edit(
  changes: &[FileTextChanges],
  language_server: &language_server::Inner,
) -> LspResult<Option<lsp::WorkspaceEdit>> {
  let mut all_ops = Vec::<lsp::DocumentChangeOperation>::new();
  for change in changes {
    let ops = match change.to_text_document_change_ops(language_server) {
      Ok(op) => op,
      Err(err) => {
        error!("Unable to convert changes to edits: {}", err);
        return Err(LspError::internal_error());
      }
    };
    all_ops.extend(ops);
  }

  Ok(Some(lsp::WorkspaceEdit {
    document_changes: Some(lsp::DocumentChanges::Operations(all_ops)),
    ..Default::default()
  }))
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RefactorEditInfo {
  edits: Vec<FileTextChanges>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub rename_location: Option<u32>,
}

impl RefactorEditInfo {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    for changes in &mut self.edits {
      changes.normalize(specifier_map)?;
    }
    Ok(())
  }

  pub async fn to_workspace_edit(
    &self,
    language_server: &language_server::Inner,
  ) -> LspResult<Option<lsp::WorkspaceEdit>> {
    file_text_changes_to_workspace_edit(&self.edits, language_server)
  }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeAction {
  description: String,
  changes: Vec<FileTextChanges>,
  #[serde(skip_serializing_if = "Option::is_none")]
  commands: Option<Vec<Value>>,
}

impl CodeAction {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    for changes in &mut self.changes {
      changes.normalize(specifier_map)?;
    }
    Ok(())
  }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CodeFixAction {
  pub description: String,
  pub changes: Vec<FileTextChanges>,
  // These are opaque types that should just be passed back when applying the
  // action.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub commands: Option<Vec<Value>>,
  pub fix_name: String,
  // It appears currently that all fixIds are strings, but the protocol
  // specifies an opaque type, the problem is that we need to use the id as a
  // hash key, and `Value` does not implement hash (and it could provide a false
  // positive depending on JSON whitespace, so we deserialize it but it might
  // break in the future)
  #[serde(skip_serializing_if = "Option::is_none")]
  pub fix_id: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub fix_all_description: Option<String>,
}

impl CodeFixAction {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    for changes in &mut self.changes {
      changes.normalize(specifier_map)?;
    }
    Ok(())
  }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CombinedCodeActions {
  pub changes: Vec<FileTextChanges>,
  pub commands: Option<Vec<Value>>,
}

impl CombinedCodeActions {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    for changes in &mut self.changes {
      changes.normalize(specifier_map)?;
    }
    Ok(())
  }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferencedSymbol {
  pub definition: ReferencedSymbolDefinitionInfo,
  pub references: Vec<ReferencedSymbolEntry>,
}

impl ReferencedSymbol {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    self.definition.normalize(specifier_map)?;
    for reference in &mut self.references {
      reference.normalize(specifier_map)?;
    }
    Ok(())
  }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferencedSymbolDefinitionInfo {
  #[serde(flatten)]
  pub definition_info: DefinitionInfo,
}

impl ReferencedSymbolDefinitionInfo {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    self.definition_info.normalize(specifier_map)?;
    Ok(())
  }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferencedSymbolEntry {
  #[serde(default)]
  pub is_definition: bool,
  #[serde(flatten)]
  pub entry: ReferenceEntry,
}

impl ReferencedSymbolEntry {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    self.entry.normalize(specifier_map)?;
    Ok(())
  }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceEntry {
  // is_write_access: bool,
  // is_in_string: Option<bool>,
  #[serde(flatten)]
  pub document_span: DocumentSpan,
}

impl ReferenceEntry {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    self.document_span.normalize(specifier_map)?;
    Ok(())
  }
}

impl ReferenceEntry {
  pub fn to_location(
    &self,
    line_index: Arc<LineIndex>,
    url_map: &LspUrlMap,
  ) -> lsp::Location {
    let specifier = resolve_url(&self.document_span.file_name)
      .unwrap_or_else(|_| INVALID_SPECIFIER.clone());
    let uri = url_map
      .normalize_specifier(&specifier)
      .unwrap_or_else(|_| LspClientUrl::new(INVALID_SPECIFIER.clone()));
    lsp::Location {
      uri: uri.into_url(),
      range: self.document_span.text_span.to_range(line_index),
    }
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyItem {
  name: String,
  kind: ScriptElementKind,
  #[serde(skip_serializing_if = "Option::is_none")]
  kind_modifiers: Option<String>,
  file: String,
  span: TextSpan,
  selection_span: TextSpan,
  #[serde(skip_serializing_if = "Option::is_none")]
  container_name: Option<String>,
}

impl CallHierarchyItem {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    self.file = specifier_map.normalize(&self.file)?.to_string();
    Ok(())
  }

  pub fn try_resolve_call_hierarchy_item(
    &self,
    language_server: &language_server::Inner,
    maybe_root_path: Option<&Path>,
  ) -> Option<lsp::CallHierarchyItem> {
    let target_specifier = resolve_url(&self.file).ok()?;
    let target_asset_or_doc =
      language_server.get_maybe_asset_or_document(&target_specifier)?;

    Some(self.to_call_hierarchy_item(
      target_asset_or_doc.line_index(),
      language_server,
      maybe_root_path,
    ))
  }

  pub fn to_call_hierarchy_item(
    &self,
    line_index: Arc<LineIndex>,
    language_server: &language_server::Inner,
    maybe_root_path: Option<&Path>,
  ) -> lsp::CallHierarchyItem {
    let target_specifier =
      resolve_url(&self.file).unwrap_or_else(|_| INVALID_SPECIFIER.clone());
    let uri = language_server
      .url_map
      .normalize_specifier(&target_specifier)
      .unwrap_or_else(|_| LspClientUrl::new(INVALID_SPECIFIER.clone()));

    let use_file_name = self.is_source_file_item();
    let maybe_file_path = if uri.as_url().scheme() == "file" {
      specifier_to_file_path(uri.as_url()).ok()
    } else {
      None
    };
    let name = if use_file_name {
      if let Some(file_path) = maybe_file_path.as_ref() {
        file_path.file_name().unwrap().to_string_lossy().to_string()
      } else {
        uri.as_str().to_string()
      }
    } else {
      self.name.clone()
    };
    let detail = if use_file_name {
      if let Some(file_path) = maybe_file_path.as_ref() {
        // TODO: update this to work with multi root workspaces
        let parent_dir = file_path.parent().unwrap();
        if let Some(root_path) = maybe_root_path {
          parent_dir
            .strip_prefix(root_path)
            .unwrap_or(parent_dir)
            .to_string_lossy()
            .to_string()
        } else {
          parent_dir.to_string_lossy().to_string()
        }
      } else {
        String::new()
      }
    } else {
      self.container_name.as_ref().cloned().unwrap_or_default()
    };

    let mut tags: Option<Vec<lsp::SymbolTag>> = None;
    if let Some(modifiers) = self.kind_modifiers.as_ref() {
      let kind_modifiers = parse_kind_modifier(modifiers);
      if kind_modifiers.contains("deprecated") {
        tags = Some(vec![lsp::SymbolTag::DEPRECATED]);
      }
    }

    lsp::CallHierarchyItem {
      name,
      tags,
      uri: uri.into_url(),
      detail: Some(detail),
      kind: self.kind.clone().into(),
      range: self.span.to_range(line_index.clone()),
      selection_range: self.selection_span.to_range(line_index),
      data: None,
    }
  }

  fn is_source_file_item(&self) -> bool {
    self.kind == ScriptElementKind::ScriptElement
      || self.kind == ScriptElementKind::ModuleElement
        && self.selection_span.start == 0
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyIncomingCall {
  from: CallHierarchyItem,
  from_spans: Vec<TextSpan>,
}

impl CallHierarchyIncomingCall {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    self.from.normalize(specifier_map)?;
    Ok(())
  }

  pub fn try_resolve_call_hierarchy_incoming_call(
    &self,
    language_server: &language_server::Inner,
    maybe_root_path: Option<&Path>,
  ) -> Option<lsp::CallHierarchyIncomingCall> {
    let target_specifier = resolve_url(&self.from.file).ok()?;
    let target_asset_or_doc =
      language_server.get_maybe_asset_or_document(&target_specifier)?;

    Some(lsp::CallHierarchyIncomingCall {
      from: self.from.to_call_hierarchy_item(
        target_asset_or_doc.line_index(),
        language_server,
        maybe_root_path,
      ),
      from_ranges: self
        .from_spans
        .iter()
        .map(|span| span.to_range(target_asset_or_doc.line_index()))
        .collect(),
    })
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyOutgoingCall {
  to: CallHierarchyItem,
  from_spans: Vec<TextSpan>,
}

impl CallHierarchyOutgoingCall {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    self.to.normalize(specifier_map)?;
    Ok(())
  }

  pub fn try_resolve_call_hierarchy_outgoing_call(
    &self,
    line_index: Arc<LineIndex>,
    language_server: &language_server::Inner,
    maybe_root_path: Option<&Path>,
  ) -> Option<lsp::CallHierarchyOutgoingCall> {
    let target_specifier = resolve_url(&self.to.file).ok()?;
    let target_asset_or_doc =
      language_server.get_maybe_asset_or_document(&target_specifier)?;

    Some(lsp::CallHierarchyOutgoingCall {
      to: self.to.to_call_hierarchy_item(
        target_asset_or_doc.line_index(),
        language_server,
        maybe_root_path,
      ),
      from_ranges: self
        .from_spans
        .iter()
        .map(|span| span.to_range(line_index.clone()))
        .collect(),
    })
  }
}

/// Used to convert completion code actions into a command and additional text
/// edits to pass in the completion item.
fn parse_code_actions(
  maybe_code_actions: Option<&Vec<CodeAction>>,
  data: &CompletionItemData,
  specifier: &ModuleSpecifier,
  language_server: &language_server::Inner,
) -> Result<(Option<lsp::Command>, Option<Vec<lsp::TextEdit>>), AnyError> {
  if let Some(code_actions) = maybe_code_actions {
    let mut additional_text_edits: Vec<lsp::TextEdit> = Vec::new();
    let mut has_remaining_commands_or_edits = false;
    for ts_action in code_actions {
      if ts_action.commands.is_some() {
        has_remaining_commands_or_edits = true;
      }

      let asset_or_doc =
        language_server.get_asset_or_document(&data.specifier)?;
      for change in &ts_action.changes {
        if data.specifier.as_str() == change.file_name {
          additional_text_edits.extend(change.text_changes.iter().map(|tc| {
            let mut text_edit = tc.as_text_edit(asset_or_doc.line_index());
            if let Some(specifier_rewrite) = &data.specifier_rewrite {
              text_edit.new_text = text_edit
                .new_text
                .replace(&specifier_rewrite.0, &specifier_rewrite.1);
            }
            text_edit
          }));
        } else {
          has_remaining_commands_or_edits = true;
        }
      }
    }

    let mut command: Option<lsp::Command> = None;
    if has_remaining_commands_or_edits {
      let actions: Vec<Value> = code_actions
        .iter()
        .map(|ca| {
          let changes: Vec<FileTextChanges> = ca
            .changes
            .clone()
            .into_iter()
            .filter(|ch| ch.file_name == data.specifier.as_str())
            .collect();
          json!({
            "commands": ca.commands,
            "description": ca.description,
            "changes": changes,
          })
        })
        .collect();
      command = Some(lsp::Command {
        title: "".to_string(),
        command: "_typescript.applyCompletionCodeAction".to_string(),
        arguments: Some(vec![json!(specifier.to_string()), json!(actions)]),
      });
    }

    if additional_text_edits.is_empty() {
      Ok((command, None))
    } else {
      Ok((command, Some(additional_text_edits)))
    }
  } else {
    Ok((None, None))
  }
}

// Based on https://github.com/microsoft/vscode/blob/1.81.1/extensions/typescript-language-features/src/languageFeatures/util/snippetForFunctionCall.ts#L49.
fn get_parameters_from_parts(parts: &[SymbolDisplayPart]) -> Vec<String> {
  let mut parameters = Vec::with_capacity(3);
  let mut is_in_fn = false;
  let mut paren_count = 0;
  let mut brace_count = 0;
  for (idx, part) in parts.iter().enumerate() {
    if ["methodName", "functionName", "text", "propertyName"]
      .contains(&part.kind.as_str())
    {
      if paren_count == 0 && brace_count == 0 {
        is_in_fn = true;
      }
    } else if part.kind == "parameterName" {
      if paren_count == 1 && brace_count == 0 && is_in_fn {
        let is_optional =
          matches!(parts.get(idx + 1), Some(next) if next.text == "?");
        // Skip `this` and optional parameters.
        if !is_optional && part.text != "this" {
          parameters.push(part.text.clone());
        }
      }
    } else if part.kind == "punctuation" {
      if part.text == "(" {
        paren_count += 1;
      } else if part.text == ")" {
        paren_count -= 1;
        if paren_count <= 0 && is_in_fn {
          break;
        }
      } else if part.text == "..." && paren_count == 1 {
        // Found rest parmeter. Do not fill in any further arguments.
        break;
      } else if part.text == "{" {
        brace_count += 1;
      } else if part.text == "}" {
        brace_count -= 1;
      }
    }
  }
  parameters
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionEntryDetails {
  display_parts: Vec<SymbolDisplayPart>,
  documentation: Option<Vec<SymbolDisplayPart>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  tags: Option<Vec<JsDocTagInfo>>,
  name: String,
  kind: ScriptElementKind,
  kind_modifiers: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  code_actions: Option<Vec<CodeAction>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  source_display: Option<Vec<SymbolDisplayPart>>,
}

impl CompletionEntryDetails {
  fn normalize(
    &mut self,
    specifier_map: &TscSpecifierMap,
  ) -> Result<(), AnyError> {
    for action in self.code_actions.iter_mut().flatten() {
      action.normalize(specifier_map)?;
    }
    Ok(())
  }

  pub fn as_completion_item(
    &self,
    original_item: &lsp::CompletionItem,
    data: &CompletionItemData,
    specifier: &ModuleSpecifier,
    language_server: &language_server::Inner,
  ) -> Result<lsp::CompletionItem, AnyError> {
    let detail = if original_item.detail.is_some() {
      original_item.detail.clone()
    } else if !self.display_parts.is_empty() {
      Some(replace_links(display_parts_to_string(
        &self.display_parts,
        language_server,
      )))
    } else {
      None
    };
    let documentation = if let Some(parts) = &self.documentation {
      let mut value = display_parts_to_string(parts, language_server);
      if let Some(tags) = &self.tags {
        let tag_documentation = tags
          .iter()
          .map(|tag_info| get_tag_documentation(tag_info, language_server))
          .collect::<Vec<String>>()
          .join("");
        value = format!("{value}\n\n{tag_documentation}");
      }
      Some(lsp::Documentation::MarkupContent(lsp::MarkupContent {
        kind: lsp::MarkupKind::Markdown,
        value,
      }))
    } else {
      None
    };
    let mut text_edit = original_item.text_edit.clone();
    if let Some(specifier_rewrite) = &data.specifier_rewrite {
      if let Some(text_edit) = &mut text_edit {
        match text_edit {
          lsp::CompletionTextEdit::Edit(text_edit) => {
            text_edit.new_text = text_edit
              .new_text
              .replace(&specifier_rewrite.0, &specifier_rewrite.1);
          }
          lsp::CompletionTextEdit::InsertAndReplace(insert_replace_edit) => {
            insert_replace_edit.new_text = insert_replace_edit
              .new_text
              .replace(&specifier_rewrite.0, &specifier_rewrite.1);
          }
        }
      }
    }
    let (command, additional_text_edits) = parse_code_actions(
      self.code_actions.as_ref(),
      data,
      specifier,
      language_server,
    )?;
    let insert_text = if data.use_code_snippet {
      Some(format!(
        "{}({})",
        original_item
          .insert_text
          .as_ref()
          .unwrap_or(&original_item.label),
        get_parameters_from_parts(&self.display_parts).join(", "),
      ))
    } else {
      original_item.insert_text.clone()
    };

    Ok(lsp::CompletionItem {
      data: None,
      detail,
      documentation,
      command,
      text_edit,
      additional_text_edits,
      insert_text,
      // NOTE(bartlomieju): it's not entirely clear to me why we need to do that,
      // but when `completionItem/resolve` is called, we get a list of commit chars
      // even though we might have returned an empty list in `completion` request.
      commit_characters: None,
      ..original_item.clone()
    })
  }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionInfo {
  entries: Vec<CompletionEntry>,
  // this is only used by Microsoft's telemetrics, which Deno doesn't use and
  // there are issues with the value not matching the type definitions.
  // flags: Option<CompletionInfoFlags>,
  is_global_completion: bool,
  is_member_completion: bool,
  is_new_identifier_location: bool,
  metadata: Option<Value>,
  optional_replacement_span: Option<TextSpan>,
}

impl CompletionInfo {
  pub fn as_completion_response(
    &self,
    line_index: Arc<LineIndex>,
    settings: &config::CompletionSettings,
    specifier: &ModuleSpecifier,
    position: u32,
    language_server: &language_server::Inner,
  ) -> lsp::CompletionResponse {
    let items = self
      .entries
      .iter()
      .map(|entry| {
        entry.as_completion_item(
          line_index.clone(),
          self,
          settings,
          specifier,
          position,
          language_server,
        )
      })
      .collect();
    let is_incomplete = self
      .metadata
      .clone()
      .map(|v| {
        v.as_object()
          .unwrap()
          .get("isIncomplete")
          .unwrap_or(&json!(false))
          .as_bool()
          .unwrap()
      })
      .unwrap_or(false);
    lsp::CompletionResponse::List(lsp::CompletionList {
      is_incomplete,
      items,
    })
  }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionItemData {
  pub specifier: ModuleSpecifier,
  pub position: u32,
  pub name: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub source: Option<String>,
  /// If present, the code action / text edit corresponding to this item should
  /// be rewritten by replacing the first string with the second. Intended for
  /// auto-import specifiers to be reverse-import-mapped.
  #[serde(skip_serializing_if = "Option::is_none")]
  pub specifier_rewrite: Option<(String, String)>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub data: Option<Value>,
  pub use_code_snippet: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompletionEntryDataImport {
  module_specifier: String,
  file_name: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionEntry {
  name: String,
  kind: ScriptElementKind,
  #[serde(skip_serializing_if = "Option::is_none")]
  kind_modifiers: Option<String>,
  sort_text: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  insert_text: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  is_snippet: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  replacement_span: Option<TextSpan>,
  #[serde(skip_serializing_if = "Option::is_none")]
  has_action: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  source: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  source_display: Option<Vec<SymbolDisplayPart>>,
  #[serde(skip_serializing_if = "Option::is_none")]
  label_details: Option<CompletionEntryLabelDetails>,
  #[serde(skip_serializing_if = "Option::is_none")]
  is_recommended: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  is_from_unchecked_file: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  is_package_json_import: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  is_import_statement_completion: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  data: Option<Value>,
}

impl CompletionEntry {
  fn get_commit_characters(
    &self,
    info: &CompletionInfo,
    settings: &config::CompletionSettings,
  ) -> Option<Vec<String>> {
    if info.is_new_identifier_location {
      return None;
    }

    let mut commit_characters = vec![];
    match self.kind {
      ScriptElementKind::MemberGetAccessorElement
      | ScriptElementKind::MemberSetAccessorElement
      | ScriptElementKind::ConstructSignatureElement
      | ScriptElementKind::CallSignatureElement
      | ScriptElementKind::IndexSignatureElement
      | ScriptElementKind::EnumElement
      | ScriptElementKind::InterfaceElement => {
        commit_characters.push(".");
        commit_characters.push(";");
      }
      ScriptElementKind::ModuleElement
      | ScriptElementKind::Alias
      | ScriptElementKind::ConstElement
      | ScriptElementKind::LetElement
      | ScriptElementKind::VariableElement
      | ScriptElementKind::LocalVariableElement
      | ScriptElementKind::MemberVariableElement
      | ScriptElementKind::ClassElement
      | ScriptElementKind::FunctionElement
      | ScriptElementKind::MemberFunctionElement
      | ScriptElementKind::Keyword
      | ScriptElementKind::ParameterElement => {
        commit_characters.push(".");
        commit_characters.push(",");
        commit_characters.push(";");
        if !settings.complete_function_calls {
          commit_characters.push("(");
        }
      }
      _ => (),
    }

    if commit_characters.is_empty() {
      None
    } else {
      Some(commit_characters.into_iter().map(String::from).collect())
    }
  }

  fn get_filter_text(&self) -> Option<String> {
    if self.name.starts_with('#') {
      if let Some(insert_text) = &self.insert_text {
        if insert_text.starts_with("this.#") {
          return Some(insert_text.replace("this.#", ""));
        } else {
          return Some(insert_text.clone());
        }
      } else {
        return None;
      }
    }

    if let Some(insert_text) = &self.insert_text {
      if insert_text.starts_with("this.") {
        return None;
      }
      if insert_text.starts_with('[') {
        return Some(
          BRACKET_ACCESSOR_RE
            .replace(insert_text, |caps: &Captures| format!(".{}", &caps[1]))
            .to_string(),
        );
      }
    }

    self.insert_text.clone()
  }

  pub fn as_completion_item(
    &self,
    line_index: Arc<LineIndex>,
    info: &CompletionInfo,
    settings: &config::CompletionSettings,
    specifier: &ModuleSpecifier,
    position: u32,
    language_server: &language_server::Inner,
  ) -> lsp::CompletionItem {
    let mut label = self.name.clone();
    let mut label_details: Option<lsp::CompletionItemLabelDetails> = None;
    let mut kind: Option<lsp::CompletionItemKind> =
      Some(self.kind.clone().into());
    let mut specifier_rewrite = None;

    let mut sort_text = if self.source.is_some() {
      format!("\u{ffff}{}", self.sort_text)
    } else {
      self.sort_text.clone()
    };

    let preselect = self.is_recommended;
    let use_code_snippet = settings.complete_function_calls
      && (kind == Some(lsp::CompletionItemKind::FUNCTION)
        || kind == Some(lsp::CompletionItemKind::METHOD));
    let commit_characters = self.get_commit_characters(info, settings);
    let mut insert_text = self.insert_text.clone();
    let insert_text_format = match self.is_snippet {
      Some(true) => Some(lsp::InsertTextFormat::SNIPPET),
      _ => None,
    };
    let range = self.replacement_span.clone();
    let mut filter_text = self.get_filter_text();
    let mut tags = None;
    let mut detail = None;

    if let Some(kind_modifiers) = &self.kind_modifiers {
      let kind_modifiers = parse_kind_modifier(kind_modifiers);
      if kind_modifiers.contains("optional") {
        if insert_text.is_none() {
          insert_text = Some(label.clone());
        }
        if filter_text.is_none() {
          filter_text = Some(label.clone());
        }
        label += "?";
      }
      if kind_modifiers.contains("deprecated") {
        tags = Some(vec![lsp::CompletionItemTag::DEPRECATED]);
      }
      if kind_modifiers.contains("color") {
        kind = Some(lsp::CompletionItemKind::COLOR);
      }
      if self.kind == ScriptElementKind::ScriptElement {
        for ext_modifier in FILE_EXTENSION_KIND_MODIFIERS {
          if kind_modifiers.contains(ext_modifier) {
            detail = if self.name.to_lowercase().ends_with(ext_modifier) {
              Some(self.name.clone())
            } else {
              Some(format!("{}{}", self.name, ext_modifier))
            };
            break;
          }
        }
      }
    }

    if let Some(source) = &self.source {
      let mut display_source = source.clone();
      if let Some(data) = &self.data {
        if let Ok(import_data) =
          serde_json::from_value::<CompletionEntryDataImport>(data.clone())
        {
          if let Ok(import_specifier) = resolve_url(&import_data.file_name) {
            if let Some(new_module_specifier) = language_server
              .get_ts_response_import_mapper()
              .check_specifier(&import_specifier, specifier)
              .or_else(|| relative_specifier(specifier, &import_specifier))
            {
              display_source = new_module_specifier.clone();
              if new_module_specifier != import_data.module_specifier {
                specifier_rewrite =
                  Some((import_data.module_specifier, new_module_specifier));
              }
            }
          }
        }
      }
      // We want relative or bare (import-mapped or otherwise) specifiers to
      // appear at the top.
      if resolve_url(&display_source).is_err() {
        sort_text += "_0";
      } else {
        sort_text += "_1";
      }
      label_details
        .get_or_insert_with(Default::default)
        .description = Some(display_source);
    }

    let text_edit =
      if let (Some(text_span), Some(new_text)) = (range, &insert_text) {
        let range = text_span.to_range(line_index);
        let insert_replace_edit = lsp::InsertReplaceEdit {
          new_text: new_text.clone(),
          insert: range,
          replace: range,
        };
        Some(insert_replace_edit.into())
      } else {
        None
      };

    let tsc = CompletionItemData {
      specifier: specifier.clone(),
      position,
      name: self.name.clone(),
      source: self.source.clone(),
      specifier_rewrite,
      data: self.data.clone(),
      use_code_snippet,
    };

    lsp::CompletionItem {
      label,
      label_details,
      kind,
      sort_text: Some(sort_text),
      preselect,
      text_edit,
      filter_text,
      insert_text,
      insert_text_format,
      detail,
      tags,
      commit_characters,
      data: Some(json!({ "tsc": tsc })),
      ..Default::default()
    }
  }
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompletionEntryLabelDetails {
  #[serde(skip_serializing_if = "Option::is_none")]
  detail: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub enum OutliningSpanKind {
  #[serde(rename = "comment")]
  Comment,
  #[serde(rename = "region")]
  Region,
  #[serde(rename = "code")]
  Code,
  #[serde(rename = "imports")]
  Imports,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutliningSpan {
  text_span: TextSpan,
  // hint_span: TextSpan,
  // banner_text: String,
  // auto_collapse: bool,
  kind: OutliningSpanKind,
}

const FOLD_END_PAIR_CHARACTERS: &[u8] = &[b'}', b']', b')', b'`'];

impl OutliningSpan {
  pub fn to_folding_range(
    &self,
    line_index: Arc<LineIndex>,
    content: &[u8],
    line_folding_only: bool,
  ) -> lsp::FoldingRange {
    let range = self.text_span.to_range(line_index.clone());
    lsp::FoldingRange {
      start_line: range.start.line,
      start_character: if line_folding_only {
        None
      } else {
        Some(range.start.character)
      },
      end_line: self.adjust_folding_end_line(
        &range,
        line_index,
        content,
        line_folding_only,
      ),
      end_character: if line_folding_only {
        None
      } else {
        Some(range.end.character)
      },
      kind: self.get_folding_range_kind(&self.kind),
      collapsed_text: None,
    }
  }

  fn adjust_folding_end_line(
    &self,
    range: &lsp::Range,
    line_index: Arc<LineIndex>,
    content: &[u8],
    line_folding_only: bool,
  ) -> u32 {
    if line_folding_only && range.end.line > 0 && range.end.character > 0 {
      let offset_end: usize = line_index.offset(range.end).unwrap().into();
      let fold_end_char = content[offset_end - 1];
      if FOLD_END_PAIR_CHARACTERS.contains(&fold_end_char) {
        return cmp::max(range.end.line - 1, range.start.line);
      }
    }

    range.end.line
  }

  fn get_folding_range_kind(
    &self,
    span_kind: &OutliningSpanKind,
  ) -> Option<lsp::FoldingRangeKind> {
    match span_kind {
      OutliningSpanKind::Comment => Some(lsp::FoldingRangeKind::Comment),
      OutliningSpanKind::Region => Some(lsp::FoldingRangeKind::Region),
      OutliningSpanKind::Imports => Some(lsp::FoldingRangeKind::Imports),
      _ => None,
    }
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureHelpItems {
  items: Vec<SignatureHelpItem>,
  // applicable_span: TextSpan,
  selected_item_index: u32,
  argument_index: u32,
  // argument_count: u32,
}

impl SignatureHelpItems {
  pub fn into_signature_help(
    self,
    language_server: &language_server::Inner,
  ) -> lsp::SignatureHelp {
    lsp::SignatureHelp {
      signatures: self
        .items
        .into_iter()
        .map(|item| item.into_signature_information(language_server))
        .collect(),
      active_parameter: Some(self.argument_index),
      active_signature: Some(self.selected_item_index),
    }
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureHelpItem {
  // is_variadic: bool,
  prefix_display_parts: Vec<SymbolDisplayPart>,
  suffix_display_parts: Vec<SymbolDisplayPart>,
  // separator_display_parts: Vec<SymbolDisplayPart>,
  parameters: Vec<SignatureHelpParameter>,
  documentation: Vec<SymbolDisplayPart>,
  // tags: Vec<JsDocTagInfo>,
}

impl SignatureHelpItem {
  pub fn into_signature_information(
    self,
    language_server: &language_server::Inner,
  ) -> lsp::SignatureInformation {
    let prefix_text =
      display_parts_to_string(&self.prefix_display_parts, language_server);
    let params_text = self
      .parameters
      .iter()
      .map(|param| {
        display_parts_to_string(&param.display_parts, language_server)
      })
      .collect::<Vec<String>>()
      .join(", ");
    let suffix_text =
      display_parts_to_string(&self.suffix_display_parts, language_server);
    let documentation =
      display_parts_to_string(&self.documentation, language_server);
    lsp::SignatureInformation {
      label: format!("{prefix_text}{params_text}{suffix_text}"),
      documentation: Some(lsp::Documentation::MarkupContent(
        lsp::MarkupContent {
          kind: lsp::MarkupKind::Markdown,
          value: documentation,
        },
      )),
      parameters: Some(
        self
          .parameters
          .into_iter()
          .map(|param| param.into_parameter_information(language_server))
          .collect(),
      ),
      active_parameter: None,
    }
  }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureHelpParameter {
  // name: String,
  documentation: Vec<SymbolDisplayPart>,
  display_parts: Vec<SymbolDisplayPart>,
  // is_optional: bool,
}

impl SignatureHelpParameter {
  pub fn into_parameter_information(
    self,
    language_server: &language_server::Inner,
  ) -> lsp::ParameterInformation {
    let documentation =
      display_parts_to_string(&self.documentation, language_server);
    lsp::ParameterInformation {
      label: lsp::ParameterLabel::Simple(display_parts_to_string(
        &self.display_parts,
        language_server,
      )),
      documentation: Some(lsp::Documentation::MarkupContent(
        lsp::MarkupContent {
          kind: lsp::MarkupKind::Markdown,
          value: documentation,
        },
      )),
    }
  }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionRange {
  text_span: TextSpan,
  #[serde(skip_serializing_if = "Option::is_none")]
  parent: Option<Box<SelectionRange>>,
}

impl SelectionRange {
  pub fn to_selection_range(
    &self,
    line_index: Arc<LineIndex>,
  ) -> lsp::SelectionRange {
    lsp::SelectionRange {
      range: self.text_span.to_range(line_index.clone()),
      parent: self.parent.as_ref().map(|parent_selection| {
        Box::new(parent_selection.to_selection_range(line_index))
      }),
    }
  }
}

#[derive(Debug, Clone, Deserialize)]
struct Response {
  // id: usize,
  data: Value,
}

#[derive(Debug, Default)]
pub struct TscSpecifierMap {
  normalized_specifiers: DashMap<String, ModuleSpecifier>,
  denormalized_specifiers: DashMap<ModuleSpecifier, String>,
}

impl TscSpecifierMap {
  pub fn new() -> Self {
    Self::default()
  }

  /// Convert the specifier to one compatible with tsc. Cache the resulting
  /// mapping in case it needs to be reversed.
  // TODO(nayeemrmn): Factor in out-of-band media type here.
  pub fn denormalize(&self, specifier: &ModuleSpecifier) -> String {
    let original = specifier;
    if let Some(specifier) = self.denormalized_specifiers.get(original) {
      return specifier.to_string();
    }
    let mut specifier = original.to_string();
    let media_type = MediaType::from_specifier(original);
    // If the URL-inferred media type doesn't correspond to tsc's path-inferred
    // media type, force it to be the same by appending an extension.
    if MediaType::from_path(Path::new(specifier.as_str())) != media_type {
      specifier += media_type.as_ts_extension();
    }
    if specifier != original.as_str() {
      self
        .normalized_specifiers
        .insert(specifier.clone(), original.clone());
    }
    specifier
  }

  /// Convert the specifier from one compatible with tsc. Cache the resulting
  /// mapping in case it needs to be reversed.
  pub fn normalize<S: AsRef<str>>(
    &self,
    specifier: S,
  ) -> Result<ModuleSpecifier, AnyError> {
    let original = specifier.as_ref();
    if let Some(specifier) = self.normalized_specifiers.get(original) {
      return Ok(specifier.clone());
    }
    let specifier_str = original.replace(".d.ts.d.ts", ".d.ts");
    let specifier = match ModuleSpecifier::parse(&specifier_str) {
      Ok(s) => s,
      Err(err) => return Err(err.into()),
    };
    if specifier.as_str() != original {
      self
        .denormalized_specifiers
        .insert(specifier.clone(), original.to_string());
    }
    Ok(specifier)
  }
}

// TODO(bartlomieju): we have similar struct in `cli/tsc/mod.rs` - maybe at least change
// the name of the struct to avoid confusion?
struct State {
  last_id: usize,
  performance: Arc<Performance>,
  response: Option<Response>,
  state_snapshot: Arc<StateSnapshot>,
  specifier_map: Arc<TscSpecifierMap>,
  project_version: Arc<AtomicUsize>,
  token: CancellationToken,
}

impl State {
  fn new(
    state_snapshot: Arc<StateSnapshot>,
    specifier_map: Arc<TscSpecifierMap>,
    performance: Arc<Performance>,
    project_version: Arc<AtomicUsize>,
  ) -> Self {
    Self {
      last_id: 1,
      performance,
      response: None,
      state_snapshot,
      specifier_map,
      project_version,
      token: Default::default(),
    }
  }

  fn get_asset_or_document(
    &self,
    specifier: &ModuleSpecifier,
  ) -> Option<AssetOrDocument> {
    let snapshot = &self.state_snapshot;
    if specifier.scheme() == "asset" {
      snapshot.assets.get(specifier).map(AssetOrDocument::Asset)
    } else {
      snapshot
        .documents
        .get(specifier)
        .map(AssetOrDocument::Document)
    }
  }

  fn script_version(&self, specifier: &ModuleSpecifier) -> Option<String> {
    if specifier.scheme() == "asset" {
      if self.state_snapshot.assets.contains_key(specifier) {
        Some("1".to_string())
      } else {
        None
      }
    } else {
      self
        .state_snapshot
        .documents
        .get(specifier)
        .map(|d| d.script_version())
    }
  }
}

#[op2(fast)]
fn op_is_cancelled(state: &mut OpState) -> bool {
  let state = state.borrow_mut::<State>();
  state.token.is_cancelled()
}

#[op2(fast)]
fn op_is_node_file(state: &mut OpState, #[string] path: String) -> bool {
  let state = state.borrow::<State>();
  let mark = state.performance.mark("tsc.op.op_is_node_file");
  let r = match ModuleSpecifier::parse(&path) {
    Ok(specifier) => state
      .state_snapshot
      .npm
      .as_ref()
      .map(|n| n.npm_resolver.in_npm_package(&specifier))
      .unwrap_or(false),
    Err(_) => false,
  };
  state.performance.measure(mark);
  r
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LoadResponse {
  data: Arc<str>,
  script_kind: i32,
  version: Option<String>,
}

#[op2]
fn op_load<'s>(
  scope: &'s mut v8::HandleScope,
  state: &mut OpState,
  #[string] specifier: &str,
) -> Result<v8::Local<'s, v8::Value>, AnyError> {
  let state = state.borrow_mut::<State>();
  let mark = state
    .performance
    .mark_with_args("tsc.op.op_load", specifier);
  let specifier = state.specifier_map.normalize(specifier)?;
  let maybe_load_response =
    if specifier.as_str() == "internal:///missing_dependency.d.ts" {
      None
    } else {
      let asset_or_document = state.get_asset_or_document(&specifier);
      asset_or_document.map(|doc| LoadResponse {
        data: doc.text(),
        script_kind: crate::tsc::as_ts_script_kind(doc.media_type()),
        version: state.script_version(&specifier),
      })
    };

  let serialized = serde_v8::to_v8(scope, maybe_load_response)?;

  state.performance.measure(mark);
  Ok(serialized)
}

#[op2]
fn op_resolve<'s>(
  scope: &'s mut v8::HandleScope,
  state: &mut OpState,
  #[serde] args: ResolveArgs,
) -> Result<v8::Local<'s, v8::Value>, AnyError> {
  let state = state.borrow_mut::<State>();
  let mark = state.performance.mark_with_args("tsc.op.op_resolve", &args);
  let referrer = state.specifier_map.normalize(&args.base)?;
  let specifiers = match state.get_asset_or_document(&referrer) {
    Some(referrer_doc) => {
      let resolved = state.state_snapshot.documents.resolve(
        args.specifiers,
        &referrer_doc,
        state.state_snapshot.npm.as_ref(),
      );
      resolved
        .into_iter()
        .map(|o| {
          o.map(|(s, mt)| {
            (
              state.specifier_map.denormalize(&s),
              mt.as_ts_extension().to_string(),
            )
          })
        })
        .collect()
    }
    None => {
      lsp_warn!(
        "Error resolving. Referring specifier \"{}\" was not found.",
        args.base
      );
      vec![None; args.specifiers.len()]
    }
  };

  let response = serde_v8::to_v8(scope, specifiers)?;
  state.performance.measure(mark);
  Ok(response)
}

#[op2]
fn op_respond(state: &mut OpState, #[serde] args: Response) {
  let state = state.borrow_mut::<State>();
  state.response = Some(args);
}

#[op2]
#[serde]
fn op_script_names(state: &mut OpState) -> Vec<String> {
  let state = state.borrow_mut::<State>();
  let mark = state.performance.mark("tsc.op.op_script_names");
  let documents = &state.state_snapshot.documents;
  let all_docs = documents.documents(DocumentsFilter::AllDiagnosable);
  let mut seen = HashSet::new();
  let mut result = Vec::new();

  if documents.has_injected_types_node_package() {
    // ensure this is first so it resolves the node types first
    let specifier = "asset:///node_types.d.ts";
    result.push(specifier.to_string());
    seen.insert(specifier);
  }

  // inject these next because they're global
  for import in documents.module_graph_imports() {
    if seen.insert(import.as_str()) {
      result.push(import.to_string());
    }
  }

  // finally include the documents and all their dependencies
  for doc in &all_docs {
    let specifiers = std::iter::once(doc.specifier()).chain(
      doc
        .dependencies()
        .values()
        .filter_map(|dep| dep.get_type().or_else(|| dep.get_code())),
    );
    for specifier in specifiers {
      if seen.insert(specifier.as_str()) {
        if let Some(specifier) = documents.resolve_specifier(specifier) {
          // only include dependencies we know to exist otherwise typescript will error
          if documents.exists(&specifier) {
            result.push(specifier.to_string());
          }
        }
      }
    }
  }

  let r = result
    .into_iter()
    .map(|s| match ModuleSpecifier::parse(&s) {
      Ok(s) => state.specifier_map.denormalize(&s),
      Err(_) => s,
    })
    .collect();
  state.performance.measure(mark);
  r
}

#[op2]
#[string]
fn op_script_version(
  state: &mut OpState,
  #[string] specifier: &str,
) -> Result<Option<String>, AnyError> {
  let state = state.borrow_mut::<State>();
  let mark = state.performance.mark("tsc.op.op_script_version");
  let specifier = state.specifier_map.normalize(specifier)?;
  let r = state.script_version(&specifier);
  state.performance.measure(mark);
  Ok(r)
}

#[op2]
#[string]
fn op_project_version(state: &mut OpState) -> String {
  let state = state.borrow_mut::<State>();
  let mark = state.performance.mark("tsc.op.op_project_version");
  let r = state.project_version.load(Ordering::Relaxed).to_string();
  state.performance.measure(mark);
  r
}

fn run_tsc_thread(
  mut request_rx: UnboundedReceiver<Request>,
  performance: Arc<Performance>,
  cache: Arc<dyn HttpCache>,
  specifier_map: Arc<TscSpecifierMap>,
  project_version: Arc<AtomicUsize>,
  maybe_inspector_server: Option<Arc<InspectorServer>>,
) {
  let has_inspector_server = maybe_inspector_server.is_some();
  // Create and setup a JsRuntime based on a snapshot. It is expected that the
  // supplied snapshot is an isolate that contains the TypeScript language
  // server.
  let mut tsc_runtime = JsRuntime::new(RuntimeOptions {
    extensions: vec![deno_tsc::init_ops(
      performance,
      cache,
      specifier_map,
      project_version,
    )],
    startup_snapshot: Some(tsc::compiler_snapshot()),
    inspector: maybe_inspector_server.is_some(),
    ..Default::default()
  });

  if let Some(server) = maybe_inspector_server {
    server.register_inspector(
      "ext:deno_tsc/99_main_compiler.js".to_string(),
      &mut tsc_runtime,
      false,
    );
  }

  let tsc_future = async {
    start_tsc(&mut tsc_runtime, false).unwrap();
    let (request_signal_tx, mut request_signal_rx) = mpsc::unbounded_channel::<()>();
    let tsc_runtime = Rc::new(tokio::sync::Mutex::new(tsc_runtime));
    let tsc_runtime_ = tsc_runtime.clone();
    let event_loop_fut = async {
      loop {
        if has_inspector_server {
          tsc_runtime_.lock().await.run_event_loop(PollEventLoopOptions {
            wait_for_inspector: false,
            pump_v8_message_loop: true,
          }).await.ok();
        }
        request_signal_rx.recv_many(&mut vec![], 1000).await;
      }
    };
    tokio::pin!(event_loop_fut);
    loop {
      tokio::select! {
        biased;
        (maybe_request, mut tsc_runtime) = async { (request_rx.recv().await, tsc_runtime.lock().await) } => {
          if let Some((req, state_snapshot, tx, token)) = maybe_request {
            let value = request(&mut tsc_runtime, state_snapshot, req, token.clone());
            request_signal_tx.send(()).unwrap();
            let was_sent = tx.send(value).is_ok();
            // Don't print the send error if the token is cancelled, it's expected
            // to fail in that case and this commonly occurs.
            if !was_sent && !token.is_cancelled() {
              lsp_warn!("Unable to send result to client.");
            }
          } else {
            break;
          }
        },
        _ = &mut event_loop_fut => {}
      }
    }
  }
  .boxed_local();

  let runtime = create_basic_runtime();
  runtime.block_on(tsc_future)
}

deno_core::extension!(deno_tsc,
  ops = [
    op_is_cancelled,
    op_is_node_file,
    op_load,
    op_resolve,
    op_respond,
    op_script_names,
    op_script_version,
    op_project_version,
  ],
  options = {
    performance: Arc<Performance>,
    cache: Arc<dyn HttpCache>,
    specifier_map: Arc<TscSpecifierMap>,
    project_version: Arc<AtomicUsize>,
  },
  state = |state, options| {
    state.put(State::new(
      Arc::new(StateSnapshot {
        assets: Default::default(),
        cache_metadata: CacheMetadata::new(options.cache.clone()),
        config: Default::default(),
        documents: Documents::new(options.cache.clone()),
        maybe_import_map: None,
        npm: None,
      }),
      options.specifier_map,
      options.performance,
      options.project_version,
    ));
  },
);

/// Instruct a language server runtime to start the language server and provide
/// it with a minimal bootstrap configuration.
fn start_tsc(runtime: &mut JsRuntime, debug: bool) -> Result<(), AnyError> {
  let init_config = json!({ "debug": debug });
  let init_src = format!("globalThis.serverInit({init_config});");

  runtime.execute_script(located_script_name!(), init_src.into())?;
  Ok(())
}

#[derive(Debug, Deserialize_repr, Serialize_repr)]
#[repr(u32)]
pub enum CompletionTriggerKind {
  Invoked = 1,
  TriggerCharacter = 2,
  TriggerForIncompleteCompletions = 3,
}

impl From<lsp::CompletionTriggerKind> for CompletionTriggerKind {
  fn from(kind: lsp::CompletionTriggerKind) -> Self {
    match kind {
      lsp::CompletionTriggerKind::INVOKED => Self::Invoked,
      lsp::CompletionTriggerKind::TRIGGER_CHARACTER => Self::TriggerCharacter,
      lsp::CompletionTriggerKind::TRIGGER_FOR_INCOMPLETE_COMPLETIONS => {
        Self::TriggerForIncompleteCompletions
      }
      _ => Self::Invoked,
    }
  }
}

pub type QuotePreference = config::QuoteStyle;

pub type ImportModuleSpecifierPreference = config::ImportModuleSpecifier;

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub enum ImportModuleSpecifierEnding {
  Auto,
  Minimal,
  Index,
  Js,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub enum IncludeInlayParameterNameHints {
  None,
  Literals,
  All,
}

impl From<&config::InlayHintsParamNamesEnabled>
  for IncludeInlayParameterNameHints
{
  fn from(setting: &config::InlayHintsParamNamesEnabled) -> Self {
    match setting {
      config::InlayHintsParamNamesEnabled::All => Self::All,
      config::InlayHintsParamNamesEnabled::Literals => Self::Literals,
      config::InlayHintsParamNamesEnabled::None => Self::None,
    }
  }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
pub enum IncludePackageJsonAutoImports {
  Auto,
  On,
  Off,
}

pub type JsxAttributeCompletionStyle = config::JsxAttributeCompletionStyle;

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetCompletionsAtPositionOptions {
  #[serde(flatten)]
  pub user_preferences: UserPreferences,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub trigger_character: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub trigger_kind: Option<CompletionTriggerKind>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserPreferences {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub disable_suggestions: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub quote_preference: Option<QuotePreference>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_completions_for_module_exports: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_completions_for_import_statements: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_completions_with_snippet_text: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_automatic_optional_chain_completions: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_completions_with_insert_text: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_completions_with_class_member_snippets: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_completions_with_object_literal_method_snippets: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub use_label_details_in_completion_entries: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub allow_incomplete_completions: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub import_module_specifier_preference:
    Option<ImportModuleSpecifierPreference>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub import_module_specifier_ending: Option<ImportModuleSpecifierEnding>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub allow_text_changes_in_new_files: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub provide_prefix_and_suffix_text_for_rename: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_package_json_auto_imports: Option<IncludePackageJsonAutoImports>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub provide_refactor_not_applicable_reason: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub jsx_attribute_completion_style: Option<JsxAttributeCompletionStyle>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_inlay_parameter_name_hints:
    Option<IncludeInlayParameterNameHints>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_inlay_parameter_name_hints_when_argument_matches_name:
    Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_inlay_function_parameter_type_hints: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_inlay_variable_type_hints: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_inlay_variable_type_hints_when_type_matches_name: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_inlay_property_declaration_type_hints: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_inlay_function_like_return_type_hints: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub include_inlay_enum_member_value_hints: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub allow_rename_of_import_path: Option<bool>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub auto_import_file_exclude_patterns: Option<Vec<String>>,
}

impl UserPreferences {
  pub fn from_config_for_specifier(
    config: &config::Config,
    fmt_config: &FmtOptionsConfig,
    specifier: &ModuleSpecifier,
  ) -> Self {
    let base_preferences = Self {
      allow_incomplete_completions: Some(true),
      allow_text_changes_in_new_files: Some(specifier.scheme() == "file"),
      // TODO(nayeemrmn): Investigate why we use `Index` here.
      import_module_specifier_ending: Some(ImportModuleSpecifierEnding::Index),
      include_completions_with_snippet_text: Some(
        config.client_capabilities.snippet_support,
      ),
      provide_refactor_not_applicable_reason: Some(true),
      quote_preference: Some(fmt_config.into()),
      use_label_details_in_completion_entries: Some(true),
      ..Default::default()
    };
    let Some(language_settings) =
      config.language_settings_for_specifier(specifier)
    else {
      return base_preferences;
    };
    Self {
      auto_import_file_exclude_patterns: Some(
        language_settings
          .preferences
          .auto_import_file_exclude_patterns
          .clone(),
      ),
      include_automatic_optional_chain_completions: Some(
        language_settings.suggest.enabled
          && language_settings
            .suggest
            .include_automatic_optional_chain_completions,
      ),
      include_completions_for_import_statements: Some(
        language_settings.suggest.enabled
          && language_settings
            .suggest
            .include_completions_for_import_statements,
      ),
      include_completions_for_module_exports: Some(
        language_settings.suggest.enabled
          && language_settings.suggest.auto_imports,
      ),
      include_completions_with_class_member_snippets: Some(
        language_settings.suggest.enabled
          && language_settings.suggest.class_member_snippets.enabled
          && config.client_capabilities.snippet_support,
      ),
      include_completions_with_insert_text: Some(
        language_settings.suggest.enabled,
      ),
      include_completions_with_object_literal_method_snippets: Some(
        language_settings.suggest.enabled
          && language_settings
            .suggest
            .object_literal_method_snippets
            .enabled
          && config.client_capabilities.snippet_support,
      ),
      import_module_specifier_preference: Some(
        language_settings.preferences.import_module_specifier,
      ),
      include_inlay_parameter_name_hints: Some(
        (&language_settings.inlay_hints.parameter_names.enabled).into(),
      ),
      include_inlay_parameter_name_hints_when_argument_matches_name: Some(
        !language_settings
          .inlay_hints
          .parameter_names
          .suppress_when_argument_matches_name,
      ),
      include_inlay_function_parameter_type_hints: Some(
        language_settings.inlay_hints.parameter_types.enabled,
      ),
      include_inlay_variable_type_hints: Some(
        language_settings.inlay_hints.variable_types.enabled,
      ),
      include_inlay_variable_type_hints_when_type_matches_name: Some(
        !language_settings
          .inlay_hints
          .variable_types
          .suppress_when_type_matches_name,
      ),
      include_inlay_property_declaration_type_hints: Some(
        language_settings
          .inlay_hints
          .property_declaration_types
          .enabled,
      ),
      include_inlay_function_like_return_type_hints: Some(
        language_settings
          .inlay_hints
          .function_like_return_types
          .enabled,
      ),
      include_inlay_enum_member_value_hints: Some(
        language_settings.inlay_hints.enum_member_values.enabled,
      ),
      jsx_attribute_completion_style: Some(
        language_settings.preferences.jsx_attribute_completion_style,
      ),
      provide_prefix_and_suffix_text_for_rename: Some(
        language_settings.preferences.use_aliases_for_renames,
      ),
      // Only use workspace settings for quote style if there's no `deno.json`.
      quote_preference: if config.has_config_file() {
        base_preferences.quote_preference
      } else {
        Some(language_settings.preferences.quote_style)
      },
      ..base_preferences
    }
  }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureHelpItemsOptions {
  #[serde(skip_serializing_if = "Option::is_none")]
  pub trigger_reason: Option<SignatureHelpTriggerReason>,
}

#[derive(Debug, Serialize)]
pub enum SignatureHelpTriggerKind {
  #[serde(rename = "characterTyped")]
  CharacterTyped,
  #[serde(rename = "invoked")]
  Invoked,
  #[serde(rename = "retrigger")]
  Retrigger,
  #[serde(rename = "unknown")]
  Unknown,
}

impl From<lsp::SignatureHelpTriggerKind> for SignatureHelpTriggerKind {
  fn from(kind: lsp::SignatureHelpTriggerKind) -> Self {
    match kind {
      lsp::SignatureHelpTriggerKind::INVOKED => Self::Invoked,
      lsp::SignatureHelpTriggerKind::TRIGGER_CHARACTER => Self::CharacterTyped,
      lsp::SignatureHelpTriggerKind::CONTENT_CHANGE => Self::Retrigger,
      _ => Self::Unknown,
    }
  }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureHelpTriggerReason {
  pub kind: SignatureHelpTriggerKind,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub trigger_character: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GetCompletionDetailsArgs {
  pub specifier: ModuleSpecifier,
  pub position: u32,
  pub name: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub format_code_settings: Option<FormatCodeSettings>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub source: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub preferences: Option<UserPreferences>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub data: Option<Value>,
}

impl From<&CompletionItemData> for GetCompletionDetailsArgs {
  fn from(item_data: &CompletionItemData) -> Self {
    Self {
      specifier: item_data.specifier.clone(),
      position: item_data.position,
      name: item_data.name.clone(),
      source: item_data.source.clone(),
      preferences: None,
      format_code_settings: None,
      data: item_data.data.clone(),
    }
  }
}

#[derive(Debug)]
pub struct GetNavigateToItemsArgs {
  pub search: String,
  pub max_result_count: Option<u32>,
  pub file: Option<String>,
}

#[derive(Clone, Debug)]
struct TscRequest {
  method: &'static str,
  args: Value,
}

/// Send a request into a runtime and return the JSON value of the response.
fn request(
  runtime: &mut JsRuntime,
  state_snapshot: Arc<StateSnapshot>,
  request: TscRequest,
  token: CancellationToken,
) -> Result<Value, AnyError> {
  if token.is_cancelled() {
    return Err(anyhow!("Operation was cancelled."));
  }
  let (performance, id) = {
    let op_state = runtime.op_state();
    let mut op_state = op_state.borrow_mut();
    let state = op_state.borrow_mut::<State>();
    state.state_snapshot = state_snapshot;
    state.token = token;
    state.last_id += 1;
    let id = state.last_id;
    (state.performance.clone(), id)
  };
  let mark = performance.mark_with_args(
    format!("tsc.host.{}", request.method),
    request.args.clone(),
  );
  assert!(
    request.args.is_array(),
    "Internal error: expected args to be array"
  );
  let request_src = format!(
    "globalThis.serverRequest({id}, \"{}\", {});",
    request.method, &request.args
  );
  runtime.execute_script(located_script_name!(), request_src.into())?;

  let op_state = runtime.op_state();
  let mut op_state = op_state.borrow_mut();
  let state = op_state.borrow_mut::<State>();

  performance.measure(mark);
  if let Some(response) = state.response.take() {
    Ok(response.data)
  } else {
    Err(custom_error(
      "RequestError",
      "The response was not received for the request.",
    ))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cache::GlobalHttpCache;
  use crate::cache::HttpCache;
  use crate::cache::RealDenoCacheEnv;
  use crate::http_util::HeadersMap;
  use crate::lsp::cache::CacheMetadata;
  use crate::lsp::config::WorkspaceSettings;
  use crate::lsp::documents::Documents;
  use crate::lsp::documents::LanguageId;
  use crate::lsp::text::LineIndex;
  use pretty_assertions::assert_eq;
  use std::path::Path;
  use test_util::TempDir;

  fn mock_state_snapshot(
    fixtures: &[(&str, &str, i32, LanguageId)],
    location: &Path,
  ) -> StateSnapshot {
    let cache = Arc::new(GlobalHttpCache::new(
      location.to_path_buf(),
      RealDenoCacheEnv,
    ));
    let mut documents = Documents::new(cache.clone());
    for (specifier, source, version, language_id) in fixtures {
      let specifier =
        resolve_url(specifier).expect("failed to create specifier");
      documents.open(
        specifier.clone(),
        *version,
        *language_id,
        (*source).into(),
      );
    }
    StateSnapshot {
      documents,
      assets: Default::default(),
      cache_metadata: CacheMetadata::new(cache),
      config: Default::default(),
      maybe_import_map: None,
      npm: None,
    }
  }

  async fn setup(
    temp_dir: &TempDir,
    config: Value,
    sources: &[(&str, &str, i32, LanguageId)],
  ) -> (TsServer, Arc<StateSnapshot>, Arc<GlobalHttpCache>) {
    let location = temp_dir.path().join("deps").to_path_buf();
    let cache =
      Arc::new(GlobalHttpCache::new(location.clone(), RealDenoCacheEnv));
    let snapshot = Arc::new(mock_state_snapshot(sources, &location));
    let performance = Arc::new(Performance::default());
    let ts_server = TsServer::new(performance, cache.clone());
    ts_server.start(None);
    let ts_config = TsConfig::new(config);
    assert!(ts_server
      .configure(snapshot.clone(), ts_config,)
      .await
      .unwrap());
    (ts_server, snapshot, cache)
  }

  #[test]
  fn test_replace_links() {
    let actual = replace_links(r"test {@link http://deno.land/x/mod.ts} test");
    assert_eq!(
      actual,
      r"test [http://deno.land/x/mod.ts](http://deno.land/x/mod.ts) test"
    );
    let actual =
      replace_links(r"test {@link http://deno.land/x/mod.ts a link} test");
    assert_eq!(actual, r"test [a link](http://deno.land/x/mod.ts) test");
    let actual =
      replace_links(r"test {@linkcode http://deno.land/x/mod.ts a link} test");
    assert_eq!(actual, r"test [`a link`](http://deno.land/x/mod.ts) test");
  }

  #[tokio::test]
  async fn test_project_configure() {
    let temp_dir = TempDir::new();
    setup(
      &temp_dir,
      json!({
        "target": "esnext",
        "module": "esnext",
        "noEmit": true,
      }),
      &[],
    )
    .await;
  }

  #[tokio::test]
  async fn test_project_reconfigure() {
    let temp_dir = TempDir::new();
    let (ts_server, snapshot, _) = setup(
      &temp_dir,
      json!({
        "target": "esnext",
        "module": "esnext",
        "noEmit": true,
      }),
      &[],
    )
    .await;
    let ts_config = TsConfig::new(json!({
      "target": "esnext",
      "module": "esnext",
      "noEmit": true,
      "lib": ["deno.ns", "deno.worker"]
    }));
    assert!(ts_server.configure(snapshot, ts_config).await.unwrap());
  }

  #[tokio::test]
  async fn test_get_diagnostics() {
    let temp_dir = TempDir::new();
    let (ts_server, snapshot, _) = setup(
      &temp_dir,
      json!({
        "target": "esnext",
        "module": "esnext",
        "noEmit": true,
      }),
      &[(
        "file:///a.ts",
        r#"console.log("hello deno");"#,
        1,
        LanguageId::TypeScript,
      )],
    )
    .await;
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let diagnostics = ts_server
      .get_diagnostics(snapshot, vec![specifier], Default::default())
      .await
      .unwrap();
    assert_eq!(
      json!(diagnostics),
      json!({
        "file:///a.ts": [
          {
            "start": {
              "line": 0,
              "character": 0,
            },
            "end": {
              "line": 0,
              "character": 7
            },
            "fileName": "file:///a.ts",
            "messageText": "Cannot find name 'console'. Do you need to change your target library? Try changing the \'lib\' compiler option to include 'dom'.",
            "sourceLine": "console.log(\"hello deno\");",
            "category": 1,
            "code": 2584
          }
        ]
      })
    );
  }

  #[tokio::test]
  async fn test_get_diagnostics_lib() {
    let temp_dir = TempDir::new();
    let (ts_server, snapshot, _) = setup(
      &temp_dir,
      json!({
        "target": "esnext",
        "module": "esnext",
        "jsx": "react",
        "lib": ["esnext", "dom", "deno.ns"],
        "noEmit": true,
      }),
      &[(
        "file:///a.ts",
        r#"console.log(document.location);"#,
        1,
        LanguageId::TypeScript,
      )],
    )
    .await;
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let diagnostics = ts_server
      .get_diagnostics(snapshot, vec![specifier], Default::default())
      .await
      .unwrap();
    assert_eq!(json!(diagnostics), json!({ "file:///a.ts": [] }));
  }

  #[tokio::test]
  async fn test_module_resolution() {
    let temp_dir = TempDir::new();
    let (ts_server, snapshot, _) = setup(
      &temp_dir,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      &[(
        "file:///a.ts",
        r#"
        import { B } from "https://deno.land/x/b/mod.ts";

        const b = new B();

        console.log(b);
      "#,
        1,
        LanguageId::TypeScript,
      )],
    )
    .await;
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let diagnostics = ts_server
      .get_diagnostics(snapshot, vec![specifier], Default::default())
      .await
      .unwrap();
    assert_eq!(json!(diagnostics), json!({ "file:///a.ts": [] }));
  }

  #[tokio::test]
  async fn test_bad_module_specifiers() {
    let temp_dir = TempDir::new();
    let (ts_server, snapshot, _) = setup(
      &temp_dir,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      &[(
        "file:///a.ts",
        r#"
        import { A } from ".";
        "#,
        1,
        LanguageId::TypeScript,
      )],
    )
    .await;
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let diagnostics = ts_server
      .get_diagnostics(snapshot, vec![specifier], Default::default())
      .await
      .unwrap();
    assert_eq!(
      json!(diagnostics),
      json!({
        "file:///a.ts": [{
          "start": {
            "line": 1,
            "character": 8
          },
          "end": {
            "line": 1,
            "character": 30
          },
          "fileName": "file:///a.ts",
          "messageText": "\'A\' is declared but its value is never read.",
          "sourceLine": "        import { A } from \".\";",
          "category": 2,
          "code": 6133,
        }]
      })
    );
  }

  #[tokio::test]
  async fn test_remote_modules() {
    let temp_dir = TempDir::new();
    let (ts_server, snapshot, _) = setup(
      &temp_dir,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      &[(
        "file:///a.ts",
        r#"
        import { B } from "https://deno.land/x/b/mod.ts";

        const b = new B();

        console.log(b);
      "#,
        1,
        LanguageId::TypeScript,
      )],
    )
    .await;
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let diagnostics = ts_server
      .get_diagnostics(snapshot, vec![specifier], Default::default())
      .await
      .unwrap();
    assert_eq!(json!(diagnostics), json!({ "file:///a.ts": [] }));
  }

  #[tokio::test]
  async fn test_partial_modules() {
    let temp_dir = TempDir::new();
    let (ts_server, snapshot, _) = setup(
      &temp_dir,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      &[(
        "file:///a.ts",
        r#"
        import {
          Application,
          Context,
          Router,
          Status,
        } from "https://deno.land/x/oak@v6.3.2/mod.ts";

        import * as test from
      "#,
        1,
        LanguageId::TypeScript,
      )],
    )
    .await;
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let diagnostics = ts_server
      .get_diagnostics(snapshot, vec![specifier], Default::default())
      .await
      .unwrap();
    assert_eq!(
      json!(diagnostics),
      json!({
        "file:///a.ts": [{
          "start": {
            "line": 1,
            "character": 8
          },
          "end": {
            "line": 6,
            "character": 55,
          },
          "fileName": "file:///a.ts",
          "messageText": "All imports in import declaration are unused.",
          "sourceLine": "        import {",
          "category": 2,
          "code": 6192,
        }, {
          "start": {
            "line": 8,
            "character": 29
          },
          "end": {
            "line": 8,
            "character": 29
          },
          "fileName": "file:///a.ts",
          "messageText": "Expression expected.",
          "sourceLine": "        import * as test from",
          "category": 1,
          "code": 1109
        }]
      })
    );
  }

  #[tokio::test]
  async fn test_no_debug_failure() {
    let temp_dir = TempDir::new();
    let (ts_server, snapshot, _) = setup(
      &temp_dir,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      &[(
        "file:///a.ts",
        r#"const url = new URL("b.js", import."#,
        1,
        LanguageId::TypeScript,
      )],
    )
    .await;
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let diagnostics = ts_server
      .get_diagnostics(snapshot, vec![specifier], Default::default())
      .await
      .unwrap();
    assert_eq!(
      json!(diagnostics),
      json!({
        "file:///a.ts": [
          {
            "start": {
              "line": 0,
              "character": 35,
            },
            "end": {
              "line": 0,
              "character": 35
            },
            "fileName": "file:///a.ts",
            "messageText": "Identifier expected.",
            "sourceLine": "const url = new URL(\"b.js\", import.",
            "category": 1,
            "code": 1003,
          }
        ]
      })
    );
  }

  #[tokio::test]
  async fn test_request_assets() {
    let temp_dir = TempDir::new();
    let (ts_server, snapshot, _) = setup(&temp_dir, json!({}), &[]).await;
    let assets = get_isolate_assets(&ts_server, snapshot).await;
    let mut asset_names = assets
      .iter()
      .map(|a| {
        a.specifier()
          .to_string()
          .replace("asset:///lib.", "")
          .replace(".d.ts", "")
      })
      .collect::<Vec<_>>();
    let mut expected_asset_names: Vec<String> = serde_json::from_str(
      include_str!(concat!(env!("OUT_DIR"), "/lib_file_names.json")),
    )
    .unwrap();
    asset_names.sort();

    expected_asset_names.sort();
    assert_eq!(asset_names, expected_asset_names);

    // get some notification when the size of the assets grows
    let mut total_size = 0;
    for asset in assets {
      total_size += asset.text().len();
    }
    assert!(total_size > 0);
    assert!(total_size < 2_000_000); // currently as of TS 4.6, it's 0.7MB
  }

  #[tokio::test]
  async fn test_modify_sources() {
    let temp_dir = TempDir::new();
    let (ts_server, snapshot, cache) = setup(
      &temp_dir,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      &[(
        "file:///a.ts",
        r#"
          import * as a from "https://deno.land/x/example/a.ts";
          if (a.a === "b") {
            console.log("fail");
          }
        "#,
        1,
        LanguageId::TypeScript,
      )],
    )
    .await;
    let specifier_dep =
      resolve_url("https://deno.land/x/example/a.ts").unwrap();
    cache
      .set(
        &specifier_dep,
        HeadersMap::default(),
        b"export const b = \"b\";\n",
      )
      .unwrap();
    let specifier = resolve_url("file:///a.ts").unwrap();
    let diagnostics = ts_server
      .get_diagnostics(snapshot.clone(), vec![specifier], Default::default())
      .await
      .unwrap();
    assert_eq!(
      json!(diagnostics),
      json!({
        "file:///a.ts": [
          {
            "start": {
              "line": 2,
              "character": 16,
            },
            "end": {
              "line": 2,
              "character": 17
            },
            "fileName": "file:///a.ts",
            "messageText": "Property \'a\' does not exist on type \'typeof import(\"https://deno.land/x/example/a\")\'.",
            "sourceLine": "          if (a.a === \"b\") {",
            "code": 2339,
            "category": 1,
          }
        ]
      })
    );
    cache
      .set(
        &specifier_dep,
        HeadersMap::default(),
        b"export const b = \"b\";\n\nexport const a = \"b\";\n",
      )
      .unwrap();
    ts_server.increment_project_version();
    let specifier = resolve_url("file:///a.ts").unwrap();
    let diagnostics = ts_server
      .get_diagnostics(snapshot.clone(), vec![specifier], Default::default())
      .await
      .unwrap();
    assert_eq!(
      json!(diagnostics),
      json!({
        "file:///a.ts": []
      })
    );
  }

  #[test]
  fn test_completion_entry_filter_text() {
    let fixture = CompletionEntry {
      kind: ScriptElementKind::MemberVariableElement,
      name: "['foo']".to_string(),
      insert_text: Some("['foo']".to_string()),
      ..Default::default()
    };
    let actual = fixture.get_filter_text();
    assert_eq!(actual, Some(".foo".to_string()));

    let fixture = CompletionEntry {
      kind: ScriptElementKind::MemberVariableElement,
      name: "#abc".to_string(),
      ..Default::default()
    };
    let actual = fixture.get_filter_text();
    assert_eq!(actual, None);

    let fixture = CompletionEntry {
      kind: ScriptElementKind::MemberVariableElement,
      name: "#abc".to_string(),
      insert_text: Some("this.#abc".to_string()),
      ..Default::default()
    };
    let actual = fixture.get_filter_text();
    assert_eq!(actual, Some("abc".to_string()));
  }

  #[tokio::test]
  async fn test_completions() {
    let fixture = r#"
      import { B } from "https://deno.land/x/b/mod.ts";

      const b = new B();

      console.
    "#;
    let line_index = LineIndex::new(fixture);
    let position = line_index
      .offset_tsc(lsp::Position {
        line: 5,
        character: 16,
      })
      .unwrap();
    let temp_dir = TempDir::new();
    let (ts_server, snapshot, _) = setup(
      &temp_dir,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      &[("file:///a.ts", fixture, 1, LanguageId::TypeScript)],
    )
    .await;
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let info = ts_server
      .get_completions(
        snapshot.clone(),
        specifier.clone(),
        position,
        GetCompletionsAtPositionOptions {
          user_preferences: UserPreferences {
            include_completions_with_insert_text: Some(true),
            ..Default::default()
          },
          trigger_character: Some(".".to_string()),
          trigger_kind: None,
        },
        Default::default(),
      )
      .await
      .unwrap();
    assert_eq!(info.entries.len(), 22);
    let details = ts_server
      .get_completion_details(
        snapshot.clone(),
        GetCompletionDetailsArgs {
          specifier,
          position,
          name: "log".to_string(),
          format_code_settings: None,
          source: None,
          preferences: None,
          data: None,
        },
      )
      .await
      .unwrap()
      .unwrap();
    assert_eq!(
      json!(details),
      json!({
        "name": "log",
        "kindModifiers": "declare",
        "kind": "method",
        "displayParts": [
          {
            "text": "(",
            "kind": "punctuation"
          },
          {
            "text": "method",
            "kind": "text"
          },
          {
            "text": ")",
            "kind": "punctuation"
          },
          {
            "text": " ",
            "kind": "space"
          },
          {
            "text": "Console",
            "kind": "interfaceName"
          },
          {
            "text": ".",
            "kind": "punctuation"
          },
          {
            "text": "log",
            "kind": "methodName"
          },
          {
            "text": "(",
            "kind": "punctuation"
          },
          {
            "text": "...",
            "kind": "punctuation"
          },
          {
            "text": "data",
            "kind": "parameterName"
          },
          {
            "text": ":",
            "kind": "punctuation"
          },
          {
            "text": " ",
            "kind": "space"
          },
          {
            "text": "any",
            "kind": "keyword"
          },
          {
            "text": "[",
            "kind": "punctuation"
          },
          {
            "text": "]",
            "kind": "punctuation"
          },
          {
            "text": ")",
            "kind": "punctuation"
          },
          {
            "text": ":",
            "kind": "punctuation"
          },
          {
            "text": " ",
            "kind": "space"
          },
          {
            "text": "void",
            "kind": "keyword"
          }
        ],
        "documentation": []
      })
    );
  }

  #[tokio::test]
  async fn test_completions_fmt() {
    let fixture_a = r#"
      console.log(someLongVaria)
    "#;
    let fixture_b = r#"
      export const someLongVariable = 1
    "#;
    let line_index = LineIndex::new(fixture_a);
    let position = line_index
      .offset_tsc(lsp::Position {
        line: 1,
        character: 33,
      })
      .unwrap();
    let temp_dir = TempDir::new();
    let (ts_server, snapshot, _) = setup(
      &temp_dir,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      &[
        ("file:///a.ts", fixture_a, 1, LanguageId::TypeScript),
        ("file:///b.ts", fixture_b, 1, LanguageId::TypeScript),
      ],
    )
    .await;
    let specifier = resolve_url("file:///a.ts").expect("could not resolve url");
    let fmt_options_config = FmtOptionsConfig {
      semi_colons: Some(false),
      single_quote: Some(true),
      ..Default::default()
    };
    let info = ts_server
      .get_completions(
        snapshot.clone(),
        specifier.clone(),
        position,
        GetCompletionsAtPositionOptions {
          user_preferences: UserPreferences {
            quote_preference: Some((&fmt_options_config).into()),
            include_completions_for_module_exports: Some(true),
            include_completions_with_insert_text: Some(true),
            ..Default::default()
          },
          ..Default::default()
        },
        FormatCodeSettings::from(&fmt_options_config),
      )
      .await
      .unwrap();
    let entry = info
      .entries
      .iter()
      .find(|e| &e.name == "someLongVariable")
      .unwrap();
    let details = ts_server
      .get_completion_details(
        snapshot.clone(),
        GetCompletionDetailsArgs {
          specifier,
          position,
          name: entry.name.clone(),
          format_code_settings: Some(FormatCodeSettings::from(
            &fmt_options_config,
          )),
          source: entry.source.clone(),
          preferences: Some(UserPreferences {
            quote_preference: Some((&fmt_options_config).into()),
            ..Default::default()
          }),
          data: entry.data.clone(),
        },
      )
      .await
      .unwrap()
      .unwrap();
    let actions = details.code_actions.unwrap();
    let action = actions
      .iter()
      .find(|a| &a.description == r#"Add import from "./b.ts""#)
      .unwrap();
    let changes = action.changes.first().unwrap();
    let change = changes.text_changes.first().unwrap();
    assert_eq!(
      change.new_text,
      "import { someLongVariable } from './b.ts'\n"
    );
  }

  #[tokio::test]
  async fn test_get_edits_for_file_rename() {
    let temp_dir = TempDir::new();
    let (ts_server, snapshot, _) = setup(
      &temp_dir,
      json!({
        "target": "esnext",
        "module": "esnext",
        "lib": ["deno.ns", "deno.window"],
        "noEmit": true,
      }),
      &[
        (
          "file:///a.ts",
          r#"import "./b.ts";"#,
          1,
          LanguageId::TypeScript,
        ),
        ("file:///b.ts", r#""#, 1, LanguageId::TypeScript),
      ],
    )
    .await;
    let changes = ts_server
      .get_edits_for_file_rename(
        snapshot,
        resolve_url("file:///b.ts").unwrap(),
        resolve_url("file:///c.ts").unwrap(),
        FormatCodeSettings::default(),
        UserPreferences::default(),
      )
      .await
      .unwrap();
    assert_eq!(
      changes,
      vec![FileTextChanges {
        file_name: "file:///a.ts".to_string(),
        text_changes: vec![TextChange {
          span: TextSpan {
            start: 8,
            length: 6,
          },
          new_text: "./c.ts".to_string(),
        }],
        is_new_file: None,
      }]
    );
  }

  #[test]
  fn include_suppress_inlay_hint_settings() {
    let mut settings = WorkspaceSettings::default();
    settings
      .typescript
      .inlay_hints
      .parameter_names
      .suppress_when_argument_matches_name = true;
    settings
      .typescript
      .inlay_hints
      .variable_types
      .suppress_when_type_matches_name = true;
    let mut config = config::Config::new();
    config.set_workspace_settings(settings, None);
    let user_preferences = UserPreferences::from_config_for_specifier(
      &config,
      &Default::default(),
      &ModuleSpecifier::parse("file:///foo.ts").unwrap(),
    );
    assert_eq!(
      user_preferences.include_inlay_variable_type_hints_when_type_matches_name,
      Some(false)
    );
    assert_eq!(
      user_preferences
        .include_inlay_parameter_name_hints_when_argument_matches_name,
      Some(false)
    );
  }
}
