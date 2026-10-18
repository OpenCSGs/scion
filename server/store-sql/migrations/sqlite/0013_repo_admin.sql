-- 仓库属性与软删除（仓库管理接口）。deleted_at_ms 非空表示已删除：按名字找不到，数据保留到清除，
-- 清除之前名字不能重新使用。
ALTER TABLE repos ADD COLUMN default_branch TEXT NOT NULL DEFAULT 'main';
ALTER TABLE repos ADD COLUMN description TEXT NOT NULL DEFAULT '';
ALTER TABLE repos ADD COLUMN deleted_at_ms INTEGER;
ALTER TABLE repos ADD COLUMN forked_from TEXT;
CREATE INDEX repos_deleted ON repos (deleted_at_ms) WHERE deleted_at_ms IS NOT NULL;
