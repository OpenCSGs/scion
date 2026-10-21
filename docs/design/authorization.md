# Authorization: several sources, one chain

English | [简体中文](authorization.zh-CN.md)

Status: design, not implemented yet.

Scion has to decide who may do what in very different deployments:
- standalone with a few teams;
- behind starhub-server, whose rebac owns the permissions;
- behind a company IdP whose groups say who belongs where;
- with OPA policies written by a security team.

Several of these must also work together, for example starhub for model repositories and the built-in policy for an internal namespace. This document describes how the server combines several kinds of authorization. It also adds organizations for standalone deployments.

## Where this sits

As in git hosting, users and organizations belong to the application layer, not to storage. Git and Gitaly know nothing about them; GitHub, GitLab and Gitea do.

- **Storage is not touched.** The storage core of Scion (`store`, `land`, `query`, `tree`) asks one question through `Guard::check`: may this principal do this action on this repository, ref or path? Nothing in this document changes it, and the read and write paths stay as fast as they are.
- **Authentication is unchanged too.** It already supports several methods: PATs, the server's JWTs, external JWTs over JWKS or HS256, RFC 7662 introspection, and OIDC browser login. They all produce a `Principal`: a subject, groups and optional grants.

This design is about what happens between the principal and the answer.

## Today

`Guard::check` hard-wires three sources, and a request is allowed if any of them allows it:

1. the saved ACL policy (`scion admin policy set`), or the built-in default when there is none;
2. grants carried by the token (`repos_claim`);
3. one authorization callback (`[authz_webhook]`).

This has three limits:
- there can be only one callback;
- nothing can deny, so a later source cannot override an earlier one;
- group membership comes only from tokens, so changing it means issuing new tokens.

## The model

There are two kinds of plug-ins, kept apart on purpose.

**Membership sources** answer "which groups is this subject in?". They run once per authenticated principal, cached, and add groups before any decision is made:

| Source | Groups it adds |
|---|---|
| Token claims (today) | the token's `groups` or the IdP's `groups_claim` |
| Organizations (new, built-in) | `org:ORG:ROLE` for each membership, such as `org:opencsg:maintainer` |
| Static groups (new) | `[groups]` in the policy: `eng = ["user:alice", "group:agents"]`, nested and checked for cycles |
| External directory (later) | GitHub teams, LDAP, SCIM, behind one adapter interface |

**Deciders** answer "is this request allowed?". Each one returns **allow**, **deny** or **abstain**:

| Decider | Allows | Denies | Abstains |
|---|---|---|---|
| `policy` | a matching allow rule | a matching `deny` rule (new) | otherwise |
| `orgs` | the member's role covers the action in the organization's namespace | — | the repository is not in an organization, or the principal is not a member |
| `grants` | a grant in the token covers the request | — | otherwise |
| `webhook:NAME` | `{"result": true}` or `{"result": {"allow": true}}` | `{"result": false}` or `{"result": {"allow": false}}` when the hook is configured with `decides = "allow-deny"` | no result, or `false` when `decides = "allow-only"` (the default, so today's hooks keep their meaning) |

**The chain.** Deciders run in a configured order, and the first one that allows or denies settles the request. This is how Kubernetes combines its authorizers. If every decider abstains, the request is denied. System administration (policies, tokens, organizations) goes through the same chain as an `admin` action on the repository `*`.

```toml
# auth.toml
[authorization]
chain = ["policy", "orgs", "grants", "webhook:starhub"]

[[authorization.webhook]]
name = "starhub"
url = "http://starhub-server:8080/api/v1/internal/scion/authz"
bearer_env = "STARHUB_API_TOKEN"
decides = "allow-deny"         # starhub's rebac is the authority for its repositories
repos = ["models/*", "datasets/*", "spaces/*"]   # asked only about these; abstains elsewhere
cache_ttl = "30s"
on_error = "closed"

[[authorization.webhook]]
name = "opa"
url = "http://opa:8181/v1/data/scion/allow"
```

- **`repos` on a decider** limits which repositories it is asked about. This lets one server send `models/*` to starhub, keep `internal/*` under the built-in policy and organizations, and leave everything else to OPA.
- **Without `[authorization]`**, the chain is today's behaviour: `["policy", "grants", "webhook"]` with the old `[authz_webhook]` as the single webhook and `decides = "allow-only"`, plus the built-in default policy when none is saved and no webhook is configured. Existing deployments behave exactly as before.

## Organizations (the `orgs` decider and membership source)

These are for standalone deployments that want self-service teams without writing policy files. A deployment that leaves `orgs` out of the chain never sees them.

- **An organization is a repository namespace.** Organization `opencsg` owns every repository whose name starts with `opencsg/`. A repository with no organization prefix is governed by the other deciders only.
- **Roles map onto the existing four actions:**

| Role | Action | Can also |
|---|---|---|
| `owner` | admin | manage members and settings, create, delete and fork repositories in the namespace |
| `maintainer` | land | |
| `developer` | write | |
| `reporter` | read | |

- **Visibility.** An organization can be `private` (default: only members read) or `internal` (every signed-in user reads). Single repositories can still be opened to `anonymous` through the policy.
- **Data.** The metadata store, SQLite and PostgreSQL alike, gets two new tables:
  - `orgs(name, visibility, created_at, created_by)`;
  - `org_members(org, subject, role, added_at, added_by)`.
- **Membership takes effect immediately.** It is looked up on the server and not carried in tokens. A user token only says who you are (no `--group` needed), and adding or removing a member works within the decision cache TTL without issuing new tokens.
- **API.** A new `OrgService`:
  - `CreateOrg`, `GetOrg`, `ListOrgs`, `DeleteOrg`, `SetOrgVisibility`;
  - `AddMember`, `RemoveMember`, `ListMembers`, `SetRole`.
  - An organization's owners manage it themselves, and system administrators manage every organization.
  - The last owner cannot be removed.
- **Commands:** `scn org create / ls / info / rm`, `scn org member add / rm / ls / role`, and the same under `scion admin org` for operators working on the database directly.

## Deny rules in the policy

Deny rules exist so that one decider can override another, for example to block a contractor group from a secrets path even though the organization role would allow it.

```toml
[[rule]]
effect   = "deny"                # allow when omitted
subjects = ["group:contractors"]
repos    = ["opencsg/*"]
paths    = ["secrets"]
actions  = ["read"]
```

Within the policy, deny wins over allow. Across deciders, the chain order decides, so put `policy` first for its deny rules to bind.

## Performance

- **Read paths stay where they are.**
  - Policy, static groups and grants are evaluated in memory.
  - Organization membership is loaded per subject and cached for `cache_ttl` (default 30 s), the same bound as policy reloads today.
  - Decisions are cached per `(principal, repo, action, ref, path-prefix)`.
  - Object reads by id already need repository read and are cached per token in caches and the daemon, so nothing new happens per object.
- **Webhooks keep their current caches.** Allows are cached for `cache_ttl` and denials for 5 s, per webhook. A webhook with a `repos` filter is never called for other repositories.

## Personal namespaces

Every signed-in user owns `user/SUBJECT/*`, as personal accounts do on GitHub: they can create, delete and fork repositories there and are `owner` of each. The `personal` decider allows this; it abstains for every other repository. Subjects that are not a valid repository name segment (letters, digits, `-`, `_`, `.`) get no personal namespace.

## GitHub

Open-source users mostly have GitHub accounts, so the first version supports GitHub as an identity provider and as a membership source.

- **Login.** `[github]` names an OAuth app (`client_id`). `scn login` runs GitHub's device flow when the server offers it, the server reads `GET /user`, and the subject is the GitHub login (with an optional `subject_prefix`). The server then issues its own refresh and access tokens, as with OIDC.
- **Teams as groups.** With `orgs = ["opencsg"]` and a server token that can read them (`token_env`), the server keeps the members of those organizations and their teams in memory, refreshed every `refresh` (default 5 minutes). A login gets the groups `github:ORG` and `github:ORG/TEAM`, so policies and organizations can name them: `subjects = ["group:github:opencsg/core"]`.

## Rollout

Each step is one commit with tests, and none of them changes behaviour until it is configured:

1. A decider chain behind `Guard::check`, with allow, deny and abstain. Today's three sources become deciders, in today's order. (Done.)
2. `[authorization]` in `auth.toml`: the chain order, several named webhooks with `decides` and `repos`, and the old `[authz_webhook]` still accepted.
3. `effect = "deny"` rules and `[groups]` in the policy.
4. The `orgs` and `org_members` tables (SQLite and PostgreSQL migrations), the `orgs` membership source and decider, and the `personal` decider.
5. `OrgService` and `scion admin org`.
6. `scn org`.
7. GitHub login through the device flow.
8. GitHub organizations and teams as groups.
9. Documentation in `docs/guide/auth.md` and an end-to-end test with namespaces handled by policy, orgs, personal namespaces and a webhook.

## Decisions

- Personal namespaces `user/SUBJECT/*` are part of the first version.
- GitHub login and GitHub teams as groups are part of the first version, so open-source deployments can use GitHub accounts.
