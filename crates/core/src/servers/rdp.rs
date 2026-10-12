//! Remote desktop clients and how each is started (MX-12). Pure: from a
//! [`DesktopServer`], the resolved sign-in and the client found by the Driver Manager to
//! a [`LaunchPlan`] (program, arguments, files, what goes on stdin). Passwords never go
//! on a command line:
//!
//! - FreeRDP (`xfreerdp3`, `xfreerdp`): `/from-stdin:force`, the password on stdin.
//! - mstsc (Windows): a generated `.rdp` file with the user name; mstsc asks for the
//!   password itself (and offers to remember it in Credential Manager). `cmdkey /pass:`
//!   would put the password on a command line, so it is not used (`docs/DECISIONS.md`).
//! - Windows App / Microsoft Remote Desktop (macOS): a `.rdp` file opened with `open`;
//!   these apps take no password from outside, so the user types it (or the app's own
//!   keychain entry fills it).
//! - Remmina: opens the `.rdp` file or a `vnc://` URL; it asks for the password.
//! - TigerVNC `vncviewer`: the password in its `VNC_PASSWORD` environment variable (only
//!   the same user can read a process's environment, unlike its arguments).
//! - macOS Screen Sharing: `open vnc://host:port`; it asks for the password.

use std::path::{Path, PathBuf};

use secrecy::SecretString;
use switchyard_store::{DesktopProtocol, DesktopScreen, DesktopServer};

/// Driver Manager component of the RDP client.
pub const RDP_COMPONENT: &str = "rdp-client";
/// Driver Manager component of the VNC client.
pub const VNC_COMPONENT: &str = "vnc-client";

/// A client program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    /// FreeRDP (`xfreerdp3` and friends: `v3` syntax).
    FreeRdp {
        /// FreeRDP 3 option syntax.
        v3: bool,
    },
    /// Windows' Remote Desktop Connection.
    Mstsc,
    /// macOS Windows App / Microsoft Remote Desktop, through `open`.
    MacRdp,
    /// Remmina (RDP or VNC).
    Remmina,
    /// TigerVNC's `vncviewer` (or another `vncviewer` taking its options).
    VncViewer,
    /// macOS Screen Sharing through `open vnc://`.
    MacScreenSharing,
}

impl Client {
    /// Name for messages.
    pub fn name(self) -> &'static str {
        match self {
            Client::FreeRdp { .. } => "FreeRDP",
            Client::Mstsc => "Remote Desktop Connection",
            Client::MacRdp => "Windows App",
            Client::Remmina => "Remmina",
            Client::VncViewer => "TigerVNC",
            Client::MacScreenSharing => "Screen Sharing",
        }
    }

    /// Which client a detected program or app bundle is; `version` decides FreeRDP's
    /// syntax for an unversioned `xfreerdp`.
    pub fn from_location(location: &Path, version: Option<&str>) -> Option<Client> {
        let name = location.file_name()?.to_string_lossy().to_lowercase();
        let stem = name
            .strip_suffix(".exe")
            .or_else(|| name.strip_suffix(".app"))
            .unwrap_or(&name)
            .to_owned();
        let v3 = version
            .and_then(|v| v.split('.').next()?.parse::<u32>().ok())
            .is_some_and(|m| m >= 3);
        Some(match stem.as_str() {
            "xfreerdp3" | "wlfreerdp3" | "sdl-freerdp3" | "sdl-freerdp" => {
                Client::FreeRdp { v3: true }
            }
            "xfreerdp" | "wlfreerdp" => Client::FreeRdp { v3 },
            "mstsc" => Client::Mstsc,
            "windows app" | "microsoft remote desktop" => Client::MacRdp,
            "remmina" => Client::Remmina,
            "vncviewer" | "tigervnc" | "xtigervncviewer" => Client::VncViewer,
            "screen sharing" => Client::MacScreenSharing,
            _ => return None,
        })
    }

    /// Whether the client opens `protocol`.
    pub fn speaks(self, protocol: DesktopProtocol) -> bool {
        match protocol {
            DesktopProtocol::Rdp => matches!(
                self,
                Client::FreeRdp { .. } | Client::Mstsc | Client::MacRdp | Client::Remmina
            ),
            DesktopProtocol::Vnc => matches!(
                self,
                Client::VncViewer | Client::Remmina | Client::MacScreenSharing
            ),
        }
    }
}

/// The sign-in for one launch (the server's own, or a credential's).
#[derive(Clone, Debug, Default)]
pub struct Login {
    /// User (RDP); empty lets the client ask.
    pub user: String,
    /// Domain.
    pub domain: Option<String>,
    /// Password, if known.
    pub password: Option<SecretString>,
}

/// The RD Gateway sign-in.
#[derive(Clone, Debug, Default)]
pub struct GatewayLogin {
    /// Gateway host.
    pub host: String,
    /// Its own sign-in; `None` uses the server's.
    pub login: Option<Login>,
}

/// Where the client connects (the server, or `127.0.0.1:<port>` of a tunnel).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// Host.
    pub host: String,
    /// Port.
    pub port: u16,
}

impl Endpoint {
    /// `host:port` (IPv6 in brackets).
    pub fn host_port(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// How to start a client.
#[derive(Clone, Debug)]
pub struct LaunchPlan {
    /// The client.
    pub client: Client,
    /// Program.
    pub program: PathBuf,
    /// Arguments (never a secret).
    pub args: Vec<String>,
    /// Files to write into the private temp folder first: (name, content).
    pub files: Vec<(String, String)>,
    /// Lines written to the client's stdin, then stdin is closed.
    pub stdin: Vec<SecretString>,
    /// Secret environment variables of the client.
    pub env: Vec<(String, SecretString)>,
    /// The program hands the session to another app and exits at once (`open`): files
    /// are kept a little while for the app to read.
    pub detached: bool,
}

fn qualified(user: &str, domain: Option<&str>) -> String {
    match domain.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) if !user.contains('\\') && !user.contains('@') => format!("{d}\\{user}"),
        _ => user.to_owned(),
    }
}

/// The `.rdp` file for mstsc, the macOS apps and Remmina. Holds no password: the client
/// asks for it (`prompt for credentials:i:1`).
pub fn rdp_file(
    server: &DesktopServer,
    at: &Endpoint,
    login: &Login,
    gateway: Option<&GatewayLogin>,
) -> String {
    let mut l: Vec<String> = vec![format!("full address:s:{}", at.host_port())];
    let user = login.user.trim();
    if !user.is_empty() {
        l.push(format!(
            "username:s:{}",
            qualified(user, login.domain.as_deref())
        ));
    }
    match server.display.screen {
        DesktopScreen::FullScreen => l.push("screen mode id:i:2".into()),
        DesktopScreen::Window { width, height } => {
            l.push("screen mode id:i:1".into());
            l.push(format!("desktopwidth:i:{width}"));
            l.push(format!("desktopheight:i:{height}"));
        }
    }
    l.push(format!(
        "use multimon:i:{}",
        u8::from(server.display.multi_monitor)
    ));
    l.push(format!(
        "redirectclipboard:i:{}",
        u8::from(server.display.clipboard)
    ));
    if server.display.drives {
        l.push("drivestoredirect:s:*".into());
    }
    if server.admin_session {
        l.push("administrative session:i:1".into());
    }
    l.push("authentication level:i:2".into());
    l.push("prompt for credentials:i:1".into());
    if let Some(g) = gateway {
        l.push(format!("gatewayhostname:s:{}", g.host.trim()));
        l.push("gatewayusagemethod:i:1".into());
        l.push("gatewayprofileusagemethod:i:1".into());
        l.push("gatewaycredentialssource:i:0".into());
        l.push(format!(
            "promptcredentialonce:i:{}",
            u8::from(g.login.is_none())
        ));
        if let Some(gl) = &g.login
            && !gl.user.trim().is_empty()
        {
            l.push(format!(
                "gatewayusername:s:{}",
                qualified(gl.user.trim(), gl.domain.as_deref())
            ));
        }
    }
    let mut out = l.join("\r\n");
    out.push_str("\r\n");
    out
}

/// FreeRDP arguments. The password is read from stdin (`/from-stdin:force`); a changed
/// server certificate is refused (`/cert:tofu` accepts only the first one).
pub fn freerdp_args(
    server: &DesktopServer,
    at: &Endpoint,
    login: &Login,
    gateway: Option<&GatewayLogin>,
    v3: bool,
) -> Vec<String> {
    let mut a = vec![format!("/v:{}", at.host_port())];
    let user = login.user.trim();
    if !user.is_empty() {
        a.push(format!("/u:{user}"));
    }
    if let Some(d) = login
        .domain
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        a.push(format!("/d:{d}"));
    }
    a.push("/from-stdin:force".into());
    a.push("/cert:tofu".into());
    match server.display.screen {
        DesktopScreen::FullScreen => a.push("/f".into()),
        DesktopScreen::Window { width, height } => a.push(format!("/size:{width}x{height}")),
    }
    if server.display.multi_monitor {
        a.push("/multimon".into());
    }
    a.push(
        match (server.display.clipboard, v3) {
            (true, true) => "/clipboard",
            (true, false) => "+clipboard",
            (false, _) => "-clipboard",
        }
        .into(),
    );
    if server.display.drives {
        a.push(if v3 { "/drives" } else { "+drives" }.into());
    }
    if server.admin_session {
        a.push("/admin".into());
    }
    if let Some(g) = gateway {
        let (gu, gd) = match &g.login {
            Some(gl) => (gl.user.trim().to_owned(), gl.domain.clone()),
            None => (user.to_owned(), login.domain.clone()),
        };
        let gd = gd.map(|d| d.trim().to_owned()).filter(|d| !d.is_empty());
        if v3 {
            let mut s = format!("/gateway:g:{}", g.host.trim());
            if !gu.is_empty() {
                s.push_str(&format!(",u:{gu}"));
            }
            if let Some(d) = gd {
                s.push_str(&format!(",d:{d}"));
            }
            a.push(s);
        } else {
            a.push(format!("/g:{}", g.host.trim()));
            if !gu.is_empty() {
                a.push(format!("/gu:{gu}"));
            }
            if let Some(d) = gd {
                a.push(format!("/gd:{d}"));
            }
        }
    }
    a
}

/// What FreeRDP reads on stdin: the server password, then the gateway's (its own, or
/// the server's again).
fn freerdp_stdin(login: &Login, gateway: Option<&GatewayLogin>) -> Vec<SecretString> {
    let mut out: Vec<SecretString> = login.password.iter().cloned().collect();
    if let Some(g) = gateway {
        let pw = match &g.login {
            Some(gl) => gl.password.clone(),
            None => login.password.clone(),
        };
        out.extend(pw);
    }
    out
}

/// The plan for `server` with `client` at `program`. `dir` is the private temp folder
/// files go into.
pub fn plan(
    server: &DesktopServer,
    client: Client,
    program: &Path,
    at: &Endpoint,
    login: &Login,
    gateway: Option<&GatewayLogin>,
    dir: &Path,
) -> LaunchPlan {
    let mut p = LaunchPlan {
        client,
        program: program.to_owned(),
        args: Vec::new(),
        files: Vec::new(),
        stdin: Vec::new(),
        env: Vec::new(),
        detached: false,
    };
    let rdp_name = "session.rdp".to_owned();
    let rdp_path = dir.join(&rdp_name).display().to_string();
    match client {
        Client::FreeRdp { v3 } => {
            p.args = freerdp_args(server, at, login, gateway, v3);
            p.stdin = freerdp_stdin(login, gateway);
        }
        Client::Mstsc => {
            p.files
                .push((rdp_name, rdp_file(server, at, login, gateway)));
            p.args.push(rdp_path);
            if server.admin_session {
                p.args.push("/admin".into());
            }
        }
        Client::MacRdp => {
            p.files
                .push((rdp_name, rdp_file(server, at, login, gateway)));
            p.args = vec!["-a".into(), program.display().to_string(), rdp_path];
            p.program = PathBuf::from("/usr/bin/open");
            p.detached = true;
        }
        Client::Remmina => match server.protocol {
            DesktopProtocol::Rdp => {
                p.files
                    .push((rdp_name, rdp_file(server, at, login, gateway)));
                p.args = vec!["-c".into(), rdp_path];
            }
            DesktopProtocol::Vnc => {
                p.args = vec!["-c".into(), format!("vnc://{}", at.host_port())];
            }
        },
        Client::VncViewer => {
            if let DesktopScreen::FullScreen = server.display.screen {
                p.args.push("-FullScreen".into());
            }
            let clip = u8::from(server.display.clipboard);
            p.args.push(format!("-AcceptClipboard={clip}"));
            p.args.push(format!("-SendClipboard={clip}"));
            p.args.push(format!("{}::{}", at.host, at.port));
            if let Some(pw) = &login.password {
                p.env.push(("VNC_PASSWORD".into(), pw.clone()));
            }
        }
        Client::MacScreenSharing => {
            p.program = PathBuf::from("/usr/bin/open");
            p.args = vec![format!("vnc://{}", at.host_port())];
            p.detached = true;
        }
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret as _;
    use switchyard_store::{DesktopDisplay, RdpGateway};

    fn server() -> DesktopServer {
        let mut s = DesktopServer::new("dc-1", DesktopProtocol::Rdp);
        s.address = "dc1.corp.example".into();
        s
    }

    fn login() -> Login {
        Login {
            user: "admin".into(),
            domain: Some("CORP".into()),
            password: Some(SecretString::from("S3cret!pw")),
        }
    }

    fn at() -> Endpoint {
        Endpoint {
            host: "dc1.corp.example".into(),
            port: 3389,
        }
    }

    #[test]
    fn rdp_file_full_screen() {
        insta::assert_snapshot!(rdp_file(&server(), &at(), &login(), None).replace("\r\n", "\n"));
    }

    #[test]
    fn rdp_file_window_gateway_admin_through_a_tunnel() {
        let mut s = server();
        s.display = DesktopDisplay {
            screen: DesktopScreen::Window {
                width: 1600,
                height: 900,
            },
            multi_monitor: true,
            clipboard: false,
            drives: true,
        };
        s.admin_session = true;
        s.gateway = Some(RdpGateway {
            host: "gw.corp.example".into(),
            user: String::new(),
            credential: None,
        });
        let gw = GatewayLogin {
            host: "gw.corp.example".into(),
            login: Some(Login {
                user: "gwuser".into(),
                domain: Some("CORP".into()),
                password: Some(SecretString::from("gw-pass")),
            }),
        };
        let tunnel = Endpoint {
            host: "127.0.0.1".into(),
            port: 50123,
        };
        let text = rdp_file(&s, &tunnel, &login(), Some(&gw));
        assert!(!text.contains("S3cret") && !text.contains("gw-pass"));
        assert!(text.ends_with("\r\n"));
        insta::assert_snapshot!(text.replace("\r\n", "\n"));
    }

    #[test]
    fn freerdp_argv_never_holds_a_password() {
        let mut s = server();
        s.gateway = Some(RdpGateway {
            host: "gw".into(),
            ..Default::default()
        });
        let gw = GatewayLogin {
            host: "gw".into(),
            login: Some(Login {
                user: "g".into(),
                domain: None,
                password: Some(SecretString::from("gw-pass")),
            }),
        };
        for v3 in [true, false] {
            let p = plan(
                &s,
                Client::FreeRdp { v3 },
                Path::new("/usr/bin/xfreerdp"),
                &at(),
                &login(),
                Some(&gw),
                Path::new("/tmp/x"),
            );
            let argv = p.args.join(" ");
            assert!(
                !argv.contains("S3cret") && !argv.contains("gw-pass"),
                "{argv}"
            );
            assert!(
                !p.args
                    .iter()
                    .any(|a| a.starts_with("/p:") || a.starts_with("/gp:"))
            );
            assert!(argv.contains("/from-stdin:force"));
            assert!(argv.contains("/u:admin") && argv.contains("/d:CORP"));
            let stdin: Vec<&str> = p.stdin.iter().map(|s| s.expose_secret()).collect();
            assert_eq!(stdin, ["S3cret!pw", "gw-pass"]);
        }
        let p = plan(
            &server(),
            Client::FreeRdp { v3: true },
            Path::new("/usr/bin/xfreerdp3"),
            &at(),
            &login(),
            None,
            Path::new("/tmp/x"),
        );
        assert_eq!(
            p.args,
            [
                "/v:dc1.corp.example:3389",
                "/u:admin",
                "/d:CORP",
                "/from-stdin:force",
                "/cert:tofu",
                "/f",
                "/clipboard"
            ]
        );
    }

    #[test]
    fn mstsc_vnc_and_mac_plans() {
        let p = plan(
            &server(),
            Client::Mstsc,
            Path::new("C:/Windows/System32/mstsc.exe"),
            &at(),
            &login(),
            None,
            Path::new("/tmp/x"),
        );
        assert!(p.args.iter().all(|a| !a.contains("S3cret")));
        assert!(p.stdin.is_empty() && p.env.is_empty());
        assert_eq!(p.files[0].0, "session.rdp");
        assert!(p.files[0].1.contains("username:s:CORP\\admin"));
        assert!(!p.files[0].1.contains("S3cret"));

        let mut vnc = DesktopServer::new("lab", DesktopProtocol::Vnc);
        vnc.address = "lab".into();
        let l = Login {
            password: Some(SecretString::from("vncpw")),
            ..Login::default()
        };
        let v = plan(
            &vnc,
            Client::VncViewer,
            Path::new("/usr/bin/vncviewer"),
            &Endpoint {
                host: "127.0.0.1".into(),
                port: 5901,
            },
            &l,
            None,
            Path::new("/tmp/x"),
        );
        assert_eq!(v.args.last().map(String::as_str), Some("127.0.0.1::5901"));
        assert!(v.args.iter().all(|a| !a.contains("vncpw")));
        assert_eq!(v.env[0].0, "VNC_PASSWORD");
        let m = plan(
            &vnc,
            Client::MacScreenSharing,
            Path::new("/System/Applications/Utilities/Screen Sharing.app"),
            &at(),
            &l,
            None,
            Path::new("/tmp/x"),
        );
        assert!(m.detached);
        assert_eq!(m.args, ["vnc://dc1.corp.example:3389"]);
    }

    #[test]
    fn clients_from_detected_paths() {
        let c = |p: &str, v: Option<&str>| Client::from_location(Path::new(p), v);
        assert_eq!(
            c("/usr/bin/xfreerdp3", None),
            Some(Client::FreeRdp { v3: true })
        );
        assert_eq!(
            c("/usr/bin/xfreerdp", Some("2.11.5")),
            Some(Client::FreeRdp { v3: false })
        );
        assert_eq!(
            c("/usr/bin/xfreerdp", Some("3.10.0")),
            Some(Client::FreeRdp { v3: true })
        );
        assert_eq!(
            c("C:/Windows/System32/mstsc.exe", None),
            Some(Client::Mstsc)
        );
        assert_eq!(
            c("/Applications/Windows App.app", None),
            Some(Client::MacRdp)
        );
        assert_eq!(c("/usr/bin/vncviewer", None), Some(Client::VncViewer));
        assert_eq!(c("/usr/bin/unknown", None), None);
        assert!(Client::Remmina.speaks(DesktopProtocol::Vnc));
        assert!(!Client::Mstsc.speaks(DesktopProtocol::Vnc));
    }
}
