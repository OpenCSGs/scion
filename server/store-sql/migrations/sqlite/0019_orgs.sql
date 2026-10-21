-- Organizations and their members (docs/design/authorization.md). An organization owns the
-- repositories named `NAME/…`; members have a role: owner, maintainer, developer or reporter.
CREATE TABLE orgs (
    name          TEXT    PRIMARY KEY,
    visibility    TEXT    NOT NULL,
    created_at_ms INTEGER NOT NULL,
    created_by    TEXT    NOT NULL
) WITHOUT ROWID;

CREATE TABLE org_members (
    org         TEXT    NOT NULL REFERENCES orgs (name) ON DELETE CASCADE,
    subject     TEXT    NOT NULL,
    role        TEXT    NOT NULL,
    added_at_ms INTEGER NOT NULL,
    added_by    TEXT    NOT NULL,
    PRIMARY KEY (org, subject)
) WITHOUT ROWID;
CREATE INDEX org_members_subject ON org_members (subject);
