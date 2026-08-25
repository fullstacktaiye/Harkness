//! How a file is treated once the context engine has found it.
//!
//! [`FileClass`] is the vocabulary and [`FileSample::classify`] is the total
//! function that assigns it, by the precedence documented there. A sample is a
//! path, a size, and at most [`BINARY_SNIFF_BYTES`] of a file's opening bytes:
//! everything else a classification could want — the rest of the content, the
//! repository, the index — is deliberately out of reach, so the answer is
//! reproducible from what a persisted row could carry.
//!
//! Classification is not the denial layer. The inventory's built-in denials are
//! matched *before* a path is ever recorded, so a `.env` never reaches a sample
//! at all; [`FileClass::SecretSensitive`] is the weaker, recorded answer for a
//! name that merely looks credential-bearing. The two are versioned together by
//! [`CLASSIFY_VERSION`] and documented in `docs/context-inventory.md`.

use serde::{Deserialize, Serialize};

use crate::path::RepoPath;

/// The version of the classification rules and of the inventory's built-in
/// denial list.
///
/// Bumping it invalidates everything derived from a classification — [#114]'s
/// cached rows above all — rather than silently reclassifying evidence that was
/// recorded under the old rules. Adding a pattern, moving a precedence step, or
/// widening a denial is a bump; renaming a private helper is not.
///
/// [#114]: https://github.com/fullstacktaiye/harkness/issues/114
pub const CLASSIFY_VERSION: u32 = 1;

/// How much of a file's opening bytes a classification may read.
///
/// The window bounds every content-derived answer: an ineligible file is never
/// read past it, and a file that is eligible is read again by whoever indexes
/// it rather than here.
pub const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// The size past which a file's content stops being worth indexing whole.
///
/// Mirrors `harkness_git::DEFAULT_MAX_DIFF_FILE_SIZE`, which draws the same line
/// for the same reason one layer down.
pub const OVERSIZED_FILE_THRESHOLD: u64 = 1024 * 1024;

/// How much of the window an `@generated` marker must appear in.
const GENERATED_MARKER_BYTES: usize = 1024;

/// The marker convention generators write into their output.
const GENERATED_MARKER: &[u8] = b"@generated";

/// The average line length past which sniffed `.js`/`.css` reads as minified.
const MINIFIED_AVERAGE_LINE_BYTES: usize = 512;

/// What kind of file a path holds, for retrieval and exclusion decisions.
///
/// # Forward compatibility
///
/// The enum is [`non_exhaustive`], so a later release may add a class and
/// downstream crates must keep a wildcard arm. Deserialization is deliberately
/// *not* forgiving in the same way: a spelling this build does not define is a
/// hard error, never a silent coercion to [`FileClass::UnknownText`]. The
/// catalog takes the same position on same-version unknown fields, and for the
/// same reason — a file quietly reclassified from `secret_sensitive` to
/// something benign is an exclusion that stops happening without anyone being
/// told. A build that meets a class it does not know is out of date, and saying
/// so is the safe answer.
///
/// [`non_exhaustive`]: https://doc.rust-lang.org/reference/attributes/type_system.html
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum FileClass {
    /// Program text the user wrote and the model may be asked to change.
    Source,
    /// Program text whose purpose is to exercise other program text.
    TestCode,
    /// Settings consumed by a tool or a runtime.
    Configuration,
    /// Prose written for people: guides, references, changelogs.
    Documentation,
    /// Prose written for an agent: the discovered instruction set.
    Instruction,
    /// A manifest that declares a build's dependencies and targets.
    BuildManifest,
    /// Output of a generator, reproducible from something else in the tree.
    Generated,
    /// Third-party code checked into the tree rather than depended on.
    Vendor,
    /// A resolved dependency graph, large and rarely worth reading.
    Lockfile,
    /// Content with no useful text form.
    Binary,
    /// Content that matches a secret-bearing rule and must not be retrieved.
    SecretSensitive,
    /// Text past the size at which retrieval stops being worthwhile.
    Oversized,
    /// Text whose kind could not be determined.
    UnknownText,
    /// Content that claims to be text but cannot be decoded as any encoding
    /// Harkness reads.
    UnsupportedEncoding,
}

impl FileClass {
    /// Every file class in its stable declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Source,
        Self::TestCode,
        Self::Configuration,
        Self::Documentation,
        Self::Instruction,
        Self::BuildManifest,
        Self::Generated,
        Self::Vendor,
        Self::Lockfile,
        Self::Binary,
        Self::SecretSensitive,
        Self::Oversized,
        Self::UnknownText,
        Self::UnsupportedEncoding,
    ];

    /// Returns the stable persisted spelling of this class.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::TestCode => "test_code",
            Self::Configuration => "configuration",
            Self::Documentation => "documentation",
            Self::Instruction => "instruction",
            Self::BuildManifest => "build_manifest",
            Self::Generated => "generated",
            Self::Vendor => "vendor",
            Self::Lockfile => "lockfile",
            Self::Binary => "binary",
            Self::SecretSensitive => "secret_sensitive",
            Self::Oversized => "oversized",
            Self::UnknownText => "unknown_text",
            Self::UnsupportedEncoding => "unsupported_encoding",
        }
    }

    /// Reads a persisted spelling back, refusing one this build does not define.
    ///
    /// The refusal is the same position [`Deserialize`] takes and it is taken
    /// for the same reason: a class this build does not know means a newer build
    /// wrote the row, and coercing it to something benign is how a
    /// [`SecretSensitive`](Self::SecretSensitive) file quietly stops being
    /// excluded. A caller that meets [`None`] is holding a row it should refuse,
    /// not one it should guess about.
    #[must_use]
    pub fn parse(spelling: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|class| class.as_str() == spelling)
    }

    /// Whether a file of this class may ever be shown to a model.
    ///
    /// Advisory: the exclusion itself is enforced where retrieval happens. The
    /// answer lives beside the vocabulary so two components cannot disagree
    /// about what `secret_sensitive` means.
    #[must_use]
    pub const fn is_retrievable(self) -> bool {
        !matches!(
            self,
            Self::SecretSensitive | Self::Binary | Self::UnsupportedEncoding
        )
    }

    /// Whether a file of this class may have its content indexed.
    ///
    /// Stricter than [`Self::is_retrievable`] by exactly one class, and the
    /// difference is deliberate: a secret is *forbidden*, while an
    /// [`Oversized`] file is merely refused whole. Nothing may index a
    /// megabyte-plus file as one unit, but a later stage that learns to serve a
    /// bounded slice of one is not violating a prohibition, because there is
    /// none to violate.
    ///
    /// [`Oversized`]: Self::Oversized
    #[must_use]
    pub const fn is_eligible(self) -> bool {
        self.is_retrievable() && !matches!(self, Self::Oversized)
    }
}

impl std::fmt::Display for FileClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One file as the classifier is allowed to see it.
///
/// A sample is deliberately small: a repository-relative path, a size, and at
/// most [`BINARY_SNIFF_BYTES`] of opening bytes. Classification is therefore
/// pure and total — every combination of those three yields exactly one
/// [`FileClass`] and reads nothing else — which is what lets a persisted
/// classification be re-derived and compared rather than trusted.
///
/// A sample with no window is the honest shape for something whose content was
/// never read: a symlink, a repository boundary, or a file whose bytes could not
/// be opened. Content-derived classes are then unreachable, and the answer comes
/// from the path and the size alone.
///
/// ```
/// use harkness_context::{FileClass, FileSample, RepoPath};
///
/// let path = RepoPath::from_bytes(b"src/main.rs".to_vec());
/// let sample = FileSample::new(&path, 42).with_window(b"fn main() {}\n");
/// assert_eq!(sample.classify(), FileClass::Source);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct FileSample<'a> {
    path: &'a RepoPath,
    byte_size: u64,
    window: Option<&'a [u8]>,
}

impl<'a> FileSample<'a> {
    /// Describes a file by path and size, with no content read.
    #[must_use]
    pub const fn new(path: &'a RepoPath, byte_size: u64) -> Self {
        Self {
            path,
            byte_size,
            window: None,
        }
    }

    /// Attaches the bytes a bounded read produced.
    ///
    /// The window is expected to be the file's first [`BINARY_SNIFF_BYTES`] or
    /// the whole file, whichever is shorter; a longer one is read no further.
    /// Whether it covers the whole file is derived from `byte_size`, which is
    /// what lets a UTF-8 sequence cut in half by the window boundary be
    /// tolerated instead of read as an undecodable file.
    #[must_use]
    pub fn with_window(mut self, window: &'a [u8]) -> Self {
        let end = window.len().min(BINARY_SNIFF_BYTES);
        self.window = Some(&window[..end]);
        self
    }

    /// Whether this file's *name* alone makes it secret-sensitive.
    ///
    /// The first step of [`Self::classify`], asked on its own so a caller can
    /// answer "may I open this file" before opening it. A walk uses it to keep
    /// the promise that a credential-looking name is never read: the alternative
    /// is running the whole cascade and comparing the answer against one class,
    /// which reads the file's bytes to decide whether it was allowed to.
    #[must_use]
    pub fn is_secret_by_name(&self) -> bool {
        is_secret_sensitive(file_name(self.path.as_bytes()))
    }

    /// The one class this file holds.
    ///
    /// Exactly one class is assigned, by the first rule that matches, in this
    /// order. The order is the contract; the individual patterns are the part
    /// that may grow under a [`CLASSIFY_VERSION`] bump.
    ///
    /// | # | Class | Decided by |
    /// | --- | --- | --- |
    /// | 1 | [`SecretSensitive`] | a name heuristic beyond the inventory's denial list |
    /// | 2 | [`Binary`] | a NUL byte in the sniff window |
    /// | 3 | [`Oversized`] | [`OVERSIZED_FILE_THRESHOLD`] |
    /// | 4 | [`UnsupportedEncoding`] | the window decodes as neither UTF-8 nor UTF-16 |
    /// | 5 | [`Lockfile`] | an exact resolved-dependency file name |
    /// | 6 | [`Vendor`] | a `vendor`, `node_modules`, `third_party` or `.venv` segment |
    /// | 7 | [`Generated`] | an output segment, a `.min.*` name, an `@generated` marker, or minified line lengths |
    /// | 8 | [`Instruction`] | an agent-instruction name, or `.harkness/**.md` |
    /// | 9 | [`BuildManifest`] | an exact build-manifest name |
    /// | 10 | [`TestCode`] | a test segment, or a `*_test.*` / `*.test.*` name |
    /// | 11 | [`Configuration`] | a settings extension, or a dotfile |
    /// | 12 | [`Documentation`] | a prose extension, or a `docs` segment |
    /// | 13 | [`Source`] | a known language extension |
    /// | 14 | [`UnknownText`] | nothing above matched |
    ///
    /// Three positions earn their places. `Binary` precedes `Oversized` so that
    /// a large image is reported by what it *is* rather than by its size, which
    /// is what a user is asking when they wonder why a file is not in context.
    /// `Oversized` precedes `UnsupportedEncoding` because a file too large to
    /// index is never decoded past the window, so calling it undecodable would
    /// be a claim about eight kilobytes rather than about the file. And
    /// `Vendor` precedes `Generated` so that `node_modules/x/dist/y.js` reads
    /// as somebody else's code rather than as this repository's output.
    ///
    /// [`Binary`]: FileClass::Binary
    /// [`BuildManifest`]: FileClass::BuildManifest
    /// [`Configuration`]: FileClass::Configuration
    /// [`Documentation`]: FileClass::Documentation
    /// [`Generated`]: FileClass::Generated
    /// [`Instruction`]: FileClass::Instruction
    /// [`Lockfile`]: FileClass::Lockfile
    /// [`Oversized`]: FileClass::Oversized
    /// [`SecretSensitive`]: FileClass::SecretSensitive
    /// [`Source`]: FileClass::Source
    /// [`TestCode`]: FileClass::TestCode
    /// [`UnknownText`]: FileClass::UnknownText
    /// [`UnsupportedEncoding`]: FileClass::UnsupportedEncoding
    /// [`Vendor`]: FileClass::Vendor
    #[must_use]
    pub fn classify(&self) -> FileClass {
        let path = self.path.as_bytes();
        let name = file_name(path);

        if is_secret_sensitive(name) {
            return FileClass::SecretSensitive;
        }

        let encoding = self.window.map(|window| {
            let complete = u64::try_from(window.len()).unwrap_or(u64::MAX) >= self.byte_size;
            sniff(window, complete)
        });
        if encoding == Some(ContentSniff::Binary) {
            return FileClass::Binary;
        }
        if self.byte_size > OVERSIZED_FILE_THRESHOLD {
            return FileClass::Oversized;
        }
        if encoding == Some(ContentSniff::Undecodable) {
            return FileClass::UnsupportedEncoding;
        }

        if is_lockfile(name) {
            FileClass::Lockfile
        } else if is_vendor(path) {
            FileClass::Vendor
        } else if is_generated(path, name, self.window.unwrap_or_default()) {
            FileClass::Generated
        } else if is_instruction(path, name) {
            FileClass::Instruction
        } else if is_build_manifest(name) {
            FileClass::BuildManifest
        } else if is_test_code(path, name) {
            FileClass::TestCode
        } else if is_configuration(name) {
            FileClass::Configuration
        } else if is_documentation(path, name) {
            FileClass::Documentation
        } else if is_source(name) {
            FileClass::Source
        } else {
            FileClass::UnknownText
        }
    }
}

/// What a bounded read says about a file's encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContentSniff {
    /// Decodable as UTF-8 or as UTF-16 with a byte-order mark.
    Text,
    /// Holds a NUL byte, which no text encoding Harkness reads produces
    /// outside the UTF-16 forms a byte-order mark announces.
    Binary,
    /// Neither: bytes that claim to be text in an encoding Harkness does not
    /// read.
    Undecodable,
}

/// The byte-order marks that make a NUL byte part of a text encoding.
const UTF16_LE_BOM: &[u8] = &[0xFF, 0xFE];
const UTF16_BE_BOM: &[u8] = &[0xFE, 0xFF];
const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Decides an encoding from the opening bytes.
///
/// `complete` says whether the window is the whole file, and it is what
/// separates "this file ends mid-character" from "this window does". A
/// truncated trailing sequence is tolerated; the same bytes at the end of a
/// whole file are not.
fn sniff(window: &[u8], complete: bool) -> ContentSniff {
    if window.starts_with(UTF16_LE_BOM) || window.starts_with(UTF16_BE_BOM) {
        // Checked before the NUL scan, and only ever because of the mark: a
        // UTF-16 file is half NUL bytes by construction, so a scan first would
        // report every one of them as binary.
        let big_endian = window.starts_with(UTF16_BE_BOM);
        return if decodes_as_utf16(&window[2..], big_endian, complete) {
            ContentSniff::Text
        } else {
            // A mark is a claim, not proof. Bytes that open `FF FE` and then
            // fail to be UTF-16 — a UTF-32LE file, whose mark is `FF FE 00 00`,
            // or any binary format starting the same way — fall back to the scan
            // every other file gets rather than to a verdict about encoding.
            binary_or_utf8(window, complete)
        };
    }
    binary_or_utf8(window, complete)
}

/// The verdict for bytes that made no UTF-16 claim, or made one and failed it.
fn binary_or_utf8(window: &[u8], complete: bool) -> ContentSniff {
    let body = window.strip_prefix(UTF8_BOM).unwrap_or(window);
    if body.contains(&0) {
        return ContentSniff::Binary;
    }
    match std::str::from_utf8(body) {
        Ok(_) => ContentSniff::Text,
        // `error_len() == None` is serde-free for "the input ended inside a
        // character", which is exactly what a window boundary produces.
        Err(error) if !complete && error.error_len().is_none() => ContentSniff::Text,
        Err(_) => ContentSniff::Undecodable,
    }
}

/// Whether the bytes after a byte-order mark are UTF-16 code units.
///
/// A NUL character fails the decode rather than passing it. Nothing else here
/// would notice one: a UTF-32LE file opens with the UTF-16 LE mark, and every
/// `0x0000` half of its code points decodes as a perfectly valid `U+0000`, so a
/// decoder that accepted them would call a NUL-riddled binary "text" and hand it
/// to an indexer. Real UTF-16 prose does not contain NUL characters.
fn decodes_as_utf16(body: &[u8], big_endian: bool, complete: bool) -> bool {
    if complete && !body.len().is_multiple_of(2) {
        return false;
    }
    let units = body.as_chunks::<2>().0.iter().map(|pair| {
        if big_endian {
            u16::from_be_bytes([pair[0], pair[1]])
        } else {
            u16::from_le_bytes([pair[0], pair[1]])
        }
    });
    let mut decoder = char::decode_utf16(units);
    loop {
        match decoder.next() {
            None => return true,
            Some(Ok('\0')) => return false,
            Some(Ok(_)) => {}
            // A lone leading surrogate as the last unit is a pair the window
            // cut in half; anywhere else it is not UTF-16 at all.
            Some(Err(_)) => return !complete && decoder.next().is_none(),
        }
    }
}

/// Names that look credential-bearing without being denied outright.
///
/// Applied to the file name, never to a directory, and never to a file carrying
/// a language extension: source code *about* credentials is source code, and
/// `token.rs` is the name of a parser far more often than of a secret. The
/// denial list has no such exemption, because a file named `id_rsa.go` is still
/// refused.
///
/// The fragments are qualified rather than bare for the same reason. A bare
/// `token` classifies `tokens.json`, `design-tokens.css`, and most of an i18n
/// or design-system tree — none of which hold a credential, and all of which
/// would become unretrievable with no layer able to re-include a *class*.
/// `access_token` does not have that problem, and the files this rule exists for
/// are named that way.
fn is_secret_sensitive(name: &[u8]) -> bool {
    if is_source(name) {
        return false;
    }
    const PREFIXES: &[&str] = &["secret", "credential"];
    const FRAGMENTS: &[&str] = &[
        "access_token",
        "access-token",
        "auth_token",
        "auth-token",
        "api_token",
        "api-token",
        "refresh_token",
        "refresh-token",
        "bearer_token",
        "bearer-token",
        "apikey",
        "api_key",
        "api-key",
        "password",
        "passwd",
    ];
    const SUFFIXES: &[&str] = &[".dump"];

    PREFIXES
        .iter()
        .any(|prefix| starts_with_ignore_ascii_case(name, prefix.as_bytes()))
        || FRAGMENTS
            .iter()
            .any(|fragment| contains_ignore_ascii_case(name, fragment.as_bytes()))
        || SUFFIXES
            .iter()
            .any(|suffix| ends_with_ignore_ascii_case(name, suffix.as_bytes()))
}

/// Resolved dependency graphs, matched by exact name.
///
/// A `*.lock` glob would be wrong here: `projects.lock` in this project's own
/// data directory is a lock *file* in the other sense, and so is every advisory
/// lock a repository happens to check in.
fn is_lockfile(name: &[u8]) -> bool {
    const NAMES: &[&str] = &[
        "Cargo.lock",
        "package-lock.json",
        "npm-shrinkwrap.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lockb",
        "poetry.lock",
        "Pipfile.lock",
        "uv.lock",
        "go.sum",
        "Gemfile.lock",
        "composer.lock",
        "flake.lock",
        "gradle.lockfile",
    ];
    NAMES.iter().any(|candidate| name == candidate.as_bytes())
}

/// Third-party code checked into the tree.
fn is_vendor(path: &[u8]) -> bool {
    const SEGMENTS: &[&str] = &[
        "vendor",
        "node_modules",
        "third_party",
        "thirdparty",
        ".venv",
    ];
    SEGMENTS
        .iter()
        .any(|segment| has_directory_segment(path, segment.as_bytes()))
}

/// Output of a generator, by location, by name, or by what the bytes look like.
fn is_generated(path: &[u8], name: &[u8], window: &[u8]) -> bool {
    const SEGMENTS: &[&str] = &["target", "build", "dist", "out"];
    const SUFFIXES: &[&str] = &[".min.js", ".min.css", ".min.mjs"];

    if SEGMENTS
        .iter()
        .any(|segment| has_directory_segment(path, segment.as_bytes()))
    {
        return true;
    }
    if SUFFIXES
        .iter()
        .any(|suffix| ends_with_ignore_ascii_case(name, suffix.as_bytes()))
    {
        return true;
    }
    let marker_window = &window[..window.len().min(GENERATED_MARKER_BYTES)];
    if contains_ignore_ascii_case(marker_window, GENERATED_MARKER) {
        return true;
    }
    // A minified bundle carries no marker and may sit anywhere, so the last
    // signal is the shape of the bytes themselves. Restricted to the two
    // languages that are actually shipped minified, because long lines are
    // ordinary prose everywhere else.
    extension_is(name, &["js", "mjs", "cjs", "css"])
        && average_line_length(window) > MINIFIED_AVERAGE_LINE_BYTES
}

/// Prose written for an agent rather than for a person.
fn is_instruction(path: &[u8], name: &[u8]) -> bool {
    const NAMES: &[&str] = &["AGENTS.md", "CLAUDE.md", "CONTRIBUTING.md"];
    NAMES.iter().any(|candidate| name == candidate.as_bytes())
        || (path.starts_with(b".harkness/") && extension_is(name, &["md"]))
}

/// A manifest that declares a build's dependencies and targets.
fn is_build_manifest(name: &[u8]) -> bool {
    const NAMES: &[&str] = &[
        "Cargo.toml",
        "package.json",
        "CMakeLists.txt",
        "pyproject.toml",
        "setup.py",
        "go.mod",
        "Makefile",
        "GNUmakefile",
        "Gemfile",
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        "meson.build",
    ];
    NAMES.iter().any(|candidate| name == candidate.as_bytes())
}

/// Program text whose purpose is to exercise other program text.
fn is_test_code(path: &[u8], name: &[u8]) -> bool {
    const SEGMENTS: &[&str] = &["tests", "test", "__tests__"];
    if SEGMENTS
        .iter()
        .any(|segment| has_directory_segment(path, segment.as_bytes()))
    {
        return true;
    }
    let stem = stem(name);
    ends_with_ignore_ascii_case(stem, b"_test") || ends_with_ignore_ascii_case(stem, b".test")
}

/// Settings consumed by a tool or a runtime.
fn is_configuration(name: &[u8]) -> bool {
    extension_is(
        name,
        &[
            "toml",
            "yaml",
            "yml",
            "json",
            "ini",
            "cfg",
            "conf",
            "properties",
        ],
    ) || name.starts_with(b".")
}

/// Prose written for people.
fn is_documentation(path: &[u8], name: &[u8]) -> bool {
    extension_is(name, &["md", "markdown", "rst", "txt", "adoc", "org"])
        || has_directory_segment(path, b"docs")
        || has_directory_segment(path, b"doc")
}

/// Program text in a language Harkness recognizes.
fn is_source(name: &[u8]) -> bool {
    const EXTENSIONS: &[&str] = &[
        "rs", "go", "py", "pyi", "rb", "js", "mjs", "cjs", "jsx", "ts", "tsx", "java", "kt", "kts",
        "scala", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "hxx", "m", "mm", "cs", "swift", "php",
        "pl", "pm", "lua", "r", "jl", "hs", "ml", "mli", "ex", "exs", "erl", "clj", "cljs", "dart",
        "zig", "nim", "vue", "svelte", "qml", "proto", "sql", "sh", "bash", "zsh", "fish", "ps1",
    ];
    extension_is(name, EXTENSIONS)
}

/// The bytes after the last separator.
fn file_name(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|byte| *byte == b'/') {
        Some(index) => &path[index + 1..],
        None => path,
    }
}

/// The file name with its extension removed, keeping a leading dot.
fn stem(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|byte| *byte == b'.') {
        Some(index) if index > 0 => &name[..index],
        _ => name,
    }
}

/// The bytes after the last dot of a file name, if it has one.
///
/// A leading dot is a hidden-file marker rather than an extension separator, so
/// `.gitignore` has no extension and `.env.local` has `local`.
fn extension(name: &[u8]) -> Option<&[u8]> {
    match name.iter().rposition(|byte| *byte == b'.') {
        Some(index) if index > 0 => Some(&name[index + 1..]),
        _ => None,
    }
}

/// Whether a file name carries one of the given extensions, ASCII-case-blind.
///
/// Visible to the crate because `chunk` asks the same question of the same
/// names: a strategy chosen from a second, case-sensitive spelling would
/// disagree with the class this module already assigned.
pub(crate) fn extension_is(name: &[u8], candidates: &[&str]) -> bool {
    let Some(found) = extension(name) else {
        return false;
    };
    candidates
        .iter()
        .any(|candidate| found.eq_ignore_ascii_case(candidate.as_bytes()))
}

/// Whether any directory component of a path equals `segment`.
///
/// The file name is not a directory component, so a file called `vendor` is not
/// a vendored tree.
fn has_directory_segment(path: &[u8], segment: &[u8]) -> bool {
    let Some(end) = path.iter().rposition(|byte| *byte == b'/') else {
        return false;
    };
    path[..end]
        .split(|byte| *byte == b'/')
        .any(|component| component == segment)
}

fn starts_with_ignore_ascii_case(haystack: &[u8], prefix: &[u8]) -> bool {
    haystack.len() >= prefix.len() && haystack[..prefix.len()].eq_ignore_ascii_case(prefix)
}

fn ends_with_ignore_ascii_case(haystack: &[u8], suffix: &[u8]) -> bool {
    haystack.len() >= suffix.len()
        && haystack[haystack.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

/// Mean bytes per line over the sniff window, counting an unterminated tail as
/// one line.
fn average_line_length(window: &[u8]) -> usize {
    if window.is_empty() {
        return 0;
    }
    let lines = window.iter().filter(|byte| **byte == b'\n').count() + 1;
    window.len() / lines
}

#[cfg(test)]
mod tests {
    use super::FileClass;

    #[test]
    fn there_are_exactly_fourteen_documented_classes() {
        assert_eq!(FileClass::ALL.len(), 14);
        let mut spellings = FileClass::ALL
            .iter()
            .map(|class| class.as_str())
            .collect::<Vec<_>>();
        spellings.sort_unstable();
        spellings.dedup();
        assert_eq!(spellings.len(), 14, "two classes share a spelling");
    }

    #[test]
    fn every_class_serializes_as_its_snake_case_spelling() {
        for class in FileClass::ALL {
            let json = serde_json::to_string(class).unwrap();
            assert_eq!(json, format!("\"{}\"", class.as_str()));
            assert_eq!(&serde_json::from_str::<FileClass>(&json).unwrap(), class);
            assert_eq!(class.to_string(), class.as_str());
        }
    }

    #[test]
    fn an_unknown_class_fails_rather_than_coercing() {
        for spelling in ["\"Source\"", "\"secret\"", "\"executable_bit\"", "\"\""] {
            assert!(
                serde_json::from_str::<FileClass>(spelling).is_err(),
                "accepted {spelling}"
            );
        }
    }

    #[test]
    fn the_classes_that_must_never_be_retrieved_are_named() {
        let blocked = FileClass::ALL
            .iter()
            .filter(|class| !class.is_retrievable())
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(
            blocked,
            [
                FileClass::Binary,
                FileClass::SecretSensitive,
                FileClass::UnsupportedEncoding
            ]
        );
    }

    #[test]
    fn eligibility_refuses_one_more_class_than_retrieval_does() {
        let ineligible = FileClass::ALL
            .iter()
            .filter(|class| !class.is_eligible())
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(
            ineligible,
            [
                FileClass::Binary,
                FileClass::SecretSensitive,
                FileClass::Oversized,
                FileClass::UnsupportedEncoding
            ]
        );
        assert!(FileClass::Oversized.is_retrievable());
    }
}

#[cfg(test)]
mod classification_tests {
    use super::{
        BINARY_SNIFF_BYTES, ContentSniff, FileClass, FileSample, OVERSIZED_FILE_THRESHOLD, sniff,
    };
    use crate::path::RepoPath;

    /// One row of the precedence table: a path, a size, the bytes a bounded read
    /// would produce, and the one class they must produce together.
    struct Row {
        path: &'static str,
        byte_size: Option<u64>,
        window: &'static [u8],
        class: FileClass,
    }

    const fn row(path: &'static str, window: &'static [u8], class: FileClass) -> Row {
        Row {
            path,
            byte_size: None,
            window,
            class,
        }
    }

    fn classify(row: &Row) -> FileClass {
        let path = RepoPath::from_bytes(row.path.as_bytes().to_vec());
        let size = row
            .byte_size
            .unwrap_or_else(|| u64::try_from(row.window.len()).unwrap());
        FileSample::new(&path, size)
            .with_window(row.window)
            .classify()
    }

    #[test]
    fn every_class_is_reached_by_the_documented_precedence() {
        let oversized = Row {
            path: "docs/enormous.md",
            byte_size: Some(OVERSIZED_FILE_THRESHOLD + 1),
            window: b"# a very long document\n",
            class: FileClass::Oversized,
        };
        let rows = [
            row(
                "config/credentials.yaml",
                b"user: x\n",
                FileClass::SecretSensitive,
            ),
            row(
                "assets/logo.png",
                b"\x89PNG\r\n\x1a\n\0\0",
                FileClass::Binary,
            ),
            oversized,
            row(
                "notes.txt",
                b"caf\xe9 latin-1\n",
                FileClass::UnsupportedEncoding,
            ),
            row("Cargo.lock", b"[[package]]\n", FileClass::Lockfile),
            row("vendor/lib/dist/lib.js", b"export {}\n", FileClass::Vendor),
            row("web/app.min.js", b"a=1\n", FileClass::Generated),
            row("AGENTS.md", b"# instructions\n", FileClass::Instruction),
            row("Cargo.toml", b"[package]\n", FileClass::BuildManifest),
            row("tests/walk.rs", b"fn main() {}\n", FileClass::TestCode),
            row("service.yaml", b"kind: Service\n", FileClass::Configuration),
            row("README.md", b"# readme\n", FileClass::Documentation),
            row("src/main.rs", b"fn main() {}\n", FileClass::Source),
            row("LICENSE", b"MIT\n", FileClass::UnknownText),
        ];

        let reached = rows.iter().map(|row| row.class).collect::<Vec<_>>();
        for row in &rows {
            assert_eq!(classify(row), row.class, "misclassified {}", row.path);
        }
        for class in FileClass::ALL {
            assert!(reached.contains(class), "{class} is never reached");
        }
    }

    #[test]
    fn a_higher_rule_wins_over_every_lower_one_it_overlaps() {
        // Each row would match a later rule too; the earlier one has to win.
        let cases = [
            // A secret name in a documentation extension is not documentation.
            row(
                "docs/credentials.md",
                b"# how to\n",
                FileClass::SecretSensitive,
            ),
            // A binary lockfile is reported as binary, not as a lockfile.
            row("Cargo.lock", b"\0\0\0\0", FileClass::Binary),
            // Vendored generated output is vendored.
            row("node_modules/x/app.min.js", b"a=1\n", FileClass::Vendor),
            // A build manifest inside a test tree is still a manifest.
            row(
                "tests/fixture/Cargo.toml",
                b"[package]\n",
                FileClass::BuildManifest,
            ),
            // An instruction file beats both documentation and configuration.
            row(".harkness/policy.md", b"# rules\n", FileClass::Instruction),
            // A test file with a configuration extension is test code.
            row("tests/data.json", b"{}\n", FileClass::TestCode),
            // A dotfile is configuration rather than unknown text.
            row(".gitignore", b"target\n", FileClass::Configuration),
        ];
        for case in &cases {
            assert_eq!(classify(case), case.class, "misclassified {}", case.path);
        }
    }

    #[test]
    fn source_code_named_after_a_secret_stays_source() {
        // The denial list is where a credential is stopped; a name heuristic
        // must not make a repository's own token parser unindexable.
        for path in ["src/token.rs", "internal/credentials.go", "lib/password.py"] {
            let path = RepoPath::from_bytes(path.as_bytes().to_vec());
            assert_eq!(
                FileSample::new(&path, 10).with_window(b"code\n").classify(),
                FileClass::Source
            );
        }
        // A name with no language extension is still refused.
        let path = RepoPath::from_bytes(b"deploy/api_key.txt".to_vec());
        assert_eq!(
            FileSample::new(&path, 10).with_window(b"abc\n").classify(),
            FileClass::SecretSensitive
        );
    }

    #[test]
    fn a_sample_with_no_content_skips_every_content_derived_class() {
        let path = RepoPath::from_bytes(b"link-to-image.png".to_vec());
        // The same bytes with a window would be binary; without one the answer
        // comes from the path alone.
        assert_eq!(FileSample::new(&path, 4).classify(), FileClass::UnknownText);
        let large = RepoPath::from_bytes(b"src/generated.rs".to_vec());
        assert_eq!(
            FileSample::new(&large, OVERSIZED_FILE_THRESHOLD + 1).classify(),
            FileClass::Oversized,
            "size policy needs no content"
        );
    }

    #[test]
    fn a_generated_marker_is_read_only_inside_its_window() {
        let path = RepoPath::from_bytes(b"src/schema.rs".to_vec());
        let mut early = b"// @generated by a tool\n".to_vec();
        early.extend(std::iter::repeat_n(b'x', 4096));
        assert_eq!(
            FileSample::new(&path, u64::try_from(early.len()).unwrap())
                .with_window(&early)
                .classify(),
            FileClass::Generated
        );

        let mut late = vec![b'\n'; 2048];
        late.extend_from_slice(b"// @generated by a tool\n");
        assert_eq!(
            FileSample::new(&path, u64::try_from(late.len()).unwrap())
                .with_window(&late)
                .classify(),
            FileClass::Source,
            "a marker past the first kilobyte is not a marker"
        );
    }

    #[test]
    fn minified_line_lengths_only_classify_the_two_languages_that_ship_minified() {
        let bundle = vec![b'a'; 4096];
        let script = RepoPath::from_bytes(b"web/bundle.js".to_vec());
        assert_eq!(
            FileSample::new(&script, 4096)
                .with_window(&bundle)
                .classify(),
            FileClass::Generated
        );
        let prose = RepoPath::from_bytes(b"notes/long-line.txt".to_vec());
        assert_eq!(
            FileSample::new(&prose, 4096)
                .with_window(&bundle)
                .classify(),
            FileClass::Documentation
        );
    }

    #[test]
    fn a_utf16_file_is_text_rather_than_binary_despite_its_nul_bytes() {
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "hello".encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(sniff(&utf16, true), ContentSniff::Text);

        let mut big_endian = vec![0xFE, 0xFF];
        for unit in "hello".encode_utf16() {
            big_endian.extend_from_slice(&unit.to_be_bytes());
        }
        assert_eq!(sniff(&big_endian, true), ContentSniff::Text);

        let path = RepoPath::from_bytes(b"notes/utf16.txt".to_vec());
        assert_eq!(
            FileSample::new(&path, u64::try_from(utf16.len()).unwrap())
                .with_window(&utf16)
                .classify(),
            FileClass::Documentation
        );
    }

    #[test]
    fn the_window_boundary_never_invents_an_encoding_failure() {
        // "é" is two bytes; cutting it in half is a window artifact when the
        // file continues and a real defect when it does not.
        let truncated = b"caf\xc3";
        assert_eq!(sniff(truncated, false), ContentSniff::Text);
        assert_eq!(sniff(truncated, true), ContentSniff::Undecodable);

        let mut odd_utf16 = vec![0xFF, 0xFE, b'a', 0x00, b'b'];
        assert_eq!(sniff(&odd_utf16, false), ContentSniff::Text);
        // A whole file cannot end mid-unit, so the UTF-16 claim fails and the
        // bytes get the scan every other file gets — which finds the NUL.
        assert_eq!(sniff(&odd_utf16, true), ContentSniff::Binary);
        odd_utf16.pop();
        assert_eq!(sniff(&odd_utf16, true), ContentSniff::Text);
    }

    #[test]
    fn a_nul_byte_without_a_byte_order_mark_is_binary() {
        assert_eq!(sniff(b"abc\0def", true), ContentSniff::Binary);
        assert_eq!(sniff(b"", true), ContentSniff::Text);
        assert_eq!(sniff(b"\xef\xbb\xbfhello", true), ContentSniff::Text);
    }

    #[test]
    fn a_window_longer_than_the_sniff_bound_is_not_read_past_it() {
        let mut window = vec![b'a'; BINARY_SNIFF_BYTES];
        window.push(0);
        let path = RepoPath::from_bytes(b"src/main.rs".to_vec());
        assert_eq!(
            FileSample::new(&path, u64::try_from(window.len()).unwrap())
                .with_window(&window)
                .classify(),
            FileClass::Source,
            "the NUL byte sits past the bound and must not be seen"
        );
    }

    #[test]
    fn classification_is_total_and_deterministic() {
        // Deterministic pseudo-random paths, sizes and windows: the classifier
        // must answer every one of them, twice, with the same answer.
        let names = [
            "",
            "a",
            ".env.bak",
            "x.rs",
            "x.min.js",
            "Cargo.toml",
            "id",
            "T.MD",
            "..",
            "a.b.c",
            "secret",
            "тест.py",
            "y.dump",
        ];
        let directories = [
            "",
            "vendor/",
            "tests/",
            "docs/",
            "a/b/",
            ".harkness/",
            "out/",
        ];
        let windows: [&[u8]; 5] = [b"", b"text\n", b"\0\0\0", b"caf\xe9", b"@generated\n"];
        let sizes = [0_u64, 1, 4096, OVERSIZED_FILE_THRESHOLD, u64::MAX];

        let mut seen = 0_u32;
        for directory in directories {
            for name in names {
                for window in windows {
                    for size in sizes {
                        let path = RepoPath::from_bytes(format!("{directory}{name}").into_bytes());
                        let sample = FileSample::new(&path, size).with_window(window);
                        let first = sample.classify();
                        assert_eq!(first, sample.classify(), "unstable for {}", path.display());
                        assert!(FileClass::ALL.contains(&first));
                        seen += 1;
                    }
                }
            }
        }
        assert_eq!(seen, 2275);
    }
}
