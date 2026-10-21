//! `scn`: the Scion command line client.
//!
//! Exit codes (client design §8.2): 0 success, 1 general error, 2 usage error, 3 conflict,
//! 4 retryable error (retries exhausted), 5 authentication failed.

mod commands;
mod config;
mod creds;
mod daemon;
mod history;
mod login;
mod output;
mod progress;
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
    /// Server address; defaults to the working copy's, then http://127.0.0.1:7080.
    #[arg(long, global = true, env = "SCION_ENDPOINT")]
    endpoint: Option<String>,
    /// Repository, for commands run outside a working copy: a name, or its URL
    /// http(s)://host[:port]/REPOSITORY (whose server then replaces --endpoint).
    #[arg(long, global = true, env = "SCION_REPO")]
    repo: Option<String>,
    /// Print JSON (with a schema field) for agents and scripts.
    #[arg(long, global = true)]
    json: bool,
    /// No progress or notes on standard error (errors are still printed).
    #[arg(short, long, global = true)]
    quiet: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check the connection to the server and what it supports.
    Doctor {
        /// The server, http(s)://host[:port]; instead of --endpoint.
        server: Option<String>,
    },
    /// Print the client and protocol versions.
    Version,
    /// Check out a repository into a local directory.
    Clone {
        /// A repository name, or its URL: http(s)://host[:port]/REPOSITORY.
        repo: String,
        dir: Option<PathBuf>,
        #[arg(long = "ref", default_value = "main")]
        reference: String,
        /// Sparse checkout: only these directories (comma-separated) and the files at the root.
        /// Can be repeated: `--sparse src --sparse docs`.
        #[arg(long, value_name = "DIRS", value_delimiter = ',', num_args = 1)]
        sparse: Option<Vec<String>>,
    },
    /// The directories of a sparse checkout.
    #[command(subcommand)]
    Sparse(SparseCmd),
    /// Repositories: create, list, change, delete, restore, fork.
    #[command(subcommand)]
    Repo(RepoCmd),
    /// Branches: list, create, delete (of the working copy's repository, or --repo).
    /// git-style forms also work: `scn branch` (list), `scn branch NAME [REV]`, `scn branch -d NAME`.
    #[command(args_conflicts_with_subcommands = true)]
    Branch {
        #[command(subcommand)]
        cmd: Option<BranchCmd>,
        /// Create this branch (at REV, default the current ref).
        name: Option<String>,
        revision: Option<String>,
        /// Delete this branch.
        #[arg(
            short = 'd',
            long = "delete",
            short_alias = 'D',
            value_name = "NAME",
            conflicts_with = "name"
        )]
        delete: Option<String>,
    },
    /// Tags: fixed names for commits. `scn tag` lists, `scn tag NAME [REV]` creates (default
    /// HEAD), `scn tag -d NAME` deletes. A tag name works as a revision (`scn log v1.0`).
    Tag {
        name: Option<String>,
        revision: Option<String>,
        #[arg(short = 'd', long = "delete", value_name = "NAME", conflicts_with = "name")]
        delete: Option<String>,
    },
    /// Show local changes, like `git status`.
    Status {
        /// git's short format: `XY PATH` per change (every change is staged in scn).
        #[arg(short, long)]
        short: bool,
    },
    /// Commit the local changes (not landed yet; see `scn land`).
    Commit {
        /// Required, except with --amend (keeps the old message) or when finishing a sync or
        /// merge that stopped on conflicts (keeps that commit's message).
        #[arg(short, long)]
        message: Option<String>,
        /// Replace the pending (not yet landed) commit, adding the current local changes.
        #[arg(long)]
        amend: bool,
        /// Accepted for git compatibility (`-am`); scn always commits every local change.
        #[arg(short = 'a', long = "all")]
        all: bool,
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
        #[arg(required_unless_present_any = ["abort", "cont"])]
        reference: Option<String>,
        /// Abandon a merge that stopped on conflicts.
        #[arg(long, conflicts_with_all = ["message", "author"])]
        abort: bool,
        /// git's `merge --continue`: transitional spelling of `scn commit`.
        #[arg(long = "continue", hide = true, conflicts_with = "abort")]
        cont: bool,
        #[arg(short, long)]
        message: Option<String>,
        /// Author as "Name <email>"; defaults like `scn commit`.
        #[arg(long, env = "SCION_AUTHOR")]
        author: Option<String>,
    },
    // ---- Transitional git spellings: run the scn command and print a hint (hidden from --help)
    #[command(hide = true)]
    Push,
    #[command(hide = true)]
    Pull {
        #[arg(long)]
        rebase: bool,
    },
    #[command(hide = true)]
    Checkout {
        /// `-b NAME`: create the branch and switch to it.
        #[arg(short = 'b')]
        create: bool,
        target: Option<String>,
        #[arg(last = true)]
        paths: Vec<String>,
    },
    #[command(hide = true)]
    Reset {
        #[arg(long)]
        soft: bool,
        #[arg(long, conflicts_with = "soft")]
        hard: bool,
        target: Option<String>,
    },
    #[command(hide = true)]
    Rebase {
        #[arg(long = "continue")]
        cont: bool,
        #[arg(long, conflicts_with = "cont")]
        abort: bool,
    },
    #[command(hide = true)]
    Add {
        #[arg(short = 'A', long = "all")]
        all: bool,
        paths: Vec<String>,
    },
    #[command(hide = true)]
    Rm {
        #[arg(short = 'r')]
        recursive: bool,
        #[arg(required = true)]
        paths: Vec<String>,
    },
    #[command(hide = true)]
    Mv { from: PathBuf, to: PathBuf },
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
    /// Land the pending commit on the branch.
    Land,
    /// Update to the branch tip, rebasing a pending commit onto it.
    Sync,
    /// Print a file: [REV:]PATH.
    Cat { spec: String },
    /// List a directory: [REV:][DIR].A bare revision lists its root (`scn ls main`); `:DIR` forces a path.
    Ls { spec: Option<String> },
    /// Download an archive of [REV:][DIR] (default: the whole current branch); no working copy needed.A bare revision works too
    /// (`scn archive main`); `:DIR` forces a path.
    Archive {
        spec: Option<String>,
        /// tar.gz, zip or tar.
        #[arg(long, default_value = "tar.gz")]
        format: String,
        /// Prefix for every path in the archive, such as mono-main/.
        #[arg(long, default_value = "")]
        prefix: String,
        /// Output file; standard output when omitted.
        #[arg(short = 'o', long)]
        output: Option<PathBuf>,
    },
    /// History from REV (default: the current branch), newest first.
    Log {
        revision: Option<String>,
        /// Only commits that changed this path.Same as `-- PATH`.
        #[arg(long)]
        path: Option<String>,
        /// At most this many commits.
        #[arg(short = 'n', long = "max-count", default_value_t = 20)]
        max_count: usize,
        /// One line per commit: its short id and subject.
        #[arg(long)]
        oneline: bool,
        /// `-- PATH`, as in git.
        #[arg(last = true, value_name = "PATH")]
        paths: Vec<String>,
    },
    /// Show a commit (default HEAD) and the changes it introduced, like `git show`.
    Show {
        revision: Option<String>,
        /// Only list file names and status (A / M / D).
        #[arg(long)]
        name_only: bool,
        /// `-- PATH`: only changes under this path.
        #[arg(last = true, value_name = "PATH")]
        paths: Vec<String>,
    },
    /// The commit that last changed each line: [REV:]PATH.
    Blame { spec: String },
    /// Changes: without revisions, the uncommitted local changes; with one, what that commit introduced; with two, the difference between them.
    /// Revisions may be `A B`, `A..B` or `A...B` (from their common ancestor), as in git.
    Diff {
        revisions: Vec<String>,
        /// Only this path.Same as `-- PATH`.
        #[arg(long)]
        path: Option<String>,
        /// Only file names and status (A / M / D).
        #[arg(long)]
        name_only: bool,
        /// `-- PATH`, as in git.
        #[arg(last = true, value_name = "PATH")]
        paths: Vec<String>,
    },
    /// Import history from elsewhere.
    #[command(subcommand)]
    Import(ImportCmd),
    /// Log in to the server: in a browser (OIDC device flow) by default, or with an existing token (PAT).
    Login {
        /// The server, http(s)://host[:port]; instead of --endpoint.
        server: Option<String>,
        /// Log in with this token instead of a browser.
        #[arg(long, value_name = "TOKEN", conflicts_with = "with_token")]
        token: Option<String>,
        /// Read the token from standard input (keeps it out of the shell history).
        #[arg(long)]
        with_token: bool,
    },
    /// Delete the saved credentials.
    Logout {
        /// The server, http(s)://host[:port]; instead of --endpoint.
        server: Option<String>,
    },
    /// Show who you are logged in as.
    Whoami {
        /// The server, http(s)://host[:port]; instead of --endpoint.
        server: Option<String>,
    },
    /// The local daemon (L1 cache), started on demand by other commands; `SCION_NO_DAEMON=1` turns it off.
    #[command(subcommand)]
    Daemon(DaemonCmd),
}

#[derive(Debug, Subcommand)]
enum RepoCmd {
    /// Create a repository.
    Create {
        name: String,
        /// Default branch (main if omitted).
        #[arg(long)]
        default_branch: Option<String>,
        #[arg(long, default_value = "")]
        description: String,
    },
    /// List the repositories you can see.
    Ls {
        /// Name prefix, such as opencsg/.
        #[arg(default_value = "")]
        prefix: String,
        /// List deleted repositories that are not purged yet.
        #[arg(long)]
        deleted: bool,
        #[arg(long, default_value_t = repos::DEFAULT_LIST_LIMIT)]
        limit: usize,
    },
    /// Show a repository's settings (also for deleted repositories).
    Info { name: String },
    /// Change the default branch or the description.
    Set {
        name: String,
        #[arg(long)]
        default_branch: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Delete a repository (restorable during the grace period).
    Rm { name: String },
    /// Restore a deleted repository.
    Restore { name: String },
    /// Fork: copy all history and branches into a new repository.
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
    /// List branches (* marks the default branch).
    Ls,
    /// Create a branch at REV (default: the current branch).
    Create { name: String, revision: Option<String> },
    /// Delete a branch (not the default branch).
    Rm { name: String },
}

#[derive(Debug, Subcommand)]
pub(crate) enum SparseCmd {
    /// List the directories in the checkout.
    Ls,
    /// Set the directories (turns a full checkout into a sparse one).
    Set {
        #[arg(required = true)]
        dirs: Vec<String>,
    },
    /// Add directories and check them out.
    Add {
        #[arg(required = true)]
        dirs: Vec<String>,
    },
    /// Remove directories and their files (needs no uncommitted changes).
    Rm {
        #[arg(required = true)]
        dirs: Vec<String>,
    },
    /// Go back to a full checkout.
    Disable,
}

#[derive(Debug, Subcommand)]
enum DaemonCmd {
    /// Run the daemon in the foreground.
    Run {
        /// Disk cache limit in bytes.
        #[arg(long, value_name = "BYTES")]
        max_disk: Option<u64>,
        /// Exit after this many idle seconds; 0 runs forever.
        #[arg(long, value_name = "SECS")]
        idle_timeout: Option<u64>,
    },
    /// Show whether the daemon is running.
    Status,
    /// Stop the daemon.
    Stop,
}

#[derive(Debug, Subcommand)]
enum ImportCmd {
    /// Import the history of a local git repository into the --repo repository.
    Git {
        /// The git repository directory.
        path: PathBuf,
        /// The git ref to import.
        #[arg(long, default_value = "main")]
        git_ref: String,
        /// The Scion ref to update after importing.
        #[arg(long = "ref", default_value = "main")]
        reference: String,
    },
}

/// A git spelling accepted while people move over: says which scn command to use instead.
fn transitional(git: &str, scn: &str) {
    eprintln!("hint: `scn {git}` is a transitional git-style spelling; use `scn {scn}` instead");
}

/// `--path P` or git's `-- P`; history queries filter by one path.
fn one_path(flag: Option<String>, rest: Vec<String>) -> Result<Option<String>, CliError> {
    match (flag, rest.as_slice()) {
        (p, []) => Ok(p),
        (None, [p]) => Ok(Some(p.trim_end_matches('/').to_owned())),
        _ => Err(CliError::Usage("give one path, with --path or after --".into())),
    }
}

fn emit<T: Render>(v: Result<T, CliError>, json: bool) -> ExitCode {
    match v {
        Ok(v) => {
            // Like git, commands with nothing to say print nothing
            let text = render(&v, json);
            if !text.is_empty() {
                println!("{text}");
            }
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
        // Like git, the conflicts come first
        for c in &out.conflicts {
            eprintln!(
                "CONFLICT ({}): Merge conflict in {}",
                output::git_conflict_kind(&c.kind),
                c.path
            );
        }
        eprintln!("scn: {}", out.error);
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
    let json = cli.json;
    let mut ctx = Ctx {
        endpoint: cli.endpoint.clone(),
        repo: None,
        cwd,
        chatty: !(json || cli.quiet),
    };
    // A repository or server given as a URL replaces --endpoint for this command.
    macro_rules! arg {
        ($e:expr) => {
            match $e {
                Ok(v) => v,
                Err(e) => return fail(&e, json),
            }
        };
    }
    if let Some(r) = &cli.repo {
        ctx.repo = Some(arg!(ctx.named_repo_arg(r)));
    }
    match cli.command {
        Command::Version => emit(Ok(version_out()), json),
        Command::Doctor { server } => {
            if let Some(s) = server {
                arg!(ctx.server_arg(&s));
            }
            emit(commands::doctor(&ctx).await, json)
        }
        Command::Clone {
            repo,
            dir,
            reference,
            sparse,
        } => {
            let repo = arg!(ctx.named_repo_arg(&repo));
            emit(
                commands::clone(&ctx, &repo, dir.as_deref(), &reference, sparse.as_deref()).await,
                json,
            )
        }
        Command::Sparse(cmd) => emit(commands::sparse(&ctx, cmd).await, json),
        Command::Repo(cmd) => match cmd {
            RepoCmd::Create {
                name,
                default_branch,
                description,
            } => {
                let name = arg!(ctx.named_repo_arg(&name));
                emit(
                    repos::create(&ctx, &name, default_branch.as_deref(), &description).await,
                    json,
                )
            }
            RepoCmd::Ls { prefix, deleted, limit } => {
                let prefix = arg!(ctx.repo_arg(&prefix));
                emit(repos::list(&ctx, &prefix, deleted, limit).await, json)
            }
            RepoCmd::Info { name } => {
                let name = arg!(ctx.named_repo_arg(&name));
                emit(repos::info(&ctx, &name).await, json)
            }
            RepoCmd::Set {
                name,
                default_branch,
                description,
            } => {
                let name = arg!(ctx.named_repo_arg(&name));
                emit(
                    repos::update(&ctx, &name, default_branch.as_deref(), description.as_deref()).await,
                    json,
                )
            }
            RepoCmd::Rm { name } => {
                let name = arg!(ctx.named_repo_arg(&name));
                emit(repos::delete(&ctx, &name).await, json)
            }
            RepoCmd::Restore { name } => {
                let name = arg!(ctx.named_repo_arg(&name));
                emit(repos::restore(&ctx, &name).await, json)
            }
            RepoCmd::Fork { source, target } => {
                let (source, target) = arg!(ctx.fork_args(&source, &target));
                emit(repos::fork(&ctx, &source, &target).await, json)
            }
        },
        Command::Branch {
            cmd,
            name,
            revision,
            delete,
        } => match (cmd, name, delete) {
            (Some(BranchCmd::Ls), _, _) | (None, None, None) => emit(repos::branches(&ctx).await, json),
            (Some(BranchCmd::Create { name, revision }), _, _) => {
                emit(repos::create_branch(&ctx, &name, revision.as_deref()).await, json)
            }
            (None, Some(name), _) => emit(repos::create_branch(&ctx, &name, revision.as_deref()).await, json),
            (Some(BranchCmd::Rm { name }), _, _) | (None, None, Some(name)) => {
                emit(repos::delete_branch(&ctx, &name).await, json)
            }
        },
        Command::Tag { name, revision, delete } => match (name, delete) {
            (Some(name), _) => emit(repos::create_tag(&ctx, &name, revision.as_deref()).await, json),
            (None, Some(name)) => emit(repos::delete_tag(&ctx, &name).await, json),
            (None, None) => emit(repos::tags(&ctx).await, json),
        },
        Command::Status { short } => emit(commands::status(&ctx, short), json),
        Command::Commit {
            message,
            amend: true,
            author,
            ..
        } => emit(commands::amend(&ctx, message.as_deref(), author.as_deref()).await, json),
        Command::Commit { message, author, .. } => emit(
            commands::commit(&ctx, message.as_deref(), author.as_deref()).await,
            json,
        ),
        Command::Uncommit => emit(commands::uncommit(&ctx).await, json),
        Command::Switch { reference, create } => emit(commands::switch(&ctx, &reference, create).await, json),
        Command::Merge { abort: true, .. } => emit(commands::abort_merge(&ctx).await, json),
        Command::Merge { cont: true, .. } => {
            transitional("merge --continue", "commit");
            emit(commands::commit(&ctx, None, None).await, json)
        }
        Command::Push => {
            transitional("push", "land");
            emit(commands::land(&ctx).await, json)
        }
        Command::Pull { .. } => {
            transitional("pull", "sync");
            emit(commands::sync(&ctx).await, json)
        }
        Command::Checkout { create, target, paths } => match (create, target, paths.is_empty()) {
            (false, target, false) => {
                let mut all = paths;
                all.extend(target);
                transitional("checkout -- PATH", "restore PATH");
                emit(commands::restore(&ctx, &all).await, json)
            }
            (true, Some(name), true) => {
                transitional("checkout -b NAME", "switch -c NAME");
                emit(commands::switch(&ctx, &name, true).await, json)
            }
            // A path in the current directory restores it, anything else is a branch
            (false, Some(t), true) if ctx.cwd.join(&t).exists() => {
                transitional("checkout PATH", "restore PATH");
                emit(commands::restore(&ctx, &[t]).await, json)
            }
            (false, Some(name), true) => {
                transitional("checkout BRANCH", "switch BRANCH");
                emit(commands::switch(&ctx, &name, false).await, json)
            }
            _ => fail(
                &CliError::Usage(
                    "scn supports `checkout BRANCH`, `checkout -b NAME` and `checkout -- PATH`; \
                     use `scn switch` / `scn restore`"
                        .into(),
                ),
                json,
            ),
        },
        Command::Reset { soft, hard, target } => {
            let head = matches!(target.as_deref(), None | Some("HEAD" | "@"));
            let parent = matches!(
                target.as_deref(),
                None | Some("HEAD~1" | "HEAD~" | "HEAD^" | "@~1" | "@^")
            );
            if soft && parent {
                transitional("reset --soft HEAD~1", "uncommit");
                emit(commands::uncommit(&ctx).await, json)
            } else if hard && head {
                transitional("reset --hard", "restore .");
                emit(commands::restore(&ctx, &[".".to_owned()]).await, json)
            } else {
                fail(
                    &CliError::Usage(
                        "scn maps only `reset --soft HEAD~1` (scn uncommit) and `reset --hard` (scn restore .)".into(),
                    ),
                    json,
                )
            }
        }
        Command::Rebase { cont, abort } => {
            if cont {
                transitional("rebase --continue", "commit");
                emit(commands::commit(&ctx, None, None).await, json)
            } else if abort {
                fail(
                    &CliError::Usage(
                        "a sync that stopped on conflicts cannot be aborted: resolve the files (`scn resolve`) \
                         and `scn commit`, or `scn restore` them to drop your changes"
                            .into(),
                    ),
                    json,
                )
            } else {
                fail(
                    &CliError::Usage("scn rebases with `scn sync` (and on the server during `scn land`)".into()),
                    json,
                )
            }
        }
        Command::Add { .. } => {
            transitional("add", "commit");
            eprintln!("note: there is no staging area; `scn commit` takes every local change");
            ExitCode::SUCCESS
        }
        Command::Rm { paths, recursive } => {
            transitional("rm", "commit (after deleting the files yourself)");
            let mut first_error = None;
            for p in &paths {
                let path = ctx.cwd.join(p);
                let r = if recursive && path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                if let Err(e) = r {
                    first_error.get_or_insert_with(|| CliError::Usage(format!("rm {p}: {e}")));
                }
            }
            match first_error {
                Some(e) => fail(&e, json),
                None => ExitCode::SUCCESS,
            }
        }
        Command::Mv { from, to } => {
            transitional("mv", "commit (after moving the files yourself)");
            let (from, to) = (ctx.cwd.join(from), ctx.cwd.join(to));
            let to = if to.is_dir() {
                match from.file_name() {
                    Some(name) => to.join(name),
                    None => to,
                }
            } else {
                to
            };
            match std::fs::rename(&from, &to) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(&CliError::Usage(format!("mv {}: {e}", from.display())), json),
            }
        }
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
            oneline,
            paths,
        } => match one_path(path, paths) {
            Ok(path) => emit(
                history::log(&ctx, revision.as_deref(), path.as_deref(), max_count, oneline).await,
                json,
            ),
            Err(e) => fail(&e, json),
        },
        Command::Show {
            revision,
            name_only,
            paths,
        } => match one_path(None, paths) {
            Ok(path) => emit(
                history::show(&ctx, revision.as_deref(), path.as_deref(), name_only).await,
                json,
            ),
            Err(e) => fail(&e, json),
        },
        Command::Blame { spec } => emit(history::blame(&ctx, &spec).await, json),
        Command::Diff {
            revisions,
            path,
            name_only,
            paths,
        } => match one_path(path, paths) {
            Ok(path) => emit(history::diff(&ctx, &revisions, path.as_deref(), name_only).await, json),
            Err(e) => fail(&e, json),
        },
        Command::Login {
            server,
            token,
            with_token,
        } => {
            if let Some(s) = server {
                arg!(ctx.server_arg(&s));
            }
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
        Command::Logout { server } => {
            if let Some(s) = server {
                arg!(ctx.server_arg(&s));
            }
            emit(login::logout(&ctx), json)
        }
        Command::Whoami { server } => {
            if let Some(s) = server {
                arg!(ctx.server_arg(&s));
            }
            emit(login::whoami(&ctx).await, json)
        }
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
            Command::Branch {
                cmd: Some(BranchCmd::Create { revision: Some(_), .. }),
                ..
            }
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
        let cli = Cli::try_parse_from(["scn", "log", "HEAD~1", "--oneline", "--", "src"]).expect("parse");
        assert!(matches!(cli.command, Command::Log { ref paths, oneline: true, .. } if paths == &["src"]));
        let cli = Cli::try_parse_from(["scn", "show", "main^2", "--", "a"]).expect("parse");
        assert!(matches!(cli.command, Command::Show { revision: Some(_), ref paths, .. } if paths.len() == 1));
        let cli = Cli::try_parse_from(["scn", "commit", "-am", "msg"]).expect("parse");
        assert!(
            matches!(cli.command, Command::Commit { all: true, ref message, .. } if message.as_deref() == Some("msg"))
        );
        let cli = Cli::try_parse_from(["scn", "clone", "r", "--sparse", "a,b", "--sparse", "c"]).expect("parse");
        assert!(matches!(cli.command, Command::Clone { sparse: Some(ref d), .. } if d.len() == 3));
        for (args, want) in [
            (&["scn", "branch"][..], "ls"),
            (&["scn", "branch", "ls"][..], "ls"),
            (&["scn", "branch", "feat"][..], "create"),
            (&["scn", "branch", "feat", "main~1"][..], "create"),
            (&["scn", "branch", "create", "feat"][..], "create"),
            (&["scn", "branch", "-d", "feat"][..], "rm"),
            (&["scn", "branch", "-D", "feat"][..], "rm"),
            (&["scn", "branch", "rm", "feat"][..], "rm"),
        ] {
            let Command::Branch { cmd, name, delete, .. } = Cli::try_parse_from(args).expect("parse").command else {
                panic!("{args:?}");
            };
            let got = match (cmd, name, delete) {
                (Some(BranchCmd::Ls), ..) | (None, None, None) => "ls",
                (Some(BranchCmd::Create { .. }), ..) | (None, Some(_), _) => "create",
                _ => "rm",
            };
            assert_eq!(got, want, "{args:?}");
        }
        assert!(Cli::try_parse_from(["scn", "branch", "x", "-d", "y"]).is_err());
        // Transitional git spellings
        for args in [
            &["scn", "push"][..],
            &["scn", "pull", "--rebase"],
            &["scn", "checkout", "-b", "x"],
            &["scn", "checkout", "--", "a", "b"],
            &["scn", "reset", "--soft", "HEAD~1"],
            &["scn", "reset", "--hard"],
            &["scn", "rebase", "--continue"],
            &["scn", "merge", "--continue"],
            &["scn", "add", "-A"],
            &["scn", "rm", "-r", "dir"],
            &["scn", "mv", "a", "b"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok(), "{args:?}");
        }
        assert!(Cli::try_parse_from(["scn", "reset", "--soft", "--hard"]).is_err());
        assert!(Cli::try_parse_from(["scn", "merge", "--continue", "--abort"]).is_err());
        assert!(Cli::try_parse_from(["scn", "rm"]).is_err());
        assert_eq!(one_path(Some("a".into()), vec![]).ok(), Some(Some("a".into())));
        assert_eq!(one_path(None, vec!["src/".into()]).ok(), Some(Some("src".into())));
        assert!(one_path(Some("a".into()), vec!["b".into()]).is_err());
        assert!(one_path(None, vec!["a".into(), "b".into()]).is_err());
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
                server: None,
                token: Some(_),
                with_token: false
            }
        ));
        let cli = Cli::try_parse_from(["scn", "login", "https://h", "--token", "t"]).expect("parse");
        assert!(matches!(cli.command, Command::Login { server: Some(ref s), .. } if s == "https://h"));
        for cmd in ["doctor", "logout", "whoami"] {
            assert!(Cli::try_parse_from(["scn", cmd, "http://h:1"]).is_ok(), "{cmd}");
        }
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
