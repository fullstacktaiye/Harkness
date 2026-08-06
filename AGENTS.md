# Repository Guidelines

## Project Structure & Module Organization

Harkness is a Rust 2024 workspace split into three crates under `crates/`:

- `harkness-core`: project catalog, Git operations, and directory-listing logic shared by front ends.
- `harkness-cli`: the `harkness` command and its integration tests in `tests/`.
- `harkness-gui`: the Qt 6/KDE Kirigami application. Rust/CXX-Qt bindings live in `src/` and `cxx/`; UI components live in `qml/`.

Desktop integration assets are in `data/`. The root `CMakeLists.txt` provides release build and local installation support; Cargo remains the primary development interface.

## Build, Test, and Development Commands

- `cargo run -p harkness-cli` runs the command-line binary.
- `cargo run -p harkness-gui` launches the Kirigami application; Qt 6, KDE Frameworks 6, and `qmake` must be available.
- `cargo test --workspace` runs all unit and integration tests.
- `cargo fmt --check` verifies Rust formatting without changing files.
- `cargo clippy --workspace --all-targets` checks all targets for common Rust issues.
- `cmake -S . -B build -DCMAKE_BUILD_TYPE=Release` followed by `cmake --build build` creates the locked release workspace used for installation.

## Coding Style & Naming Conventions

Use standard `rustfmt` output (four-space indentation) and keep Clippy clean. Follow Rust conventions: `snake_case` for functions and modules, `PascalCase` for types, and `SCREAMING_SNAKE_CASE` for constants. Prefer explicit error propagation and typed domain errors over panics in production paths. Name QML components in `PascalCase.qml`, assign lower camel-case IDs/properties, and keep UI state transitions documented when they are not obvious.

## Testing Guidelines

Place focused unit tests in a `#[cfg(test)] mod tests` beside the implementation. Put executable-level behavior in crate-level `tests/*.rs`; use descriptive names such as `prints_exact_greeting`. Add regression coverage for catalog locking, Git process handling, filesystem safety, and navigation changes. Run the full test, format, and Clippy commands before submitting.

## Commit & Pull Request Guidelines

Write short, imperative commit subjects, matching history such as `Prevent concurrent imports from orphaning managed checkouts`. Keep each commit focused; append the PR number only when added by the merge workflow. Pull requests should explain the behavior change, testing performed, and relevant issue. Include screenshots for visible QML changes and call out platform or Qt/KDE dependency assumptions.
