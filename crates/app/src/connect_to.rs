//! "Connect to…" (⌘K / Ctrl+K): a quick switcher over Hosts, remote desktops and
//! connections that opens the right session (terminal, remote desktop, SQL tab, files,
//! browser). Ranking is a pure function so it is unit tested.

use std::collections::{HashMap, HashSet};

use switchyard_core::store::{Profile, ProfileId};

use crate::actions::fuzzy_score;
use crate::app_state::{Profiles, describe};

/// Extra weight of a match outside the name (address, tags, place).
const DETAIL_PENALTY: usize = 50;

/// Profiles matching `query`, best first: a name match beats an address or tag match,
/// then items of the current workspace, then the most recently connected, then by name.
/// An empty query lists recent items first, then the current workspace, then the rest.
pub(crate) fn connect_candidates(
    profiles: &Profiles,
    query: &str,
    recent: &HashMap<ProfileId, i64>,
    current: &HashSet<ProfileId>,
) -> Vec<ProfileId> {
    let q = query.trim();
    let mut scored: Vec<(usize, bool, i64, String, ProfileId)> = profiles
        .all
        .iter()
        .filter(|p| p.is_connection())
        .filter_map(|p| {
            let score = if q.is_empty() {
                0
            } else {
                match fuzzy_score(q, p.name()) {
                    Some(s) => s,
                    None => {
                        let hay = format!(
                            "{} {} {}",
                            address(p),
                            p.tags().join(" "),
                            describe(p, profiles)
                        );
                        fuzzy_score(q, &hay)? + DETAIL_PENALTY
                    }
                }
            };
            Some((
                score,
                !current.contains(p.id()),
                -recent.get(p.id()).copied().unwrap_or(i64::MIN / 2),
                p.name().to_lowercase(),
                p.id().clone(),
            ))
        })
        .collect();
    if q.is_empty() {
        // Recent first, then the workspace, then everything else.
        scored.sort_by(|a, b| {
            let ra = recent.contains_key(&a.4);
            let rb = recent.contains_key(&b.4);
            rb.cmp(&ra)
                .then(a.2.cmp(&b.2))
                .then(a.1.cmp(&b.1))
                .then(a.3.cmp(&b.3))
        });
    } else {
        scored.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(a.1.cmp(&b.1))
                .then(a.2.cmp(&b.2))
                .then(a.3.cmp(&b.3))
        });
    }
    scored.into_iter().map(|s| s.4).collect()
}

/// The address a profile is searched by.
fn address(p: &Profile) -> String {
    match p {
        Profile::Host(h) => h.address.clone(),
        Profile::Desktop(d) => d.address.clone(),
        Profile::Db(d) => d.server.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::db::Engine;
    use switchyard_core::store::{Credential, DbConnection, DesktopProtocol, DesktopServer, Host};

    fn sample() -> (Profiles, Vec<ProfileId>) {
        let web = Profile::Host(Host::new("web-prod", "10.0.0.5", "root"));
        let mut tagged = Host::new("cache", "10.0.0.7", "root");
        tagged.tags = vec!["web".into()];
        let tagged = Profile::Host(tagged);
        let db = Profile::Db(DbConnection::new("webshop", Engine::Postgres));
        let mut dc = DesktopServer::new("dc01", DesktopProtocol::Rdp);
        dc.address = "dc01.corp".into();
        let dc = Profile::Desktop(dc);
        let cred = Profile::Credential(Credential::new("web admin", "admin"));
        let ids = vec![
            web.id().clone(),
            tagged.id().clone(),
            db.id().clone(),
            dc.id().clone(),
            cred.id().clone(),
        ];
        (
            Profiles {
                all: vec![web, tagged, db, dc, cred],
            },
            ids,
        )
    }

    #[test]
    fn names_beat_tags_and_credentials_are_not_listed() {
        let (profiles, ids) = sample();
        let got = connect_candidates(&profiles, "web", &HashMap::new(), &HashSet::new());
        // Two names start with "web"; the tag match comes after; the credential never.
        assert_eq!(got, [ids[0].clone(), ids[2].clone(), ids[1].clone()]);
        let got = connect_candidates(&profiles, "10.0.0.7", &HashMap::new(), &HashSet::new());
        assert_eq!(got, [ids[1].clone()]);
    }

    #[test]
    fn workspace_and_recency_break_ties() {
        let (profiles, ids) = sample();
        // Same score ("web" at 0 in both names): the current workspace wins…
        let current: HashSet<ProfileId> = [ids[2].clone()].into();
        let got = connect_candidates(&profiles, "web", &HashMap::new(), &current);
        assert_eq!(got[0], ids[2]);
        // …and among equals the more recent one.
        let recent: HashMap<ProfileId, i64> = [(ids[2].clone(), 10), (ids[0].clone(), 20)].into();
        let got = connect_candidates(&profiles, "web", &recent, &HashSet::new());
        assert_eq!(got[0], ids[0]);
    }

    #[test]
    fn empty_query_lists_recent_then_workspace_then_rest() {
        let (profiles, ids) = sample();
        let recent: HashMap<ProfileId, i64> = [(ids[3].clone(), 5), (ids[1].clone(), 9)].into();
        let current: HashSet<ProfileId> = [ids[2].clone()].into();
        let got = connect_candidates(&profiles, "", &recent, &current);
        assert_eq!(
            got,
            [
                ids[1].clone(),
                ids[3].clone(),
                ids[2].clone(),
                ids[0].clone()
            ]
        );
    }
}
