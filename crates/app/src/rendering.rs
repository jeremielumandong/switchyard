//! Rendering policy for software GPUs and remote desktops (Windows).
//!
//! A VM without a GPU renders through WARP ("Microsoft Basic Render Driver"),
//! where every drawn pixel costs CPU. There Switchyard leaves out blurred
//! shadows and paces frames at 30 Hz; over Remote Desktop it paces frames too.
//! The renderer's own software paths (solid quad shader, native fills) follow
//! the adapter by themselves. `SWITCHYARD_LOW_POWER_RENDERING=1` (or `0`)
//! forces frame pacing on (or off). No effect on other platforms.

use gpui_kit::GpuSpecs;

/// Overrides the frame pacing decision: `0` off, anything else on.
pub const LOW_POWER_ENV: &str = "SWITCHYARD_LOW_POWER_RENDERING";

/// Sets the policy from the main window's GPU; call before its first frame.
pub fn init(specs: Option<&GpuSpecs>) {
    if !cfg!(target_os = "windows") {
        return;
    }
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

/// WARP reports the software flag; the device names cover drivers that don't.
fn software_renderer(specs: &GpuSpecs) -> bool {
    specs.is_software_emulated
        || [
            "Microsoft Basic Render Driver",
            "Microsoft Basic Display Adapter",
        ]
        .iter()
        .any(|name| specs.device_name.eq_ignore_ascii_case(name))
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
