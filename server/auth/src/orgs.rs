//! Organizations and personal namespaces (docs/design/authorization.md).
//!
//! - An organization owns the repositories named `ORG/…`. Its members have a role, and each role
//!   maps onto an action: owner = admin, maintainer = land, developer = write, reporter = read.
//!   Memberships become the groups `org:ORG:ROLE` before the chain runs, so policies can name them,
//!   and the `orgs` decider allows what a member's role covers. An `internal` organization lets
//!   every signed-in user read.
//! - Every signed-in user owns the personal namespace `user/SUBJECT/…`: the `personal` decider
//!   allows them everything there.

use crate::policy::{Action, Principal, Request};

/// The first segment of personal namespaces, which no organization can take.
pub const PERSONAL_PREFIX: &str = "user";

/// A member's role in an organization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Reporter,
    Developer,
    Maintainer,
    Owner,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Role::Owner),
            "maintainer" => Some(Role::Maintainer),
            "developer" => Some(Role::Developer),
            "reporter" => Some(Role::Reporter),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Maintainer => "maintainer",
            Role::Developer => "developer",
            Role::Reporter => "reporter",
        }
    }

    /// The highest action the role allows in the organization's repositories.
    pub fn action(self) -> Action {
        match self {
            Role::Owner => Action::Admin,
            Role::Maintainer => Action::Land,
            Role::Developer => Action::Write,
            Role::Reporter => Action::Read,
        }
    }
}

/// Organization visibility: who besides the members can read.
pub const VISIBILITIES: [&str; 2] = ["private", "internal"];

/// Whether `s` can name an organization or a personal namespace: ASCII letters, digits, `-`, `_`
/// and `.`, not starting with `.`, at most 64 bytes.
pub fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// An organization name: a valid segment other than the personal namespace prefix.
pub fn validate_org_name(name: &str) -> Result<(), String> {
    if name == PERSONAL_PREFIX {
        return Err(format!("{PERSONAL_PREFIX:?} is reserved for personal namespaces"));
    }
    if !valid_segment(name) {
        return Err(format!(
            "invalid organization name {name:?}: use letters, digits, '-', '_' and '.' (at most 64)"
        ));
    }
    Ok(())
}

/// The group a membership adds: `org:ORG:ROLE`.
pub fn membership_group(org: &str, role: Role) -> String {
    format!("org:{org}:{}", role.as_str())
}

/// The organization a repository belongs to: its first segment, when it has more than one.
pub fn org_of(repo: &str) -> Option<&str> {
    repo.split_once('/')
        .map(|(org, _)| org)
        .filter(|o| *o != PERSONAL_PREFIX)
}

/// The best role `p` has in `org`, from its `org:ORG:ROLE` groups.
pub fn role_in(p: &Principal, org: &str) -> Option<Role> {
    let prefix = format!("org:{org}:");
    p.groups
        .iter()
        .filter_map(|g| g.strip_prefix(&prefix).and_then(Role::parse))
        .max()
}

/// The `orgs` decider: allows what the member's role covers, and reads in an `internal`
/// organization; `visibility` is the organization's, None when it does not exist. Abstains
/// otherwise (no organization, not a member, or more than the role allows).
pub fn orgs_allow(p: &Principal, req: &Request<'_>, visibility: Option<&str>) -> bool {
    if visibility.is_none() || !p.authenticated {
        return false;
    }
    let Some(org) = org_of(req.repo) else { return false };
    match role_in(p, org) {
        Some(role) => role.action() >= req.action,
        None => visibility == Some("internal") && req.action == Action::Read,
    }
}

/// The `personal` decider: `user/SUBJECT/…` belongs to SUBJECT.
pub fn personal_allow(p: &Principal, repo: &str) -> bool {
    p.authenticated
        && valid_segment(&p.subject)
        && repo
            .strip_prefix(PERSONAL_PREFIX)
            .and_then(|r| r.strip_prefix('/'))
            .and_then(|r| r.strip_prefix(p.subject.as_str()))
            .is_some_and(|r| r.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_and_names() {
        for r in [Role::Owner, Role::Maintainer, Role::Developer, Role::Reporter] {
            assert_eq!(Role::parse(r.as_str()), Some(r));
        }
        assert_eq!(Role::parse("admin"), None);
        assert_eq!(Role::Owner.action(), Action::Admin);
        assert_eq!(Role::Reporter.action(), Action::Read);
        assert!(Role::Owner > Role::Maintainer && Role::Developer > Role::Reporter);
        assert!(validate_org_name("open-csg_1.x").is_ok());
        for bad in ["", "user", ".hidden", "a/b", "a b", &"x".repeat(65)] {
            assert!(validate_org_name(bad).is_err(), "{bad}");
        }
        assert_eq!(membership_group("acme", Role::Developer), "org:acme:developer");
        assert_eq!(org_of("acme/app"), Some("acme"));
        assert_eq!(org_of("acme/a/b"), Some("acme"));
        assert_eq!(org_of("mono"), None);
        assert_eq!(org_of("user/alice/x"), None);
    }

    #[test]
    fn org_decisions() {
        let dev = Principal::user("bob", vec!["org:acme:developer".into(), "org:acme:reporter".into()]);
        let req = |repo, action| Request::repo(repo, action);
        assert_eq!(role_in(&dev, "acme"), Some(Role::Developer), "the best role");
        assert!(orgs_allow(&dev, &req("acme/app", Action::Write), Some("private")));
        assert!(!orgs_allow(&dev, &req("acme/app", Action::Land), Some("private")));
        assert!(
            !orgs_allow(&dev, &req("acme/app", Action::Read), None),
            "no such organization"
        );
        assert!(!orgs_allow(&dev, &req("other/app", Action::Read), Some("private")));
        assert!(!orgs_allow(&dev, &req("mono", Action::Read), Some("private")));
        let eve = Principal::user("eve", vec![]);
        assert!(orgs_allow(&eve, &req("acme/app", Action::Read), Some("internal")));
        assert!(!orgs_allow(&eve, &req("acme/app", Action::Write), Some("internal")));
        assert!(!orgs_allow(&eve, &req("acme/app", Action::Read), Some("private")));
        assert!(!orgs_allow(
            &Principal::anonymous(),
            &req("acme/app", Action::Read),
            Some("internal")
        ));
    }

    #[test]
    fn personal_namespaces() {
        let alice = Principal::user("alice", vec![]);
        assert!(personal_allow(&alice, "user/alice/notes"));
        assert!(personal_allow(&alice, "user/alice/a/b"));
        assert!(!personal_allow(&alice, "user/alice"));
        assert!(!personal_allow(&alice, "user/alicex/notes"));
        assert!(!personal_allow(&alice, "user/bob/notes"));
        assert!(!personal_allow(&alice, "alice/notes"));
        assert!(
            !personal_allow(&Principal::user("a:b", vec![]), "user/a:b/x"),
            "not a valid segment"
        );
        assert!(!personal_allow(&Principal::anonymous(), "user/anonymous/x"));
    }
}
