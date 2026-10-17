//! Rendering policy for software GPUs and remote desktops.
//!
//! A VM without a GPU renders through WARP ("Microsoft Basic Render Driver")
//! on Windows or a CPU Vulkan driver (llvmpipe / lavapipe) on Linux, where
//! every drawn pixel costs CPU. There Switchyard leaves out blurred shadows and
//! paces frames (30 Hz on Windows); over Remote Desktop it paces frames too.
//! The Windows renderer's own software paths (solid quad shader, native fills)
//! follow the adapter by themselves. `SWITCHYARD_LOW_POWER_RENDERING=1` (or
//! `0`) forces frame pacing on (or off).

use gpui_kit::base::Root;
use gpui_kit::{AnyWindowHandle, App, GpuSpecs, Styled as _, transparent_black};

/// Overrides the frame pacing decision: `0` off, anything else on.
pub const LOW_POWER_ENV: &str = "SWITCHYARD_LOW_POWER_RENDERING";

/// Sets the policy from the main window's GPU; call before its first frame.
pub fn init(specs: Option<&GpuSpecs>) {
    let software = specs.is_some_and(software_renderer);
    let remote = remote_session();
    let low_power = low_power_policy(software, remote, std::env::var(LOW_POWER_ENV).ok());
    gpui_kit::set_software_rendering_effects(software);
    gpui_kit::set_low_power_rendering(low_power);
    tracing::info!(
        device = specs.map_or("unknown", |s| s.device_name.as_str()),
        software,
        remote,
        low_power,
        "rendering policy"
    );
}

/// Stops the window root from painting the theme background under the workspace.
///
/// gpui-component's root fills the whole window with the theme background, and the
/// workspace then covers all of it with its own: one window-sized fill per frame that
/// nobody sees. On a software renderer that is a third of a frame's pixels.
pub fn skip_root_background(window: AnyWindowHandle, cx: &mut App) {
    let Some(root) = window.downcast::<Root>() else {
        return;
    };
    let _ = root.update(cx, |root, _, cx| {
        root.style().background = Some(transparent_black().into());
        cx.notify();
    });
}

/// WARP and wgpu's CPU adapters report the software flag; the device names cover
/// drivers that don't.
fn software_renderer(specs: &GpuSpecs) -> bool {
    specs.is_software_emulated
        || [
            "Microsoft Basic Render Driver",
            "Microsoft Basic Display Adapter",
        ]
        .iter()
        .any(|name| specs.device_name.eq_ignore_ascii_case(name))
        || ["llvmpipe", "lavapipe", "softpipe", "swiftshader"]
            .iter()
            .any(|name| specs.device_name.to_ascii_lowercase().contains(name))
}

fn low_power_policy(software: bool, remote: bool, override_value: Option<String>) -> bool {
    match override_value {
        Some(value) => value.trim() != "0",
        None => software || remote,
    }
}

/// Windows names each logon session in `SESSIONNAME`: `Console` locally,
/// `RDP-Tcp#<n>` over Remote Desktop.
fn remote_session() -> bool {
    std::env::var("SESSIONNAME").is_ok_and(|name| is_rdp_session_name(&name))
}

fn is_rdp_session_name(name: &str) -> bool {
    name.get(..4)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("RDP-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs(name: &str, software: bool) -> GpuSpecs {
        GpuSpecs {
            is_software_emulated: software,
            device_name: name.into(),
            driver_name: String::new(),
            driver_info: String::new(),
        }
    }

    #[test]
    fn software_adapters_are_recognized_by_flag_or_name() {
        assert!(software_renderer(&specs("WARP", true)));
        assert!(software_renderer(&specs(
            "Microsoft Basic Render Driver",
            false
        )));
        assert!(!software_renderer(&specs("NVIDIA A10", false)));
        assert!(!software_renderer(&specs("Microsoft Hyper-V Video", false)));
        assert!(software_renderer(&specs(
            "llvmpipe (LLVM 17.0.6, 256 bits)",
            false
        )));
        assert!(!software_renderer(&specs(
            "AMD Radeon Graphics (RADV)",
            false
        )));
    }

    #[test]
    fn remote_desktop_sessions_are_recognized_by_name() {
        assert!(is_rdp_session_name("RDP-Tcp#12"));
        assert!(is_rdp_session_name("rdp-tcp#0"));
        assert!(!is_rdp_session_name("Console"));
        assert!(!is_rdp_session_name(""));
    }

    #[test]
    fn software_and_remote_sessions_pace_frames_unless_overridden() {
        assert!(low_power_policy(true, false, None));
        assert!(low_power_policy(false, true, None));
        assert!(!low_power_policy(false, false, None));
        assert!(!low_power_policy(true, true, Some("0".into())));
        assert!(low_power_policy(false, false, Some("1".into())));
    }
}
