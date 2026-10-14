# e2b-runtime-simple

A small E2B sandbox runtime. Keep the API that the E2B SDK uses and the technical requirements (Firecracker, OverlayBD layers over ublk, memory snapshots). Everything else stays as simple as possible.

## Code style

Write readable code. Split work into small methods with good names, so each one reads top to bottom in a few lines. Use free functions, `match`, `?` and iterator chains. Keep source files free of comments. Hide syscalls and byte layouts behind small types.

## Layout

One module per concept, crates.io style: `foo.rs` lists its submodules, which live in `foo/`. The README lists the modules.

## Testing

Unit tests go at the bottom of the same file. The node needs Linux with KVM and ublk, so build and test it on a Linux node. `examples/sdk-smoke` runs the official SDK against a running node.

## Before a commit

Run `cargo fmt`, `cargo clippy --all-targets` and `cargo test`.

## Working with the user

Talk in Czech, keep answers short, explain Rust ideas with Java analogies.
