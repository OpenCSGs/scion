# 授权：多种来源，一条链

[English](authorization.md) | 简体中文

状态：设计中，尚未实现。

Scion 要在差别很大的部署里决定谁能做什么：
- 几个团队独立部署；
- 接在 starhub-server 后面，权限由它的 rebac 决定；
- 接公司 IdP，由 IdP 的组说明谁属于哪里；
- 用安全团队写的 OPA 策略。

而且这几种常常要同时生效，比如模型仓库归 starhub 管，内部命名空间用内置策略。本文说明服务端如何组合多种授权，并为独立部署加上组织。

## 放在哪一层

和 git 托管一样，用户和组织属于应用层，不属于存储：git 和 Gitaly 不知道它们，GitHub、GitLab、Gitea 才知道。

- **存储不动。** Scion 的存储核心（`store`、`land`、`query`、`tree`）只通过 `Guard::check` 问一个问题：这个身份能不能对这个仓库、ref 或路径做这个动作。本文不改这一点，读写路径的速度也保持不变。
- **认证同样不变。** 它已经支持多种方式：PAT、服务端自签的 JWT、外部 JWT（JWKS 或 HS256）、RFC 7662 introspection、OIDC 浏览器登录。最终都得到一个 `Principal`，即 subject、组，以及可选的授权（grants）。

本文讨论的是从身份到"允许或拒绝"之间的部分。

## 现状

`Guard::check` 写死了三个来源，任意一个允许就放行：

1. 已保存的 ACL 策略（`scion admin policy set`），没有保存时用内置默认策略；
2. 令牌自带的授权（`repos_claim`）；
3. 一个授权回调（`[authz_webhook]`）。

这有三个限制：
- 只能有一个回调；
- 不能拒绝，排在后面的来源无法推翻前面的；
- 组成员关系只来自令牌，改成员就得重签令牌。

## 模型

插件分两类，刻意分开。

**成员关系来源**回答"这个 subject 属于哪些组"。每个已认证身份查一次（带缓存），在做任何判断之前把组补上：

| 来源 | 补上的组 |
|---|---|
| 令牌声明（现有） | 令牌的 `groups`，或 IdP 的 `groups_claim` |
| 组织（新增，内置） | 每个成员关系一个 `org:ORG:ROLE`，例如 `org:opencsg:maintainer` |
| 静态组（新增） | 策略里的 `[groups]`：`eng = ["user:alice", "group:agents"]`，可嵌套，会检查循环 |
| 外部目录（以后） | GitHub team、LDAP、SCIM，统一接在一个适配器接口后面 |

**判定器**回答"这个请求能不能做"。每个返回**允许**、**拒绝**或**弃权**：

| 判定器 | 允许 | 拒绝 | 弃权 |
|---|---|---|---|
| `policy` | 命中一条允许规则 | 命中一条 `deny` 规则（新增） | 其他情况 |
| `orgs` | 成员角色覆盖该组织命名空间里的这个动作 | — | 仓库不属于任何组织，或身份不是成员 |
| `grants` | 令牌里的授权覆盖这个请求 | — | 其他情况 |
| `webhook:NAME` | `{"result": true}` 或 `{"result": {"allow": true}}` | 配置为 `decides = "allow-deny"` 时，返回 `{"result": false}` 或 `{"result": {"allow": false}}` | 没有结果；或者在 `decides = "allow-only"` 时返回 `false`（这是默认值，现有回调含义不变） |

**链。** 判定器按配置的顺序执行，第一个给出允许或拒绝的判定器决定结果，这和 Kubernetes 组合 authorizer 的方式一样。全部弃权则拒绝。系统管理（策略、令牌、组织）也走同一条链，相当于对仓库 `*` 的 `admin` 动作。

```toml
# auth.toml
[authorization]
chain = ["policy", "orgs", "grants", "webhook:starhub"]

[[authorization.webhook]]
name = "starhub"
url = "http://starhub-server:8080/api/v1/internal/scion/authz"
bearer_env = "STARHUB_API_TOKEN"
decides = "allow-deny"         # 对这些仓库，starhub 的 rebac 说了算
repos = ["models/*", "datasets/*", "spaces/*"]   # 只问这些仓库，其他仓库弃权
cache_ttl = "30s"
on_error = "closed"

[[authorization.webhook]]
name = "opa"
url = "http://opa:8181/v1/data/scion/allow"
```

- **判定器上的 `repos`** 限定它负责哪些仓库。这样同一台服务端可以把 `models/*` 交给 starhub，`internal/*` 用内置策略和组织，其余交给 OPA。
- **不写 `[authorization]` 时**，链就是现在的行为：`["policy", "grants", "webhook"]`，原来的 `[authz_webhook]` 作为唯一的回调，`decides = "allow-only"`；没有保存策略、也没有配置回调时，仍然用内置默认策略。现有部署的行为完全不变。

## 组织（`orgs` 判定器和成员关系来源）

组织面向独立部署：想让团队自助管理，又不想写策略文件。链里不包含 `orgs` 的部署完全感知不到它。

- **组织就是仓库命名空间。** 组织 `opencsg` 拥有所有以 `opencsg/` 开头的仓库。没有组织前缀的仓库只由其他判定器管。
- **角色对应现有的四级动作：**

| 角色 | 动作 | 另外还能 |
|---|---|---|
| `owner` | admin | 管理成员和设置，在命名空间里建库、删库、fork |
| `maintainer` | land | |
| `developer` | write | |
| `reporter` | read | |

- **可见性。** 组织可以是 `private`（默认，只有成员能读）或 `internal`（所有登录用户能读）。单个仓库仍可以通过策略开放给 `anonymous`。
- **数据。** 元数据库新增两张表，SQLite 和 PostgreSQL 都支持：
  - `orgs(name, visibility, created_at, created_by)`；
  - `org_members(org, subject, role, added_at, added_by)`。
- **成员变更立即生效。** 成员关系在服务端查询，不写进令牌。用户令牌只说明你是谁（不再需要 `--group`）；加减成员在判定缓存的 TTL 内生效，不用重签令牌。
- **接口。** 新增 `OrgService`：
  - `CreateOrg`、`GetOrg`、`ListOrgs`、`DeleteOrg`、`SetOrgVisibility`；
  - `AddMember`、`RemoveMember`、`ListMembers`、`SetRole`。
  - 组织的 owner 自己管理本组织，系统管理员可以管理所有组织。
  - 最后一个 owner 不能被移除。
- **命令：** `scn org create / ls / info / rm`、`scn org member add / rm / ls / role`；`scion admin org` 下有同样的命令，供运维直接操作数据库。

## 策略里的拒绝规则

拒绝规则用来让一个判定器推翻另一个。比如组织角色本来允许，但要禁止外包组读 secrets 目录：

```toml
[[rule]]
effect   = "deny"                # 省略时为 allow
subjects = ["group:contractors"]
repos    = ["opencsg/*"]
paths    = ["secrets"]
actions  = ["read"]
```

在策略内部，拒绝优先于允许。跨判定器时按链的顺序决定，所以要让策略里的拒绝规则生效，就把 `policy` 排在最前面。

## 性能

- **读路径不增加开销。**
  - 策略、静态组和令牌授权都在内存里计算。
  - 组织成员关系按 subject 加载，缓存 `cache_ttl`（默认 30 秒），和现在策略重新加载的时延一样。
  - 判定结果按 `(principal, repo, action, ref, 路径前缀)` 缓存。
  - 按 id 读对象本来就只要求仓库读权限，而且在缓存和 daemon 里按令牌缓存，所以每个对象都不会多一次查询。
- **回调保持现有的缓存方式。** 每个回调单独缓存：允许缓存 `cache_ttl`，拒绝缓存 5 秒。配置了 `repos` 的回调，不会被问到其他仓库。

## 分步实现

每一步一个提交，带测试；在配置之前，每一步都不改变现有行为：

1. 定义 `Decider` trait（允许、拒绝、弃权），把现有三个来源接到它后面，`Guard::check` 改为执行这条链，默认顺序与现在一致。
2. `auth.toml` 增加 `[authorization]`：链的顺序、多个带名字的回调（含 `decides` 和 `repos`），旧的 `[authz_webhook]` 继续可用。
3. 策略支持 `effect = "deny"` 规则和 `[groups]`。
4. `orgs` 和 `org_members` 两张表（SQLite 与 PostgreSQL 迁移），以及 `orgs` 成员关系来源和判定器。
5. `OrgService` 和 `scion admin org`。
6. `scn org`。
7. 更新 `docs/guide/auth.md`，并加一个端到端测试：三个命名空间分别由策略、组织和回调管理。

## 待定问题

- 建一个不属于任何组织的顶层仓库，仍只允许系统管理员吗？还是像 GitHub 的个人命名空间那样，允许任何登录用户建 `user/NAME/*`？
- GitHub team 适配器要放进第一版，还是等有部署需要时再做？
