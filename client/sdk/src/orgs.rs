//! Organizations and their members (`OrgService`, docs/design/authorization.md).

use scion_proto::v1::{
    self, CreateOrgRequest, DeleteOrgRequest, GetOrgRequest, ListMembersRequest, ListOrgsRequest, PutMemberRequest,
    RemoveMemberRequest, SetOrgVisibilityRequest, org_service_client::OrgServiceClient,
};

use crate::client::Client;
use crate::error::{Error, Result};

/// An organization: it owns the repositories named `NAME/…`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrgInfo {
    pub name: String,
    /// `private` or `internal`.
    pub visibility: String,
    pub created_at_ms: i64,
    pub created_by: String,
}

impl From<v1::Org> for OrgInfo {
    fn from(o: v1::Org) -> Self {
        Self {
            name: o.name,
            visibility: o.visibility,
            created_at_ms: o.created_at_ms,
            created_by: o.created_by,
        }
    }
}

/// A member of an organization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberInfo {
    pub subject: String,
    /// `owner`, `maintainer`, `developer` or `reporter`.
    pub role: String,
    pub added_at_ms: i64,
    pub added_by: String,
}

impl From<v1::Member> for MemberInfo {
    fn from(m: v1::Member) -> Self {
        Self {
            subject: m.subject,
            role: m.role,
            added_at_ms: m.added_at_ms,
            added_by: m.added_by,
        }
    }
}

fn org_of(o: Option<v1::Org>) -> Result<OrgInfo> {
    o.map(OrgInfo::from)
        .ok_or_else(|| Error::Protocol("response without an organization".into()))
}

impl Client {
    fn orgs(&self) -> OrgServiceClient<tonic::transport::Channel> {
        OrgServiceClient::new(self.channel.clone())
    }

    /// Creates an organization, with the caller as owner; `visibility` empty means private.
    pub async fn create_org(&self, name: &str, visibility: &str) -> Result<OrgInfo> {
        let req = CreateOrgRequest {
            name: name.to_owned(),
            visibility: visibility.to_owned(),
        };
        let r = self.orgs().create_org(self.request(req)?).await?;
        org_of(r.into_inner().org)
    }

    pub async fn get_org(&self, name: &str) -> Result<OrgInfo> {
        let req = GetOrgRequest { name: name.to_owned() };
        let r = self.orgs().get_org(self.request(req)?).await?;
        org_of(r.into_inner().org)
    }

    /// Every organization, or with `mine` only the caller's; by name.
    pub async fn list_orgs(&self, mine: bool) -> Result<Vec<OrgInfo>> {
        let req = ListOrgsRequest { mine };
        let r = self.orgs().list_orgs(self.request(req)?).await?;
        Ok(r.into_inner().orgs.into_iter().map(OrgInfo::from).collect())
    }

    pub async fn set_org_visibility(&self, name: &str, visibility: &str) -> Result<OrgInfo> {
        let req = SetOrgVisibilityRequest {
            name: name.to_owned(),
            visibility: visibility.to_owned(),
        };
        let r = self.orgs().set_org_visibility(self.request(req)?).await?;
        org_of(r.into_inner().org)
    }

    pub async fn delete_org(&self, name: &str) -> Result<()> {
        let req = DeleteOrgRequest { name: name.to_owned() };
        self.orgs().delete_org(self.request(req)?).await?;
        Ok(())
    }

    /// Adds a member, or changes an existing member's role.
    pub async fn put_member(&self, org: &str, subject: &str, role: &str) -> Result<MemberInfo> {
        let req = PutMemberRequest {
            org: org.to_owned(),
            subject: subject.to_owned(),
            role: role.to_owned(),
        };
        let r = self.orgs().put_member(self.request(req)?).await?;
        r.into_inner()
            .member
            .map(MemberInfo::from)
            .ok_or_else(|| Error::Protocol("response without a member".into()))
    }

    pub async fn remove_member(&self, org: &str, subject: &str) -> Result<()> {
        let req = RemoveMemberRequest {
            org: org.to_owned(),
            subject: subject.to_owned(),
        };
        self.orgs().remove_member(self.request(req)?).await?;
        Ok(())
    }

    /// An organization's members, by subject.
    pub async fn list_members(&self, org: &str) -> Result<Vec<MemberInfo>> {
        let req = ListMembersRequest { org: org.to_owned() };
        let r = self.orgs().list_members(self.request(req)?).await?;
        Ok(r.into_inner().members.into_iter().map(MemberInfo::from).collect())
    }
}
