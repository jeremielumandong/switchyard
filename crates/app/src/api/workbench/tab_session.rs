//! Per-project request-tab sessions: which saved requests were open, in what
//! order, and which was active. The Workbench store keeps one per workspace;
//! a project opens with its last session, or with no tabs.
//!
//! Only tabs bound to a saved request are remembered (ids, never contents);
//! unsaved drafts are left out. Tabs for requests that were deleted or moved
//! to another project are dropped quietly on restore.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use switchyard_api::{RequestId, TabSession, WorkspaceId};

use gpui_kit::{Context, Window};

use super::WorkbenchPanel;

/// The latest write sequence number started per workspace, shared by the
/// blocking writes so an older one never lands after a newer one.
pub(super) type WriteOrder = Arc<Mutex<HashMap<WorkspaceId, u64>>>;

/// The position in `kept` (ascending tab positions) nearest to `target`;
/// a tie goes to the earlier tab, as closing a tab does.
fn nearest(kept: &[usize], target: usize) -> Option<usize> {
    kept.iter()
        .enumerate()
        .min_by_key(|(_, position)| (position.abs_diff(target), **position > target))
        .map(|(index, _)| index)
}

/// The session for tabs bound to `tabs` (each tab's saved request, `None` for
/// a draft), with `active` the active tab's position. Drafts are left out;
/// when the active tab is one, the nearest saved tab becomes active.
pub(super) fn session_of(tabs: &[Option<RequestId>], active: Option<usize>) -> TabSession {
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    let mut requests = Vec::new();
    for (position, id) in tabs.iter().enumerate() {
        if let Some(id) = id
            && seen.insert(id)
        {
            kept.push(position);
            requests.push(id.clone());
        }
    }
    let active = active
        .and_then(|active| nearest(&kept, active))
        .map(|index| requests[index].clone());
    TabSession { requests, active }
}

/// The tabs to reopen from `session`: the remembered requests that still
/// `exist` in the project, in order, and the position of the one to activate
/// (the remembered active tab, else the nearest survivor; `None` with no tabs).
pub(super) fn restore_order(
    session: &TabSession,
    exists: impl Fn(&RequestId) -> bool,
) -> (Vec<RequestId>, Option<usize>) {
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    let mut requests = Vec::new();
    let mut active_position = None;
    for (position, id) in session.requests.iter().enumerate() {
        if !seen.insert(id) {
            continue;
        }
        if session.active.as_ref() == Some(id) {
            active_position = Some(position);
        }
        if exists(id) {
            kept.push(position);
            requests.push(id.clone());
        }
    }
    let active = match active_position {
        Some(position) => nearest(&kept, position),
        None if requests.is_empty() => None,
        None => Some(0),
    };
    (requests, active)
}

impl WorkbenchPanel {
    /// The open tabs as a session; the active tab's request lives on the
    /// panel, not in its captured tab state.
    fn current_tab_session(&self) -> TabSession {
        let tabs = self
            .request_tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                if index == self.active_request_tab {
                    self.current_request_id.clone()
                } else {
                    tab.current_request_id.clone()
                }
            })
            .collect::<Vec<_>>();
        let active = (!tabs.is_empty()).then_some(self.active_request_tab);
        session_of(&tabs, active)
    }

    /// Reopen `session`'s tabs in the freshly hydrated project and remember
    /// it as the stored one, so only real changes are written back.
    pub(super) fn restore_tab_session(
        &mut self,
        session: TabSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(data) = self.workspace_data.as_ref() else {
            return;
        };
        let workspace = data.workspace.clone();
        let (ids, active) = restore_order(&session, |id| {
            data.requests.iter().any(|request| request.id == *id)
        });
        let requests = ids
            .iter()
            .filter_map(|id| data.requests.iter().find(|request| request.id == *id))
            .cloned()
            .collect::<Vec<_>>();
        for request in &requests {
            self.load_saved_request(request, window, cx);
        }
        if let Some(tab) = active.and_then(|index| self.request_tabs.get(index)) {
            let id = tab.id;
            self.activate_request_tab(id, window, cx);
        }
        self.persisted_tab_session = Some((workspace, session));
    }

    /// Write the open tabs when they differ from the stored session. Called
    /// every frame; cheap when nothing changed. Waits until the bound
    /// project's session has been restored, so a load never overwrites it.
    pub(super) fn sync_tab_session(&mut self, cx: &mut Context<Self>) {
        if self.storage_loading {
            return;
        }
        let Some(data) = self.workspace_data.as_ref() else {
            return;
        };
        if data.workspace != self.bound_workspace {
            return;
        }
        let Some((workspace, stored)) = self.persisted_tab_session.as_ref() else {
            return;
        };
        if *workspace != data.workspace {
            return;
        }
        let session = self.current_tab_session();
        if session == *stored {
            return;
        }
        let store = data.store.clone();
        let workspace = data.workspace.clone();
        self.persisted_tab_session = Some((workspace.clone(), session.clone()));
        self.tab_session_sequence = self.tab_session_sequence.wrapping_add(1);
        let sequence = self.tab_session_sequence;
        let order = self.tab_session_order.clone();
        let write = crate::api::compat::blocking(move || {
            let mut latest = order
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if latest.get(&workspace).is_some_and(|done| *done > sequence) {
                return Ok(());
            }
            latest.insert(workspace.clone(), sequence);
            store.save_tab_session(&workspace, &session)
        });
        cx.spawn(async move |_, _| {
            if let Err(error) = write.await {
                tracing::warn!(%error, "could not save the Workbench tab session");
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(count: usize) -> Vec<RequestId> {
        (0..count).map(|_| RequestId::new()).collect()
    }

    #[test]
    fn empty_session_restores_no_tabs() {
        let (requests, active) = restore_order(&TabSession::default(), |_| true);
        assert!(requests.is_empty());
        assert_eq!(active, None);
    }

    #[test]
    fn restore_keeps_order_and_the_active_tab() {
        let ids = ids(3);
        let session = TabSession {
            requests: vec![ids[2].clone(), ids[0].clone(), ids[1].clone()],
            active: Some(ids[0].clone()),
        };
        let (requests, active) = restore_order(&session, |_| true);
        assert_eq!(requests, session.requests);
        assert_eq!(active, Some(1));
    }

    #[test]
    fn restore_skips_missing_requests() {
        let ids = ids(4);
        let session = TabSession {
            requests: ids.clone(),
            active: Some(ids[3].clone()),
        };
        let (requests, active) = restore_order(&session, |id| *id != ids[1]);
        assert_eq!(
            requests,
            vec![ids[0].clone(), ids[2].clone(), ids[3].clone()]
        );
        assert_eq!(active, Some(2));
    }

    #[test]
    fn a_missing_active_tab_falls_back_to_the_nearest() {
        let ids = ids(5);
        let session = TabSession {
            requests: ids.clone(),
            active: Some(ids[2].clone()),
        };
        // Neighbours on both sides survive: the earlier one wins the tie.
        let (requests, active) = restore_order(&session, |id| *id != ids[2]);
        assert_eq!(requests[active.unwrap()], ids[1]);
        // Only a later one is near.
        let (requests, active) = restore_order(&session, |id| *id != ids[1] && *id != ids[2]);
        assert_eq!(requests[active.unwrap()], ids[3]);
        // Nothing survives.
        let (requests, active) = restore_order(&session, |_| false);
        assert!(requests.is_empty());
        assert_eq!(active, None);
    }

    #[test]
    fn restore_without_an_active_tab_opens_the_first() {
        let ids = ids(2);
        let session = TabSession {
            requests: ids.clone(),
            active: None,
        };
        assert_eq!(restore_order(&session, |_| true), (ids, Some(0)));
    }

    #[test]
    fn duplicate_ids_reopen_once() {
        let ids = ids(2);
        let session = TabSession {
            requests: vec![ids[0].clone(), ids[1].clone(), ids[0].clone()],
            active: Some(ids[1].clone()),
        };
        assert_eq!(
            restore_order(&session, |_| true),
            (vec![ids[0].clone(), ids[1].clone()], Some(1))
        );
    }

    #[test]
    fn sessions_leave_drafts_out() {
        let ids = ids(2);
        let tabs = vec![Some(ids[0].clone()), None, Some(ids[1].clone())];
        assert_eq!(
            session_of(&tabs, Some(2)),
            TabSession {
                requests: ids.clone(),
                active: Some(ids[1].clone()),
            }
        );
        // An active draft hands the active mark to the nearest saved tab.
        assert_eq!(session_of(&tabs, Some(1)).active, Some(ids[0].clone()));
        assert_eq!(session_of(&[None], Some(0)), TabSession::default());
        assert_eq!(session_of(&[], None), TabSession::default());
    }
}
