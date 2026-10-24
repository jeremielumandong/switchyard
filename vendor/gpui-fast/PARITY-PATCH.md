# Native list viewport contribution

This directory preserves the GPUI core from Longbridge GPUI Fast commit
`598306fcbf77bff0f764b5a178a447344e72e132`, licensed Apache-2.0.
See LICENSE, LICENSE-APACHE and the original core manifest and license link.
All 208 original core Git entries, including their link targets, were compared
with the original Git objects before modification. Their cumulative
SHA256 (path, NUL, original blob bytes) is
`c91c6a686f59f39f07a6637540db6108e48650837544b6f1c373ff2d6fa82d71`.
The Cargo Git cache is unchanged.

The original root manifest is retained as Cargo.toml.orig. The local root
manifest contains a standalone workspace for this core. Its original workspace
dependency versions, features and lints are retained; sibling path dependencies
resolve to the same original Git revision. The application's lock changes only
the existing gpui package from that Git source to this local contribution.
Every other locked package is unchanged. There remains one GPUI core in the
application graph; GPUI Kit, component, assets and all Fast sibling pins stay
unchanged.

The initial original source modification is `crates/gpui/src/elements/list.rs`:
`ListState::visible_range_with_margin(Pixels) -> Option<Range<usize>>` reads
the existing height tree, viewport bounds and scroll anchor on both sides of
the actual native viewport. It includes edge-adjacent rows, matching the
frozen T3 PR observer's threshold-zero intersection. It uses no rendering
callback, focus measurement or overdraw count as a viewport substitute.
Before layout, after reset, or while a row has neither measurement nor a
height hint, it returns None. Empty laid-out lists return Some(0..0), finite
negative margins clamp to zero, and nonfinite margins return None.

This read follows the existing Fast offset-dependency tracking. The application
still owns current list/ref/project/account epochs, actual mounting and frame
invalidation. Height hints retain the original list semantics and converge as
native layout measures rows; this method does not provide remote authority or
guarantee that a hint equals an unmeasured row's final height.

An observed actual500-row consumer regression required retaining the explicit
uniform fallback through first layout and width changes. First layout preserves
existing explicit hints; changed width invalidates actual measurements and uses
only the registered uniform fallback until rows are measured again. Reset
without explicit hints clears that fallback. Ordinary unhinted lists still
return None while offscreen heights remain unknown.
`splice_with_uniform_height(range,count,height) -> bool` inserts hints through
the existing tree/scroll mutation while preserving old measurements, focus and
scroll. Negative or nonfinite inserted heights return false without mutation.
Existing focusable/unhinted splice semantics remain unchanged. Nonfinite total
height refuses viewport projection. This adds no list or measurement owner.

Three application GPUI tests use actual headless native list layout for absent
and reset layout, margins above and below a scroll anchor, edge adjacency,
variable heights, tail clamping and empty lists. These establish native layout
behavior; they do not establish installed GUI or authenticated PR acceptance.
Two additional actual partial-layout tests cover500 hinted rows, changed-width
remeasurement, unhinted refusal,99-to198 append with retained old bounds/scroll,
tail150/narrow-width and reset. Callback counts are used only to observe bounded
native work; viewport ranges come from the actual existing height tree.

## Native physical key metadata contribution

The original Linux backend is now included at the same598306f revision. All33
original Git entries, including license link targets, were byte-verified before
modification, cumulativeSHA256(path,NUL,originalbytes)
a2ff88542a757d0f3823e05fe22217f4a070100c5f2187e74a050cf64a216718.
Only linux.rs, Wayland client/window and X11 client/window change from that
original backend; native_keyboard.rs and its licensed code table are additions.
Cargo patches only this existing gpui_linux source edge; a desktop test-only
xkbcommon0.8 edge uses its already locked version. No dependency version changes.

NativeKeyDownEvent/NativeKeyUpEvent carry NativeKeyMetadata plus an optional
original logical event. Interactivity and InteractiveElement on_native_key_down/
up listeners receive the original capture before logical shortcuts and text.
Window dispatches that native event once; a consumed or default-prevented native
event cannot enter the original logical path. Propagated logical events retain
existing GPUI keybindings, IME/text policy, pending chords and Keystroke equality.
Modifiers have no fabricated logical event. Platform text insertion reads the
same optional logical event and respects native consumption/prevention.

The Wayland producer reads current XKB state for each physical event and repeat,
including current locks/layout/composition. X11 uses its original per-event XKB
state and a native-only pressed-key set for repeat, cleared on focus loss; its
original logical is_held policy is unchanged. Original IME commit events remain
without physical metadata. Other pinned platform backends retain their original
KeyDownEvent/KeyUpEvent construction; those structures are unchanged. They do
not have a native metadata producer or observed cross-platform compilation. Root's Source terminal bridge consumes original fields and
Source helpers; see native_keyboard/ORIGIN.md and the native test corpus.

No browser DOM key-name/location adapter, physical key injection, functional
IME/global Source command/clipboard joins, installed/macOS/Windows input or
whole terminal acceptance is established by these contribution tests.

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

## Windows software rendering (Switchyard, 2026-10-16)

Patched in Switchyard (not yet in AgentOpsChat); ported from AgentOps' patch of Zed's
`gpui` 0.2.2 (`src-gpui/vendor/gpui/PATCHES.md` there), keeping only what cuts CPU on a
software adapter (WARP). `crates/gpui_windows` is copied unchanged from gpui-fast
`598306f` and then patched; Cargo patches its git source like `gpui` and `gpui_linux`.

- `gpui/src/rendering_policy.rs` (new), `gpui.rs`: `set_low_power_rendering`,
  `set_software_rendering_effects`, and the readers the platform crate needs
  (`low_power_rendering`, `low_power_frame_interval`, hidden from docs).
- `gpui/src/window.rs`: on Windows the frame loop no longer re-presents unchanged scenes
  during high-rate input; in low-power mode a scheduled draw waits the previous frame's
  cost (capped at 100 ms) and the platform move callback reuses the scene unless size,
  scale or display changed (`moved`; public `bounds_changed` still refreshes). Blurred
  drop and inset shadows are skipped while software effects are on.
- `gpui_windows/src/vsync.rs`: low-power mode sleeps 33 ms instead of `DwmFlush`.
- `gpui_windows/src/software_rendering.rs` (new): on a software adapter, a solid quad
  batch uses `solid_quad_*` shaders (`shaders.hlsl`, shared math in `quad_shading.hlsl`,
  compiled by `build.rs`), and large opaque whole-pixel quads on the window target are
  filled with `ClearView`.
- `gpui_windows/src/path_targets.rs` (new): path batches rasterize into a texture sized
  to them, placed with an even whole-pixel viewport translation (`screen_origin` /
  `texture_origin` in the path shaders), and an unchanged batch reuses its resolved
  texture. `fast/frame.rs` and `fast/draw_state.rs` route quads and paths through these;
  `directx_renderer.rs` creates them and releases them on resize and device loss.
- Tests: `software_rendering_tests.rs` draws on WARP into a composition swap chain and
  compares each path with upstream's drawing (one 8-bit step allowed); unit tests cover
  quad eligibility, crop plans and vertex reuse; `rendering_policy` tests cover pacing.
- 2026-10-17: `rendering_policy.rs` is no longer Windows-only (Switchyard enables it on
  Linux CPU Vulkan drivers too), and low-power mode also skips the high-rate-input
  keepalive presents on other platforms. The Windows 30 Hz frame thread is unchanged.

## Windows frames on demand (gpui-fast #84 backport, 2026-10-24)

`crates/gpui_windows` carries longbridge/gpui-fast PR #84, "fast: schedule Windows frames
on demand", merged upstream as `124f79b0f14c688ae87877e370c015a9dc9d7ce5`, applied onto
the `598306f` copy. Before it, the VSync thread invalidated every window at the display
refresh rate while idle. Now each window tracks frame demand, and the thread parks when
none is pending; input, presents, deferred paints, active Direct Manipulation and device
removal wake it. The new files `fast/frame_schedule.rs`, `fast/device_wake.rs` and
`fast/native_tests.rs` (ignored tests that need an interactive desktop) are byte-identical
to `124f79b`. The other hunks applied with offsets, except `fast/mod.rs`: at `598306f` it
has no `class_names` or `rounded_clip`, so its three module lines were added by hand. The
backend uses `PlatformWindow::frame_waker` / `schedule_frame`, already in the `598306f`
core, so `gpui` is unchanged. The other gpui-fast commits between `598306f` and `124f79b`
are not taken. Drop this section when AgentOpsChat moves to a revision containing `124f79b`.

It composes with the Windows software-rendering patch above: in low-power mode a woken
VSync thread still sleeps 33 ms instead of `DwmFlush`, and a draw the frame pacer skips
leaves the window dirty, which `gpui/src/window.rs` re-arms through `schedule_frame`.
