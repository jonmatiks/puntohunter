//! The group's totals: this instance's figures plus each counted member's, for the overview's
//! card and `GET /api/v1/group` (API reference section 2.11), both from one model. Without `show_members` it
//! names no member; with it, it lists each instance by its label, never by a member's name or URL.

use serde::Serialize;

use super::{Portal, pool::Overview};
use crate::{
    group::{Figures, Members},
    store::StoreError,
};

/// The group resource.
#[derive(Debug, Serialize)]
pub struct GroupTotals {
    pub name: String,
    /// This instance and its members.
    pub instances: usize,
    /// Those whose figures are in the totals, this instance always among them.
    pub reporting: usize,
    pub hash_rate: f64,
    pub devices: u64,
    pub blocks_found: u64,
    /// With `show_members`, each instance: this one first, then the members in config order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<Instance>>,
}

/// An instance as the card lists it.
#[derive(Debug, PartialEq, Serialize)]
pub struct Instance {
    pub label: String,
    /// The origin of its portal; None for this instance, or a member without one.
    pub link: Option<String>,
    /// Whether it is this instance.
    pub this: bool,
    /// Whether its figures count now; this instance always does.
    pub reporting: bool,
    /// Its figures while it is reporting.
    pub hash_rate: Option<f64>,
    pub devices: Option<u64>,
    pub blocks_found: Option<u64>,
}

impl Instance {
    fn new(label: String, link: Option<String>, this: bool, figures: Option<Figures>) -> Self {
        Instance {
            label,
            link,
            this,
            reporting: figures.is_some(),
            hash_rate: figures.map(|f| f.hash_rate),
            devices: figures.map(|f| f.devices),
            blocks_found: figures.map(|f| f.blocks_found),
        }
    }
}

impl GroupTotals {
    /// The totals of this instance's `own` figures and those of the members `members` count now.
    pub fn of(members: &Members, own: Figures) -> Self {
        let (configured, counted) = members.counted();
        let total = counted.iter().fold(own, |sum, figures| sum + *figures);
        GroupTotals {
            name: members.name.clone(),
            instances: configured + 1,
            reporting: counted.len() + 1,
            hash_rate: total.hash_rate,
            devices: total.devices,
            blocks_found: total.blocks_found,
            members: members
                .labels
                .as_ref()
                .zip(members.listed())
                .map(|(labels, listed)| {
                    std::iter::once(Instance::new(labels.own.clone(), None, true, Some(own)))
                        .chain(listed.into_iter().map(|member| {
                            Instance::new(member.label, member.link, false, member.figures)
                        }))
                        .collect()
                }),
        }
    }
}

impl Overview {
    /// The figures a member reads from this instance's pool resource.
    pub fn figures(&self) -> Figures {
        Figures {
            hash_rate: self.pools.iter().map(|pool| pool.hash_rate).sum(),
            devices: self.pools.iter().map(|pool| pool.devices as u64).sum(),
            blocks_found: self.blocks_found,
        }
    }
}

impl Portal {
    /// The group's totals now, this instance's figures read as the overview reads them; None without
    /// `[group]`.
    pub async fn group_totals(&self) -> Result<Option<GroupTotals>, StoreError> {
        let Some(members) = &self.group else {
            return Ok(None);
        };
        let own = self.overview(None).await?.figures();
        Ok(Some(GroupTotals::of(members, own)))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::group::Labels;

    fn figures(hash_rate: f64, devices: u64, blocks_found: u64) -> Figures {
        Figures {
            hash_rate,
            devices,
            blocks_found,
        }
    }

    #[test]
    fn adds_the_instance_and_each_counted_member() {
        let refresh = Duration::from_secs(30);
        let now = Instant::now();
        let members = Members::holding(
            "PuntoHunter Group",
            refresh,
            vec![
                Some((figures(2e12, 3, 4), now - refresh)),
                // Its last answer is three refreshes old, and no longer counts.
                Some((figures(9e12, 9, 9), now - refresh * 3)),
                // Never answered.
                None,
            ],
        );
        let totals = GroupTotals::of(&members, figures(1e12, 1, 2));
        assert_eq!(totals.name, "PuntoHunter Group");
        assert_eq!((totals.instances, totals.reporting), (4, 2));
        assert_eq!(
            (totals.hash_rate, totals.devices, totals.blocks_found),
            (3e12, 4, 6)
        );
    }

    #[test]
    fn lists_each_instance_by_label_with_show_members() {
        let refresh = Duration::from_secs(30);
        let now = Instant::now();
        let answers = vec![Some((figures(2e12, 3, 4), now - refresh)), None];
        let plain = GroupTotals::of(
            &Members::holding("Group", refresh, answers.clone()),
            figures(1e12, 1, 2),
        );
        assert_eq!(
            plain.members, None,
            "without show_members, no member is named"
        );
        assert!(
            !serde_json::to_string(&plain).unwrap().contains("members"),
            "nor is the key sent"
        );

        let members = Members::holding("Group", refresh, answers).labelled(Labels {
            own: "US East".into(),
            members: vec![
                (
                    "Second site".into(),
                    Some("https://pool.example.com".into()),
                ),
                ("Japan".into(), None),
            ],
        });
        let totals = GroupTotals::of(&members, figures(1e12, 1, 2));
        assert_eq!(
            totals.members.unwrap(),
            [
                Instance::new("US East".into(), None, true, Some(figures(1e12, 1, 2))),
                Instance::new(
                    "Second site".into(),
                    Some("https://pool.example.com".into()),
                    false,
                    Some(figures(2e12, 3, 4))
                ),
                Instance {
                    label: "Japan".into(),
                    link: None,
                    this: false,
                    reporting: false,
                    hash_rate: None,
                    devices: None,
                    blocks_found: None,
                },
            ]
        );
    }

    #[test]
    fn with_no_member_counted_the_totals_are_the_instances_own() {
        let members = Members::holding("Group", Duration::from_secs(30), vec![None]);
        let totals = GroupTotals::of(&members, figures(1e12, 1, 2));
        assert_eq!((totals.instances, totals.reporting), (2, 1));
        assert_eq!(
            (totals.hash_rate, totals.devices, totals.blocks_found),
            (1e12, 1, 2)
        );
    }
}
