//! Language detection and bounded structural symbol extraction.
//!
//! This module deliberately stops at syntax. A [`LanguageAdapter`] turns one
//! file into declarations and unresolved name mentions; it never claims type
//! resolution, cross-file definition lookup, or any other LSP guarantee.
//! Tree-sitter values stay private to the adapters so retrieval depends only on
//! the typed records below and a future semantic source can implement the same
//! narrow boundary.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use harkness_git::Cancellation;
use tree_sitter::{Language as TreeSitterLanguage, Node, Parser, Query, Tree};

use crate::{ByteRange, Language, OutlineNode, RepoPath, StructuralOutline, SymbolId};

/// Maximum bytes retained from a declaration's first line.
pub const MAX_SIGNATURE_BYTES: usize = 512;
/// Maximum declarations accepted from one file.
pub const MAX_SYMBOLS_PER_FILE: usize = 16_384;
/// Maximum unresolved mentions accepted from one file.
pub const MAX_REFERENCES_PER_FILE: usize = 65_536;
/// Maximum syntax-error ranges retained for one file.
pub const MAX_PARSE_ERROR_RANGES: usize = 1_024;
/// Version of detection and extraction behavior independent of grammar crates.
pub const SYMBOL_EXTRACTION_VERSION: &str = "1";

const RUST_GRAMMAR_VERSION: &str = "tree-sitter-rust-0.24.2/extractor-1";
const TOML_GRAMMAR_VERSION: &str = "tree-sitter-toml-ng-0.7.0/extractor-1";
const MARKDOWN_GRAMMAR_VERSION: &str = "tree-sitter-md-0.5.6/extractor-1";
const UNSUPPORTED_VERSION: &str = "unsupported-1";

/// Why a language classification won.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LanguageDetectionSource {
    /// The repository-relative filename or extension decided.
    Extension,
    /// An interpreter directive on the first line decided.
    Shebang,
    /// A bounded content signature decided after stronger signals were absent.
    Heuristic,
}

/// The language classification for one file.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LanguageDetection {
    /// Detected language, or `None` when no bounded rule claims the file.
    pub language: Option<Language>,
    /// Signal that decided, absent when the language is unknown.
    pub source: Option<LanguageDetectionSource>,
}

/// Detects a language using filename, then shebang, then bounded heuristics.
#[must_use]
pub fn detect_language(path: &RepoPath, content_head: &[u8]) -> LanguageDetection {
    if let Some(language) = extension_language(path.as_bytes()) {
        return detected(language, LanguageDetectionSource::Extension);
    }
    if let Some(language) = shebang_language(content_head) {
        return detected(language, LanguageDetectionSource::Shebang);
    }
    if let Some(language) = heuristic_language(content_head) {
        return detected(language, LanguageDetectionSource::Heuristic);
    }
    LanguageDetection {
        language: None,
        source: None,
    }
}

fn detected(language: &str, source: LanguageDetectionSource) -> LanguageDetection {
    LanguageDetection {
        language: Language::new(language).ok(),
        source: Some(source),
    }
}

fn extension_language(path: &[u8]) -> Option<&'static str> {
    let name = path.rsplit(|byte| *byte == b'/').next().unwrap_or(path);
    let lower = name.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>();
    match lower.as_slice() {
        b"cargo.toml" | b"pyproject.toml" => return Some("toml"),
        b"dockerfile" => return Some("dockerfile"),
        b"makefile" | b"gnumakefile" => return Some("make"),
        b"cmakelists.txt" => return Some("cmake"),
        _ => {}
    }
    let extension = lower.rsplit(|byte| *byte == b'.').next()?;
    Some(match extension {
        b"rs" => "rust",
        b"toml" => "toml",
        b"md" | b"markdown" | b"mdown" => "markdown",
        b"py" | b"pyi" => "python",
        b"js" | b"mjs" | b"cjs" => "javascript",
        b"ts" | b"mts" | b"cts" => "typescript",
        b"tsx" => "tsx",
        b"jsx" => "jsx",
        b"json" | b"jsonc" => "json",
        b"yaml" | b"yml" => "yaml",
        b"sh" | b"bash" | b"zsh" | b"fish" => "shell",
        b"c" | b"h" => "c",
        b"cc" | b"cpp" | b"cxx" | b"hpp" | b"hxx" => "cpp",
        b"cs" => "csharp",
        b"go" => "go",
        b"java" => "java",
        b"kt" | b"kts" => "kotlin",
        b"rb" => "ruby",
        b"php" => "php",
        b"swift" => "swift",
        b"sql" => "sql",
        b"html" | b"htm" => "html",
        b"css" | b"scss" | b"sass" => "css",
        b"qml" => "qml",
        _ => return None,
    })
}

fn shebang_language(content: &[u8]) -> Option<&'static str> {
    let line = content.split(|byte| *byte == b'\n').next()?;
    let directive = line.strip_prefix(b"#!")?;
    let words = directive
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|word| !word.is_empty());
    for word in words {
        let interpreter = word.rsplit(|byte| *byte == b'/').next().unwrap_or(word);
        let interpreter = interpreter.strip_suffix(b"\r").unwrap_or(interpreter);
        if interpreter.starts_with(b"-") {
            continue;
        }
        return match interpreter {
            b"env" => continue,
            b"python" | b"python3" => Some("python"),
            b"node" | b"nodejs" => Some("javascript"),
            b"bash" | b"sh" | b"zsh" | b"fish" => Some("shell"),
            b"ruby" => Some("ruby"),
            _ if interpreter.starts_with(b"python") => Some("python"),
            _ => None,
        };
    }
    None
}

fn heuristic_language(content: &[u8]) -> Option<&'static str> {
    let head = &content[..content.len().min(4096)];
    if head.starts_with(b"# ") || head.windows(3).any(|window| window == b"\n# ") {
        return Some("markdown");
    }
    if head.split(|byte| *byte == b'\n').any(|line| {
        let line = line.trim_ascii_start();
        line.starts_with(b"fn ")
            || line.starts_with(b"pub fn ")
            || line.starts_with(b"impl ")
            || line.starts_with(b"pub mod ")
    }) {
        return Some("rust");
    }
    if head.windows(10).any(|window| window == b"[package]\n")
        || head.windows(15).any(|window| window == b"[dependencies]\n")
    {
        return Some("toml");
    }
    None
}

/// Parser-owned declaration category.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum SymbolKind {
    /// Free function.
    Function,
    /// Function nested in an implementation or trait.
    Method,
    /// Struct declaration.
    Struct,
    /// Enum declaration; variants are deliberately not separate declarations.
    Enum,
    /// Trait declaration.
    Trait,
    /// Implementation block.
    Impl,
    /// Source module or configuration table.
    Module,
    /// Constant declaration or configuration key.
    Constant,
    /// Static declaration.
    Static,
    /// Type alias.
    TypeAlias,
    /// Import declaration.
    Import,
    /// Export declaration.
    Export,
    /// Documentation section.
    Section,
}

impl SymbolKind {
    /// Stable cache spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Impl => "impl",
            Self::Module => "module",
            Self::Constant => "constant",
            Self::Static => "static",
            Self::TypeAlias => "type_alias",
            Self::Import => "import",
            Self::Export => "export",
            Self::Section => "section",
        }
    }

    /// Parses a stable cache spelling.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "function" => Self::Function,
            "method" => Self::Method,
            "struct" => Self::Struct,
            "enum" => Self::Enum,
            "trait" => Self::Trait,
            "impl" => Self::Impl,
            "module" => Self::Module,
            "constant" => Self::Constant,
            "static" => Self::Static,
            "type_alias" => Self::TypeAlias,
            "import" => Self::Import,
            "export" => Self::Export,
            "section" => Self::Section,
            _ => return None,
        })
    }
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One declaration extracted from repository syntax.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Symbol {
    /// Stable, content-independent declaration identity.
    pub id: SymbolId,
    /// Declaration category.
    pub kind: SymbolKind,
    /// Bare declaration name.
    pub name: String,
    /// Structural path from file root to this declaration.
    pub qualified_name: String,
    /// Deterministic duplicate ordinal in byte order.
    pub ordinal: u32,
    /// Original-file half-open definition bytes and line hints.
    pub byte_range: ByteRange,
    /// Enclosing declaration, when one was extracted.
    pub parent: Option<SymbolId>,
    /// Bounded first line of the declaration.
    pub signature: Option<String>,
    /// Whether Rust test attributes or an enclosing test module apply.
    pub is_test: bool,
    /// Whether invalid UTF-8 in the identifier was replaced for display/storage.
    pub name_is_lossy: bool,
}

/// One unresolved name mention exposed cheaply by a grammar.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SymbolReference {
    /// Mentioned spelling; no target is implied.
    pub name: String,
    /// Exact mention range in the original bytes.
    pub byte_range: ByteRange,
    /// Whether invalid UTF-8 was replaced in `name`.
    pub name_is_lossy: bool,
}

/// Why symbol extraction did not run for an otherwise eligible file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ExtractionSkipReason {
    /// Detection found a language for which no adapter is registered.
    UnsupportedLanguage,
    /// No bounded detection rule claimed the file.
    UnknownLanguage,
}

impl ExtractionSkipReason {
    /// Stable cache spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedLanguage => "unsupported_language",
            Self::UnknownLanguage => "unknown_language",
        }
    }
}

/// Honest parser result for one exact file version.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ParseHealth {
    /// Parsing completed without syntax error nodes.
    Complete,
    /// Usable declarations were recovered around bounded syntax errors.
    Partial {
        /// Exact original-file ranges of syntax error nodes.
        error_ranges: Vec<ByteRange>,
    },
    /// The adapter could not produce a structural answer.
    Failed {
        /// Stable, bounded failure spelling.
        reason: String,
    },
    /// Extraction was deliberately not attempted.
    Skipped {
        /// Named reason, distinct from a supported empty file.
        reason: ExtractionSkipReason,
    },
}

/// Raw adapter output before path-based identities are attached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExtractionResult {
    /// Parser health.
    pub health: ParseHealth,
    /// Declaration records in source order.
    pub symbols: Vec<RawSymbol>,
    /// Unresolved mentions in source order.
    pub references: Vec<RawReference>,
}

/// Adapter declaration with a parent index rather than a path-based identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawSymbol {
    /// Exact identifier bytes.
    pub name: Vec<u8>,
    /// Parser-owned kind.
    pub kind: SymbolKind,
    /// Exact definition range.
    pub byte_range: ByteRange,
    /// Earlier raw symbol enclosing this one.
    pub parent: Option<usize>,
    /// Whether test attributes or an enclosing test module apply.
    pub is_test: bool,
}

/// Adapter mention before display decoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RawReference {
    /// Exact mention bytes.
    pub name: Vec<u8>,
    /// Exact mention range.
    pub byte_range: ByteRange,
}

/// Fully identified extraction for one file.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct FileSymbols {
    /// Language detection, including unsupported/unknown answers.
    pub detection: LanguageDetection,
    /// Per-adapter marker stored beside this file version.
    pub grammar_version: String,
    /// Parse health.
    pub health: ParseHealth,
    /// Stable declarations.
    pub symbols: Vec<Symbol>,
    /// Best-effort unresolved mentions.
    pub references: Vec<SymbolReference>,
    /// Non-overlapping declaration projection used by source chunking.
    pub outline: StructuralOutline,
}

/// Syntax-only extraction adapter for one language.
pub trait LanguageAdapter: Send + Sync {
    /// Language this adapter accepts.
    fn language(&self) -> Language;
    /// Marker whose change invalidates only files of this language.
    fn grammar_version(&self) -> &'static str;
    /// Extracts raw declarations and unresolved mentions.
    fn extract(&self, source: &[u8], cancellation: &Cancellation) -> ExtractionResult;
}

/// Narrow structural-source seam consumed by indexing.
///
/// The default source is [`LanguageRegistry`]. A future LSP-backed source may
/// implement this trait without exposing protocol or tree-sitter types to
/// chunking, ranking, or retrieval; callers must still treat every declaration
/// as advisory structural data rather than semantic resolution.
pub trait SymbolSource: Send + Sync {
    /// Registered language/version pairs in deterministic order.
    fn versions(&self) -> Vec<(String, String)>;
    /// Expected marker for one already detected language.
    fn expected_version(&self, language: Option<&Language>) -> &str;
    /// Extracts one file's structural inventory.
    fn extract(&self, path: &RepoPath, source: &[u8], cancellation: &Cancellation) -> FileSymbols;
}

/// Adapter registration or query-compilation failure.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SymbolError {
    /// A built-in tree-sitter query did not compile.
    #[error("the {language} symbol query is invalid: {detail}")]
    InvalidQuery {
        /// Language whose adapter failed.
        language: &'static str,
        /// Tree-sitter diagnostic.
        detail: String,
    },
    /// Two adapters claimed one language.
    #[error("more than one symbol adapter is registered for {language}")]
    DuplicateAdapter {
        /// Duplicated stable language spelling.
        language: String,
    },
}

/// Deterministic adapter registry and the narrow symbol-source seam.
#[derive(Clone)]
pub struct LanguageRegistry {
    adapters: Arc<BTreeMap<String, Arc<dyn LanguageAdapter>>>,
}

impl std::fmt::Debug for LanguageRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LanguageRegistry")
            .field("languages", &self.adapters.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl LanguageRegistry {
    /// Builds the Rust, TOML, and Markdown registry, compiling every query now.
    pub fn built_in() -> Result<Self, SymbolError> {
        Self::new(vec![
            Arc::new(TreeSitterAdapter::rust()?),
            Arc::new(TreeSitterAdapter::toml()?),
            Arc::new(TreeSitterAdapter::markdown()?),
        ])
    }

    /// Builds a registry from independent adapters.
    pub fn new(adapters: Vec<Arc<dyn LanguageAdapter>>) -> Result<Self, SymbolError> {
        let mut by_language = BTreeMap::new();
        for adapter in adapters {
            let language = adapter.language().as_str().to_owned();
            if by_language.insert(language.clone(), adapter).is_some() {
                return Err(SymbolError::DuplicateAdapter { language });
            }
        }
        Ok(Self {
            adapters: Arc::new(by_language),
        })
    }

    /// Registered `(language, grammar_version)` pairs in stable language order.
    #[must_use]
    pub fn versions(&self) -> Vec<(String, String)> {
        self.adapters
            .iter()
            .map(|(language, adapter)| (language.clone(), adapter.grammar_version().to_owned()))
            .collect()
    }

    /// Expected per-file parser marker for an already detected language.
    #[must_use]
    pub fn expected_version(&self, language: Option<&Language>) -> &str {
        language
            .and_then(|language| self.adapters.get(language.as_str()))
            .map_or(UNSUPPORTED_VERSION, |adapter| adapter.grammar_version())
    }

    /// Detects and extracts one file, containing adapter panics to that file.
    pub fn extract(
        &self,
        path: &RepoPath,
        source: &[u8],
        cancellation: &Cancellation,
    ) -> FileSymbols {
        let detection = detect_language(path, source);
        let Some(language) = detection.language.clone() else {
            return FileSymbols {
                detection,
                grammar_version: UNSUPPORTED_VERSION.to_owned(),
                health: ParseHealth::Skipped {
                    reason: ExtractionSkipReason::UnknownLanguage,
                },
                symbols: Vec::new(),
                references: Vec::new(),
                outline: StructuralOutline::default(),
            };
        };
        let Some(adapter) = self.adapters.get(language.as_str()) else {
            return FileSymbols {
                detection,
                grammar_version: UNSUPPORTED_VERSION.to_owned(),
                health: ParseHealth::Skipped {
                    reason: ExtractionSkipReason::UnsupportedLanguage,
                },
                symbols: Vec::new(),
                references: Vec::new(),
                outline: StructuralOutline::default(),
            };
        };
        let started = std::time::Instant::now();
        let grammar_version = adapter.grammar_version().to_owned();
        let extracted = catch_unwind(AssertUnwindSafe(|| adapter.extract(source, cancellation)))
            .unwrap_or_else(|_| ExtractionResult {
                health: ParseHealth::Failed {
                    reason: "adapter_panicked".to_owned(),
                },
                symbols: Vec::new(),
                references: Vec::new(),
            });
        let identified = identify(
            path,
            &language,
            detection,
            grammar_version,
            source,
            extracted,
        );
        tracing::debug!(
            language = language.as_str(),
            grammar_version = identified.grammar_version.as_str(),
            health = parse_health_name(&identified.health),
            symbols = identified.symbols.len(),
            references = identified.references.len(),
            duration_micros = started.elapsed().as_micros(),
            "context symbols extracted"
        );
        identified
    }
}

const fn parse_health_name(health: &ParseHealth) -> &'static str {
    match health {
        ParseHealth::Complete => "complete",
        ParseHealth::Partial { .. } => "partial",
        ParseHealth::Failed { .. } => "failed",
        ParseHealth::Skipped { .. } => "skipped",
    }
}

impl SymbolSource for LanguageRegistry {
    fn versions(&self) -> Vec<(String, String)> {
        Self::versions(self)
    }

    fn expected_version(&self, language: Option<&Language>) -> &str {
        Self::expected_version(self, language)
    }

    fn extract(&self, path: &RepoPath, source: &[u8], cancellation: &Cancellation) -> FileSymbols {
        Self::extract(self, path, source, cancellation)
    }
}

#[derive(Clone, Copy, Debug)]
enum AdapterKind {
    Rust,
    Toml,
    Markdown,
}

struct TreeSitterAdapter {
    kind: AdapterKind,
    language: Language,
    tree_language: TreeSitterLanguage,
    grammar_version: &'static str,
    _query: Query,
}

impl TreeSitterAdapter {
    fn rust() -> Result<Self, SymbolError> {
        Self::new(
            AdapterKind::Rust,
            "rust",
            tree_sitter_rust::LANGUAGE.into(),
            RUST_GRAMMAR_VERSION,
            include_str!("queries/rust.scm"),
        )
    }

    fn toml() -> Result<Self, SymbolError> {
        Self::new(
            AdapterKind::Toml,
            "toml",
            tree_sitter_toml::LANGUAGE.into(),
            TOML_GRAMMAR_VERSION,
            include_str!("queries/toml.scm"),
        )
    }

    fn markdown() -> Result<Self, SymbolError> {
        Self::new(
            AdapterKind::Markdown,
            "markdown",
            tree_sitter_md::LANGUAGE.into(),
            MARKDOWN_GRAMMAR_VERSION,
            include_str!("queries/markdown.scm"),
        )
    }

    fn new(
        kind: AdapterKind,
        language: &'static str,
        tree_language: TreeSitterLanguage,
        grammar_version: &'static str,
        query_source: &'static str,
    ) -> Result<Self, SymbolError> {
        let query = Query::new(&tree_language, query_source).map_err(|error| {
            SymbolError::InvalidQuery {
                language,
                detail: error.to_string(),
            }
        })?;
        Ok(Self {
            kind,
            language: Language::new(language).expect("built-in language is valid"),
            tree_language,
            grammar_version,
            _query: query,
        })
    }
}

impl LanguageAdapter for TreeSitterAdapter {
    fn language(&self) -> Language {
        self.language.clone()
    }

    fn grammar_version(&self) -> &'static str {
        self.grammar_version
    }

    fn extract(&self, source: &[u8], cancellation: &Cancellation) -> ExtractionResult {
        if cancellation.is_cancelled() {
            return failed("cancelled");
        }
        let mut parser = Parser::new();
        if parser.set_language(&self.tree_language).is_err() {
            return failed("grammar_incompatible");
        }
        let Some(tree) = parser.parse(source, None) else {
            return failed("parse_failed");
        };
        let mut output = match self.kind {
            AdapterKind::Rust => extract_rust(&tree, source),
            AdapterKind::Toml => extract_toml(&tree, source),
            AdapterKind::Markdown => extract_markdown(&tree, source),
        };
        output.health = parse_health(&tree);
        if output.symbols.len() > MAX_SYMBOLS_PER_FILE {
            output.symbols.truncate(MAX_SYMBOLS_PER_FILE);
            output.health = ParseHealth::Failed {
                reason: "symbol_budget_exhausted".to_owned(),
            };
        }
        output.references.truncate(MAX_REFERENCES_PER_FILE);
        output
    }
}

fn failed(reason: &str) -> ExtractionResult {
    ExtractionResult {
        health: ParseHealth::Failed {
            reason: reason.to_owned(),
        },
        symbols: Vec::new(),
        references: Vec::new(),
    }
}

fn parse_health(tree: &Tree) -> ParseHealth {
    if !tree.root_node().has_error() {
        return ParseHealth::Complete;
    }
    let mut ranges = Vec::new();
    walk(tree.root_node(), &mut |node| {
        if node.is_error() || node.is_missing() {
            ranges.push(node_range(node));
        }
    });
    ranges.sort_by_key(|range| (range.start, range.end));
    ranges.dedup();
    ranges.truncate(MAX_PARSE_ERROR_RANGES);
    if ranges.is_empty() {
        ParseHealth::Complete
    } else {
        ParseHealth::Partial {
            error_ranges: ranges,
        }
    }
}

fn extract_rust(tree: &Tree, source: &[u8]) -> ExtractionResult {
    let mut raw = Vec::new();
    let mut references = Vec::new();
    let mut pending = vec![(tree.root_node(), None, false)];
    while let Some((node, parent, enclosing_test)) = pending.pop() {
        let (child_parent, child_test) = rust_node(
            node,
            source,
            parent,
            enclosing_test,
            &mut raw,
            &mut references,
        );
        let mut cursor = node.walk();
        let children = node.children(&mut cursor).collect::<Vec<_>>();
        pending.extend(
            children
                .into_iter()
                .rev()
                .map(|child| (child, child_parent, child_test)),
        );
    }
    ExtractionResult {
        health: ParseHealth::Complete,
        symbols: raw,
        references,
    }
}

fn rust_node(
    node: Node<'_>,
    source: &[u8],
    parent: Option<usize>,
    enclosing_test: bool,
    symbols: &mut Vec<RawSymbol>,
    references: &mut Vec<RawReference>,
) -> (Option<usize>, bool) {
    let kind = node.kind();
    let in_method_container = parent
        .is_some_and(|index| matches!(symbols[index].kind, SymbolKind::Impl | SymbolKind::Trait));
    let declaration = match kind {
        "function_item" | "function_signature_item" => Some(if in_method_container {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        }),
        "struct_item" => Some(SymbolKind::Struct),
        "enum_item" => Some(SymbolKind::Enum),
        "trait_item" => Some(SymbolKind::Trait),
        "impl_item" => Some(SymbolKind::Impl),
        "mod_item" => Some(SymbolKind::Module),
        "const_item" => Some(SymbolKind::Constant),
        "static_item" => Some(SymbolKind::Static),
        "type_item" => Some(SymbolKind::TypeAlias),
        "use_declaration" => {
            let mut cursor = node.walk();
            Some(
                if node
                    .named_children(&mut cursor)
                    .any(|child| child.kind() == "visibility_modifier")
                {
                    SymbolKind::Export
                } else {
                    SymbolKind::Import
                },
            )
        }
        _ => None,
    };
    let mut child_parent = parent;
    let mut child_test = enclosing_test;
    if let Some(symbol_kind) = declaration {
        let name_node = if kind == "impl_item" {
            node.child_by_field_name("type")
        } else if kind == "use_declaration" {
            node.child_by_field_name("argument")
        } else {
            node.child_by_field_name("name")
        };
        if let Some(name_node) = name_node {
            let is_test = enclosing_test || declaration_is_test(node, source);
            let index = symbols.len();
            symbols.push(RawSymbol {
                name: source[name_node.byte_range()].to_vec(),
                kind: symbol_kind,
                byte_range: node_range(node),
                parent,
                is_test,
            });
            if matches!(
                symbol_kind,
                SymbolKind::Impl | SymbolKind::Trait | SymbolKind::Module
            ) {
                child_parent = Some(index);
                child_test = is_test;
            }
        }
    } else if matches!(kind, "identifier" | "type_identifier" | "field_identifier") {
        references.push(RawReference {
            name: source[node.byte_range()].to_vec(),
            byte_range: node_range(node),
        });
    }
    (child_parent, child_test)
}

fn declaration_is_test(node: Node<'_>, source: &[u8]) -> bool {
    let start = node.start_byte();
    let prefix = &source[start.saturating_sub(1_024)..start];
    let attributes = prefix.trim_ascii_end();
    if !attributes.ends_with(b"]") {
        return false;
    }
    let boundary = attributes
        .iter()
        .rposition(|byte| matches!(byte, b'}' | b';'))
        .map_or(0, |index| index + 1);
    let nearest = &attributes[boundary..];
    nearest.windows(7).any(|window| window == b"#[test]")
        || nearest.windows(9).any(|window| window == b"cfg(test)")
}

fn extract_toml(tree: &Tree, source: &[u8]) -> ExtractionResult {
    let mut symbols = Vec::new();
    let mut pending = vec![(tree.root_node(), None)];
    while let Some((node, parent)) = pending.pop() {
        let Some(child_parent) = toml_node(node, source, parent, &mut symbols) else {
            continue;
        };
        let mut cursor = node.walk();
        let children = node.children(&mut cursor).collect::<Vec<_>>();
        pending.extend(
            children
                .into_iter()
                .rev()
                .map(|child| (child, child_parent)),
        );
    }
    ExtractionResult {
        health: ParseHealth::Complete,
        symbols,
        references: Vec::new(),
    }
}

fn toml_node(
    node: Node<'_>,
    source: &[u8],
    parent: Option<usize>,
    symbols: &mut Vec<RawSymbol>,
) -> Option<Option<usize>> {
    let mut child_parent = parent;
    match node.kind() {
        "table" | "table_array_element" => {
            let name = toml_header_name(node, source);
            if !name.is_empty() {
                let index = symbols.len();
                symbols.push(RawSymbol {
                    name: name.to_vec(),
                    kind: SymbolKind::Module,
                    byte_range: node_range(node),
                    parent: None,
                    is_test: false,
                });
                child_parent = Some(index);
            }
        }
        "pair" => {
            if let Some(key) = node
                .child_by_field_name("key")
                .or_else(|| node.named_child(0))
            {
                symbols.push(RawSymbol {
                    name: trim_quotes(&source[key.byte_range()]).to_vec(),
                    kind: SymbolKind::Constant,
                    byte_range: node_range(node),
                    parent,
                    is_test: false,
                });
            }
            return None;
        }
        _ => {}
    }
    Some(child_parent)
}

fn toml_header_name<'a>(node: Node<'_>, source: &'a [u8]) -> &'a [u8] {
    let bytes = &source[node.byte_range()];
    let first = bytes.split(|byte| *byte == b'\n').next().unwrap_or(bytes);
    trim_byte(trim_byte(first.trim_ascii(), b'[', true), b']', false).trim_ascii()
}

fn trim_quotes(bytes: &[u8]) -> &[u8] {
    bytes
        .strip_prefix(b"\"")
        .and_then(|bytes| bytes.strip_suffix(b"\""))
        .or_else(|| {
            bytes
                .strip_prefix(b"'")
                .and_then(|bytes| bytes.strip_suffix(b"'"))
        })
        .unwrap_or(bytes)
}

fn trim_byte(mut bytes: &[u8], needle: u8, from_start: bool) -> &[u8] {
    if from_start {
        while bytes.first() == Some(&needle) {
            bytes = &bytes[1..];
        }
    } else {
        while bytes.last() == Some(&needle) {
            bytes = &bytes[..bytes.len() - 1];
        }
    }
    bytes
}

fn extract_markdown(tree: &Tree, source: &[u8]) -> ExtractionResult {
    let mut symbols = Vec::new();
    let mut heading_stack: Vec<(usize, usize)> = Vec::new();
    walk(tree.root_node(), &mut |node| {
        if matches!(node.kind(), "atx_heading" | "setext_heading") {
            let text = &source[node.byte_range()];
            let first = text.split(|byte| *byte == b'\n').next().unwrap_or(text);
            let level = if node.kind() == "atx_heading" {
                first
                    .iter()
                    .take_while(|byte| **byte == b'#')
                    .count()
                    .max(1)
            } else if text.windows(2).any(|window| window == b"==") {
                1
            } else {
                2
            };
            let name = trim_byte(trim_byte(first.trim_ascii(), b'#', true), b'#', false)
                .trim_ascii()
                .to_vec();
            while heading_stack
                .last()
                .is_some_and(|(depth, _)| *depth >= level)
            {
                heading_stack.pop();
            }
            let parent = heading_stack.last().map(|(_, index)| *index);
            let index = symbols.len();
            symbols.push(RawSymbol {
                name,
                kind: SymbolKind::Section,
                byte_range: node_range(node),
                parent,
                is_test: false,
            });
            heading_stack.push((level, index));
        }
    });
    ExtractionResult {
        health: ParseHealth::Complete,
        symbols,
        references: Vec::new(),
    }
}

fn identify(
    path: &RepoPath,
    language: &Language,
    detection: LanguageDetection,
    grammar_version: String,
    source: &[u8],
    mut extracted: ExtractionResult,
) -> FileSymbols {
    // Adapters emit a preorder: every parent precedes its children, and
    // siblings are already in byte order. Sorting here would invalidate the
    // parent indexes carried by `RawSymbol`.
    extracted
        .references
        .sort_by_key(|reference| (reference.byte_range.start, reference.byte_range.end));
    let mut symbols: Vec<Symbol> = Vec::with_capacity(extracted.symbols.len());
    let mut duplicates: HashMap<(String, SymbolKind), u32> = HashMap::new();
    for raw in &extracted.symbols {
        let (name, name_is_lossy) = lossy(&raw.name);
        let qualified_name = raw
            .parent
            .and_then(|parent| symbols.get(parent))
            .map_or_else(
                || name.clone(),
                |parent| format!("{}::{name}", parent.qualified_name),
            );
        let ordinal = duplicates
            .entry((qualified_name.clone(), raw.kind))
            .and_modify(|ordinal| *ordinal = ordinal.saturating_add(1))
            .or_insert(0);
        let ordinal = *ordinal;
        // The landed SymbolId contract has no separate ordinal component. Keep
        // its frozen derivation and disambiguate only residual duplicates in
        // the qualified identity input; the user-facing qualified name remains
        // unchanged and the ordinary (ordinal zero) identity is untouched.
        let identity_name = if ordinal == 0 {
            qualified_name.clone()
        } else {
            format!("{qualified_name}#duplicate:{ordinal}")
        };
        let id = SymbolId::derive(path, language.as_str(), &identity_name, raw.kind.as_str());
        let signature = signature(source, &raw.byte_range);
        let parent = raw
            .parent
            .and_then(|parent| symbols.get(parent))
            .map(|parent| parent.id.clone());
        symbols.push(Symbol {
            id,
            kind: raw.kind,
            name,
            qualified_name,
            ordinal,
            byte_range: raw.byte_range,
            parent,
            signature,
            is_test: raw.is_test,
            name_is_lossy,
        });
    }
    let references = extracted
        .references
        .into_iter()
        .map(|raw| {
            let (name, name_is_lossy) = lossy(&raw.name);
            SymbolReference {
                name,
                byte_range: raw.byte_range,
                name_is_lossy,
            }
        })
        .collect();
    let outline = outline(language, &symbols);
    FileSymbols {
        detection,
        grammar_version,
        health: extracted.health,
        symbols,
        references,
        outline,
    }
}

fn outline(language: &Language, symbols: &[Symbol]) -> StructuralOutline {
    let parents = symbols
        .iter()
        .filter_map(|symbol| symbol.parent.clone())
        .collect::<HashSet<_>>();
    let nodes = symbols
        .iter()
        .filter(|candidate| !parents.contains(&candidate.id))
        .map(|symbol| OutlineNode {
            anchor_path: symbol
                .qualified_name
                .split("::")
                .map(ToOwned::to_owned)
                .collect(),
            byte_range: symbol.byte_range.start..symbol.byte_range.end,
            kind: symbol.kind.as_str().to_owned(),
            symbol: Some(symbol.id.clone()),
        })
        .collect();
    StructuralOutline {
        nodes,
        language: Some(language.clone()),
    }
}

fn signature(source: &[u8], range: &ByteRange) -> Option<String> {
    let start = usize::try_from(range.start).ok()?;
    let end = usize::try_from(range.end).ok()?.min(source.len());
    let bytes = source.get(start..end)?;
    let line = bytes
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::trim_ascii)
        .find(|line| !line.is_empty() && !line.starts_with(b"#["))
        .unwrap_or(bytes);
    let decoded = String::from_utf8_lossy(line);
    let trimmed = decoded.trim();
    let end = crate::text::floor_char_boundary(trimmed, MAX_SIGNATURE_BYTES);
    Some(trimmed[..end].to_owned())
}

fn lossy(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(value) => (value.to_owned(), false),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
    }
}

fn node_range(node: Node<'_>) -> ByteRange {
    ByteRange {
        start: u64::try_from(node.start_byte()).unwrap_or(u64::MAX),
        end: u64::try_from(node.end_byte()).unwrap_or(u64::MAX),
        first_line: Some(u32::try_from(node.start_position().row + 1).unwrap_or(u32::MAX)),
        last_line: Some(u32::try_from(node.end_position().row + 1).unwrap_or(u32::MAX)),
    }
}

fn walk(node: Node<'_>, visit: &mut impl FnMut(Node<'_>)) {
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        visit(node);
        let mut cursor = node.walk();
        let children = node.children(&mut cursor).collect::<Vec<_>>();
        pending.extend(children.into_iter().rev());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> RepoPath {
        RepoPath::from_bytes(value.as_bytes().to_vec())
    }

    #[test]
    fn detection_precedence_is_extension_then_shebang_then_heuristic() {
        let extension = detect_language(&path("script.rs"), b"#!/usr/bin/python3\nprint('x')");
        assert_eq!(extension.language.unwrap().as_str(), "rust");
        assert_eq!(extension.source, Some(LanguageDetectionSource::Extension));

        let shebang = detect_language(&path("script"), b"#!/usr/bin/env sh\necho hi\n");
        assert_eq!(shebang.language.unwrap().as_str(), "shell");
        assert_eq!(shebang.source, Some(LanguageDetectionSource::Shebang));

        let heuristic = detect_language(&path("README"), b"# Heading\nbody\n");
        assert_eq!(heuristic.language.unwrap().as_str(), "markdown");
        assert_eq!(heuristic.source, Some(LanguageDetectionSource::Heuristic));
    }

    #[test]
    fn rust_extracts_types_impl_methods_tests_and_stable_parents() {
        let registry = LanguageRegistry::built_in().unwrap();
        let source = br#"
pub struct Service;
pub trait Runner { fn run(&self); }
impl Service {
    pub fn create(&self) {}
}
#[cfg(test)]
mod tests {
    #[test]
    fn creates() {}
}
"#;
        let extracted = registry.extract(&path("src/lib.rs"), source, &Cancellation::default());
        assert_eq!(extracted.health, ParseHealth::Complete);
        let inventory = extracted
            .symbols
            .iter()
            .map(|symbol| {
                (
                    symbol.kind,
                    symbol.qualified_name.as_str(),
                    symbol.parent.is_some(),
                    symbol.is_test,
                )
            })
            .collect::<Vec<_>>();
        assert!(inventory.contains(&(SymbolKind::Struct, "Service", false, false)));
        assert!(inventory.contains(&(SymbolKind::Trait, "Runner", false, false)));
        assert!(inventory.contains(&(SymbolKind::Method, "Runner::run", true, false)));
        assert!(inventory.contains(&(SymbolKind::Impl, "Service", false, false)));
        assert!(inventory.contains(&(SymbolKind::Method, "Service::create", true, false)));
        assert!(inventory.contains(&(SymbolKind::Function, "tests::creates", true, true)));
    }

    #[test]
    fn the_versioned_rust_fixture_has_an_exact_symbol_inventory() {
        let registry = LanguageRegistry::built_in().unwrap();
        let source = include_bytes!("fixtures/rust-v1.rs");
        let extracted = registry.extract(&path("src/project.rs"), source, &Cancellation::default());
        let by_id = extracted
            .symbols
            .iter()
            .map(|symbol| (symbol.id.clone(), symbol.qualified_name.as_str()))
            .collect::<HashMap<_, _>>();
        let rendered = extracted
            .symbols
            .iter()
            .map(|symbol| {
                format!(
                    "{}|{}|{}|{}..{}|{}|{}|{}",
                    symbol.kind,
                    symbol.name,
                    symbol.qualified_name,
                    symbol.byte_range.start,
                    symbol.byte_range.end,
                    symbol
                        .parent
                        .as_ref()
                        .and_then(|parent| by_id.get(parent))
                        .copied()
                        .unwrap_or("-"),
                    symbol.is_test,
                    symbol.name_is_lossy,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        assert_eq!(rendered, include_str!("fixtures/rust-v1.txt"));
    }

    #[test]
    fn toml_and_markdown_are_adapters_not_core_special_cases() {
        let registry = LanguageRegistry::built_in().unwrap();
        let toml = registry.extract(
            &path("Cargo.toml"),
            b"name = \"root\"\n[package]\nname = \"demo\"\n[dependencies]\nserde = \"1\"\n",
            &Cancellation::default(),
        );
        let toml_inventory = toml
            .symbols
            .iter()
            .map(|symbol| (symbol.kind, symbol.qualified_name.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            toml_inventory,
            [
                (SymbolKind::Constant, "name"),
                (SymbolKind::Module, "package"),
                (SymbolKind::Constant, "package::name"),
                (SymbolKind::Module, "dependencies"),
                (SymbolKind::Constant, "dependencies::serde"),
            ]
        );

        let markdown = registry.extract(
            &path("README.md"),
            b"# Install\ntext\n## Fedora\ntext\n# Usage\n",
            &Cancellation::default(),
        );
        assert_eq!(
            markdown
                .symbols
                .iter()
                .map(|symbol| symbol.qualified_name.as_str())
                .collect::<Vec<_>>(),
            ["Install", "Install::Fedora", "Usage"]
        );
    }

    #[test]
    fn syntax_errors_are_partial_and_do_not_hide_surrounding_symbols() {
        let registry = LanguageRegistry::built_in().unwrap();
        let source = b"fn before() {}\nfn broken( {\nfn after() {}\n";
        let extracted = registry.extract(&path("broken.rs"), source, &Cancellation::default());
        let ParseHealth::Partial { error_ranges } = &extracted.health else {
            panic!("a syntax error must be recorded as partial health")
        };
        assert!(!error_ranges.is_empty());
        assert!(
            error_ranges
                .iter()
                .all(|range| range.start <= range.end && range.end <= source.len() as u64)
        );
        let names = extracted
            .symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"before"));
        assert!(names.contains(&"after"));
    }

    #[test]
    fn unsupported_language_is_not_an_extracted_empty_file() {
        let registry = LanguageRegistry::built_in().unwrap();
        let extracted = registry.extract(
            &path("script.py"),
            b"def answer(): return 42\n",
            &Cancellation::default(),
        );
        assert!(matches!(
            extracted.health,
            ParseHealth::Skipped {
                reason: ExtractionSkipReason::UnsupportedLanguage
            }
        ));
        assert!(extracted.symbols.is_empty());

        let supported_empty = registry.extract(
            &path("empty.rs"),
            b"// deliberately no declarations\n",
            &Cancellation::default(),
        );
        assert_eq!(supported_empty.health, ParseHealth::Complete);
        assert!(supported_empty.symbols.is_empty());
    }

    #[test]
    fn unrelated_body_edits_preserve_other_symbol_ids() {
        let registry = LanguageRegistry::built_in().unwrap();
        let first = registry.extract(
            &path("lib.rs"),
            b"fn one() { old(); }\nfn two() { same(); }\n",
            &Cancellation::default(),
        );
        let second = registry.extract(
            &path("lib.rs"),
            b"fn one() { changed(); }\nfn two() { same(); }\n",
            &Cancellation::default(),
        );
        let first_two = first
            .symbols
            .iter()
            .find(|symbol| symbol.name == "two")
            .unwrap();
        let second_two = second
            .symbols
            .iter()
            .find(|symbol| symbol.name == "two")
            .unwrap();
        assert_eq!(first_two.id, second_two.id);
    }

    #[test]
    fn lossy_names_keep_exact_ranges_and_duplicate_ordinals_are_stable() {
        let path = path("src/raw.rs");
        let language = Language::new("rust").unwrap();
        let source = [b"fn ".as_slice(), &[0xff], &vec![b'x'; 600]].concat();
        let raw = RawSymbol {
            name: vec![0xff],
            kind: SymbolKind::Function,
            byte_range: ByteRange {
                start: 0,
                end: u64::try_from(source.len()).unwrap(),
                first_line: Some(1),
                last_line: Some(1),
            },
            parent: None,
            is_test: false,
        };
        let first = identify(
            &path,
            &language,
            detected("rust", LanguageDetectionSource::Extension),
            "fixture-1".to_owned(),
            &source,
            ExtractionResult {
                health: ParseHealth::Complete,
                symbols: vec![raw.clone(), raw],
                references: Vec::new(),
            },
        );
        let second = first.clone();

        assert_eq!(first, second);
        assert!(first.symbols.iter().all(|symbol| symbol.name_is_lossy));
        assert_eq!(first.symbols[0].byte_range.end, source.len() as u64);
        assert_eq!(first.symbols[0].ordinal, 0);
        assert_eq!(first.symbols[1].ordinal, 1);
        assert_ne!(first.symbols[0].id, first.symbols[1].id);
        assert!(
            first.symbols[0]
                .signature
                .as_ref()
                .is_some_and(|signature| signature.len() <= MAX_SIGNATURE_BYTES)
        );
    }

    struct PanickingAdapter;

    impl LanguageAdapter for PanickingAdapter {
        fn language(&self) -> Language {
            Language::new("rust").unwrap()
        }

        fn grammar_version(&self) -> &'static str {
            "panic-1"
        }

        fn extract(&self, _: &[u8], _: &Cancellation) -> ExtractionResult {
            panic!("fault injection")
        }
    }

    #[test]
    fn adapter_panic_degrades_exactly_one_file() {
        let registry = LanguageRegistry::new(vec![Arc::new(PanickingAdapter)]).unwrap();
        let failed = registry.extract(&path("bad.rs"), b"fn bad() {}", &Cancellation::default());
        assert_eq!(
            failed.health,
            ParseHealth::Failed {
                reason: "adapter_panicked".to_owned()
            }
        );
        let next = registry.extract(&path("notes.md"), b"# Fine", &Cancellation::default());
        assert!(matches!(next.health, ParseHealth::Skipped { .. }));
    }

    #[test]
    #[ignore = "release-mode extraction throughput benchmark"]
    fn rust_extraction_meets_the_single_file_throughput_target() {
        let registry = LanguageRegistry::built_in().unwrap();
        let mut source = Vec::with_capacity(256 * 1024);
        let mut index = 0_u32;
        loop {
            let item = format!(
                "pub fn item_{index}() {{ let value = {index}; /* {} */ }}\n",
                "representative source body ".repeat(16)
            );
            if source.len() + item.len() > 256 * 1024 {
                break;
            }
            source.extend_from_slice(item.as_bytes());
            index += 1;
        }
        source.extend_from_slice(b"// ");
        source.resize(256 * 1024, b'x');
        let started = std::time::Instant::now();
        let extracted = registry.extract(&path("src/medium.rs"), &source, &Cancellation::default());
        let elapsed = started.elapsed();
        let mib_per_second = (source.len() as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
        eprintln!(
            "symbol extraction: {} bytes, {} symbols, {:.2} ms, {:.2} MiB/s",
            source.len(),
            extracted.symbols.len(),
            elapsed.as_secs_f64() * 1_000.0,
            mib_per_second,
        );
        assert!(!extracted.symbols.is_empty());
        if !cfg!(debug_assertions) {
            assert!(elapsed < std::time::Duration::from_millis(200));
            assert!(mib_per_second >= 5.0);
        }
    }
}
