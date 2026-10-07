# envd-rs

envd-rs is a Rust drop-in replacement for the E2B `envd` daemon written in Go. A client must be able to swap the Go binary for this one and see no difference.

## API parity

Match the upstream Go envd exactly at the edges: routes, status codes, headers, JSON shapes and error texts. Read the Go source first, then write the same behaviour in idiomatic Rust. Keep the inside of the code shaped the Rust way, using tokio, axum and connectrpc conventions.

## Code style

Write simple, plain code: free functions, `match`, `?` and iterator chains. Let good names explain the code and keep source files free of comments. Use structs with named fields for every piece of data.

Put readability first. Split work into small, well-named methods with low cognitive complexity, so each one reads top to bottom in a few lines. Keep it simple and direct, and reach for an abstraction when it makes the calling code read like the domain.

Hide low-level details such as `unsafe`, syscalls, ioctls and byte layouts behind small types with a clean, safe API. Return errors as `String`, `io::Error`, `ConnectError` or `ErrorResponse` holding the exact text the client sees.

Write code as method chains. Transform `Option`, `Result` and iterators with combinators like `map`, `and_then`, `filter`, `map_or`, `is_none_or` and `try_for_each`, the way `keepalive_interval` reads a header in one chain. Build values with builder chains, for example `ErrorResponse::builder().status(StatusCode::NOT_FOUND).message(text).build()`, give a struct its own `builder()` when it is created in several places, and use library builders like `OpenOptions` and `Response::builder`.

## Project layout

Follow the crates.io style: `foo.rs` lists its submodules and they live in the `foo/` folder. The `project-structure` skill says where each kind of code belongs. Update that skill when the layout grows.

## Testing

Put unit tests at the bottom of the same file in `#[cfg(test)] mod tests`. Put HTTP behaviour tests in `crates/<crate>/tests/*_contract.rs`, talking to a real running server. Run Linux-only checks with `scripts/test-linux.sh`, which uses Docker.

## Before a commit

Run `cargo fmt`, `cargo clippy --all-targets` and `cargo test`, and commit when all three are clean. Commit locally and leave pushing to the user.

## Working with the user

Talk in Czech, keep answers short and explain Rust ideas with Java analogies. The user edits files in RustRover at the same time, so read a file right before editing it. Take Rust skills from `~/IdeaProjects/vm-data-plane/.claude/skills`.
