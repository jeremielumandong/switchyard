//! Questions from the runtime: unknown SSH host keys, passwords and passphrases,
//! keyboard-interactive (MFA) prompts, and Microsoft Entra sign-ins. They queue above every
//! other overlay.

use gpui_kit::component::input::{Input, InputState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, Focusable as _, FontWeight,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use secrecy::SecretString;
use switchyard_core::remote::ssh::{HostKeyDecision, HostKeyRequest, InteractiveRequest};
use switchyard_core::{Command, PromptAnswer, RequestId};

use crate::theme::{MONO, Palette};
use crate::ui::{self, Kind};
use crate::workspace::{Tab, Workspace};

/// One pending question.
pub enum SshPrompt {
    /// Trust an unknown host key?
    HostKey {
        /// Answer id.
        request: RequestId,
        /// The key.
        key: HostKeyRequest,
    },
    /// Password or passphrase.
    Secret {
        /// Answer id.
        request: RequestId,
        /// Host label.
        host: String,
        /// Prompt.
        prompt: String,
        /// Masked input.
        input: Entity<InputState>,
    },
    /// Keyboard-interactive round.
    Interactive {
        /// Answer id.
        request: RequestId,
        /// The questions.
        req: InteractiveRequest,
        /// One input per prompt.
        inputs: Vec<Entity<InputState>>,
    },
    /// Microsoft Entra sign-in in progress (browser or device code).
    Entra {
        /// Request id (answer [`PromptAnswer::Cancel`] to give up).
        request: RequestId,
        /// Connection name.
        connection: String,
        /// Sign-in page (browser) or where to enter the code.
        url: String,
        /// Device code and Microsoft's instructions, for device-code sign-in.
        device: Option<(String, String)>,
    },
}

impl SshPrompt {
    fn request(&self) -> RequestId {
        match self {
            SshPrompt::HostKey { request, .. }
            | SshPrompt::Secret { request, .. }
            | SshPrompt::Interactive { request, .. }
            | SshPrompt::Entra { request, .. } => *request,
        }
    }
}

fn masked(window: &mut Window, cx: &mut Context<Workspace>, masked: bool) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).masked(masked))
}

impl Workspace {
    /// Queue a host key question.
    pub(crate) fn push_host_key_prompt(
        &mut self,
        request: RequestId,
        key: HostKeyRequest,
        cx: &mut Context<Self>,
    ) {
        self.prompts.push_back(SshPrompt::HostKey { request, key });
        cx.notify();
    }

    /// Queue a password / passphrase question.
    pub(crate) fn push_secret_prompt(
        &mut self,
        request: RequestId,
        host: String,
        prompt: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = masked(window, cx, true);
        if self.prompts.is_empty() {
            input.update(cx, |i, cx| i.focus(window, cx));
        }
        self.prompts.push_back(SshPrompt::Secret {
            request,
            host,
            prompt,
            input,
        });
        cx.notify();
    }

    /// Queue keyboard-interactive questions.
    pub(crate) fn push_interactive_prompt(
        &mut self,
        request: RequestId,
        req: InteractiveRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let inputs: Vec<_> = req
            .prompts
            .iter()
            .map(|(_, echo)| masked(window, cx, !echo))
            .collect();
        if self.prompts.is_empty()
            && let Some(first) = inputs.first()
        {
            first.update(cx, |i, cx| i.focus(window, cx));
        }
        self.prompts.push_back(SshPrompt::Interactive {
            request,
            req,
            inputs,
        });
        cx.notify();
    }

    /// Queue a Microsoft Entra sign-in. The browser opens right away for interactive
    /// sign-in; device-code sign-in shows the code first.
    pub(crate) fn push_entra_prompt(
        &mut self,
        request: RequestId,
        connection: String,
        url: String,
        device: Option<(String, String)>,
        cx: &mut Context<Self>,
    ) {
        if device.is_none() {
            cx.open_url(&url);
        }
        self.prompts.push_back(SshPrompt::Entra {
            request,
            connection,
            url,
            device,
        });
        cx.notify();
    }

    /// The runtime no longer needs an answer (sign-in finished or failed).
    pub(crate) fn close_prompt(
        &mut self,
        request: RequestId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(i) = self.prompts.iter().position(|p| p.request() == request) else {
            return;
        };
        if i == 0 {
            self.prompts.pop_front();
            self.focus_next_prompt(window, cx);
        } else {
            self.prompts.remove(i);
        }
        cx.notify();
    }

    fn finish_prompt(&mut self, answer: PromptAnswer, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(p) = self.prompts.pop_front() {
            self.core.send(Command::AnswerPrompt {
                request: p.request(),
                answer,
            });
        }
        self.focus_next_prompt(window, cx);
        cx.notify();
    }

    fn focus_next_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Focus the next question's first field.
        match self.prompts.front() {
            Some(SshPrompt::Secret { input, .. }) => {
                input.update(cx, |i, cx| i.focus(window, cx));
            }
            Some(SshPrompt::Interactive { inputs, .. }) => {
                if let Some(i) = inputs.first() {
                    i.update(cx, |i, cx| i.focus(window, cx));
                }
            }
            Some(SshPrompt::HostKey { .. } | SshPrompt::Entra { .. }) => {}
            // Last question answered: typing goes back to the active terminal.
            None => {
                if let Some(Tab::Terminal(t)) = self.tabs.get(self.active) {
                    let handle = t.read(cx).focus_handle(cx);
                    window.focus(&handle, cx);
                }
            }
        }
    }

    /// Cancel the front question (Escape).
    pub(crate) fn cancel_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let answer = match self.prompts.front() {
            None => return false,
            Some(SshPrompt::HostKey { .. }) => PromptAnswer::HostKey(HostKeyDecision::Reject),
            Some(SshPrompt::Secret { .. }) => PromptAnswer::Secret(None),
            Some(SshPrompt::Interactive { .. }) => PromptAnswer::Interactive(None),
            Some(SshPrompt::Entra { .. }) => PromptAnswer::Cancel,
        };
        self.finish_prompt(answer, window, cx);
        true
    }

    fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = match self.prompts.front() {
            Some(SshPrompt::Secret { input, .. }) => {
                PromptAnswer::Secret(Some(SecretString::from(input.read(cx).value().to_string())))
            }
            Some(SshPrompt::Interactive { inputs, .. }) => PromptAnswer::Interactive(Some(
                inputs
                    .iter()
                    .map(|i| SecretString::from(i.read(cx).value().to_string()))
                    .collect(),
            )),
            _ => return,
        };
        self.finish_prompt(answer, window, cx);
    }

    /// The front question, if any.
    pub(crate) fn render_ssh_prompt(
        &self,
        p: &Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let front = self.prompts.front()?;
        let more = self.prompts.len() - 1;
        let body: AnyElement = match front {
            SshPrompt::HostKey { key, .. } => div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .mb(px(4.))
                        .child(format!("Unknown host key for {}", key.host)),
                )
                .child(
                    div()
                        .text_color(p.fg2)
                        .text_size(px(12.5))
                        .mb(px(12.))
                        .child(format!(
                            "This is the first connection to {}. Compare the fingerprint with the one your admin published before trusting it.",
                            key.address
                        )),
                )
                .child(
                    div()
                        .font_family(MONO)
                        .text_size(px(12.))
                        .line_height(px(19.))
                        .bg(p.bg)
                        .border_1()
                        .border_color(p.bd)
                        .rounded(px(6.))
                        .px(px(10.))
                        .py(px(8.))
                        .mb(px(14.))
                        .child(key.algorithm.clone())
                        .child(key.fingerprint.clone()),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .justify_end()
                        .child(ui::button("hk-cancel", "Cancel", Kind::Ghost, p).on_click(
                            cx.listener(|this, _, w, cx| {
                                this.finish_prompt(
                                    PromptAnswer::HostKey(HostKeyDecision::Reject),
                                    w,
                                    cx,
                                )
                            }),
                        ))
                        .child(ui::button("hk-once", "Trust once", Kind::Secondary, p).on_click(
                            cx.listener(|this, _, w, cx| {
                                this.finish_prompt(
                                    PromptAnswer::HostKey(HostKeyDecision::TrustOnce),
                                    w,
                                    cx,
                                )
                            }),
                        ))
                        .child(
                            ui::button("hk-save", "Trust and connect", Kind::Primary, p).on_click(
                                cx.listener(|this, _, w, cx| {
                                    this.finish_prompt(
                                        PromptAnswer::HostKey(HostKeyDecision::TrustAndSave),
                                        w,
                                        cx,
                                    )
                                }),
                            ),
                        ),
                )
                .into_any_element(),
            SshPrompt::Secret {
                host,
                prompt,
                input,
                ..
            } => div()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .mb(px(4.))
                        .child(format!("Sign in to {host}")),
                )
                .child(
                    div()
                        .text_color(p.fg2)
                        .text_size(px(12.5))
                        .mb(px(10.))
                        .child(prompt.clone()),
                )
                .child(field(input, p))
                .child(
                    div()
                        .text_color(p.fg3)
                        .text_size(px(11.))
                        .mt(px(6.))
                        .mb(px(14.))
                        .child("Used for this connection only. Save it in the Host's settings to keep it in the keychain."),
                )
                .child(buttons(p, cx))
                .into_any_element(),
            SshPrompt::Interactive { req, inputs, .. } => {
                let title = if req.name.trim().is_empty() {
                    format!("{} asks for verification", req.host)
                } else {
                    req.name.clone()
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .when(!req.instructions.trim().is_empty(), |d| {
                        d.child(
                            div()
                                .text_color(p.fg2)
                                .text_size(px(12.5))
                                .child(req.instructions.clone()),
                        )
                    })
                    .children(req.prompts.iter().zip(inputs).map(|((label, _), input)| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(5.))
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(p.fg2)
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(label.trim().to_owned()),
                            )
                            .child(field(input, p))
                    }))
                    .child(div().h(px(6.)))
                    .child(buttons(p, cx))
                    .into_any_element()
            }
            SshPrompt::Entra {
                connection,
                url,
                device,
                ..
            } => entra_body(connection, url, device.as_ref(), p, cx),
        };
        Some(
            div()
                .id("ssh-prompt-scrim")
                .absolute()
                .inset_0()
                .bg(p.scrim)
                .flex()
                .items_center()
                .justify_center()
                .occlude()
                .key_context("SshPrompt")
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, w, cx| {
                    match ev.keystroke.key.as_str() {
                        "escape" => {
                            this.cancel_prompt(w, cx);
                            cx.stop_propagation();
                        }
                        "enter" => {
                            this.submit_prompt(w, cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
                }))
                .child(
                    div()
                        .id("ssh-prompt")
                        .w(px(480.))
                        .bg(p.elev)
                        .rounded(px(10.))
                        .shadow(ui::shadow(p))
                        .px(px(20.))
                        .py(px(18.))
                        .text_size(px(12.5))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(body)
                        .when(more > 0, |d| {
                            d.child(
                                div()
                                    .mt(px(10.))
                                    .text_size(px(11.))
                                    .text_color(p.fg3)
                                    .child(format!("{more} more waiting")),
                            )
                        }),
                )
                .into_any_element(),
        )
    }
}

fn field(input: &Entity<InputState>, p: &Palette) -> impl IntoElement {
    div()
        .h(px(28.))
        .flex()
        .items_center()
        .px(px(9.))
        .border_1()
        .border_color(p.bd2)
        .rounded(px(6.))
        .bg(p.bg)
        .child(Input::new(input).appearance(false).text_size(px(12.5)))
}

fn buttons(p: &Palette, cx: &mut Context<Workspace>) -> impl IntoElement {
    div()
        .flex()
        .gap(px(6.))
        .justify_end()
        .child(
            ui::button("sp-cancel", "Cancel", Kind::Ghost, p).on_click(cx.listener(
                |this, _, w, cx| {
                    this.cancel_prompt(w, cx);
                },
            )),
        )
        .child(
            ui::button("sp-ok", "Connect", Kind::Primary, p)
                .on_click(cx.listener(|this, _, w, cx| this.submit_prompt(w, cx))),
        )
}

fn entra_body(
    connection: &str,
    url: &str,
    device: Option<&(String, String)>,
    p: &Palette,
    cx: &mut Context<Workspace>,
) -> AnyElement {
    let title = div()
        .text_size(px(14.))
        .font_weight(FontWeight::SEMIBOLD)
        .mb(px(4.))
        .child(format!("Sign in to Microsoft for {connection}"));
    let note = |text: String| {
        div()
            .text_color(p.fg2)
            .text_size(px(12.5))
            .mb(px(12.))
            .child(text)
    };
    let cancel = ui::button("entra-cancel", "Cancel", Kind::Ghost, p).on_click(cx.listener(
        |this, _, w, cx| {
            this.cancel_prompt(w, cx);
        },
    ));
    let open_url = url.to_owned();
    match device {
        Some((code, message)) => {
            let copy = code.clone();
            div()
                .flex()
                .flex_col()
                .child(title)
                .child(note(message.clone()))
                .child(
                    div()
                        .font_family(MONO)
                        .text_size(px(22.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_center()
                        .bg(p.bg)
                        .border_1()
                        .border_color(p.bd)
                        .rounded(px(6.))
                        .py(px(10.))
                        .mb(px(14.))
                        .child(code.clone()),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .justify_end()
                        .child(cancel)
                        .child(
                            ui::button("entra-copy", "Copy code", Kind::Secondary, p).on_click(
                                move |_, _, cx| {
                                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                        copy.clone(),
                                    ))
                                },
                            ),
                        )
                        .child(
                            ui::button("entra-open", "Open sign-in page", Kind::Primary, p)
                                .on_click(move |_, _, cx| cx.open_url(&open_url)),
                        ),
                )
                .into_any_element()
        }
        None => {
            let copy = url.to_owned();
            div()
                .flex()
                .flex_col()
                .child(title)
                .child(note(
                    "Finish signing in in your browser, including any MFA step. Switchyard \
                     connects as soon as Microsoft confirms."
                        .into(),
                ))
                .child(
                    div()
                        .text_color(p.fg3)
                        .text_size(px(11.))
                        .mb(px(14.))
                        .child("No browser window? Open it again, or copy the link into a browser on this computer."),
                )
                .child(
                    div()
                        .flex()
                        .gap(px(6.))
                        .justify_end()
                        .child(cancel)
                        .child(
                            ui::button("entra-copy", "Copy link", Kind::Secondary, p).on_click(
                                move |_, _, cx| {
                                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                                        copy.clone(),
                                    ))
                                },
                            ),
                        )
                        .child(
                            ui::button("entra-open", "Open browser again", Kind::Primary, p)
                                .on_click(move |_, _, cx| cx.open_url(&open_url)),
                        ),
                )
                .into_any_element()
        }
    }
}
