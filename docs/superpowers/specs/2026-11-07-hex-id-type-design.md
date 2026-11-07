# Design: `HashedId<T: Hexable>` for public and internal IDs

Date: 2026-11-07  
Scope: `sim-cluster` (follow-ups in frontend / systemtest only if wire JSON field shapes stay 16-digit hex strings)

## Intent

Board / port / similar identifiers are currently carried as either `String` (JSON / some APIs) or raw `u64` (internal processing, iceoryx payloads). Call sites re-parse or re-format, and string-typed wire fields allow `&str` to leak into processing (e.g. GDB attach).

Introduce `HashedId<T: Hexable>(T)` so that:

- In-process values are the numeric identity (`u64` today).
- Serde (JSON / TCP backend bodies that use serde) always exchanges a 16-digit lowercase hex string with no `0x` prefix.
- iceoryx zero-copy messages can store `HashedId<u64>` with the same layout as `u64`.

## Goals

- One type for values managed as a **hash** or an **id** (board / path / port / edge hashes, `probe_id`, `name_hash`, iceoryx `from` / `board_hash`, public hex wire fields, related `HashMap` keys, etc.).
- Automatic string ↔ value conversion only at serde boundaries.
- `Hexable` implemented for `u64` only for now; trait stays open for later widths.
- Replace ad-hoc `format_id_hex` / `parse_id_hex` / `{:016x}` call sites for these IDs with `HashedId` / `Hexable`. After migration, delete `format_id_hex` and `parse_id_hex` if nothing references them (`Display` / `FromStr` / serde on `HashedId` are expected to cover former call sites).

## Non-goals

- Changing the public JSON *shape* (still 16 lowercase hex digits).
- Wrapping human-readable topology labels into `HashedId`. `board_label` and topology `path` / port *name* strings used for matching the topology definition stay `String` / `&str`. They are source text for hashing, not public hex ids.
- Introducing `BoardId` / `PortId` type aliases in the first cut (may add later).
- Frontend / Python changes beyond what broken tests force if the wire format is unchanged.

Error / log display of a **hashed** id uses `HashedId` (via `Display` / `ToString`), not a separate habit of calling `format_id_hex` on a bare `u64` at each site.

## Anti-goals

- Using bare `String` or bare `u64` again for the same class of IDs on new APIs.
- Deriving serde on iceoryx payload types so the same struct is used for both JSON and zero-copy (paths stay separate).

## Constraints

- Existing public API: exactly 16 lowercase hex digits (`parse_id_hex` rules).
- iceoryx payloads use `#[repr(C)]` + `ZeroCopySend` and copy raw bytes; they do **not** call serde.
- `HashedId<u64>` must be `#[repr(transparent)]`, `Copy`, and usable inside `ZeroCopySend` types (same size/align as `u64`).
- Worktree / branch: `feature/issue-108-public-id-hex` (builds on issue #108 hex unification).

## Approaches considered

| # | Approach | Tradeoff |
|---|----------|----------|
| 1 | `HashedId` for domain + JSON; keep iceoryx fields as raw `u64`; convert at publish/receive | Clear JSON vs binary split; more conversions at IPC edges |
| 2 | Same `HashedId`, also store `HashedId<u64>` inside iceoryx message fields | One type everywhere; needs transparent + `ZeroCopySend`-safe `HashedId` |
| 3 | Add `BoardId` / `PortId` aliases on top of `HashedId` immediately | Extra names before the core wrapper settles |

**Chosen: 2.** Serde and iceoryx zero-copy coexist on `HashedId` because iceoryx never invokes `Serialize`; it only copies the transparent `u64` bits. Serde runs only when a JSON/TCP-serde type containing `HashedId` is serialized.

## Design

### Types (in `sim-cluster/src/ipc/hash.rs`, re-exported from `ipc`)

```rust
pub trait Hexable: Copy + Eq + Hash { /* encode/decode 16-digit lowercase hex */ }

impl Hexable for u64 { /* 16-digit lowercase hex encode/decode rules */ }

#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct HashedId<T: Hexable>(T); // field private

impl<T: Hexable> HashedId<T> {
    pub const fn new(value: T) -> Self { Self(value) }
    pub const fn get(self) -> T { self.0 }
}

// Serialize / Deserialize as string via Hexable
// Display / FromStr: same 16-digit lowercase hex as serde (required)
// From<T> / Into<T> for ergonomics at hash-construction sites
// ZeroCopySend for T = u64 (as required by iceoryx2)
```

`Display` for `HashedId<T>` is **required** so error messages and logs can write `{board_hash}` / `board_hash.to_string()` and get the public hex form without calling standalone format helpers.

Rust type spelling: `HashedId` (PascalCase; `Id` not `ID`), trait name remains `Hexable`. Field is **private**; use `new` / `get` (and `From`/`Into` as needed).

### Replacement targets

- **Include (`HashedId<u64>`):** anything managed as a hash or an id — e.g. board / path / port / edge hashes, `probe_id`, `name_hash`, iceoryx `from` / `board_hash` / probe id fields, public JSON hex id fields (`AttachGdbRequest.board_id`, topology view ids/ports, probe catalog `board`, …), and `HashMap` keys for those values.
- **Keep as `String` / `&str`:** human-readable topology labels (`board_label`), topology `path` / port *name* strings used for definition matching, probe *names*, action device/action strings, simulation session UUID strings (not xxh3 hashes).
- **Keep as bare numeric (not `HashedId`):** quantities that are not identities — e.g. `virtual_time_ns`, lengths/counts, TCP/GDB host `port: u16`, analog millivolts / UART data words, enum discriminants stored as `u8`.

### Serde behavior

- Serialize: always 16-digit lowercase hex.
- Deserialize: reject anything `parse_id_hex` would reject (wrong length, uppercase, `0x` prefix).
- Error mapping at HTTP boundary: invalid `HashedId` in JSON → existing `bad_param` style errors (same as today’s empty/invalid board_id checks where applicable).
- Where an id today is only binary / internal and later appears in serde JSON (e.g. `ProbePointName.probe_id`, `board_hash`), the JSON form becomes a hex string (hard cut; both ends in-tree).

### Migration notes

- Rule of thumb: if the value is produced by `*_hash` / `probe_id_hash` / `edge_id` / similar, or named `*_id` / `*_hash` as an identity, use `HashedId`. If it is a duration, counter, or physical measurement, leave bare.
- TCP backend catalog rows: numeric id fields that become `HashedId` serialize as hex strings after the cut. Document in PR.

## Settled calls (author / agent)

- **Scope rule (user):** Values managed as hashes or ids are `HashedId` replacement targets, including `probe_id` and `name_hash` (not only board/port public wire fields).
- **Deploy compatibility:** In-tree HTTP worker and TCP backend ship together. No dual deserialize for old numeric id JSON; hard cut to hex strings on that internal wire.
- **HTTP errors:** Invalid hex fails at serde deserialize into `HashedId`. Map to existing `bad_param` where the handler already does; exact legacy message strings are not a hard requirement.
- **Branching:** Continue on `feature/issue-108-public-id-hex` (same line of work as issue #108), not a separate base.
- **HashMap keys:** Maps keyed by those hashes/ids use `HashedId<u64>` keys.
- **Topology hash tables:** Include `boards` / `paths` / `ports` / `edges` tables’ hash keys (values remain human-readable `String` labels).
- **Module:** Define `HashedId` / `Hexable` in `ipc/hash.rs` (no separate `hex.rs`).
- **Field visibility:** `HashedId`’s inner value is private (`new` / `get` / `From`/`Into`).
- **Helpers:** Remove `format_id_hex` / `parse_id_hex` when unused after migration (encoding lives on `Hexable` / `Display` / `FromStr`).

## Left to the implementer

- Ordering of commits (type + serde tests first, then contract, then iceoryx, then call sites).
- `clippy` / `ZeroCopySend` attribute details for `HashedId`.
- Exact names of constructors/accessors if `new`/`get` are renamed for local style.

## Verification

- Unit tests: serde round-trip, reject bad hex, `size_of::<HashedId<u64>>() == 8`, iceoryx-related types still `ZeroCopySend`.
- `cargo test -p sim-cluster --lib` (and existing HTTP / topology tests that assert hex strings).
