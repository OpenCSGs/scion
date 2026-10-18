-- 清除已开始：分批清除已删除仓库的第一批就置位，之后不能再恢复（已删掉的数据不会回来）。
ALTER TABLE repos ADD COLUMN purging INTEGER NOT NULL DEFAULT 0;
