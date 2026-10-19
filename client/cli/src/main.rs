//! `scn`：Scion 命令行客户端。
//!
//! 退出码（客户端文档 §8.2）：0 成功、1 一般错误、2 用法错误、3 冲突、
//! 4 可重试错误（已用尽重试）、5 认证失败。

mod commands;
mod config;
mod creds;
mod daemon;
mod history;
mod login;
mod output;
mod repos;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::commands::Ctx;
use crate::output::{CliError, Render, error_out, render, version_out};

#[derive(Debug, Parser)]
#[command(name = "scn", version, about = "Scion command line client")]
struct Cli {
    /// 服务端地址；默认取工作副本记录的地址，再默认 http://127.0.0.1:7080。
    #[arg(long, global = true, env = "SCION_ENDPOINT")]
    endpoint: Option<String>,
    /// 仓库（cat / ls 在工作副本之外使用）。
    #[arg(long, global = true, env = "SCION_REPO")]
    repo: Option<String>,
    /// 输出 JSON（带 schema 字段，供 Agent 和脚本解析）。
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 检查与服务端的连接和能力协商。
    Doctor,
    /// 打印客户端版本与协议版本。
    Version,
    /// 把仓库检出到本地目录。
    Clone {
        repo: String,
        dir: Option<PathBuf>,
        #[arg(long = "ref", default_value = "main")]
        reference: String,
        /// 稀疏检出：只检出这些目录（逗号分隔）和根目录下的文件。
        #[arg(long, value_name = "DIRS", value_delimiter = ',')]
        sparse: Option<Vec<String>>,
    },
    /// 稀疏检出的范围。
    #[command(subcommand)]
    Sparse(SparseCmd),
    /// 仓库管理：创建、列出、修改、删除、恢复、fork。
    #[command(subcommand)]
    Repo(RepoCmd),
    /// 分支：列出、创建、删除（当前工作副本的仓库，或 --repo）。
    #[command(subcommand)]
    Branch(BranchCmd),
    /// 显示本地改动。
    Status,
    /// 把本地改动创建为提交（尚未合入）。
    Commit {
        /// Required, except with --amend (keeps the old message) or when finishing a sync or
        /// merge that stopped on conflicts (keeps that commit's message).
        #[arg(short, long)]
        message: Option<String>,
        /// Replace the pending (not yet landed) commit, adding the current local changes.
        #[arg(long)]
        amend: bool,
        /// Author as "Name <email>". Defaults to `scn config` `user.name` / `user.email`, then git's
        /// `user.name` / `user.email`.
        #[arg(long, env = "SCION_AUTHOR")]
        author: Option<String>,
    },
    /// Move the working copy to another branch (-c creates it at the current snapshot).
    Switch {
        reference: String,
        #[arg(short = 'c', long)]
        create: bool,
    },
    /// Merge another branch into this one as a pending merge commit (`scn land` publishes it).
    /// On conflicts the files get conflict markers; see `scn resolve`.
    Merge {
        /// The branch to merge (not needed with --abort).
        #[arg(required_unless_present = "abort")]
        reference: Option<String>,
        /// Abandon a merge that stopped on conflicts.
        #[arg(long, conflicts_with_all = ["message", "author"])]
        abort: bool,
        #[arg(short, long)]
        message: Option<String>,
        /// Author as "Name <email>"; defaults like `scn commit`.
        #[arg(long, env = "SCION_AUTHOR")]
        author: Option<String>,
    },
    /// Mark conflicted files resolved (after editing out the markers), or take one side's
    /// version. Without PATHS, lists the conflicts. `.` means all.
    Resolve {
        paths: Vec<String>,
        #[arg(long, conflicts_with = "theirs")]
        ours: bool,
        #[arg(long)]
        theirs: bool,
        /// Accept files that still contain conflict markers.
        #[arg(long)]
        force: bool,
    },
    /// Drop the pending (not yet landed) commit; its changes stay as uncommitted local changes.
    Uncommit,
    /// Discard local changes to tracked files under PATHS (`.` for everything). Added files are kept.
    Restore {
        #[arg(required = true)]
        paths: Vec<String>,
    },
    /// User settings: the default commit author (`user.name`, `user.email`).
    #[command(subcommand)]
    Config(ConfigCmd),
    /// 合入已提交的改动。
    Land,
    /// 更新到 ref 的最新提交；有未合入的提交时在其上做 rebase。
    Sync,
    /// 输出文件内容：[REV:]PATH。
    Cat { spec: String },
    /// 列出目录：[REV:][DIR]。
    Ls { spec: Option<String> },
    /// 打包下载：[REV:][DIR]（默认当前 ref 的整个仓库），不需要工作副本。
    Archive {
        spec: Option<String>,
        /// tar.gz、zip 或 tar。
        #[arg(long, default_value = "tar.gz")]
        format: String,
        /// 加在归档中每个路径前面，例如 mono-main/。
        #[arg(long, default_value = "")]
        prefix: String,
        /// 输出文件；不指定时写到标准输出。
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
    },
    /// 历史：从 REV（默认当前 ref）开始，按时间从新到旧。
    Log {
        revision: Option<String>,
        /// 只显示改动了该路径的提交。
        #[arg(long)]
        path: Option<String>,
        /// 最多显示的条数。
        #[arg(short = 'n', long = "max-count", default_value_t = 20)]
        max_count: usize,
    },
    /// 逐行显示最后改动的提交：[REV:]PATH。
    Blame { spec: String },
    /// 差异：不带参数为本地未提交的改动；一个 REV 为该提交引入的改动；两个 REV 为两者之间的差异。
    Diff {
        revisions: Vec<String>,
        /// 只比较该路径。
        #[arg(long)]
        path: Option<String>,
        /// 只列出文件名与状态（A / M / D）。
        #[arg(long)]
        name_only: bool,
    },
    /// 导入外部历史。
    #[command(subcommand)]
    Import(ImportCmd),
    /// 登录服务端：默认在浏览器中完成（OIDC device flow），也可以用已有的令牌（PAT）。
    Login {
        /// 用这个令牌登录，而不是在浏览器中登录。
        #[arg(long, value_name = "TOKEN", conflicts_with = "with_token")]
        token: Option<String>,
        /// 从标准输入读取令牌（避免出现在 shell 历史中）。
        #[arg(long)]
        with_token: bool,
    },
    /// 删除保存的登录凭据。
    Logout,
    /// 显示当前身份。
    Whoami,
    /// 本机 daemon（L1 缓存）：通常由其它命令自动拉起，`SCION_NO_DAEMON=1` 关闭。
    #[command(subcommand)]
    Daemon(DaemonCmd),
}

#[derive(Debug, Subcommand)]
enum RepoCmd {
    /// 创建仓库。
    Create {
        name: String,
        /// 默认分支（默认 main）。
        #[arg(long)]
        default_branch: Option<String>,
        #[arg(long, default_value = "")]
        description: String,
    },
    /// 列出能看到的仓库。
    Ls {
        /// 名字前缀，例如 opencsg/。
        #[arg(default_value = "")]
        prefix: String,
        /// 列出已删除、尚未清除的仓库。
        #[arg(long)]
        deleted: bool,
        #[arg(long, default_value_t = repos::DEFAULT_LIST_LIMIT)]
        limit: usize,
    },
    /// 显示仓库属性（包括已删除的仓库）。
    Info { name: String },
    /// 修改默认分支或描述。
    Set {
        name: String,
        #[arg(long)]
        default_branch: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// 删除仓库（宽限期内可以用 restore 恢复）。
    Rm { name: String },
    /// 恢复已删除的仓库。
    Restore { name: String },
    /// fork：复制全部历史与分支到新仓库。
    Fork { source: String, target: String },
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    /// Print every setting.
    List,
    /// Print one setting.
    Get { key: String },
    /// Change one setting.
    Set { key: String, value: String },
    /// Remove one setting.
    Unset { key: String },
}

#[derive(Debug, Subcommand)]
enum BranchCmd {
    /// 列出分支（* 为默认分支）。
    Ls,
    /// 创建分支，指向 REV（默认为当前跟踪的 ref）。
    Create { name: String, revision: Option<String> },
    /// 删除分支（不能删除默认分支）。
    Rm { name: String },
}

#[derive(Debug, Subcommand)]
pub(crate) enum SparseCmd {
    /// 列出范围内的目录。
    Ls,
    /// 把范围设为这些目录（全部检出的工作副本由此变为稀疏检出）。
    Set {
        #[arg(required = true)]
        dirs: Vec<String>,
    },
    /// 加入目录并检出。
    Add {
        #[arg(required = true)]
        dirs: Vec<String>,
    },
    /// 移出目录并删除其中的文件（不能有未提交的改动）。
    Rm {
        #[arg(required = true)]
        dirs: Vec<String>,
    },
    /// 恢复为全部检出。
    Disable,
}

#[derive(Debug, Subcommand)]
enum DaemonCmd {
    /// 在前台运行 daemon。
    Run {
        /// 磁盘缓存上限（字节）。
        #[arg(long, value_name = "BYTES")]
        max_disk: Option<u64>,
        /// 没有请求多少秒后退出；0 表示一直运行。
        #[arg(long, value_name = "SECS")]
        idle_timeout: Option<u64>,
    },
    /// 查看 daemon 是否在运行。
    Status,
    /// 停止 daemon。
    Stop,
}

#[derive(Debug, Subcommand)]
enum ImportCmd {
    /// 把本地 git 仓库的历史导入 --repo 指定的 Scion 仓库。
    Git {
        /// git 仓库目录。
        path: PathBuf,
        /// 要导入的 git 引用。
        #[arg(long, default_value = "main")]
        git_ref: String,
        /// 导入后更新的 Scion ref。
        #[arg(long = "ref", default_value = "main")]
        reference: String,
    },
}

fn emit<T: Render>(v: Result<T, CliError>, json: bool) -> ExitCode {
    match v {
        Ok(v) => {
            println!("{}", render(&v, json));
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e, json),
    }
}

fn fail(e: &CliError, json: bool) -> ExitCode {
    let out = error_out(e);
    if json {
        println!("{}", serde_json::to_string(&out).unwrap_or_default());
    } else {
        eprintln!("scn: {}", out.error);
        for c in &out.conflicts {
            eprintln!("  conflict ({}): {}", c.kind, c.path);
        }
        if let Some(h) = out.hint {
            eprintln!("hint: {h}");
        }
    }
    ExitCode::from(out.exit_code)
}

async fn run(cli: Cli) -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            return fail(
                &CliError::Usage(format!("cannot read current directory: {e}")),
                cli.json,
            );
        }
    };
    let ctx = Ctx {
        endpoint: cli.endpoint.clone(),
        repo: cli.repo.clone(),
        cwd,
    };
    let json = cli.json;
    match cli.command {
        Command::Version => emit(Ok(version_out()), json),
        Command::Doctor => emit(commands::doctor(&ctx).await, json),
        Command::Clone {
            repo,
            dir,
            reference,
            sparse,
        } => emit(
            commands::clone(&ctx, &repo, dir.as_deref(), &reference, sparse.as_deref()).await,
            json,
        ),
        Command::Sparse(cmd) => emit(commands::sparse(&ctx, cmd).await, json),
        Command::Repo(cmd) => match cmd {
            RepoCmd::Create {
                name,
                default_branch,
                description,
            } => emit(
                repos::create(&ctx, &name, default_branch.as_deref(), &description).await,
                json,
            ),
            RepoCmd::Ls { prefix, deleted, limit } => emit(repos::list(&ctx, &prefix, deleted, limit).await, json),
            RepoCmd::Info { name } => emit(repos::info(&ctx, &name).await, json),
            RepoCmd::Set {
                name,
                default_branch,
                description,
            } => emit(
                repos::update(&ctx, &name, default_branch.as_deref(), description.as_deref()).await,
                json,
            ),
            RepoCmd::Rm { name } => emit(repos::delete(&ctx, &name).await, json),
            RepoCmd::Restore { name } => emit(repos::restore(&ctx, &name).await, json),
            RepoCmd::Fork { source, target } => emit(repos::fork(&ctx, &source, &target).await, json),
        },
        Command::Branch(cmd) => match cmd {
            BranchCmd::Ls => emit(repos::branches(&ctx).await, json),
            BranchCmd::Create { name, revision } => {
                emit(repos::create_branch(&ctx, &name, revision.as_deref()).await, json)
            }
            BranchCmd::Rm { name } => emit(repos::delete_branch(&ctx, &name).await, json),
        },
        Command::Status => emit(commands::status(&ctx), json),
        Command::Commit {
            message,
            amend: true,
            author,
        } => emit(commands::amend(&ctx, message.as_deref(), author.as_deref()).await, json),
        Command::Commit { message, author, .. } => emit(
            commands::commit(&ctx, message.as_deref(), author.as_deref()).await,
            json,
        ),
        Command::Uncommit => emit(commands::uncommit(&ctx).await, json),
        Command::Switch { reference, create } => emit(commands::switch(&ctx, &reference, create).await, json),
        Command::Merge { abort: true, .. } => emit(commands::abort_merge(&ctx).await, json),
        Command::Merge {
            reference,
            message,
            author,
            ..
        } => emit(
            commands::merge(
                &ctx,
                reference.as_deref().unwrap_or_default(),
                message.as_deref(),
                author.as_deref(),
            )
            .await,
            json,
        ),
        Command::Resolve {
            paths,
            ours,
            theirs,
            force,
        } => {
            let side = match (ours, theirs) {
                (true, _) => Some(scion_wc::Side::Ours),
                (_, true) => Some(scion_wc::Side::Theirs),
                _ => None,
            };
            emit(commands::resolve(&ctx, &paths, side, force).await, json)
        }
        Command::Restore { paths } => emit(commands::restore(&ctx, &paths).await, json),
        Command::Config(cmd) => {
            let path = config::path(|k| std::env::var(k).ok());
            emit(
                match cmd {
                    ConfigCmd::List => config::list(&path),
                    ConfigCmd::Get { key } => config::get(&path, &key),
                    ConfigCmd::Set { key, value } => config::set(&path, &key, &value),
                    ConfigCmd::Unset { key } => config::unset(&path, &key),
                },
                json,
            )
        }
        Command::Land => emit(commands::land(&ctx).await, json),
        Command::Sync => emit(commands::sync(&ctx).await, json),
        Command::Ls { spec } => emit(commands::ls(&ctx, spec.as_deref()).await, json),
        Command::Archive {
            spec,
            format,
            prefix,
            output,
        } => {
            let mut stdout = std::io::stdout();
            let r = commands::archive(&ctx, spec.as_deref(), &format, &prefix, output.as_deref(), &mut stdout).await;
            match r {
                // 写到标准输出时不再输出摘要（会混进归档内容）
                Ok(_) if output.is_none() => match std::io::Write::flush(&mut stdout) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => fail(&CliError::Usage(format!("write stdout: {e}")), json),
                },
                other => emit(other, json),
            }
        }
        Command::Log {
            revision,
            path,
            max_count,
        } => emit(
            history::log(&ctx, revision.as_deref(), path.as_deref(), max_count).await,
            json,
        ),
        Command::Blame { spec } => emit(history::blame(&ctx, &spec).await, json),
        Command::Diff {
            revisions,
            path,
            name_only,
        } => emit(history::diff(&ctx, &revisions, path.as_deref(), name_only).await, json),
        Command::Login { token, with_token } => {
            let token = if with_token {
                let mut s = String::new();
                match std::io::Read::read_to_string(&mut std::io::stdin(), &mut s) {
                    Ok(_) => Some(s),
                    Err(e) => return fail(&CliError::Usage(format!("read stdin: {e}")), json),
                }
            } else {
                token
            };
            match token {
                Some(t) => emit(login::login_with_token(&ctx, &t).await, json),
                None => emit(login::login_device(&ctx, |m| eprintln!("{m}")).await, json),
            }
        }
        Command::Logout => emit(login::logout(&ctx), json),
        Command::Whoami => emit(login::whoami(&ctx).await, json),
        Command::Daemon(cmd) => {
            let mut cfg = daemon::config(|k| std::env::var(k).ok());
            match cmd {
                DaemonCmd::Run { max_disk, idle_timeout } => {
                    if let Some(b) = max_disk {
                        cfg.disk_bytes = b;
                    }
                    if let Some(t) = idle_timeout {
                        cfg.idle_timeout = (t > 0).then(|| std::time::Duration::from_secs(t));
                    }
                    match daemon::run(cfg).await {
                        Ok(()) => ExitCode::SUCCESS,
                        Err(e) => fail(&e, json),
                    }
                }
                DaemonCmd::Status => emit(Ok(daemon::status(&cfg).await), json),
                DaemonCmd::Stop => emit(daemon::stop(&cfg).await, json),
            }
        }
        Command::Import(ImportCmd::Git {
            path,
            git_ref,
            reference,
        }) => emit(commands::import_git(&ctx, &path, &git_ref, &reference).await, json),
        // 文本模式下原样输出字节（文件可能不是文本）
        Command::Cat { spec } => match commands::cat(&ctx, &spec).await {
            Ok(out) if !json => {
                let mut stdout = std::io::stdout().lock();
                match stdout.write_all(&out.data).and_then(|()| stdout.flush()) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => fail(&CliError::Usage(format!("write stdout: {e}")), false),
                }
            }
            other => emit(other, json),
        },
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // clap 对用法错误以退出码 2 结束，符合约定
    run(Cli::parse()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parse_global_flags_after_subcommand() {
        let cli = Cli::try_parse_from(["scn", "doctor", "--json", "--endpoint", "http://h:1"]).expect("parse");
        assert!(cli.json);
        assert_eq!(cli.endpoint.as_deref(), Some("http://h:1"));
    }

    #[test]
    fn parse_commands() {
        let cli = Cli::try_parse_from(["scn", "clone", "org/repo", "dir", "--ref", "dev"]).expect("parse");
        assert!(
            matches!(cli.command, Command::Clone { ref repo, ref reference, .. } if repo == "org/repo" && reference == "dev")
        );
        let cli = Cli::try_parse_from(["scn", "clone", "r", "--sparse", "a/b,c"]).expect("parse");
        assert!(
            matches!(cli.command, Command::Clone { sparse: Some(ref d), .. } if d == &["a/b".to_string(), "c".into()])
        );
        let cli = Cli::try_parse_from(["scn", "sparse", "add", "x", "y"]).expect("parse");
        assert!(matches!(cli.command, Command::Sparse(SparseCmd::Add { ref dirs }) if dirs.len() == 2));
        assert!(Cli::try_parse_from(["scn", "sparse", "set"]).is_err(), "dirs required");
        let cli = Cli::try_parse_from(["scn", "repo", "ls", "org/", "--deleted", "--limit", "5"]).expect("parse");
        assert!(matches!(
            cli.command,
            Command::Repo(RepoCmd::Ls {
                deleted: true,
                limit: 5,
                ..
            })
        ));
        let cli = Cli::try_parse_from(["scn", "branch", "create", "dev", "main"]).expect("parse");
        assert!(matches!(
            cli.command,
            Command::Branch(BranchCmd::Create { revision: Some(_), .. })
        ));
        let cli = Cli::try_parse_from(["scn", "sparse", "disable"]).expect("parse");
        assert!(matches!(cli.command, Command::Sparse(SparseCmd::Disable)));
        let cli = Cli::try_parse_from(["scn", "commit", "-m", "msg", "--author", "A <a@b>"]).expect("parse");
        assert!(
            matches!(cli.command, Command::Commit { ref message, amend: false, .. } if message.as_deref() == Some("msg"))
        );
        assert!(Cli::try_parse_from(["scn", "commit"]).is_ok(), "checked at run time");
        let cli = Cli::try_parse_from(["scn", "commit", "--amend"]).expect("parse");
        assert!(matches!(
            cli.command,
            Command::Commit {
                message: None,
                amend: true,
                ..
            }
        ));
        assert!(Cli::try_parse_from(["scn", "restore"]).is_err(), "paths required");
        let cli = Cli::try_parse_from(["scn", "restore", "src", "a.txt"]).expect("parse");
        assert!(matches!(cli.command, Command::Restore { ref paths } if paths.len() == 2));
        assert!(matches!(
            Cli::try_parse_from(["scn", "uncommit"]).expect("parse").command,
            Command::Uncommit
        ));
        let cli =
            Cli::try_parse_from(["scn", "import", "git", "../repo", "--repo", "r", "--git-ref", "dev"]).expect("parse");
        assert!(matches!(cli.command, Command::Import(ImportCmd::Git { ref git_ref, .. }) if git_ref == "dev"));
        let cli = Cli::try_parse_from(["scn", "log", "main", "--path", "src", "-n", "5"]).expect("parse");
        assert!(matches!(cli.command, Command::Log { max_count: 5, .. }));
        let cli = Cli::try_parse_from(["scn", "diff", "a", "b", "--name-only"]).expect("parse");
        assert!(matches!(cli.command, Command::Diff { ref revisions, name_only: true, .. } if revisions.len() == 2));
        let cli = Cli::try_parse_from(["scn", "ls"]).expect("parse");
        assert!(matches!(cli.command, Command::Ls { spec: None }));
        let cli = Cli::try_parse_from(["scn", "daemon", "run", "--idle-timeout", "0"]).expect("parse");
        assert!(matches!(
            cli.command,
            Command::Daemon(DaemonCmd::Run {
                idle_timeout: Some(0),
                ..
            })
        ));
        let cli = Cli::try_parse_from(["scn", "login", "--token", "t"]).expect("parse");
        assert!(matches!(
            cli.command,
            Command::Login {
                token: Some(_),
                with_token: false
            }
        ));
        assert!(Cli::try_parse_from(["scn", "login", "--token", "t", "--with-token"]).is_err());
        let cli = Cli::try_parse_from(["scn", "daemon", "stop"]).expect("parse");
        assert!(matches!(cli.command, Command::Daemon(DaemonCmd::Stop)));
    }

    #[test]
    fn fail_returns_exit_code() {
        assert_eq!(fail(&CliError::Usage("x".into()), true), ExitCode::from(2));
        assert_eq!(fail(&CliError::Wc(scion_wc::Error::Dirty), false), ExitCode::from(1));
    }
}
