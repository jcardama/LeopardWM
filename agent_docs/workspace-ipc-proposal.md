# Workspace IPC for native status-bar integration

Status: historical design record. The implementation has landed on this branch;
the maintained contract is [`ipc-events.md`](ipc-events.md). This document retains
the original baseline and design rationale for review history.
Baseline: `main` at `bad5b42` (LeopardWM 0.2.8, IPC protocol 2).
Target consumer: a native YASB workspace widget, with no YASB dependency in LeopardWM.

## Outcome

A bar obtains every monitor's workspace membership in one coherent subscription,
including inactive and empty workspaces, and switches the workspace on the monitor
the user actually clicked. YASB controls filtering, icons, labels, and styling.
The persisted workspace-state.json file is no longer the integration API.

## Existing implementation and gaps

- `crates/ipc/src/lib.rs`: Subscribe, WorkspaceChanged, LayoutChanged, and
  SwitchWorkspace already exist. Preserve these messages and their semantics.
- `crates/daemon/src/main.rs::handle_ipc_subscribe`: receiver creation and startup
  snapshot are atomic under AppState's mutex. Reuse this ordering guarantee.
  Existing startup layout data covers only the focused workspace.
- LayoutChanged contains tiled columns, not floating membership. It is unsuitable
  as the authoritative state of all workspaces.
- SwitchWorkspace takes a 1-based index and implicitly uses focused_monitor.
- Workspaces are lazily allocated; persistence omits inactive empty workspaces.
  A bar must still be able to show all nine configured workspace slots.
- `ipc_server.rs` caps each serialized JSON line at 64 KiB. A full-state protocol
  must account for that bound rather than silently dropping a large snapshot.
- The existing IpcEvent enum has no unknown-event fallback. Adding a new event to
  the old Subscribe empty-filter default would break older typed clients.

## Chosen approach and alternatives

Extend the existing `Subscribe` command and `IpcEvent` contract with an explicit
`workspace_state` event filter. Use `lwm subscribe --events workspace_state` to
receive complete initial and replacement snapshots. There is no new subscription
subcommand or transport. Add a one-shot
workspace query and monitor-targeted switching within the existing query/command
infrastructure. Protocol 3 provisionally records the additive capability; existing protocol 1/2
wire requests retain their behavior.

Choose opt-in for the first upstream PR. Plain `lwm subscribe` and an empty wire
filter retain the current legacy event set; they do not start emitting new variants
to older clients. Internally, distinguish `EventKind::all()` (every known kind)
from a named `legacy_default()` set used to expand an empty subscription filter.
Update CLI help, documentation, and tests that currently say empty means all.
Explicit lists can combine `workspace_state` with existing event kinds. Making it
a default later requires an explicit compatibility/migration decision, not a silent
change to the legacy default in this PR.

A file watcher would retain persistence-schema coupling and poor liveness semantics.
Adding only more LayoutChanged messages would still conflate layout and membership.
Fine-grained membership deltas would save bandwidth but require more recovery and
ordering logic. Start with complete, deduplicated snapshots on the existing event
stream; optimize only with measurements.

IPC v3 is provisional: PR #110 also proposes v3. Reconcile the version/history
against upstream at merge time; capability detection uses the acknowledged filter,
not a version-number assumption. The legacy-default exception is an explicit
compatibility decision for maintainer review, not an existing upstream rule.

## Proposed commands

| Wire command | CLI | Result |
| --- | --- | --- |
| `{"type":"subscribe","events":["workspace_state"]}` | `lwm subscribe --events workspace_state` | Existing subscribed ack, initial snapshot, replacement snapshots, heartbeats |
| `{"type":"query_workspace_state"}` | `lwm query workspaces` | Query ack, one snapshot, then EOF |
| `{"type":"switch_workspace_on_monitor","monitor_device_name":"\\\\.\\DISPLAY2","index":2}` | `lwm workspace 2 --monitor '\\.\DISPLAY2'` | Existing ok/error response |

The existing `Subscribe { events }` shape is unchanged. Add `WorkspaceState` to
EventKind, and snapshot begin/chunk/end/error variants to IpcEvent, all classified
as `workspace_state`. The existing `lwm subscribe` CLI consumes the Subscribed ack
and forwards events as NDJSON, including these new variants when requested.

For the one-shot query, the wire ack is
`{"status":"workspace_state_ready","protocol_version":3}`; the CLI consumes it
and prints the same snapshot event frames as the subscription. The query shares
the snapshot builder and bounded encoder, then closes after one complete snapshot.
It is not one unbounded JSON response. Commands and queries use separate
connections from an ongoing subscription, as they do today.

## State model

The snapshot is a compact semantic view built from in-memory AppState.

| Record | Fields and meaning |
| --- | --- |
| Monitor | `monitor_device_name: string`, `monitor_id: i64` (current HMONITOR), `active_workspace_index: u8` |
| Workspace | `monitor_device_name: string`, `workspace_index: u8`, `name: string or null` |
| Window membership | `monitor_device_name: string`, `workspace_index: u8`, `hwnd: u64`, `is_floating: bool`, `is_sticky: bool` |

- Snapshot begin also contains `focused_monitor_device_name: string or null`.
- Emit all connected monitors and all nine slots (indexes 0 through 8) for each,
  even if an empty workspace has not been instantiated in AppState.
- Serialize monitors by device name, workspaces by index, and memberships by HWND
  for deterministic output and exact equality comparison.
- Exclude drag placeholders and internal overlay windows. Retain managed minimized
  and inactive tab windows; visibility is not the same as membership.
- Sticky windows are reported in their current owning workspace with is_sticky.
  Clients may project them across that monitor's workspaces as a display policy.
  Hidden scratchpad windows are omitted until assigned to a workspace again.
- Membership is authoritative WM state, not the set of windows currently visible
  to desktop enumeration. Icon lookup failure must not make a workspace empty.
- Device names identify monitors within the current topology, not permanently
  across hardware changes. HWND and HMONITOR values are transient and must never
  be persisted as stable identities by clients.
- No application titles, executable lookups, images, glyphs, or filesystem paths
  are required in this API. YASB resolves HWNDs through its existing Windows icon
  and process utilities, caches results, and supplies generic fallbacks. Keep
  potentially blocking Win32 icon/process calls out of AppState's lock.

Active and focused are distinct: each monitor has one active workspace; the
snapshot separately identifies the globally focused monitor. Population follows
membership, independently of both. These are the inputs for show-inactive,
show-empty, show-icons, include-floating, and icon-deduplication options in YASB.

## Snapshot framing and consistency

Illustrative wire sequence after requesting `workspace_state` (records shortened
for clarity). The CLI consumes the first line, exactly as existing subscribe does:

```json
{"status":"subscribed","events":["workspace_state"]}
{"type":"workspace_snapshot_begin","protocol_version":3,"session_id":"opaque-daemon-instance","revision":12,"focused_monitor_device_name":"\\\\.\\DISPLAY2"}
{"type":"workspace_snapshot_chunk","revision":12,"records":[{"kind":"monitor","monitor_device_name":"\\\\.\\DISPLAY2","monitor_id":65537,"active_workspace_index":1}]}
{"type":"workspace_snapshot_chunk","revision":12,"records":[{"kind":"workspace","monitor_device_name":"\\\\.\\DISPLAY2","workspace_index":1,"name":"Code"},{"kind":"window","monitor_device_name":"\\\\.\\DISPLAY2","workspace_index":1,"hwnd":123456,"is_floating":false,"is_sticky":false}]}
{"type":"workspace_snapshot_end","revision":12}
```

The real sequence includes all nine workspace records for every connected monitor.

1. Capture an immutable snapshot and attach the existing broadcast receiver
   atomically under the state mutex; mixed subscriptions share that receiver.
   Start at revision 0; advance monotonically when semantic state changes. Include
   protocol version and session ID in each begin event. The session ID changes on
   daemon restart, not on client reconnect.
2. Preflight frame sizes in memory under the lock; serialize transport frames
   and write outside the lock. Pack records by actual UTF-8 serialized byte length, including the trailing newline; every frame is at most 64 KiB.
   Preflight all record sizes. A single oversized record produces
   `workspace_snapshot_error` with a bounded message, then closes the connection.
   Never truncate membership or substitute a successful snapshot end. Avoid the
   old generic oversized-event-to-Lagged substitution for these snapshot frames.
3. The client stages begin/chunk records and replaces its displayed model only at
   the matching end. EOF/error before end discards the staged snapshot. Never mix
   chunks from different revisions or daemon sessions.
4. Publish snapshot begin/chunk/end events through the existing
   `AppState::broadcast_event` and its 256-event broadcast channel. Build and
   preflight the complete frame sequence before publishing under the state lock,
   so other state mutations cannot interleave a snapshot. Do not add a watch channel.
5. Reuse subscription heartbeat, disconnect handling, and connection permit release.
   Defer heartbeats while a snapshot transaction is being written. On broadcast
   lag, workspace-state subscribers receive Lagged and disconnect; they discard any
   partial snapshot and reconnect for a fresh atomic initial snapshot. Legacy-only
   subscribers retain existing recovery behavior. A bounded write timeout releases
   stalled clients. Oversized initial snapshots are written directly and are not
   limited to 256 frames; lag on later large bursts is explicit, never silent.
6. Read-only queries and new subscriptions do not themselves increment revisions.
   The one-shot query terminates after its complete snapshot, with no heartbeat.

Extend IpcEvent and reuse its serialization/writing infrastructure; place record
types and byte-bounded snapshot encoding in a focused module, not a second public
subscription protocol. The existing `status`-ack to `type`-event parser transition
is unchanged. Only explicitly opted-in subscriptions receive the new variants.
Older daemons reject the unknown workspace_state filter; the CLI/client reports an
unsupported capability rather than silently reverting to incomplete data.

## Change publication

Add an AppState helper that constructs the compact workspace view and compares it
with the previous view. Invoke publication after each completed daemon event,
including window lifecycle, hotkey/IPC commands, configuration reload, display
reconfiguration, and focus changes. Initialize the cached view on the first completed event or subscription/query capture.
Audit early-continue paths so none bypass a relevant state publication.

Compare semantic values exactly, not only a hash. Window rectangles, animation
progress, scroll positions, and icon changes are intentionally absent, so they do
not trigger workspace snapshots. Name changes and background window removal do.
Do not reuse persisted_signature: it includes geometry and omits relevant labels.

The initial snapshot operation must publish/capture any current pending view under
the same lock before attaching the receiver. This prevents a query/subscription
from seeing older cached state between a mutation and the event-loop publication.

## Monitor-targeted switching

The new command validates the device name and index before any state mutation.
Unknown/disconnected monitor or index outside 1..9 returns error without changing
focus, dismissing an overview, or cancelling a drag.

On success it activates the requested monitor and workspace and restores that
workspace's eligible focused window. Clicking an already active workspace on an
unfocused monitor still focuses that monitor. Empty destinations select that
monitor/workspace without inventing a window to focus. Other monitors retain their
active workspace indexes.

Refactor the existing switch implementation to accept an explicit monitor target;
preserve the old command as a wrapper supplying focused_monitor. Audit its use of
focused_workspace, floating_focus, sticky rehoming, overview dismissal, drag
cleanup, and pending transition guards. Do not implement this as two independently
queued focus-monitor and switch-workspace commands: intervening focus changes
would recreate the race the explicit target is intended to remove.

Snapshot and membership indexes stay zero-based; switch-command/CLI indexes stay
one-based to preserve the established API. Convert once at the command boundary.

## Implementation units and inspectable diffs

1. **Contract and serialization** — extend EventKind/IpcEvent and add the query
   and targeted-switch variants in `crates/ipc/src/lib.rs` (use a focused
   workspace_state submodule for record types). Separate the legacy default event
   set from all known kinds. Add protocol 3 and old-client compatibility fixtures.
2. **Snapshot builder and publication** — add
   `crates/daemon/src/workspace_ipc.rs`; wire state, startup/subscription handling,
   and post-event publication through `state.rs`, `events.rs`, and `main.rs`.
   Keep snapshot projection testable with synthetic monitor/workspace data.
3. **Transport** — extend existing Subscribe handling in `ipc_server.rs` to include
   workspace-state events only on opt-in, and route the one-shot query through
   the same snapshot builder/encoder. Reuse connection lifecycle and event writing.
   Cover mixed subscriptions, atomic capture, frame limits, and slow readers.
4. **Switch command and CLI** — update `command_handler.rs`, CLI args/dispatch,
   response handling, and daemon_cmds event-filter parsing. Preserve `lwm subscribe`
   behavior without the new filter, and document its legacy default explicitly.
   Add `query workspaces` and `workspace N --monitor`; reuse transition behavior.
5. **Documentation and consumer contract** — update `agent_docs/ipc-events.md`
   and public Rust API comments with opt-in examples, indexes, monitor semantics,
   and recovery rules. Add a target-release CHANGELOG.md entry for the IPC/CLI
   additions. YASB implementation is a separate repository PR.

Each implementation commit includes its relevant tests. No generated/vendor files,
new third-party dependencies, runtime deployment, or YASB edits are required here.

## Contribution and PR requirements

The upstream gates and architecture boundaries below come from
[CONTRIBUTING.md](../CONTRIBUTING.md),
[AGENTS.md](../AGENTS.md), and the current [CI workflow](../.github/workflows/ci.yml).
PR separation and consumer compatibility are design choices for this contribution.

- Keep this feature on a main-based branch in the personal fork. Submit generic
  workspace IPC support to LeopardWM; do not include personal bar configuration,
  icon fonts, or a YASB dependency. YASB consumes the documented protocol in its
  own main-based feature branch and PR, with its required LeopardWM capability
  and unsupported-version behavior clearly stated.
- Preserve the repository boundaries: core_layout stays platform-independent;
  all new Windows API calls belong in platform_win32 using windows-rs; daemon owns
  state and event orchestration; cli remains a thin IPC client. Membership records
  and serialization live in ipc and must not depend on YASB or the daemon crate.
- Use conventional commits (`feat:`, `fix:`, `docs:`, `test:`, `refactor:` as
  appropriate), document public APIs, and add tests for new functionality.
- Before submitting implementation, run `cargo fmt --all`, then
  `cargo fmt --all -- --check`, `cargo test --all --locked`,
  `cargo clippy --all --locked -- -D warnings`, and
  `cargo build --release --locked` on the configured MSVC target. Locked resolution
  supplements the contribution guide without changing its build/test/lint gates.
- CI must also pass its existing Scoop-manifest and GUI-subsystem verification.
  Do not edit generated distribution artifacts just to suppress unrelated failures.
  Record any pre-existing failure separately; do not claim PR readiness until the
  required checks are satisfied.
- PR descriptions explain the initial-membership problem, before/after behavior,
  opt-in compatibility, wire examples, tests, and the dependent YASB PR when one
  exists. Keep the final diff focused; consolidate this proposal into maintained
  IPC documentation before submission if a standalone design file is unnecessary.
- Merge requires passing CI and at least one approving review. Changes to .github/
  or SECURITY.md require owner review; neither is part of this proposal's scope.
  Contributions retain GPL-3.0 licensing.

YASB's local checkout has no root CONTRIBUTING.md. Check its own documented
contribution guidance, widget conventions, and CI when preparing that separate
branch; LeopardWM's Rust-specific gates do not establish YASB PR readiness.

## Required verification before implementation is called complete

| Test | Expected evidence |
| --- | --- |
| Legacy wire fixtures | Old query/switch/subscribe messages round-trip unchanged; empty/default subscriptions never see new frames; new filter rejected clearly by old daemons |
| Opt-in and mixed filters | workspace_state alone and combined with legacy kinds deliver requested data; snapshot transactions never interleave legacy events; CLI consumes existing ack |
| Full initial state | Two monitors, nine slots each, inactive tiled and floating windows included without first switching to them |
| Projection policies | Minimized/tabbed membership retained; placeholders excluded; sticky and scratchpad cases match documented ownership |
| Membership updates | Open/close/move/float/sticky transitions update correct source and destination, including inactive workspaces |
| Names/topology/focus | Same-length name changes, monitor add/remove, and focus onto an empty monitor update snapshots |
| Deduplication | Geometry/animation-only changes do not advance revision or publish another snapshot |
| Atomic startup | A change racing subscription is present in initial state or subsequent replacement, never lost |
| Slow reader/reconnect | Bounded retained state; newest complete snapshot after skipped revisions; fresh session after restart |
| Framing | More than 64 KiB total succeeds across bounded frames; non-ASCII byte sizes counted correctly; partial transaction never commits |
| Explicit targeting | Switching monitor B while A is focused changes only B's active index; active-on-B click focuses B; invalid target has no side effects |
| Build gates | `cargo fmt --all -- --check`, `cargo test --all --locked`, `cargo clippy --all --locked -- -D warnings`, `cargo build --release --locked`, existing CI artifact checks |
| Desktop acceptance | Separate opt-in deployment verifies two-monitor switching, floating focus restoration, restart, and monitor disconnect |

## Research basis

YASB already separates widget state from presentation and uses Windows HWND icon
resolution in its Komorebi and GlazeWM workspace widgets. This proposal supplies
that same category of data without importing their WM-specific layout semantics.

- https://github.com/amnweb/yasb/blob/main/src/core/widgets/komorebi/workspaces.py
- https://github.com/amnweb/yasb/blob/main/src/core/widgets/glazewm/workspaces.py
- https://github.com/amnweb/yasb/blob/main/src/core/utils/win32/app_icons.py
- Existing LeopardWM contract: [ipc-events.md](ipc-events.md).

The references describe upstream patterns. The baseline analysis above was checked
against this feature worktree, and the local YASB checkout was inspected separately.
