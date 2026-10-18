-- ref 变化的事件流：与 ref 的修改在同一个事务里写入，消费方按 seq 增量读取（EventService、事件推送）。
-- 记录仓库名而不是 repo_id：仓库清除后事件仍然可读，直到按保留期清理。
CREATE TABLE ref_events (
    seq           BIGINT PRIMARY KEY,
    repo          TEXT    NOT NULL,
    ref           TEXT    NOT NULL,
    old_target    BYTEA,
    new_target    BYTEA,
    version       BIGINT NOT NULL,
    created_at_ms BIGINT NOT NULL
);
CREATE INDEX ref_events_repo ON ref_events (repo, seq);
CREATE INDEX ref_events_created ON ref_events (created_at_ms);

-- seq 计数器（单行）。与 redaction_seq 相同：写入事务先更新这一行，行锁让 seq 顺序与提交顺序一致，
-- 增量读取不会漏掉晚提交的小序号。
CREATE TABLE ref_event_seq (
    id  BIGINT PRIMARY KEY CHECK (id = 1),
    seq BIGINT NOT NULL
);
INSERT INTO ref_event_seq (id, seq) VALUES (1, 0);
