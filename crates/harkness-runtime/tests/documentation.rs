//! The documentation, checked against the tree it describes.
//!
//! Three things go stale silently and are therefore checked here rather than
//! believed: the worked example a document claims to have copied, the repository
//! paths a document cites, and the links between documents.
//!
//! What is deliberately *not* here is the mapping from a claim to the test that
//! proves it. That needs `cargo test -- --list`, so it lives in
//! `.github/scripts/verify-doc-references.sh` beside `verify-suite-mapping.sh`,
//! which makes the same bargain for the verification suite.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Every Markdown file whose paths, links, and anchors are checked.
const DOCUMENTS: &[&str] = &[
    "README.md",
    "AGENTS.md",
    "CLAUDE.md",
    "CHANGELOG.md",
    "docs/release-readiness-v0.3.md",
    "docs/architecture-runtime.md",
    "docs/tool-authoring.md",
    "docs/policy.md",
    "docs/approvals.md",
    "docs/run-lifecycle-and-storage.md",
    "docs/mock-agent-scenarios.md",
    "docs/observability.md",
    "docs/verification-suite.md",
    "docs/agents.md",
    "docs/acp.md",
    "docs/architecture-context.md",
    "docs/context-inventory.md",
    "docs/context-identity.md",
    "docs/context-index.md",
    "docs/context-search.md",
    "docs/filesystem-and-process-safety.md",
];

/// Prefixes that make a backticked token a claim about this repository.
const REPOSITORY_PREFIXES: &[&str] = &["crates/", "docs/", ".github/", "scripts/"];

#[test]
fn the_tool_authoring_example_is_the_file_it_claims_to_be() {
    let root = repository_root();
    let document = read(&root, "docs/tool-authoring.md");
    let mirrored = mirrored_blocks(&document);
    assert_eq!(
        mirrored.len(),
        1,
        "docs/tool-authoring.md should mirror exactly one file: the worked example"
    );
    let (source, block) = &mirrored[0];
    assert_eq!(
        source, "crates/harkness-runtime/examples/word_count_tool.rs",
        "the tool-authoring example should be the compiled one"
    );
    let file = fs::read_to_string(root.join(source))
        .unwrap_or_else(|error| panic!("{source} could not be read: {error}"));
    if let Some(drift) = first_difference(block, &file) {
        panic!(
            "docs/tool-authoring.md and {source} have drifted apart at {drift}\n\
             The document mirrors the file, so copy the file back into the block it marks."
        );
    }
}

#[test]
fn a_mirrored_block_is_compared_by_content_rather_than_by_line_ending() {
    let block = "fn main() {\n    // one\n}\n";
    assert_eq!(
        first_difference(block, "fn main() {\r\n    // one\r\n}\r\n"),
        None
    );
    assert!(first_difference(block, "fn main() {\n    // two\n}\n").is_some());
    assert!(first_difference(block, "fn main() {\n}\n").is_some());
}

#[test]
fn every_repository_path_the_documentation_cites_exists() {
    let root = repository_root();
    let mut missing = Vec::new();
    for document in DOCUMENTS {
        let text = read(&root, document);
        for token in inline_code(&text) {
            if !REPOSITORY_PREFIXES
                .iter()
                .any(|prefix| token.starts_with(prefix))
            {
                continue;
            }
            // A pattern is a claim about a family of files rather than about one
            // file, and this check is about the ones a reader can open.
            if token.contains(['{', '}', '*', '<', '>', '…', '(', ')']) {
                continue;
            }
            let path = token.trim_end_matches(&['.', ',', ':', ';'][..]);
            if !root.join(path).exists() {
                missing.push(format!("{document} cites `{path}`, which does not exist"));
            }
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

#[test]
fn every_link_between_documents_resolves_to_a_file_and_a_heading() {
    let root = repository_root();
    let mut broken = Vec::new();
    for document in DOCUMENTS {
        let text = read(&root, document);
        let directory = Path::new(document)
            .parent()
            .unwrap_or(Path::new(""))
            .to_path_buf();
        for target in markdown_links(&text) {
            if target.starts_with("http://") || target.starts_with("https://") {
                continue;
            }
            let (file, fragment) = match target.split_once('#') {
                Some((file, fragment)) => (file, Some(fragment)),
                None => (target.as_str(), None),
            };
            let resolved = if file.is_empty() {
                PathBuf::from(document)
            } else {
                directory.join(file)
            };
            if !root.join(&resolved).exists() {
                broken.push(format!(
                    "{document} links to {}, which does not exist",
                    resolved.display()
                ));
                continue;
            }
            let Some(fragment) = fragment else { continue };
            // Only a Markdown target has headings to name.
            if resolved
                .extension()
                .is_none_or(|extension| extension != "md")
            {
                continue;
            }
            let anchors = anchors(&fs::read_to_string(root.join(&resolved)).unwrap_or_default());
            if !anchors.contains(fragment) {
                broken.push(format!(
                    "{document} links to #{fragment} in {}, which has no such heading",
                    resolved.display()
                ));
            }
        }
    }
    assert!(broken.is_empty(), "{}", broken.join("\n"));
}

// ---------------------------------------------------------------------------
// comparison
// ---------------------------------------------------------------------------

/// Whether a mirrored block still matches its file, and where it stops matching.
///
/// Compared line by line rather than as two whole strings, for two reasons.
///
/// A whole-string `assert_eq!` over a ten-kilobyte file prints it twice and
/// leaves the reader to find the character that moved; a drift is a real thing a
/// contributor has to fix, so the failure names the line.
///
/// And a line *ending* is not part of the claim. The block is rebuilt from
/// parsed lines, while the file arrives however the checkout wrote it — Git for
/// Windows converts to CRLF by default, which would otherwise fail this on that
/// platform and nowhere else. What is asserted is that the documented code is
/// the compiled code, not that a contributor's `core.autocrlf` is set one way.
fn first_difference(block: &str, file: &str) -> Option<String> {
    let mut documented = block.lines();
    let mut compiled = file.lines();
    let mut number = 0;
    loop {
        number += 1;
        return match (documented.next(), compiled.next()) {
            (None, None) => None,
            (Some(left), Some(right)) if left == right => continue,
            (Some(left), Some(right)) => Some(format!(
                "line {number}:\n  documented: {left:?}\n  compiled:   {right:?}"
            )),
            (Some(left), None) => Some(format!(
                "line {number}: the document has {left:?}, the file has ended"
            )),
            (None, Some(right)) => Some(format!(
                "line {number}: the file has {right:?}, the block has ended"
            )),
        };
    }
}

// ---------------------------------------------------------------------------
// extraction
// ---------------------------------------------------------------------------

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate sits two levels below the repository root")
        .to_path_buf()
}

fn read(root: &Path, document: &str) -> String {
    fs::read_to_string(root.join(document))
        .unwrap_or_else(|error| panic!("{document} could not be read: {error}"))
}

/// Every `<!-- mirrors: PATH -->` marker and the fenced block that follows it.
///
/// A marker rather than "the first code block" so that a document may hold as
/// many ordinary snippets as it needs, and so the block itself says which file
/// it is a copy of.
fn mirrored_blocks(document: &str) -> Vec<(String, String)> {
    let lines = document.lines().collect::<Vec<_>>();
    let mut mirrored = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let Some(source) = mirror_source(lines[index]) else {
            index += 1;
            continue;
        };
        let mut fence = index + 1;
        while fence < lines.len() && lines[fence].trim().is_empty() {
            fence += 1;
        }
        assert!(
            lines
                .get(fence)
                .is_some_and(|line| line.trim_start().starts_with("```")),
            "the mirror marker on line {} introduces no fenced block",
            index + 1
        );
        let mut body = Vec::new();
        let mut cursor = fence + 1;
        while cursor < lines.len() && lines[cursor].trim() != "```" {
            body.push(lines[cursor]);
            cursor += 1;
        }
        let mut text = body.join("\n");
        text.push('\n');
        mirrored.push((source, text));
        index = cursor + 1;
    }
    mirrored
}

/// The path a `<!-- mirrors: PATH -->` line names, if it is one.
fn mirror_source(line: &str) -> Option<String> {
    let body = line
        .trim()
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim();
    Some(body.strip_prefix("mirrors:")?.trim().to_owned())
}

/// Every backticked span that contains no whitespace, outside fenced blocks.
///
/// Fenced blocks are skipped because a shell example and a JSON payload are full
/// of backticks and slashes that name nothing in this tree.
fn inline_code(document: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut fenced = false;
    for line in document.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find('`') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('`') else { break };
            let token = &after[..end];
            if !token.is_empty() && !token.contains(char::is_whitespace) {
                tokens.push(token.to_owned());
            }
            rest = &after[end + 1..];
        }
    }
    tokens
}

/// Every `[text](target)` link target, outside fenced blocks.
fn markdown_links(document: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut fenced = false;
    for line in document.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        let bytes = line.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] != b']' || bytes.get(index + 1) != Some(&b'(') {
                index += 1;
                continue;
            }
            let after = &line[index + 2..];
            let Some(end) = after.find(')') else { break };
            let target = after[..end].trim();
            if !target.is_empty() {
                targets.push(target.to_owned());
            }
            index += 2 + end + 1;
        }
    }
    targets
}

/// The GitHub anchor of every ATX heading in a document.
fn anchors(document: &str) -> BTreeSet<String> {
    let mut anchors = BTreeSet::new();
    let mut fenced = false;
    for line in document.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || !line.starts_with('#') {
            continue;
        }
        let title = line.trim_start_matches('#').trim();
        if title.is_empty() {
            continue;
        }
        anchors.insert(slug(title));
    }
    anchors
}

/// GitHub's heading slug: lowercased, punctuation dropped, spaces hyphenated.
fn slug(title: &str) -> String {
    let mut slug = String::with_capacity(title.len());
    for character in title.chars() {
        if character.is_alphanumeric() || character == '_' || character == '-' {
            slug.extend(character.to_lowercase());
        } else if character == ' ' {
            slug.push('-');
        }
    }
    slug
}
