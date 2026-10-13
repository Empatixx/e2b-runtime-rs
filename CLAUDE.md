# envd-rs

envd-rs is a Rust drop-in replacement for the E2B `envd` daemon and orchestrator written in Go. It must be exchangeable with the Go service: the same API, the same behaviour for every service that talks to it, and internals shaped like upstream (the same components, logic flow, order of steps and names for the same concepts), so a Rust component can be validated side by side with its Go counterpart. It does not copy Go syntax, Go error wording or Go library quirks.

## API parity

Keep the API the same as upstream Go: routes, status codes, the headers callers read, JSON and proto shapes, Connect and gRPC framing, metric names, storage paths and formats. Behave close enough that the orchestrator, the SDK and the other services work unchanged. Do not copy Go library quirks the callers never see: net/http error pages, exact error wording, JSON escaping, chunking thresholds, Go formatting helpers. Read the Go source first and follow its structure: one Rust module for each Go package or component, the same steps in the same order. Write each step as idiomatic Rust with tokio, axum and connectrpc.

Before porting any difference, ask one question: would the orchestrator, the SDK, the client-proxy or a Go node in the same cluster notice it? If yes, match Go. If no, write the plain Rust version and move on. Values that cross to Go services or into storage (proto, Redis, ClickHouse, object paths, header formats, layer hashes) must stay compatible byte for byte; everything else only needs to behave the same.

## Code style

Write simple, plain code: free functions, `match`, `?` and iterator chains. Let good names explain the code and keep source files free of comments. Use structs with named fields for every piece of data.

Put readability first. Split work into small, well-named methods with low cognitive complexity, so each one reads top to bottom in a few lines. Keep it simple and direct, and reach for an abstraction when it makes the calling code read like the domain.

Hide low-level details such as `unsafe`, syscalls, ioctls and byte layouts behind small types with a clean, safe API. Return errors as `String`, `io::Error`, `ConnectError` or `ErrorResponse` with a clear message.

Write code as method chains. Transform `Option`, `Result` and iterators with combinators like `map`, `and_then`, `filter`, `map_or`, `is_none_or` and `try_for_each`, the way `keepalive_interval` reads a header in one chain. Build values with builder chains, for example `ErrorResponse::builder().status(StatusCode::NOT_FOUND).message(text).build()`, give a struct its own `builder()` when it is created in several places, and use library builders like `OpenOptions` and `Response::builder`. Builders are always generated with `bon`, never written by hand; prefer well-known crates over hand-rolled code.

## Platform

Target Linux only, like upstream. Use Linux APIs (`clone3`, pidfd, userfaultfd, NBD, netlink, cgroup v2) directly. Do not write macOS or other fallbacks, and do not add `cfg(not(target_os = "linux"))` stubs to new code.

## Project layout

Follow the crates.io style: `foo.rs` lists its submodules and they live in the `foo/` folder. The `project-structure` skill says where each kind of code belongs. Update that skill when the layout grows.

## Testing

Put unit tests at the bottom of the same file in `#[cfg(test)] mod tests`. Put HTTP behaviour tests in `crates/<crate>/tests/*_contract.rs`, talking to a real running server. Run Linux-only checks with `scripts/test-linux.sh`, which uses Docker.

## Backlog

`docs/BACKLOG.md` lists only gaps to Go parity that a caller would notice, each with the milestone where it starts to matter. Add every deferred gap of that kind there when a review ends, and delete the line when it is fixed. Anything that already behaves like Go, and any difference no caller sees (error wording, net/http quirks, internal metrics counts), does not belong there. Deliberate differences from Go and features upstream does not have go to `docs/new-features.md`, with their open items.

## Writing docs and posts

Update the documentation in the same change as the code: `docs/BACKLOG.md`, `docs/new-features.md`, the website pages under `website/content/docs/` in English and Czech, and the README when the change is visible there. A change is not done while a page still describes the old behaviour.

README, docs and the website use plain, short English (and a Czech translation on the website). Follow the `writing-blogs` skill. `scripts/check-prose.py` rejects AI-sounding words, arrows, dashes and emoji; the git pre-push hook (`git config core.hooksPath scripts/hooks`) and the Claude Code hook in `.claude/settings.json` run it before every push.

## Before a commit

Run `cargo fmt`, `cargo clippy --all-targets` and `cargo test`, and commit when all three are clean. Commit locally and leave pushing to the user.

## Working with the user

Talk in Czech, keep answers short and explain Rust ideas with Java analogies. The user edits files in RustRover at the same time, so read a file right before editing it. Take Rust skills from `~/IdeaProjects/vm-data-plane/.claude/skills`.
