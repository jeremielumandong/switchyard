# GPUI Kit 0.7.3 upgrade (2026-10-12)

The upstream base is now `gpui-base` 0.7.3. Published archive SHA256:
`2e9955b6f4b048fd50ca15fc8e225d80bb00ba9ebc8fecdddde5ad26dbef9270`.
The 0.7.1 application contributions below were three-way merged onto the
published 0.7.3 sources, preserving structured input/history, selection,
retained text layout, and accessibility changes. Upstream context menu,
completion, typography, and reduced frame writes are included.

Overlapping changes required retaining both metadata_fence and selection_focus
in the input constructor, borrowing the new RefCell-backed selection runs in
read-only projection accessors, and retaining styled text after upstream's
separate background/foreground paint passes (without painting it twice).

Kit/component/assets are 0.7.3. The small facades in `vendor/gpui-fast/compat`
satisfy the 0.3.9 snapshot dependency names while preserving the application's
patched, pinned Fast backend. This is a locally maintained Fast compatibility
stack, not the published Zed gpui-pre runtime. See that directory's README.

## Historical 0.7.1 contribution record

### Native structured input contribution

This directory preserves published Apache-2.0 `gpui-base` 0.7.1. Its original
crate archive SHA256 is
`3cf698677b253118459f552fe15513858c669d79a25837a94e480db2ae330050`,
verified against the pre-patch Cargo lock. All208 original cached regular
files were compared byte for byte with that archive before review. The Cargo
registry/cache is unchanged. Generated Cargo.lock/cache markers are omitted.
The existing pinned GPUI Fast core and GPUI Kit/component/assets versions stay
unchanged. See LICENSE-APACHE and Cargo.toml.orig for upstream attribution.

The contribution changes seven original source files and adds one source file:

- `input/base/inline_tokens.rs`: `replace_with_content` and
  `replace_range_with_content` on InputState and TextareaState's shared
  engine, with coherent InputContent, multiple tokens, whole
  overlap expansion, pre-edit grapheme/normalization validation and one atomic
  text/token/selection undo transaction. Existing single-token operations use
  the same path. Active IME, masked and editor modes retain refusal.
- `input/base/state.rs`: the pending insertion contains the validated token
  spans rather than only one token; existing TokenDelta undo/replay is reused.
  Deferred native clipboard pastes also retain the original selection epoch.
- `input/base/cursor.rs`: an actual selection revision advances on mutations
  through the existing selection owner, including moving away and returning
  before deferred events run. Read-only and no-op mutations preserve it.
- `input/base/kind.rs`: TextareaMode's presentation-only decorations are
  exposed through the existing native shaping path and cleared on edits.
- `input/textarea/mod.rs`: `text_revision`, `selection_revision`, `set_text_decorations` and
  `text_decorations`. Complete byte-range styling snapshots are revision
  fenced and atomically validated. Styles do not edit text, tokens, selection,
  undo or IME. They are intended for the application's current Markdown
  producer, which remains responsible for its original generation/context.
  Selection revision is paired with text revision and the parent's actual
  view, focus, IME and authorization fences; it supplies no authority itself.
- `input/base/undo_manager.rs`: actual native history states carry opaque
  process-local owner/state IDs. Coalescing retains the first before-state and
  latest after-state, native undo/redo returns those exact identities, canceled
  composition returns to its original state, and reset/branch edits use fresh
  IDs. Existing transaction and replay ownership stays in this manager.
- `input/base/history_snapshot.rs` (new) and `input/mod.rs`: public
  `gpui_kit::base::input::{InputHistoryEvent,InputHistoryKind,
  InputHistorySnapshot,InputHistorySelection,InputHistoryStateId}` and actual
  `InputState`/`TextareaState::history_snapshot()` plus
  `retained_history_state_ids()`. Separate immutable events capture native
  content/token identity, complete selection directions/column anchors, text
  and selection revisions and composition state at operation completion.
  GPUI notification delivery remains deferred; consumers use captured values,
  never query a later value as the earlier event. Undo/redo emits one complete
  replay snapshot after all native text/tokens/selections restore. The application
  associates typed context/attachment metadata with actual reachable IDs and
  prunes it after reconciling the current native capture. IDs are neither
  serialized nor credentials; the application retains its real capability,
  context, attachment and original view authority.
  Snapshots also contain `InputHistoryTopology`: actual undo and redo
  destinations nearest replay first, current ID, and separate uncommitted
  composition before/after IDs. Deferred events retain this original topology.
  `commit_metadata_transaction(state_id, text_revision, selection_revision, cx)`
  supplies one bounded native undo transaction for an actual parent-owned
  metadata edit, preserving text/tokens/cursors/auto-pairs. It refuses stale or
  foreign captures, active composition, disabled/readonly or unsupported modes,
  and an open native transaction. It emits a separate Metadata history event;
  application no-op detection, metadata association and durable actor/current
  attachment authorization stay with the parent. Undo/Redo uses this same owner.

This is a dependency contribution, not a second input state or browser editor.
The native application still supplies actual Markdown/chip/paste producers,
accessibility acceptance and installed/platform evidence. Headless tests do
not establish physical IME, clipboard or installed GUI acceptance.

## Disabled native button accessibility (2026-10-10)

The existing Base Button's disabled input/focus guard now also sets the same
state on its AccessKit node through StatefulInteractiveElement::aria_disabled.
GPUI's sparse AriaProperties/NO_ARIA and node writer retain/clear that flag;
false clears disabled state on re-enable. The accessibility builder supplies
semantics only; each control continues owning its interaction guards. No
renderer, toolkit version, dependency pin or rich input owner changes.

The actual desktop Kit/GPUI test failed before repair (disabled() None rather
than Some(true)) and passes disable/click, re-enable/click, disable-again/click
with exactly one activation. Headless AccessKit/input evidence is distinct
from supported-platform assistive technology and installed acceptance.
