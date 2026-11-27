//! Language server: diagnostics, go to definition, hover, find references, rename, outline,
//! semantic tokens, completion; for `.qd` files and for interface files (`.qdi`), whose names
//! and expressions are checked with the code (quadra_qdi::check): a field renamed in a `.qd`
//! file is an error in the `.qdi` files that use it, at once, and renaming it renames it there.

mod line_index;
pub mod qdi;
mod semantic;

use line_index::LineIndex;
use quadra_sema::{ide, Analysis, FileId, Project, Severity};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

/// More diagnostics of a checked project, from outside the language server (the hints of how
/// the code meets the database: `quadra_store::hints`).
pub type Extra = fn(&quadra_analysis::Checked) -> Vec<quadra_sema::Diagnostic>;

pub fn run() {
    run_with(None)
}

/// The server, with the diagnostics of `extra` added to those of the analysis.
pub fn run_with(extra: Option<Extra>) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async {
        let (service, socket) = LspService::new(move |client| Backend::new(client, extra));
        Server::new(tokio::io::stdin(), tokio::io::stdout(), socket)
            .serve(service)
            .await;
    });
}

struct Snapshot {
    /// the project (with the scripts generated for its interface files, if any)
    project: Project,
    /// its analysis, with the references the type checker resolved (`o.total`) and those of
    /// the interface files
    analysis: Analysis,
    /// diagnostics of every phase: names, types, cycles, flows, certified metrics, interfaces
    diagnostics: Vec<quadra_sema::Diagnostic>,
    /// the generated scripts (not files of the editor)
    generated: std::collections::HashSet<FileId>,
    /// the parameters of the app of each interface file
    params: HashMap<FileId, Vec<(String, Option<String>)>>,
    /// entity → (fields, actions), for completion
    entities: qdi::Entities,
}

struct Backend {
    client: Client,
    /// Unsaved contents of open documents.
    docs: Mutex<HashMap<PathBuf, String>>,
    /// Analysis per project root, dropped when any document of the project changes.
    cache: Mutex<HashMap<PathBuf, Arc<Snapshot>>>,
    extra: Option<Extra>,
}

fn normalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn path_of(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok().map(|p| normalize(&p))
}

fn project_root(path: &Path) -> PathBuf {
    quadra_sema::find_project_root(path)
        .unwrap_or_else(|| path.parent().unwrap_or(path).to_path_buf())
}

impl Backend {
    fn new(client: Client, extra: Option<Extra>) -> Backend {
        Backend {
            client,
            docs: Mutex::new(HashMap::new()),
            cache: Mutex::new(HashMap::new()),
            extra,
        }
    }

    fn snapshot(&self, path: &Path) -> Arc<Snapshot> {
        let root = project_root(path);
        if let Some(s) = self.cache.lock().unwrap().get(&root) {
            return s.clone();
        }
        let overlays = self.docs.lock().unwrap().clone();
        let project = Project::load_with_overlays(&root, &overlays).unwrap_or_else(|_| {
            let sources = overlays
                .into_iter()
                .filter(|(p, _)| p.starts_with(&root))
                .collect();
            Project::from_sources(&root, sources)
        });
        let mut checked = quadra_analysis::check(&project);
        let mut diagnostics = checked.diagnostics.clone();
        if let Some(extra) = self.extra {
            diagnostics.extend(extra(&checked));
        }
        let entities: qdi::Entities = checked
            .model
            .as_ref()
            .map(|m| {
                m.entities
                    .iter()
                    .map(|e| {
                        (
                            e.name.clone(),
                            (
                                e.fields.iter().map(|f| f.name.clone()).collect(),
                                e.actions
                                    .iter()
                                    .map(|a| a.name.clone())
                                    .chain(e.flows.iter().flat_map(|f| {
                                        f.transitions.iter().filter_map(|t| t.by.clone())
                                    }))
                                    .collect(),
                            ),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let snap = match checked.interfaces.take() {
            Some(i) => {
                let generated = i.generated.keys().copied().collect();
                Snapshot {
                    project: i.project,
                    analysis: i.analysis,
                    diagnostics,
                    generated,
                    params: i.params,
                    entities,
                }
            }
            None => {
                let mut analysis = project.analyze();
                // the members the type checker resolved
                for (span, (e, m)) in &checked.report.accesses {
                    if let Some((d, k)) = analysis
                        .find_anywhere(e)
                        .and_then(|d| analysis.lookup_member(d, m))
                    {
                        if !analysis
                            .references
                            .iter()
                            .any(|r| r.file == span.file && r.range == span.range)
                        {
                            analysis.references.push(quadra_sema::Reference {
                                file: span.file,
                                range: span.range,
                                target: quadra_sema::Target::Member(d, k),
                            });
                        }
                    }
                }
                Snapshot {
                    project,
                    analysis,
                    diagnostics,
                    generated: Default::default(),
                    params: HashMap::new(),
                    entities,
                }
            }
        };
        let snap = Arc::new(snap);
        self.cache.lock().unwrap().insert(root, snap.clone());
        snap
    }

    fn invalidate(&self, path: &Path) {
        let root = project_root(path);
        self.cache.lock().unwrap().remove(&root);
    }

    /// Run the analysis for the project containing `path` and publish diagnostics for
    /// every file of the project (an empty list clears old ones).
    async fn refresh(&self, path: &Path) {
        self.invalidate(path);
        let snap = self.snapshot(path);
        let mut by_file: HashMap<FileId, Vec<Diagnostic>> = HashMap::new();
        for f in snap
            .project
            .files()
            .filter(|f| !snap.generated.contains(&f.id) && f.kind != quadra_sema::FileKind::Std)
        {
            by_file.insert(f.id, Vec::new());
        }
        for d in &snap.diagnostics {
            if snap.project.file(d.file).kind == quadra_sema::FileKind::Std
                || snap.generated.contains(&d.file)
            {
                continue;
            }
            let index = LineIndex::new(&snap.project.file(d.file).text);
            by_file.entry(d.file).or_default().push(Diagnostic {
                range: index.range(d.range),
                severity: Some(match d.severity {
                    Severity::Error => DiagnosticSeverity::ERROR,
                    Severity::Warning => DiagnosticSeverity::WARNING,
                    Severity::Hint => DiagnosticSeverity::HINT,
                }),
                source: Some("quadra".into()),
                message: d.message.clone(),
                ..Default::default()
            });
        }
        for (file, diags) in by_file {
            if let Ok(uri) = Url::from_file_path(&snap.project.file(file).path) {
                self.client.publish_diagnostics(uri, diags, None).await;
            }
        }
    }

    fn locate(&self, uri: &Url) -> Option<(Arc<Snapshot>, FileId)> {
        let path = path_of(uri)?;
        let snap = self.snapshot(&path);
        let id = snap.project.file_by_path(&path)?.id;
        Some((snap, id))
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "quadra".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".into(), "@".into(), ":".into()]),
                    ..Default::default()
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                document_symbol_provider: Some(OneOf::Left(true)),
                semantic_tokens_provider: Some(
                    SemanticTokensServerCapabilities::SemanticTokensOptions(
                        SemanticTokensOptions {
                            legend: semantic::legend(),
                            full: Some(SemanticTokensFullOptions::Bool(true)),
                            range: None,
                            ..Default::default()
                        },
                    ),
                ),
                ..Default::default()
            },
        })
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let Some(path) = path_of(&params.text_document.uri) else {
            return;
        };
        self.docs
            .lock()
            .unwrap()
            .insert(path.clone(), params.text_document.text);
        self.refresh(&path).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let Some(path) = path_of(&params.text_document.uri) else {
            return;
        };
        if let Some(change) = params.content_changes.into_iter().last() {
            self.docs.lock().unwrap().insert(path.clone(), change.text);
        }
        self.refresh(&path).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if let Some(path) = path_of(&params.text_document.uri) {
            self.refresh(&path).await;
        }
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        // a `.qd` or `.qdi` file changed on disk (another editor, `quadra dev` writing an app
        // back): the project is analysed again
        let mut roots: Vec<PathBuf> = Vec::new();
        for c in params.changes {
            if let Some(p) = path_of(&c.uri).or_else(|| c.uri.to_file_path().ok()) {
                let r = project_root(&p);
                if !roots.contains(&r) {
                    roots.push(r);
                    self.refresh(&p).await;
                }
            }
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let Some(path) = path_of(&params.text_document.uri) else {
            return;
        };
        self.docs.lock().unwrap().remove(&path);
        self.refresh(&path).await;
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let pos = params.text_document_position_params;
        let Some((snap, file)) = self.locate(&pos.text_document.uri) else {
            return Ok(None);
        };
        let index = LineIndex::new(&snap.project.file(file).text);
        let offset = index.offset(pos.position);
        // a declaration of the project, or a method of the language (`.group_by`)
        let found = target_at(&snap, file, offset)
            .map(|(range, t)| (range, ide::hover_target(&snap.project, &snap.analysis, t)))
            .or_else(|| ide::method_hover(&snap.project.file(file).text, offset));
        Ok(found.map(|(range, text)| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: text,
            }),
            range: Some(index.range(range)),
        }))
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let pos = params.text_document_position;
        let Some((snap, file)) = self.locate(&pos.text_document.uri) else {
            return Ok(None);
        };
        let text = &snap.project.file(file).text;
        let offset = LineIndex::new(text).offset(pos.position);
        if snap.project.file(file).kind == quadra_sema::FileKind::Interface {
            let params = snap.params.get(&file).cloned().unwrap_or_default();
            let items: Vec<CompletionItem> =
                qdi::complete(text, usize::from(offset), &snap.entities, &params)
                    .into_iter()
                    .map(|i| CompletionItem {
                        label: i.label,
                        kind: Some(match i.kind {
                            "field" => CompletionItemKind::FIELD,
                            "method" => CompletionItemKind::METHOD,
                            "class" => CompletionItemKind::CLASS,
                            "variable" => CompletionItemKind::VARIABLE,
                            "enum" => CompletionItemKind::ENUM_MEMBER,
                            "property" => CompletionItemKind::PROPERTY,
                            _ => CompletionItemKind::KEYWORD,
                        }),
                        detail: (!i.detail.is_empty()).then_some(i.detail),
                        ..Default::default()
                    })
                    .collect();
            return Ok((!items.is_empty()).then_some(CompletionResponse::Array(items)));
        }
        let items: Vec<CompletionItem> = ide::method_completions(text, offset)
            .into_iter()
            .map(|(name, sig, doc)| CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::METHOD),
                detail: Some(format!(".{sig}")),
                documentation: Some(Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: doc.to_string(),
                })),
                ..Default::default()
            })
            .collect();
        Ok((!items.is_empty()).then_some(CompletionResponse::Array(items)))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = params.text_document_position_params;
        let Some((snap, file)) = self.locate(&pos.text_document.uri) else {
            return Ok(None);
        };
        let offset = LineIndex::new(&snap.project.file(file).text).offset(pos.position);
        let Some(target) = target_at(&snap, file, offset)
            .and_then(|(_, t)| ide::target_definition(&snap.analysis, &t, file))
        else {
            return Ok(None);
        };
        let target_file = snap.project.file(target.file);
        let Ok(uri) = Url::from_file_path(&target_file.path) else {
            return Ok(None);
        };
        let range = LineIndex::new(&target_file.text).range(target.range);
        Ok(Some(GotoDefinitionResponse::Scalar(Location {
            uri,
            range,
        })))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let pos = params.text_document_position;
        let Some((snap, file)) = self.locate(&pos.text_document.uri) else {
            return Ok(None);
        };
        let offset = LineIndex::new(&snap.project.file(file).text).offset(pos.position);
        let Some((_, target)) = target_at(&snap, file, offset) else {
            return Ok(None);
        };
        let mut out = Vec::new();
        for n in ide::references(&snap.analysis, &target) {
            if snap.generated.contains(&n.file)
                || snap.project.file(n.file).kind == quadra_sema::FileKind::Std
            {
                continue;
            }
            if !params.context.include_declaration
                && ide::target_definition(&snap.analysis, &target, file).as_ref() == Some(&n)
            {
                continue;
            }
            let f = snap.project.file(n.file);
            if let Ok(uri) = Url::from_file_path(&f.path) {
                out.push(Location {
                    uri,
                    range: LineIndex::new(&f.text).range(n.range),
                });
            }
        }
        Ok(Some(out))
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let Some((snap, file)) = self.locate(&params.text_document.uri) else {
            return Ok(None);
        };
        let index = LineIndex::new(&snap.project.file(file).text);
        let offset = index.offset(params.position);
        let Some((range, target)) = target_at(&snap, file, offset) else {
            return Ok(None);
        };
        // a declaration of the standard library is not renamed
        let def = ide::target_definition(&snap.analysis, &target, file);
        if def.is_some_and(|d| snap.project.file(d.file).kind == quadra_sema::FileKind::Std) {
            return Ok(None);
        }
        Ok(Some(PrepareRenameResponse::Range(index.range(range))))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let pos = params.text_document_position;
        let Some((snap, file)) = self.locate(&pos.text_document.uri) else {
            return Ok(None);
        };
        let offset = LineIndex::new(&snap.project.file(file).text).offset(pos.position);
        let Some((_, target)) = target_at(&snap, file, offset) else {
            return Ok(None);
        };
        let new = params.new_name.trim();
        let ident = new
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && new.chars().all(|c| c.is_alphanumeric() || c == '_');
        if !ident {
            return Err(tower_lsp::jsonrpc::Error::invalid_params(format!(
                "`{new}` is not a name"
            )));
        }
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for n in ide::references(&snap.analysis, &target) {
            let f = snap.project.file(n.file);
            if snap.generated.contains(&n.file) || f.kind == quadra_sema::FileKind::Std {
                continue;
            }
            if let Ok(uri) = Url::from_file_path(&f.path) {
                changes.entry(uri).or_default().push(TextEdit {
                    range: LineIndex::new(&f.text).range(n.range),
                    new_text: new.to_string(),
                });
            }
        }
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let Some((snap, file)) = self.locate(&params.text_document.uri) else {
            return Ok(None);
        };
        let index = LineIndex::new(&snap.project.file(file).text);
        let symbols = ide::document_symbols(&snap.project, &snap.analysis, file);
        Ok(Some(DocumentSymbolResponse::Nested(
            symbols.iter().map(|s| to_lsp_symbol(s, &index)).collect(),
        )))
    }

    async fn semantic_tokens_full(
        &self,
        params: SemanticTokensParams,
    ) -> Result<Option<SemanticTokensResult>> {
        let Some((snap, file)) = self.locate(&params.text_document.uri) else {
            return Ok(None);
        };
        let data = semantic::tokens(&snap.project, &snap.analysis, file);
        Ok(Some(SemanticTokensResult::Tokens(SemanticTokens {
            result_id: None,
            data,
        })))
    }
}

/// What the name at `offset` refers to: a reference recorded there (interface files, members
/// the type checker resolved), else what the analysis of the code says.
fn target_at(
    snap: &Snapshot,
    file: FileId,
    offset: quadra_syntax::TextSize,
) -> Option<(quadra_syntax::TextRange, quadra_sema::Target)> {
    if snap.project.file(file).kind == quadra_sema::FileKind::Interface {
        return ide::reference_at(&snap.analysis, file, offset);
    }
    ide::reference_at(&snap.analysis, file, offset)
        .or_else(|| ide::target_at(&snap.project, &snap.analysis, file, offset))
}

#[allow(deprecated)]
fn to_lsp_symbol(s: &ide::Symbol, index: &LineIndex) -> DocumentSymbol {
    use quadra_sema::{DeclKind as D, MemberKind as M};
    let kind = match &s.kind {
        ide::SymbolKind::Decl(k) => match k {
            D::Entity | D::Record => SymbolKind::CLASS,
            D::Trait => SymbolKind::INTERFACE,
            D::Enum => SymbolKind::ENUM,
            D::Ledger => SymbolKind::STRUCT,
            D::Fn => SymbolKind::FUNCTION,
            D::Const => SymbolKind::CONSTANT,
            D::Role | D::Fixture | D::Migration | D::Change => SymbolKind::NAMESPACE,
            D::View | D::Report | D::Document => SymbolKind::OBJECT,
            D::Metric | D::Control => SymbolKind::KEY,
            D::Event => SymbolKind::EVENT,
        },
        ide::SymbolKind::Member(k) => match k {
            M::Field | M::LedgerKey | M::LedgerMeasure | M::LedgerAttr => SymbolKind::FIELD,
            M::Formula => SymbolKind::PROPERTY,
            M::Action | M::FlowAction | M::Method => SymbolKind::METHOD,
        },
        ide::SymbolKind::Extend => SymbolKind::NAMESPACE,
    };
    DocumentSymbol {
        name: s.name.clone(),
        detail: (!s.detail.is_empty()).then(|| s.detail.clone()),
        kind,
        tags: None,
        deprecated: None,
        range: index.range(s.range),
        selection_range: index.range(s.selection),
        children: (!s.children.is_empty())
            .then(|| s.children.iter().map(|c| to_lsp_symbol(c, index)).collect()),
    }
}
