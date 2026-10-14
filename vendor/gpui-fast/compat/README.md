# GPUI Kit 0.7.3 compatibility facades

These Apache-2.0 re-export crates originate in Longbridge GPUI Fast commit
598306fcbf77bff0f764b5a178a447344e72e132 (`compat/*`), whose existing
0.3.8 facades were already used by this application. The local facades now
advertise the 0.3.9 package versions required by Kit/Base/Component 0.7.3.
They retain the exact same native core, macros, platform, web and sum-tree
implementations. This is an application-maintained API compatibility layer,
not a copy of the published gpui-pre 0.3.9 implementation.

The application has substantial patches to this native core (keyboard dispatch,
accessibility, retained rendering, Linux integration). Switching to either the
unpatched published Fast runtime or Zed's pre runtime would remove them. The
Kit 0.7.3 source and new Diff component compile against this shared Fast core.
Native input, selection, editor and review tests validate the compatibility.
Cross-platform testing is separate from the Linux validation performed here.

Each facade's `lib.rs` only re-exports its pinned implementation. No UI or
editing behavior is implemented in these facades. The native GPUI and Linux
packages still resolve to this application's vendored patches; other Fast
packages remain pinned to the original revision.
