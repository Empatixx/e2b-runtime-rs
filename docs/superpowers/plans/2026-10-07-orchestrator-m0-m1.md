# Orchestrator M0 + M1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the `orchestrator` crate to a Cargo workspace, set up GitHub Actions CI (with a KVM job that proves the E2B Firecracker fork boots and checks whether `ublk_drv` and `nbd` are available on the runner), then implement the storage layer: upstream header V3 format, mapping merge for diffs, build paths and an object-storage backend (local FS + S3/MinIO).

**Architecture:**
- The envd crate stays at the repository root as the root package, and the workspace adds `crates/orchestrator`.
- The storage code lives in `crates/orchestrator/src/storage/`, split into three parts:
  - `header/`: pure, synchronous byte format and mapping maths, unit-tested;
  - `paths.rs`: object names;
  - `backend.rs`: async object storage over the `object_store` crate.

**Tech Stack:** Rust 1.98 (edition 2024), tokio, uuid, roaring, object_store (aws), bytes, futures, GitHub Actions (ubuntu-24.04 with `/dev/kvm`), MinIO.

**Spec:** `docs/superpowers/specs/2026-10-07-orchestrator-rs-design.md`

## Global Constraints

- Code style comes from `CLAUDE.md`:
  - readability first: small well-named methods with low cognitive complexity, method chains, builders, named fields;
  - no comments of any kind, and no tuple structs or `.0` access;
  - crates.io module layout (`foo.rs` + `foo/`, no `mod.rs`);
  - low-level details go behind small safe types.
- Header V3 byte layout must match upstream exactly:
  - The 64-byte metadata block is little-endian `u64 version, u64 block_size, u64 size, u64 generation, [16] build_id, [16] base_build_id`.
  - It is followed by 40-byte entries: little-endian `u64 offset, u64 length, [16] build_id, u64 storage_offset`.
- Accepted header versions are `version & 0xFFFF` in `1..=3`. New template headers use version `3`. Page size is `4096`.
- Build object names are `{build_id}/memfile`, `{build_id}/memfile.header`, `{build_id}/rootfs.ext4`, `{build_id}/rootfs.ext4.header`, `{build_id}/snapfile` and `{build_id}/metadata.json`.
- E2B Firecracker fork: release `v1.14.1_621718e` of `github.com/e2b-dev/fc-versions`, asset `firecracker-amd64`.
- Guest kernel: release `2026.07.28` of `github.com/e2b-dev/fc-kernels`, asset `vmlinux-6.1.158-amd64.bin`.
- Errors are `String` values holding a readable message.
- Before every commit run `cargo fmt`, `cargo clippy --workspace --all-targets` and `cargo test --workspace`.
- Commit locally. Pushing the `orchestrator` branch to GitHub (needed for CI) happens only with the user's approval.

## Review Focus

1. **A header whose mapping block ends mid-entry** (truncated upload) must fail to decode with `failed to read block mapping: unexpected EOF`, not silently drop the tail. Covered in Task 3.
2. **Locating an offset at the last byte or exactly at `size`**: the last byte resolves to the last mapping, and `size` is out of bounds. Covered in Task 4.
3. **A diff touching the first or last block, or the whole device**, must produce a contiguous mapping from 0 to `size`. Covered in Task 4.
4. **An empty diff** (nothing dirty, nothing empty) gives the next generation with mappings equal to the parent's. Covered in Task 4.
5. **A `get_range` past the end of an object** must return an error, not a short buffer the caller mistakes for data. Covered in Task 6.

---

### Task 1: Workspace and orchestrator crate skeleton

**Files:**
- Modify: `Cargo.toml` (root)
- Create: `crates/orchestrator/Cargo.toml`, `crates/orchestrator/src/main.rs`, `crates/orchestrator/src/lib.rs`
- Modify: `.claude/skills/project-structure/SKILL.md` (add the workspace and orchestrator crate to the tree)

**Interfaces:**
- Produces: library crate `orchestrator` (`crates/orchestrator/src/lib.rs`) and binary `orchestrator`.

- [ ] **Step 1: Write the failing test**

`crates/orchestrator/src/lib.rs`:
```rust
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_the_crate_version() {
        assert_eq!(super::version(), "0.1.0");
    }
}
```

`crates/orchestrator/Cargo.toml`:
```toml
[package]
name = "orchestrator"
version = "0.1.0"
edition = "2024"

[dependencies]
```

- [ ] **Step 2: Run the test to see it fail**

Run: `cargo test -p orchestrator`
Expected: FAIL, ``package ID specification `orchestrator` did not match any packages``. The crate is not in a workspace yet.

- [ ] **Step 3: Add the workspace and the binary**

Append to the root `Cargo.toml`, above `[profile.release]`:
```toml
[workspace]
members = [".", "crates/orchestrator"]
default-members = [".", "crates/orchestrator"]
```

`crates/orchestrator/src/main.rs`:
```rust
fn main() {
    println!("orchestrator {}", orchestrator::version());
}
```

In `.claude/skills/project-structure/SKILL.md`, add these lines under the tree's first line `src/`:
```
Cargo.toml             root package (envd) + [workspace] members [".", "crates/orchestrator"]
crates/orchestrator/   orchestrator crate (same style and layout rules as src/)
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test --workspace && cargo run -p orchestrator`
Expected: all envd tests pass; `version_is_the_crate_version` passes; the binary prints `orchestrator 0.1.0`.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets
git add Cargo.toml Cargo.lock crates/orchestrator .claude/skills/project-structure/SKILL.md
git commit -m "feat: cargo workspace with orchestrator crate skeleton"
```

---

### Task 2: GitHub Actions CI and the KVM spike

**Files:**
- Create: `.github/workflows/ci.yml`, `scripts/ci/kvm-spike.sh`, `scripts/ci/minio.sh`

**Interfaces:**
- Produces:
  - A `test` CI job that runs fmt, clippy and workspace tests as root, with MinIO reachable at `http://127.0.0.1:9000`. The bucket is `e2b-test` and the credentials are `minio` / `minio123`.
  - The env var `E2B_TEST_S3_ENDPOINT=http://127.0.0.1:9000`, which Task 6 reads.
  - A `kvm-spike` job.

- [ ] **Step 1: Write the spike script**

`scripts/ci/kvm-spike.sh`:
```bash
#!/usr/bin/env bash
set -euo pipefail

FC_RELEASE=v1.14.1_621718e
KERNEL_RELEASE=2026.07.28
KERNEL_ASSET=vmlinux-6.1.158-amd64.bin
WORK=$(mktemp -d)
SOCKET="$WORK/fc.sock"

echo "host kernel: $(uname -r)"
curl -fsSL -o "$WORK/firecracker" "https://github.com/e2b-dev/fc-versions/releases/download/$FC_RELEASE/firecracker-amd64"
curl -fsSL -o "$WORK/vmlinux" "https://github.com/e2b-dev/fc-kernels/releases/download/$KERNEL_RELEASE/$KERNEL_ASSET"
chmod +x "$WORK/firecracker"
"$WORK/firecracker" --version

"$WORK/firecracker" --api-sock "$SOCKET" > "$WORK/console.log" 2>&1 &
FC_PID=$!
until [ -S "$SOCKET" ]; do sleep 0.05; done

api() {
  curl -fsS --unix-socket "$SOCKET" -X PUT "http://localhost/$1" -H 'content-type: application/json' -d "$2"
}
api machine-config '{"vcpu_count":1,"mem_size_mib":128}'
api boot-source "{\"kernel_image_path\":\"$WORK/vmlinux\",\"boot_args\":\"console=ttyS0 reboot=k panic=1 pci=off\"}"
api actions '{"action_type":"InstanceStart"}'

for _ in $(seq 1 100); do
  grep -q "Linux version" "$WORK/console.log" && break
  sleep 0.1
done
grep -m1 "Linux version" "$WORK/console.log"
kill "$FC_PID" 2>/dev/null || true

if sudo modprobe ublk_drv && [ -e /dev/ublk-control ]; then echo "ublk: available"; else echo "ublk: NOT available"; fi
if sudo modprobe nbd nbds_max=4 && [ -e /dev/nbd0 ]; then echo "nbd: available"; else echo "nbd: NOT available"; fi
```

`scripts/ci/minio.sh`:
```bash
#!/usr/bin/env bash
set -euo pipefail

docker run -d --name minio -p 9000:9000 \
  -e MINIO_ROOT_USER=minio -e MINIO_ROOT_PASSWORD=minio123 \
  minio/minio server /data
until curl -sf http://127.0.0.1:9000/minio/health/live; do sleep 0.5; done
docker run --rm --network host --entrypoint sh minio/mc -c \
  "mc alias set local http://127.0.0.1:9000 minio minio123 && mc mb --ignore-existing local/e2b-test"
```

- [ ] **Step 2: Write the workflow**

`.github/workflows/ci.yml`:
```yaml
name: ci
on:
  push:
  pull_request:

jobs:
  test:
    runs-on: ubuntu-24.04
    env:
      E2B_TEST_S3_ENDPOINT: http://127.0.0.1:9000
    steps:
      - uses: actions/checkout@v4
      - run: sudo apt-get update -qq && sudo apt-get install -y -qq protobuf-compiler
      - uses: dtolnay/rust-toolchain@master
        with:
          toolchain: "1.98"
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: scripts/ci/minio.sh
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: sudo -E env "PATH=$PATH" cargo test --workspace

  kvm-spike:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - run: |
          echo 'KERNEL=="kvm", GROUP="kvm", MODE="0666", OPTIONS+="static_node=kvm"' | sudo tee /etc/udev/rules.d/99-kvm4all.rules
          sudo udevadm control --reload-rules
          sudo udevadm trigger --name-match=kvm
      - run: scripts/ci/kvm-spike.sh
```

- [ ] **Step 3: Run the spike locally as far as possible**

Run: `bash -n scripts/ci/kvm-spike.sh && bash -n scripts/ci/minio.sh && chmod +x scripts/ci/*.sh`
Expected: no syntax errors. The scripts need Linux with KVM, so they really run in CI.

- [ ] **Step 4: Commit, push with the user's approval, and read the CI result**

```bash
git add .github/workflows/ci.yml scripts/ci
git commit -m "ci: test job with MinIO and KVM spike for the E2B Firecracker fork"
git push -u origin orchestrator
gh run watch --repo Empatixx/e2b-runtime-rs --exit-status
```
Expected:
- `test` passes.
- The `kvm-spike` log shows `Firecracker v1.14…`, a `Linux version 6.1.158` line, and one line each for ublk and nbd (`available` / `NOT available`).

Record the ublk and nbd results in the ledger. If ublk is not available, later CI tests for block exposure use the NBD fallback (spec, Risks).

---

### Task 3: Header metadata and V3 encoding

**Files:**
- Create: `crates/orchestrator/src/storage.rs`, `crates/orchestrator/src/storage/header.rs`, `crates/orchestrator/src/storage/header/metadata.rs`, `crates/orchestrator/src/storage/header/mapping.rs`
- Modify: `crates/orchestrator/src/lib.rs` (`pub mod storage;`), `crates/orchestrator/Cargo.toml` (`uuid = { version = "1", features = ["v4"] }`)

**Interfaces:**
- Produces:
  - `storage::header::Metadata { version: u64, block_size: u64, size: u64, generation: u64, build_id: Uuid, base_build_id: Uuid }`, with:
    - `Metadata::template(build_id: Uuid, block_size: u64, size: u64) -> Metadata`
    - `metadata.next_generation(build_id: Uuid) -> Metadata`
  - `storage::header::BuildMapping { offset: u64, length: u64, build_id: Uuid, storage_offset: u64 }`, with `end(&self) -> u64`.
  - `storage::header::Header { metadata: Metadata, mappings: Vec<BuildMapping> }`, with:
    - `Header::new(metadata, mappings) -> Result<Header, String>`
    - `header.encode() -> Vec<u8>`
    - `Header::decode(bytes: &[u8]) -> Result<Header, String>`
  - Constants `PAGE_SIZE: u64 = 4096`, `METADATA_SIZE: usize = 64`, `MAPPING_SIZE: usize = 40`.

- [ ] **Step 1: Write the failing tests**

At the bottom of `crates/orchestrator/src/storage/header.rs`:
```rust
#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn id(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    fn le(values: &[u64]) -> Vec<u8> {
        values.iter().flat_map(|value| value.to_le_bytes()).collect()
    }

    #[test]
    fn template_header_covers_the_whole_device_with_one_mapping() {
        let header = Header::new(Metadata::template(id(1), 4096, 10_000), Vec::new()).unwrap();
        assert_eq!(
            header.mappings,
            vec![BuildMapping {
                offset: 0,
                length: 12_288,
                build_id: id(1),
                storage_offset: 0,
            }]
        );
        assert_eq!(header.metadata.generation, 0);
        assert_eq!(header.metadata.base_build_id, id(1));
    }

    #[test]
    fn encodes_exactly_like_upstream_v3() {
        let metadata = Metadata::template(id(1), 4096, 8192).next_generation(id(2));
        let mappings = vec![
            BuildMapping {
                offset: 0,
                length: 4096,
                build_id: id(1),
                storage_offset: 0,
            },
            BuildMapping {
                offset: 4096,
                length: 4096,
                build_id: id(2),
                storage_offset: 0,
            },
        ];
        let expected: Vec<u8> = [
            le(&[3, 4096, 8192, 1]),
            vec![2; 16],
            vec![1; 16],
            le(&[0, 4096]),
            vec![1; 16],
            le(&[0, 4096, 4096]),
            vec![2; 16],
            le(&[0]),
        ]
        .concat();

        let header = Header::new(metadata, mappings).unwrap();
        assert_eq!(header.encode(), expected);
        assert_eq!(Header::decode(&expected).unwrap(), header);
    }

    #[test]
    fn decode_rejects_bad_input() {
        let valid = Header::new(Metadata::template(id(1), 4096, 4096), Vec::new())
            .unwrap()
            .encode();

        assert_eq!(Header::decode(&valid[..10]).unwrap_err(), "header too short: 10 bytes");
        assert_eq!(
            Header::decode(&valid[..valid.len() - 3]).unwrap_err(),
            "failed to read block mapping: unexpected EOF"
        );

        let mut future = valid.clone();
        future[0] = 9;
        assert_eq!(Header::decode(&future).unwrap_err(), "unsupported header version 9");

        let zero_block = Header::new(Metadata::template(id(1), 0, 4096), Vec::new());
        assert_eq!(zero_block.unwrap_err(), "block size cannot be zero");
    }
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p orchestrator storage::header`
Expected: FAIL to compile, `cannot find type Header`.

- [ ] **Step 3: Implement metadata, mapping entry and header**

`crates/orchestrator/src/storage.rs`:
```rust
pub mod header;
```

`crates/orchestrator/src/storage/header/metadata.rs`:
```rust
use uuid::Uuid;

pub const METADATA_SIZE: usize = 64;
pub const TEMPLATE_VERSION: u64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub version: u64,
    pub block_size: u64,
    pub size: u64,
    pub generation: u64,
    pub build_id: Uuid,
    pub base_build_id: Uuid,
}

impl Metadata {
    pub fn template(build_id: Uuid, block_size: u64, size: u64) -> Self {
        Self {
            version: TEMPLATE_VERSION,
            block_size,
            size,
            generation: 0,
            build_id,
            base_build_id: build_id,
        }
    }

    pub fn next_generation(&self, build_id: Uuid) -> Self {
        Self {
            generation: self.generation + 1,
            build_id,
            ..*self
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        [self.version, self.block_size, self.size, self.generation]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .chain(*self.build_id.as_bytes())
            .chain(*self.base_build_id.as_bytes())
            .collect()
    }

    pub fn decode(bytes: &[u8; METADATA_SIZE]) -> Self {
        Self {
            version: u64_at(bytes, 0),
            block_size: u64_at(bytes, 8),
            size: u64_at(bytes, 16),
            generation: u64_at(bytes, 24),
            build_id: uuid_at(bytes, 32),
            base_build_id: uuid_at(bytes, 48),
        }
    }

    pub fn format_version(&self) -> u64 {
        self.version & 0xFFFF
    }
}

pub fn u64_at(bytes: &[u8], start: usize) -> u64 {
    u64::from_le_bytes(bytes[start..start + 8].try_into().unwrap_or_default())
}

pub fn uuid_at(bytes: &[u8], start: usize) -> Uuid {
    Uuid::from_bytes(bytes[start..start + 16].try_into().unwrap_or_default())
}
```

`crates/orchestrator/src/storage/header/mapping.rs`:
```rust
use uuid::Uuid;

use super::metadata::{u64_at, uuid_at};

pub const MAPPING_SIZE: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildMapping {
    pub offset: u64,
    pub length: u64,
    pub build_id: Uuid,
    pub storage_offset: u64,
}

impl BuildMapping {
    pub fn end(&self) -> u64 {
        self.offset + self.length
    }

    pub fn encode(&self) -> Vec<u8> {
        [self.offset, self.length]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .chain(*self.build_id.as_bytes())
            .chain(self.storage_offset.to_le_bytes())
            .collect()
    }

    pub fn decode(bytes: &[u8]) -> Self {
        Self {
            offset: u64_at(bytes, 0),
            length: u64_at(bytes, 8),
            build_id: uuid_at(bytes, 16),
            storage_offset: u64_at(bytes, 32),
        }
    }
}
```

`crates/orchestrator/src/storage/header.rs`:
```rust
mod mapping;
mod metadata;

pub use mapping::{BuildMapping, MAPPING_SIZE};
pub use metadata::{METADATA_SIZE, Metadata};

pub const PAGE_SIZE: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub metadata: Metadata,
    pub mappings: Vec<BuildMapping>,
}

impl Header {
    pub fn new(metadata: Metadata, mappings: Vec<BuildMapping>) -> Result<Self, String> {
        if metadata.block_size == 0 {
            return Err("block size cannot be zero".to_owned());
        }
        let mappings = if mappings.is_empty() {
            vec![whole_device(&metadata)]
        } else {
            mappings
        };
        Ok(Self { metadata, mappings })
    }

    pub fn encode(&self) -> Vec<u8> {
        self.metadata
            .encode()
            .into_iter()
            .chain(self.mappings.iter().flat_map(BuildMapping::encode))
            .collect()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let (metadata, entries) = bytes
            .split_first_chunk::<METADATA_SIZE>()
            .ok_or_else(|| format!("header too short: {} bytes", bytes.len()))?;
        let metadata = Metadata::decode(metadata);
        if !(1..=3).contains(&metadata.format_version()) {
            return Err(format!("unsupported header version {}", metadata.version));
        }
        if entries.len() % MAPPING_SIZE != 0 {
            return Err("failed to read block mapping: unexpected EOF".to_owned());
        }
        let mappings = entries.chunks_exact(MAPPING_SIZE).map(BuildMapping::decode).collect();
        Self::new(metadata, mappings)
    }
}

fn whole_device(metadata: &Metadata) -> BuildMapping {
    BuildMapping {
        offset: 0,
        length: metadata.size.div_ceil(PAGE_SIZE) * PAGE_SIZE,
        build_id: metadata.build_id,
        storage_offset: 0,
    }
}
```

Add to `crates/orchestrator/src/lib.rs` (above `pub fn version`): `pub mod storage;`. Run `cargo add -p orchestrator uuid --features v4`.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p orchestrator storage::header`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets
git add crates/orchestrator Cargo.lock
git commit -m "feat(orchestrator): header metadata and upstream V3 encoding"
```

---

### Task 4: Mapping maths and diff headers

**Files:**
- Create: `crates/orchestrator/src/storage/header/diff.rs`
- Modify: `crates/orchestrator/src/storage/header/mapping.rs` (adds `slice`, `continues_with`, `merge`, `normalize`, `from_blocks`, `validate`), `crates/orchestrator/src/storage/header.rs` (adds `locate`, `Location`, re-exports), `crates/orchestrator/Cargo.toml` (`roaring = "0.11"`)

**Interfaces:**
- Consumes: `Header`, `Metadata`, `BuildMapping` (Task 3).
- Produces:
  - `storage::header::Location { build_id: Uuid, storage_offset: u64, length: u64 }`
  - `header.locate(offset: u64) -> Result<Location, String>`
  - `storage::header::DiffMetadata { dirty: RoaringBitmap, empty: RoaringBitmap, block_size: u64 }`
  - `diff.to_diff_header(parent: &Header, build_id: Uuid) -> Result<Header, String>`
  - Free functions in `mapping`: `merge(base, diff) -> Vec<BuildMapping>`, `normalize(&[BuildMapping]) -> Vec<BuildMapping>`, `from_blocks(build_id, &RoaringBitmap, block_size) -> Vec<BuildMapping>`, `validate(&[BuildMapping], size) -> Result<(), String>`

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/orchestrator/src/storage/header.rs`:
```rust
    use roaring::RoaringBitmap;

    fn mapping(offset: u64, length: u64, build: u8, storage_offset: u64) -> BuildMapping {
        BuildMapping {
            offset,
            length,
            build_id: id(build),
            storage_offset,
        }
    }

    fn template(blocks: u64) -> Header {
        Header::new(Metadata::template(id(1), 4096, blocks * 4096), Vec::new()).unwrap()
    }

    fn blocks(indices: &[u32]) -> RoaringBitmap {
        indices.iter().copied().collect()
    }

    fn diff(dirty: &[u32], empty: &[u32]) -> DiffMetadata {
        DiffMetadata {
            dirty: blocks(dirty),
            empty: blocks(empty),
            block_size: 4096,
        }
    }

    #[test]
    fn diff_in_the_middle_splits_the_parent() {
        let header = diff(&[2, 3], &[]).to_diff_header(&template(8), id(2)).unwrap();
        assert_eq!(header.metadata.generation, 1);
        assert_eq!(header.metadata.build_id, id(2));
        assert_eq!(header.metadata.base_build_id, id(1));
        assert_eq!(
            header.mappings,
            vec![
                mapping(0, 8192, 1, 0),
                mapping(8192, 8192, 2, 0),
                mapping(16384, 16384, 1, 16384),
            ]
        );
    }

    #[test]
    fn diff_at_both_edges_and_empty_blocks() {
        let header = diff(&[0, 7], &[3]).to_diff_header(&template(8), id(2)).unwrap();
        assert_eq!(
            header.mappings,
            vec![
                mapping(0, 4096, 2, 0),
                mapping(4096, 8192, 1, 4096),
                mapping(12288, 4096, 0, 0),
                mapping(16384, 12288, 1, 16384),
                mapping(28672, 4096, 2, 4096),
            ]
        );
    }

    #[test]
    fn diff_covering_everything_replaces_the_parent() {
        let all: Vec<u32> = (0..8).collect();
        let header = diff(&all, &[]).to_diff_header(&template(8), id(2)).unwrap();
        assert_eq!(header.mappings, vec![mapping(0, 32768, 2, 0)]);
    }

    #[test]
    fn empty_diff_keeps_parent_mappings() {
        let parent = template(8);
        let header = diff(&[], &[]).to_diff_header(&parent, id(2)).unwrap();
        assert_eq!(header.mappings, parent.mappings);
        assert_eq!(header.metadata.generation, 1);
    }

    #[test]
    fn second_generation_resolves_through_one_lookup() {
        let first = diff(&[2, 3], &[]).to_diff_header(&template(8), id(2)).unwrap();
        let second = diff(&[3, 4], &[]).to_diff_header(&first, id(3)).unwrap();
        assert_eq!(
            second.mappings,
            vec![
                mapping(0, 8192, 1, 0),
                mapping(8192, 4096, 2, 0),
                mapping(12288, 8192, 3, 0),
                mapping(20480, 12288, 1, 20480),
            ]
        );
        assert_eq!(
            second.locate(9000).unwrap(),
            Location {
                build_id: id(2),
                storage_offset: 808,
                length: 3288,
            }
        );
    }

    #[test]
    fn locate_handles_the_edges() {
        let header = diff(&[2, 3], &[]).to_diff_header(&template(8), id(2)).unwrap();
        assert_eq!(
            header.locate(32767).unwrap(),
            Location {
                build_id: id(1),
                storage_offset: 32767,
                length: 1,
            }
        );
        assert_eq!(
            header.locate(32768).unwrap_err(),
            "offset 32768 is out of bounds (size 32768)"
        );
    }

    #[test]
    fn validate_reports_gaps_and_size_mismatch() {
        assert_eq!(
            mapping::validate(&[mapping(0, 4096, 1, 0), mapping(8192, 4096, 1, 0)], 12288).unwrap_err(),
            "mapping validation failed: expected offset 4096, got 8192"
        );
        assert_eq!(
            mapping::validate(&[mapping(0, 4096, 1, 0)], 8192).unwrap_err(),
            "mapping validation failed: total 4096 != size 8192"
        );
    }
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p orchestrator storage::header`
Expected: FAIL to compile, `cannot find type DiffMetadata`.

- [ ] **Step 3: Implement the mapping functions**

Append to `crates/orchestrator/src/storage/header/mapping.rs`:
```rust
use std::ops::Range;

use roaring::RoaringBitmap;

impl BuildMapping {
    pub fn slice(&self, start: u64, end: u64) -> Self {
        Self {
            offset: start,
            length: end - start,
            build_id: self.build_id,
            storage_offset: self.storage_offset + (start - self.offset),
        }
    }

    fn continues_with(&self, next: &BuildMapping) -> bool {
        self.build_id == next.build_id
            && (next.build_id.is_nil() || next.storage_offset == self.storage_offset + self.length)
    }
}

pub fn merge(base: &[BuildMapping], diff: &[BuildMapping]) -> Vec<BuildMapping> {
    let mut merged: Vec<BuildMapping> = base
        .iter()
        .flat_map(|mapping| uncovered_parts(mapping, diff))
        .chain(diff.iter().copied())
        .filter(|mapping| mapping.length > 0)
        .collect();
    merged.sort_by_key(|mapping| mapping.offset);
    merged
}

fn uncovered_parts(mapping: &BuildMapping, diff: &[BuildMapping]) -> Vec<BuildMapping> {
    let first = diff.partition_point(|cover| cover.end() <= mapping.offset);
    let covers = diff[first..].iter().take_while(|cover| cover.offset < mapping.end());

    let mut parts = Vec::new();
    let mut cursor = mapping.offset;
    for cover in covers {
        if cover.offset > cursor {
            parts.push(mapping.slice(cursor, cover.offset));
        }
        cursor = cursor.max(cover.end());
    }
    if cursor < mapping.end() {
        parts.push(mapping.slice(cursor, mapping.end()));
    }
    parts
}

pub fn normalize(mappings: &[BuildMapping]) -> Vec<BuildMapping> {
    mappings.iter().fold(Vec::new(), |mut result: Vec<BuildMapping>, mapping| {
        match result.last_mut() {
            Some(last) if last.continues_with(mapping) => last.length += mapping.length,
            _ => result.push(*mapping),
        }
        result
    })
}

pub fn from_blocks(build_id: Uuid, blocks: &RoaringBitmap, block_size: u64) -> Vec<BuildMapping> {
    let mut storage_offset = 0;
    block_ranges(blocks)
        .into_iter()
        .map(|range| {
            let mapping = BuildMapping {
                offset: u64::from(range.start) * block_size,
                length: u64::from(range.end - range.start) * block_size,
                build_id,
                storage_offset,
            };
            storage_offset += mapping.length;
            mapping
        })
        .collect()
}

fn block_ranges(blocks: &RoaringBitmap) -> Vec<Range<u32>> {
    blocks.iter().fold(Vec::new(), |mut ranges: Vec<Range<u32>>, block| {
        match ranges.last_mut() {
            Some(range) if range.end == block => range.end += 1,
            _ => ranges.push(block..block + 1),
        }
        ranges
    })
}

pub fn validate(mappings: &[BuildMapping], size: u64) -> Result<(), String> {
    let total = mappings.iter().try_fold(0, |expected, mapping| {
        if mapping.offset == expected {
            Ok(mapping.end())
        } else {
            Err(format!(
                "mapping validation failed: expected offset {expected}, got {}",
                mapping.offset
            ))
        }
    })?;
    if total != size {
        return Err(format!("mapping validation failed: total {total} != size {size}"));
    }
    Ok(())
}
```

Move the `use uuid::Uuid;` and `use super::metadata::...` lines so all `use` statements sit together at the top of the file.

- [ ] **Step 4: Implement `locate` and `DiffMetadata`**

`crates/orchestrator/src/storage/header/diff.rs`:
```rust
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::Header;
use super::mapping::{from_blocks, merge, normalize, validate};

#[derive(Debug, Clone, Default)]
pub struct DiffMetadata {
    pub dirty: RoaringBitmap,
    pub empty: RoaringBitmap,
    pub block_size: u64,
}

impl DiffMetadata {
    pub fn to_diff_header(&self, parent: &Header, build_id: Uuid) -> Result<Header, String> {
        let dirty = from_blocks(build_id, &self.dirty, self.block_size);
        let empty = from_blocks(Uuid::nil(), &self.empty, self.block_size);
        let mappings = normalize(&merge(&parent.mappings, &merge(&dirty, &empty)));

        let header = Header::new(parent.metadata.next_generation(build_id), mappings)?;
        validate(&header.mappings, header.metadata.size)?;
        Ok(header)
    }
}
```

In `crates/orchestrator/src/storage/header.rs`, change the module lines to:
```rust
mod diff;
pub mod mapping;
mod metadata;

pub use diff::DiffMetadata;
pub use mapping::{BuildMapping, MAPPING_SIZE};
pub use metadata::{METADATA_SIZE, Metadata};

use uuid::Uuid;
```
Add after `pub const PAGE_SIZE`:
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub build_id: Uuid,
    pub storage_offset: u64,
    pub length: u64,
}
```
Add to `impl Header`:
```rust
    pub fn locate(&self, offset: u64) -> Result<Location, String> {
        let size = self.metadata.size;
        if offset >= size {
            return Err(format!("offset {offset} is out of bounds (size {size})"));
        }
        let index = self.mappings.partition_point(|mapping| mapping.offset <= offset);
        let mapping = index
            .checked_sub(1)
            .map(|index| self.mappings[index])
            .ok_or_else(|| format!("offset {offset} has no mapping"))?;
        let shift = offset - mapping.offset;
        Ok(Location {
            build_id: mapping.build_id,
            storage_offset: mapping.storage_offset + shift,
            length: mapping.length - shift,
        })
    }
```

Run `cargo add -p orchestrator roaring`.

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p orchestrator storage::header`
Expected: PASS, 10 tests.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets
git add crates/orchestrator Cargo.lock
git commit -m "feat(orchestrator): mapping merge, normalize, locate and diff headers"
```

---

### Task 5: Build object paths

**Files:**
- Create: `crates/orchestrator/src/storage/paths.rs`
- Modify: `crates/orchestrator/src/storage.rs` (`pub mod paths;`)

**Interfaces:**
- Produces: `storage::paths::BuildPaths { build_id: Uuid }` with `BuildPaths::new(build_id)`, plus `memfile()`, `memfile_header()`, `rootfs()`, `rootfs_header()`, `snapfile()` and `metadata()`, each returning `String`.

- [ ] **Step 1: Write the failing test**

`crates/orchestrator/src/storage/paths.rs`:
```rust
#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[test]
    fn object_names_match_upstream() {
        let build_id = Uuid::from_u128(0x1234);
        let paths = BuildPaths::new(build_id);
        let prefix = build_id.to_string();
        assert_eq!(paths.memfile(), format!("{prefix}/memfile"));
        assert_eq!(paths.memfile_header(), format!("{prefix}/memfile.header"));
        assert_eq!(paths.rootfs(), format!("{prefix}/rootfs.ext4"));
        assert_eq!(paths.rootfs_header(), format!("{prefix}/rootfs.ext4.header"));
        assert_eq!(paths.snapfile(), format!("{prefix}/snapfile"));
        assert_eq!(paths.metadata(), format!("{prefix}/metadata.json"));
    }
}
```
Add `pub mod paths;` to `crates/orchestrator/src/storage.rs`.

- [ ] **Step 2: Run the test to see it fail**

Run: `cargo test -p orchestrator storage::paths`
Expected: FAIL to compile, `cannot find struct BuildPaths`.

- [ ] **Step 3: Implement**

Put at the top of `crates/orchestrator/src/storage/paths.rs`:
```rust
use uuid::Uuid;

const MEMFILE: &str = "memfile";
const ROOTFS: &str = "rootfs.ext4";
const SNAPFILE: &str = "snapfile";
const METADATA: &str = "metadata.json";
const HEADER_SUFFIX: &str = ".header";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildPaths {
    pub build_id: Uuid,
}

impl BuildPaths {
    pub fn new(build_id: Uuid) -> Self {
        Self { build_id }
    }

    pub fn memfile(&self) -> String {
        self.file(MEMFILE)
    }

    pub fn memfile_header(&self) -> String {
        self.header(MEMFILE)
    }

    pub fn rootfs(&self) -> String {
        self.file(ROOTFS)
    }

    pub fn rootfs_header(&self) -> String {
        self.header(ROOTFS)
    }

    pub fn snapfile(&self) -> String {
        self.file(SNAPFILE)
    }

    pub fn metadata(&self) -> String {
        self.file(METADATA)
    }

    fn file(&self, name: &str) -> String {
        format!("{}/{name}", self.build_id)
    }

    fn header(&self, name: &str) -> String {
        format!("{}/{name}{HEADER_SUFFIX}", self.build_id)
    }
}
```

- [ ] **Step 4: Run the test to see it pass**

Run: `cargo test -p orchestrator storage::paths`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets
git add crates/orchestrator
git commit -m "feat(orchestrator): build object paths"
```

---

### Task 6: Object storage backend (local FS and S3/MinIO)

**Files:**
- Create: `crates/orchestrator/src/storage/backend.rs`
- Modify: `crates/orchestrator/src/storage.rs` (`pub mod backend;`), `crates/orchestrator/Cargo.toml` (`object_store` with feature `aws`, `bytes`, `futures`, `tokio` full; dev: `tempfile`)

**Interfaces:**
- Consumes: `Header` (Task 3).
- Produces:
  - `storage::backend::Storage` with:
    - `Storage::local(root: &Path) -> Result<Storage, String>`
    - `Storage::s3() -> S3Builder`
    - `S3Builder::bucket(self, &str) -> Self`, `endpoint(self, &str) -> Self`, `credentials(self, key: &str, secret: &str) -> Self`, `region(self, &str) -> Self`, `build(self) -> Result<Storage, String>`
    - async `put(&self, path: &str, data: Bytes) -> Result<(), String>`
    - async `get(&self, path: &str) -> Result<Bytes, String>`
    - async `get_range(&self, path: &str, range: Range<u64>) -> Result<Bytes, String>`
    - async `size(&self, path: &str) -> Result<u64, String>`
    - async `delete_prefix(&self, prefix: &str) -> Result<(), String>`
    - async `load_header(&self, path: &str) -> Result<Header, String>`
    - async `store_header(&self, path: &str, header: &Header) -> Result<(), String>`

- [ ] **Step 1: Write the failing tests**

`crates/orchestrator/src/storage/backend.rs`:
```rust
#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use uuid::Uuid;

    use super::*;
    use crate::storage::header::Metadata;

    async fn exercise(storage: &Storage, prefix: &str) {
        let path = format!("{prefix}/memfile");
        storage.put(&path, Bytes::from_static(b"0123456789")).await.unwrap();

        assert_eq!(storage.get(&path).await.unwrap(), Bytes::from_static(b"0123456789"));
        assert_eq!(storage.get_range(&path, 2..5).await.unwrap(), Bytes::from_static(b"234"));
        assert_eq!(storage.size(&path).await.unwrap(), 10);
        assert!(storage.get_range(&path, 8..20).await.is_err());

        let header = Header::new(Metadata::template(Uuid::from_u128(7), 4096, 8192), Vec::new()).unwrap();
        let header_path = format!("{prefix}/memfile.header");
        storage.store_header(&header_path, &header).await.unwrap();
        assert_eq!(storage.load_header(&header_path).await.unwrap(), header);

        storage.delete_prefix(prefix).await.unwrap();
        assert!(storage.get(&path).await.is_err());
        assert!(storage.get(&header_path).await.is_err());
    }

    #[tokio::test]
    async fn local_filesystem_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let storage = Storage::local(root.path()).unwrap();
        exercise(&storage, "build-a").await;
    }

    #[tokio::test]
    async fn s3_round_trip_when_minio_is_available() {
        let Ok(endpoint) = std::env::var("E2B_TEST_S3_ENDPOINT") else {
            return;
        };
        let storage = Storage::s3()
            .bucket("e2b-test")
            .endpoint(&endpoint)
            .credentials("minio", "minio123")
            .region("us-east-1")
            .build()
            .unwrap();
        exercise(&storage, &Uuid::new_v4().to_string()).await;
    }
}
```

Add `pub mod backend;` to `crates/orchestrator/src/storage.rs`. Run:
```bash
cargo add -p orchestrator object_store --features aws
cargo add -p orchestrator bytes futures
cargo add -p orchestrator tokio --features full
cargo add -p orchestrator --dev tempfile
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p orchestrator storage::backend`
Expected: FAIL to compile, `cannot find type Storage`.

- [ ] **Step 3: Implement**

Put at the top of `crates/orchestrator/src/storage/backend.rs`:
```rust
use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use futures::TryStreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutPayload};

use super::header::Header;

#[derive(Clone)]
pub struct Storage {
    store: Arc<dyn ObjectStore>,
}

#[derive(Default)]
pub struct S3Builder {
    bucket: String,
    endpoint: Option<String>,
    key: String,
    secret: String,
    region: String,
}

impl Storage {
    pub fn local(root: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(root).map_err(|e| format!("create storage dir {}: {e}", root.display()))?;
        let store = LocalFileSystem::new_with_prefix(root).map_err(|e| e.to_string())?;
        Ok(Self { store: Arc::new(store) })
    }

    pub fn s3() -> S3Builder {
        S3Builder::default()
    }

    pub async fn put(&self, path: &str, data: Bytes) -> Result<(), String> {
        self.store
            .put(&ObjectPath::from(path), PutPayload::from(data))
            .await
            .map(|_| ())
            .map_err(|e| format!("put {path}: {e}"))
    }

    pub async fn get(&self, path: &str) -> Result<Bytes, String> {
        let object = self
            .store
            .get(&ObjectPath::from(path))
            .await
            .map_err(|e| format!("get {path}: {e}"))?;
        object.bytes().await.map_err(|e| format!("read {path}: {e}"))
    }

    pub async fn get_range(&self, path: &str, range: Range<u64>) -> Result<Bytes, String> {
        let size = self.size(path).await?;
        if range.end > size {
            return Err(format!("range {range:?} is past the end of {path} ({size} bytes)"));
        }
        self.store
            .get_range(&ObjectPath::from(path), range.clone())
            .await
            .map_err(|e| format!("get {path} {range:?}: {e}"))
    }

    pub async fn size(&self, path: &str) -> Result<u64, String> {
        self.store
            .head(&ObjectPath::from(path))
            .await
            .map(|meta| meta.size)
            .map_err(|e| format!("head {path}: {e}"))
    }

    pub async fn delete_prefix(&self, prefix: &str) -> Result<(), String> {
        let prefix_path = ObjectPath::from(prefix);
        let objects: Vec<_> = self
            .store
            .list(Some(&prefix_path))
            .try_collect()
            .await
            .map_err(|e| format!("list {prefix}: {e}"))?;
        for object in objects {
            self.store
                .delete(&object.location)
                .await
                .map_err(|e| format!("delete {}: {e}", object.location))?;
        }
        Ok(())
    }

    pub async fn load_header(&self, path: &str) -> Result<Header, String> {
        Header::decode(&self.get(path).await?)
    }

    pub async fn store_header(&self, path: &str, header: &Header) -> Result<(), String> {
        self.put(path, Bytes::from(header.encode())).await
    }
}

impl S3Builder {
    pub fn bucket(mut self, bucket: &str) -> Self {
        self.bucket = bucket.to_owned();
        self
    }

    pub fn endpoint(mut self, endpoint: &str) -> Self {
        self.endpoint = Some(endpoint.to_owned());
        self
    }

    pub fn credentials(mut self, key: &str, secret: &str) -> Self {
        self.key = key.to_owned();
        self.secret = secret.to_owned();
        self
    }

    pub fn region(mut self, region: &str) -> Self {
        self.region = region.to_owned();
        self
    }

    pub fn build(self) -> Result<Storage, String> {
        let builder = AmazonS3Builder::new()
            .with_bucket_name(&self.bucket)
            .with_access_key_id(&self.key)
            .with_secret_access_key(&self.secret)
            .with_region(&self.region);
        let builder = match &self.endpoint {
            Some(endpoint) => builder
                .with_endpoint(endpoint)
                .with_allow_http(endpoint.starts_with("http://"))
                .with_virtual_hosted_style_request(false),
            None => builder,
        };
        let store = builder.build().map_err(|e| format!("s3 bucket {}: {e}", self.bucket))?;
        Ok(Storage { store: Arc::new(store) })
    }
}
```

If the installed `object_store` version takes `Range<usize>` in `get_range`, or exposes `size` as `usize`, convert at those two call sites with `usize::try_from` / `as u64`. Keep the public `Storage` API in `u64` and record the version in the ledger.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p orchestrator storage::backend`
Expected: PASS, 2 tests. The S3 test returns early locally and runs for real in CI with MinIO.

Then run against a local MinIO to prove the S3 path before pushing:
```bash
docker run -d --rm --name minio-local -p 9000:9000 -e MINIO_ROOT_USER=minio -e MINIO_ROOT_PASSWORD=minio123 minio/minio server /data
until curl -sf http://127.0.0.1:9000/minio/health/live; do sleep 0.5; done
docker run --rm --network host --entrypoint sh minio/mc -c "mc alias set local http://127.0.0.1:9000 minio minio123 && mc mb --ignore-existing local/e2b-test"
E2B_TEST_S3_ENDPOINT=http://127.0.0.1:9000 cargo test -p orchestrator storage::backend
docker rm -f minio-local
```
Expected: PASS, 2 tests, including `s3_round_trip_when_minio_is_available`. On macOS Docker Desktop, `--network host` for `mc` may not reach the host. If so, use `--add-host host.docker.internal:host-gateway` and `http://host.docker.internal:9000` in the `mc alias` command.

- [ ] **Step 5: Commit and push with the user's approval**

```bash
cargo fmt && cargo clippy --workspace --all-targets && cargo test --workspace
git add crates/orchestrator Cargo.lock
git commit -m "feat(orchestrator): object storage backend for local FS and S3"
git push
gh run watch --repo Empatixx/e2b-runtime-rs --exit-status
```
Expected: CI `test` is green with `s3_round_trip_when_minio_is_available` running against MinIO.
