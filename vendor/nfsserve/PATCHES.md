# Local changes to nfsserve 0.10.2

The crate is published by xetdata under the BSD-3-Clause license (see LICENSE). envd-rs uses it through `[patch.crates-io]` with these changes:

- `context` is public and `RPCContext::new` builds a context, so the orchestrator can pick the file system for each connection.
- `tcp::process_socket` is public, so the orchestrator runs its own accept loop.
- `lib.rs` allows warnings and clippy lints, because a path dependency does not get the lint cap that registry crates get.
- The `[[example]]` sections are removed, because the published crate does not ship the examples.
- The portmapper answers the proxy port only for NFS and mountd version 3 over TCP and 0 for anything else, like the upstream orchestrator's portmapper.
