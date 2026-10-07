//! The request-tab context menu described as pure data so its availability can
//! be tested without a GPUI window.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Close,
    CloseOthers,
    CloseToRight,
    CloseAll,
    Duplicate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub action: Action,
    pub label: &'static str,
    pub icon: &'static str,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    Item(Item),
    Separator,
}

#[derive(Debug, Clone, Copy)]
pub struct Subject {
    pub index: usize,
    pub total: usize,
}

pub fn rows(subject: Subject) -> Vec<Row> {
    let item = |action, label, icon, enabled| {
        Row::Item(Item {
            action,
            label,
            icon,
            enabled,
        })
    };

    vec![
        item(Action::Close, "Close", "close", true),
        item(
            Action::CloseOthers,
            "Close Others",
            "close",
            subject.total > 1,
        ),
        item(
            Action::CloseToRight,
            "Close Tabs to the Right",
            "arrow-right",
            subject.index + 1 < subject.total,
        ),
        item(Action::CloseAll, "Close All", "close", true),
        Row::Separator,
        item(Action::Duplicate, "Duplicate", "copy", true),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(rows: &[Row], action: Action) -> &Item {
        rows.iter()
            .find_map(|row| match row {
                Row::Item(item) if item.action == action => Some(item),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{action:?} is not in the menu"))
    }

    #[test]
    fn menu_lists_the_request_tab_actions_in_postman_order() {
        let labels: Vec<_> = rows(Subject { index: 0, total: 3 })
            .into_iter()
            .map(|row| match row {
                Row::Item(item) => item.label,
                Row::Separator => "-",
            })
            .collect();

        assert_eq!(
            labels,
            [
                "Close",
                "Close Others",
                "Close Tabs to the Right",
                "Close All",
                "-",
                "Duplicate",
            ]
        );
    }

    #[test]
    fn actions_that_need_other_tabs_are_disabled_when_not_applicable() {
        let lone = rows(Subject { index: 0, total: 1 });
        assert!(!item(&lone, Action::CloseOthers).enabled);
        assert!(!item(&lone, Action::CloseToRight).enabled);
        assert!(item(&lone, Action::Close).enabled);
        assert!(item(&lone, Action::CloseAll).enabled);
        assert!(item(&lone, Action::Duplicate).enabled);

        let last = rows(Subject { index: 2, total: 3 });
        assert!(item(&last, Action::CloseOthers).enabled);
        assert!(!item(&last, Action::CloseToRight).enabled);
    }
}
