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
mod remote;
mod repos;

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{FromArgMatches, Parser, Subcommand};

use crate::commands::Ctx;
use crate::history::DiffOut;
use crate::output::{CliError, Render, error_out, render, version_out};

#[derive(Debug, Parser)]
#[command(name = "scn", version, about = "Scion command line client")]
struct Cli {
    /// Server address; defaults to the working copy's, then http://127.0.0.1:7080.
    #[arg(
        long,
        global = true,
        env = "SCION_ENDPOINT",
        value_name = "URL",
        help_heading = "Global options"
    )]
    endpoint: Option<String>,
    /// Repository, for commands run outside a working copy: a name, or its URL
    /// http(s)://host[:port]/REPOSITORY (whose server then replaces --endpoint).
    #[arg(long, global = true, env = "SCION_REPO", help_heading = "Global options")]
    repo: Option<String>,
    /// Print JSON (with a schema field) for agents and scripts.
    #[arg(long, global = true, help_heading = "Global options")]
    json: bool,
    /// No progress or notes on standard error (errors are still printed).
    #[arg(short, long, global = true, help_heading = "Global options")]
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
    #[command(after_help = "Examples:\n  scn clone http://scion.example.com:7080/org/app\n  \
        scn clone http://scion.example.com:7080/org/app app-dev --ref dev\n  \
        scn clone http://scion.example.com:7080/org/app --sparse src,docs\n  \
        scn --endpoint http://scion.example.com:7080 clone org/app")]
    Clone {
        /// A repository name, or its URL: http(s)://host[:port]/REPOSITORY.
        repo: String,
        /// The directory to create (default: the repository name's last part).
        dir: Option<PathBuf>,
        /// The branch to check out (git's `-b` / `--branch` work too).
        #[arg(
            long = "ref",
            short = 'b',
            visible_alias = "branch",
            value_name = "BRANCH",
            default_value = "main"
        )]
        reference: String,
        /// Sparse checkout: only these directories (comma-separated) and the files at the root.
        /// Can be repeated: `--sparse src --sparse docs`.
        #[arg(long, value_name = "DIRS", value_delimiter = ',', num_args = 1)]
        sparse: Option<Vec<String>>,
    },
    /// The working copy's server and repository, git-style: `scn remote [-v]`, `scn remote get-url`,
    /// `scn remote set-url URL`. A working copy has one remote, named origin.
    #[command(after_help = "Examples:\n  scn remote -v\n  scn remote get-url\n  \
        scn remote set-url http://192.168.1.11:7080/org/app     # the server's new address")]
    Remote {
        /// Show the URL too.
        #[arg(short, long)]
        verbose: bool,
        #[command(subcommand)]
        cmd: Option<RemoteCmd>,
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
    #[command(after_help = "Examples:\n  scn branch\n  scn branch feature main~2\n  scn branch -d feature")]
    Branch {
        #[command(subcommand)]
        cmd: Option<BranchCmd>,
        /// Create this branch.
        name: Option<String>,
        /// Where the new branch starts (default: the current branch).
        #[arg(value_name = "REV")]
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
    #[command(after_help = "Examples:\n  scn tag\n  scn tag v1.0\n  scn tag v0.9 main~5\n  scn tag -d v0.9")]
    Tag {
        /// Create this tag.
        name: Option<String>,
        /// The commit to tag (default: HEAD in a working copy, else the default branch).
        #[arg(value_name = "REV")]
        revision: Option<String>,
        /// Delete this tag.
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
    #[command(after_help = "Examples:\n  scn commit -m \"Fix the parser\"\n  \
        scn commit --amend                 # fold new changes into the pending commit\n  \
        scn commit                         # finish a sync or merge that stopped on conflicts")]
    Commit {
        /// Required, except with --amend (keeps the old message) or when finishing a sync or
        /// merge that stopped on conflicts (keeps that commit's message). Several -m become
        /// separate paragraphs, as in git.
        #[arg(short, long)]
        message: Vec<String>,
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
        /// The branch to switch to.
        #[arg(value_name = "BRANCH")]
        reference: String,
        /// Create the branch at the current snapshot first.
        #[arg(short = 'c', long)]
        create: bool,
    },
    /// Merge another branch into this one as a pending merge commit (`scn land` publishes it).
    /// On conflicts the files get conflict markers; see `scn resolve`.
    Merge {
        /// The branch to merge (not needed with --abort).
        #[arg(required_unless_present_any = ["abort", "cont"], value_name = "BRANCH")]
        reference: Option<String>,
        /// Abandon a merge that stopped on conflicts.
        #[arg(long, conflicts_with_all = ["message", "author"])]
        abort: bool,
        /// Accepted for git compatibility: scn always makes a merge commit.
        #[arg(long = "no-ff")]
        no_ff: bool,
        /// git's `merge --continue`: transitional spelling of `scn commit`.
        #[arg(long = "continue", hide = true, conflicts_with = "abort")]
        cont: bool,
        /// Message for the merge commit (default: "Merge BRANCH into THIS").
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
    #[command(
        after_help = "Examples:\n  scn resolve                # list the conflicted files\n  \
        scn resolve src/lib.rs     # after editing out the markers\n  \
        scn resolve --theirs .     # take the other side for every file"
    )]
    Resolve {
        /// Conflicted files, relative to the current directory.
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
        /// Take your version of each file.
        #[arg(long, conflicts_with = "theirs")]
        ours: bool,
        /// Take the other side's version (the branch's tip, or the merged branch).
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
        /// Files or directories, relative to the current directory.
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<String>,
    },
    /// User settings: the default commit author (`user.name`, `user.email`).
    /// git-style forms also work: `scn config KEY [VALUE]`, `--list`, `--unset KEY`.
    #[command(
        args_conflicts_with_subcommands = true,
        after_help = "Examples:\n  scn config user.name \"Ada Lovelace\"\n  scn config user.email ada@example.com\n  \
            scn config --list\n  scn config --unset user.email"
    )]
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCmd>,
        /// user.name or user.email: print it, or set it to VALUE.
        key: Option<String>,
        /// The new value.
        value: Option<String>,
        /// Print every setting.
        #[arg(short = 'l', long, conflicts_with_all = ["key", "unset"])]
        list: bool,
        /// Remove this setting.
        #[arg(long, value_name = "KEY", conflicts_with = "key")]
        unset: Option<String>,
        /// Accepted for git compatibility: scn's settings are always per user.
        #[arg(long)]
        global: bool,
    },
    /// Land the pending commit on the branch.
    Land,
    /// Update to the branch tip, rebasing a pending commit onto it.
    Sync,
    /// Print a file; no working copy needed.
    #[command(after_help = "Examples:\n  scn cat README.md\n  scn cat main~1:src/lib.rs\n  \
        scn cat v1.0:Cargo.toml --repo http://scion.example.com:7080/org/app")]
    Cat {
        /// The file, optionally at a revision (default: the current branch).
        #[arg(value_name = "[REV:]PATH")]
        spec: String,
    },
    /// List a directory; no working copy needed.
    #[command(after_help = "Examples:\n  scn ls\n  scn ls src\n  scn ls main~3:docs\n  \
        scn ls main            # a bare revision lists its root; `:DIR` forces a path")]
    Ls {
        /// The directory, optionally at a revision (default: the root of the current branch).
        #[arg(value_name = "[REV:][DIR]")]
        spec: Option<String>,
    },
    /// Download an archive of a revision or a directory; no working copy needed.
    #[command(after_help = "Examples:\n  scn archive -o mono.tar.gz\n  \
        scn archive v1.0:src --format zip --prefix src-v1.0/ -o src.zip\n  \
        scn archive main | tar -x            # to standard output")]
    Archive {
        /// What to archive (default: the whole current branch); a bare revision works too.
        #[arg(value_name = "[REV:][DIR]")]
        spec: Option<String>,
        /// tar.gz, zip or tar.
        #[arg(long, default_value = "tar.gz")]
        format: String,
        /// Prefix for every path in the archive, such as mono-main/.
        #[arg(long, default_value = "")]
        prefix: String,
        /// Output file; standard output when omitted.
        #[arg(short = 'o', long, value_name = "FILE")]
        output: Option<PathBuf>,
    },
    /// History from REV (default: the current branch), newest first.
    #[command(
        after_help = "Examples:\n  scn log\n  scn log --oneline -n 50\n  scn log v1.0 -- src\n  \
        scn log main --repo http://scion.example.com:7080/org/app"
    )]
    Log {
        /// Where to start (default: the current branch).
        #[arg(value_name = "REV")]
        revision: Option<String>,
        /// Only commits that changed this path. Same as `-- PATH`.
        #[arg(long)]
        path: Option<String>,
        /// At most this many commits.
        #[arg(short = 'n', long = "max-count", value_name = "N", default_value_t = 20)]
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
        /// The commit (default: HEAD in a working copy, else the default branch).
        #[arg(value_name = "REV")]
        revision: Option<String>,
        /// Only the changed files' names.
        #[arg(long)]
        name_only: bool,
        /// Only the changed files' names and status (A / M / D).
        #[arg(long, conflicts_with = "name_only")]
        name_status: bool,
        /// `-- PATH`: only changes under this path.
        #[arg(last = true, value_name = "PATH")]
        paths: Vec<String>,
    },
    /// The commit that last changed each line of a file.
    Blame {
        /// The file, optionally at a revision (default: the current branch).
        #[arg(value_name = "[REV:]PATH")]
        spec: String,
    },
    /// Changes: without revisions, the uncommitted local changes; with one, what that commit introduced; with two, the difference between them.
    /// Revisions may be `A B`, `A..B` or `A...B` (from their common ancestor), as in git.
    #[command(
        after_help = "Examples:\n  scn diff                  # uncommitted local changes\n  \
        scn diff main             # what the tip of main changed\n  \
        scn diff v1.0 main        # or v1.0..main; v1.0...main diffs from their common ancestor\n  \
        scn diff HEAD~3 HEAD --name-status -- src"
    )]
    Diff {
        /// None, one or two revisions: `A`, `A B`, `A..B` or `A...B`.
        #[arg(value_name = "REV")]
        revisions: Vec<String>,
        /// Only this path. Same as `-- PATH`.
        #[arg(long)]
        path: Option<String>,
        /// Only the changed files' names.
        #[arg(long)]
        name_only: bool,
        /// Only the changed files' names and status (A / M / D).
        #[arg(long, conflicts_with = "name_only")]
        name_status: bool,
        /// Accepted for git compatibility: scn has no staging area, so every local change is
        /// staged and plain `scn diff` shows them all.
        #[arg(long, visible_alias = "staged")]
        cached: bool,
        /// `-- PATH`, as in git.
        #[arg(last = true, value_name = "PATH")]
        paths: Vec<String>,
    },
    /// Import history from elsewhere.
    #[command(subcommand)]
    Import(ImportCmd),
    /// Log in to the server: in a browser (OIDC device flow) by default, or with an existing token (PAT).
    #[command(
        override_usage = "scn login [SERVER] [TOKEN]\n       scn login [SERVER] --with-token < FILE",
        after_help = "Examples:\n  \
            scn login http://scion.example.com:7080             # in a browser\n  \
            scn login http://scion.example.com:7080 scn_pat_…   # with a token (either order)\n  \
            scn login http://scion.example.com:7080 --with-token < token.txt"
    )]
    Login {
        /// The server, http(s)://host[:port], and/or a token (scn_pat_…), in either order.
        /// Without a server: --endpoint, then the working copy's.
        #[arg(num_args = 0..=2, value_name = "SERVER | TOKEN")]
        args: Vec<String>,
        /// Log in with this token instead of a browser (same as giving it as an argument).
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
enum RemoteCmd {
    /// Print the URL: http(s)://host[:port]/REPOSITORY.
    GetUrl {
        /// origin (the only remote).
        name: Option<String>,
    },
    /// Point the working copy at a new address for its server, or at another repository with the
    /// same commits (a fork, a migrated server). The new one is checked first.
    SetUrl {
        /// `[origin] URL`, as in git.
        #[arg(num_args = 1..=2, required = true, value_name = "URL")]
        args: Vec<String>,
    },
}

#[derive(Debug, Subcommand)]
enum RepoCmd {
    /// Create a repository.
    Create {
        /// The name (such as org/app), or the repository's URL.
        #[arg(value_name = "NAME | URL")]
        name: String,
        /// Default branch (main if omitted).
        #[arg(long, value_name = "BRANCH")]
        default_branch: Option<String>,
        /// A one-line description.
        #[arg(long, value_name = "TEXT", default_value = "")]
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
        /// At most this many.
        #[arg(long, value_name = "N", default_value_t = repos::DEFAULT_LIST_LIMIT)]
        limit: usize,
    },
    /// Show a repository's settings (also for deleted repositories).
    Info {
        /// The name, or the repository's URL.
        #[arg(value_name = "NAME | URL")]
        name: String,
    },
    /// Change the default branch or the description.
    Set {
        /// The name, or the repository's URL.
        #[arg(value_name = "NAME | URL")]
        name: String,
        /// The new default branch (it must exist).
        #[arg(long, value_name = "BRANCH")]
        default_branch: Option<String>,
        /// The new description.
        #[arg(long, value_name = "TEXT")]
        description: Option<String>,
    },
    /// Delete a repository (restorable during the grace period).
    Rm {
        /// The name, or the repository's URL.
        #[arg(value_name = "NAME | URL")]
        name: String,
    },
    /// Restore a deleted repository.
    Restore {
        /// The name, or the repository's URL.
        #[arg(value_name = "NAME | URL")]
        name: String,
    },
    /// Fork: copy all history and branches into a new repository.
    #[command(after_help = "Example:\n  scn repo fork org/app me/app\n  \
        scn repo fork http://scion.example.com:7080/org/app http://scion.example.com:7080/me/app")]
    Fork {
        /// The repository to copy (name or URL).
        source: String,
        /// The new repository (name or URL, on the same server).
        target: String,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCmd {
    /// Print every setting.
    List,
    /// Print one setting.
    Get {
        /// user.name or user.email.
        key: String,
    },
    /// Change one setting.
    #[command(
        after_help = "Examples:\n  scn config set user.name \"Ada Lovelace\"\n  scn config set user.email ada@example.com"
    )]
    Set {
        /// user.name or user.email.
        key: String,
        /// The new value.
        value: String,
    },
    /// Remove one setting.
    Unset {
        /// user.name or user.email.
        key: String,
    },
}

#[derive(Debug, Subcommand)]
enum BranchCmd {
    /// List branches (* marks the current branch; outside a working copy, the default branch).
    Ls,
    /// Create a branch.
    Create {
        /// The new branch.
        name: String,
        /// Where it starts (default: the current branch).
        #[arg(value_name = "REV")]
        revision: Option<String>,
    },
    /// Delete a branch (not the default branch).
    Rm {
        /// The branch to delete.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum SparseCmd {
    /// List the directories in the checkout.
    Ls,
    /// Set the directories (turns a full checkout into a sparse one).
    Set {
        /// Directories, relative to the repository root.
        #[arg(required = true, value_name = "DIR")]
        dirs: Vec<String>,
    },
    /// Add directories and check them out.
    Add {
        /// Directories, relative to the repository root.
        #[arg(required = true, value_name = "DIR")]
        dirs: Vec<String>,
    },
    /// Remove directories and their files (needs no uncommitted changes).
    Rm {
        /// Directories, relative to the repository root.
        #[arg(required = true, value_name = "DIR")]
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
    #[command(after_help = "Examples:\n  scn import git ~/src/app --repo org/app\n  \
        scn import git ~/src/app --repo http://scion.example.com:7080/org/app --git-ref dev --ref dev\n  \
        scn import git ~/src/app --repo org/app --git-ref v1.0 --ref tags/v1.0")]
    Git {
        /// The git repository directory.
        path: PathBuf,
        /// The git ref to import.
        #[arg(long, value_name = "REF", default_value = "main")]
        git_ref: String,
        /// The Scion ref to update after importing.
        #[arg(long = "ref", value_name = "REF", default_value = "main")]
        reference: String,
    },
}

/// A git spelling accepted while people move over: says which scn command to use instead.
fn transitional(git: &str, scn: &str) {
    eprintln!("hint: `scn {git}` is a transitional git-style spelling; use `scn {scn}` instead");
}

/// Several `-m` make one message of paragraphs, as in git.
fn paragraphs(messages: &[String]) -> Option<String> {
    (!messages.is_empty()).then(|| messages.join("\n\n"))
}

/// git's `log -5` is `log -n 5`.
fn git_count_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let args: Vec<std::ffi::OsString> = args.into_iter().collect();
    // The subcommand: the first word that is not an option or a global option's value
    let mut i = 1;
    while let Some(a) = args.get(i).and_then(|a| a.to_str()) {
        match a {
            "--endpoint" | "--repo" => i += 2,
            a if a.starts_with('-') => i += 1,
            _ => break,
        }
    }
    if args.get(i).and_then(|a| a.to_str()) != Some("log") {
        return args;
    }
    let mut out: Vec<std::ffi::OsString> = Vec::with_capacity(args.len() + 1);
    let mut rest = args.into_iter();
    out.extend(rest.by_ref().take(i + 1));
    for a in rest.by_ref() {
        match a.to_str() {
            Some("--") => {
                out.push(a);
                break;
            }
            Some(s) if s.len() > 1 && s.starts_with('-') && s[1..].bytes().all(|b| b.is_ascii_digit()) => {
                out.push("-n".into());
                out.push(s[1..].into());
            }
            _ => out.push(a),
        }
    }
    out.extend(rest);
    out
}

/// `scn login`'s arguments, in either order: the one that is an http(s) URL is the server, the
/// other the token.
fn login_args(args: &[String]) -> Result<(Option<&str>, Option<&str>), CliError> {
    let is_url = |a: &str| a.starts_with("http://") || a.starts_with("https://");
    let (urls, tokens): (Vec<&str>, Vec<&str>) = args.iter().map(String::as_str).partition(|a| is_url(a));
    match (urls.as_slice(), tokens.as_slice()) {
        ([_, _], _) => Err(CliError::Usage("give one server URL".into())),
        (_, [_, _]) => Err(CliError::Usage(
            "expected a server URL, http(s)://host[:port], and a token".into(),
        )),
        (u, t) => Ok((u.first().copied(), t.first().copied())),
    }
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
        Command::Remote { verbose, cmd } => match cmd {
            None => emit(
                remote::show(
                    &ctx,
                    if verbose {
                        remote::Show::Verbose
                    } else {
                        remote::Show::Name
                    },
                ),
                json,
            ),
            Some(RemoteCmd::GetUrl { name }) => {
                arg!(remote::check_name(name.as_deref()));
                emit(remote::show(&ctx, remote::Show::Url), json)
            }
            Some(RemoteCmd::SetUrl { args }) => {
                let (name, url) = match args.as_slice() {
                    [name, url] => (Some(name.as_str()), url.as_str()),
                    _ => (None, args[0].as_str()),
                };
                arg!(remote::check_name(name));
                emit(remote::set_url(&ctx, url).await, json)
            }
        },
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
        } => emit(
            commands::amend(&ctx, paragraphs(&message).as_deref(), author.as_deref()).await,
            json,
        ),
        Command::Commit { message, author, .. } => emit(
            commands::commit(&ctx, paragraphs(&message).as_deref(), author.as_deref()).await,
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
        Command::Config {
            cmd, key, value, unset, ..
        } => {
            let path = config::path(|k| std::env::var(k).ok());
            let cmd = match (cmd, key, value, unset) {
                (Some(cmd), ..) => cmd,
                (None, _, _, Some(key)) => ConfigCmd::Unset { key },
                (None, Some(key), Some(value), None) => ConfigCmd::Set { key, value },
                (None, Some(key), None, None) => ConfigCmd::Get { key },
                // `--list`, or nothing at all
                _ => ConfigCmd::List,
            };
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
            name_status,
            paths,
        } => match one_path(None, paths) {
            Ok(path) => emit(
                history::show(&ctx, revision.as_deref(), path.as_deref(), name_only || name_status)
                    .await
                    .map(|mut s| {
                        s.diff.name_status = name_status;
                        s
                    }),
                json,
            ),
            Err(e) => fail(&e, json),
        },
        Command::Blame { spec } => emit(history::blame(&ctx, &spec).await, json),
        Command::Diff {
            revisions,
            path,
            name_only,
            name_status,
            paths,
            ..
        } => match one_path(path, paths) {
            Ok(path) => emit(
                history::diff(&ctx, &revisions, path.as_deref(), name_only || name_status)
                    .await
                    .map(|d| DiffOut { name_status, ..d }),
                json,
            ),
            Err(e) => fail(&e, json),
        },
        Command::Login {
            args,
            token,
            with_token,
        } => {
            let (server, positional) = arg!(login_args(&args));
            if let Some(s) = server {
                arg!(ctx.server_arg(s));
            }
            let token = match (token, positional) {
                (Some(_), Some(_)) => {
                    return fail(
                        &CliError::Usage("give the token once: as an argument or with --token".into()),
                        json,
                    );
                }
                (_, Some(_)) if with_token => {
                    return fail(
                        &CliError::Usage("give the token as an argument or with --with-token, not both".into()),
                        json,
                    );
                }
                (t, p) => t.or_else(|| p.map(str::to_owned)),
            };
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
    let matches = command().get_matches_from(git_count_args(std::env::args_os()));
    run(Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())).await
}

/// `scn --help` lists the commands by what they are for, like `git --help`.
const COMMANDS: &str = "\
Start a working copy:
  clone      Check out a repository into a new directory
  sparse     Choose the directories a sparse checkout has

Work on a change:
  status     Show local changes
  diff       Show changes: local, of a commit, or between two revisions
  commit     Commit the local changes (published by land)
  uncommit   Drop the pending commit, keeping its changes
  restore    Discard local changes to files
  resolve    Mark conflicts resolved, or take one side

Share and update:
  land       Land the pending commit on the branch (git push)
  sync       Update to the branch tip, rebasing a pending commit (git pull)
  remote     Show or change the working copy's server and repository

Branches and history:
  branch     List, create or delete branches
  tag        List, create or delete tags
  switch     Move the working copy to another branch
  merge      Merge another branch into this one
  log        Show the history
  show       Show a commit and its changes
  blame      Show the commit that last changed each line

Read without a working copy:
  cat        Print a file at a revision
  ls         List a directory at a revision
  archive    Download a revision or directory as tar.gz, zip or tar

Repositories, accounts and setup:
  repo       Create, list, change, delete, restore or fork repositories
  import     Import the history of a git repository
  login      Log in, in a browser or with a token
  logout     Delete the saved credentials
  whoami     Show who you are logged in as
  config     Set the default commit author
  doctor     Check the connection to a server
  daemon     The local cache daemon
  version    Print the client and protocol versions

git spellings (add, rm, mv, push, pull, checkout, reset, rebase) work too and say what to use instead.
`scn help COMMAND` or `scn COMMAND --help` shows a command's options and examples.";

fn command() -> clap::Command {
    use clap::CommandFactory;
    Cli::command().help_template(format!(
        "{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n{COMMANDS}\n\nOptions:\n{{options}}"
    ))
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
    fn git_spellings_of_options() {
        let os = |a: &[&str]| a.iter().map(std::ffi::OsString::from).collect::<Vec<_>>();
        assert_eq!(
            git_count_args(os(&["scn", "log", "-5", "main"])),
            os(&["scn", "log", "-n", "5", "main"])
        );
        assert_eq!(
            git_count_args(os(&["scn", "--repo", "log", "log", "-3", "--", "-4"])),
            os(&["scn", "--repo", "log", "log", "-n", "3", "--", "-4"])
        );
        assert_eq!(git_count_args(os(&["scn", "diff", "-5"])), os(&["scn", "diff", "-5"]));
        assert_eq!(git_count_args(os(&["scn", "log", "-"])), os(&["scn", "log", "-"]));
        assert_eq!(git_count_args(os(&["scn", "-q"])), os(&["scn", "-q"]));
        assert_eq!(paragraphs(&[]), None);
        assert_eq!(paragraphs(&["a".into(), "b".into()]).as_deref(), Some("a\n\nb"));
        let cli = Cli::try_parse_from(["scn", "clone", "r", "-b", "dev"]).expect("parse");
        assert!(matches!(cli.command, Command::Clone { ref reference, .. } if reference == "dev"));
        let cli = Cli::try_parse_from(["scn", "clone", "r", "--branch", "dev"]).expect("parse");
        assert!(matches!(cli.command, Command::Clone { ref reference, .. } if reference == "dev"));
        for args in [
            &["scn", "diff", "--cached"][..],
            &["scn", "diff", "--staged"],
            &["scn", "merge", "--no-ff", "x"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok(), "{args:?}");
        }
    }

    #[test]
    fn help_lists_every_command_once() {
        let mut listed: Vec<&str> = COMMANDS
            .lines()
            .filter_map(|l| l.strip_prefix("  ")?.split_whitespace().next())
            .collect();
        let mut visible: Vec<String> = Cli::command()
            .get_subcommands()
            .filter(|c| !c.is_hide_set() && c.get_name() != "help")
            .map(|c| c.get_name().to_owned())
            .collect();
        listed.sort_unstable();
        visible.sort_unstable();
        assert_eq!(listed, visible);
        let help = command().render_help().to_string();
        assert!(
            help.contains("Share and update:") && help.contains("--endpoint <URL>"),
            "{help}"
        );
    }

    #[test]
    fn every_argument_is_described() {
        fn walk(cmd: &clap::Command, path: &str, missing: &mut Vec<String>) {
            for a in cmd.get_arguments() {
                if !a.is_hide_set() && a.get_help().is_none() && !["help", "version"].contains(&a.get_id().as_str()) {
                    missing.push(format!("{path} {}", a.get_id()));
                }
            }
            for sub in cmd
                .get_subcommands()
                .filter(|c| !c.is_hide_set() && c.get_name() != "help")
            {
                walk(sub, &format!("{path} {}", sub.get_name()), missing);
            }
        }
        let mut missing = Vec::new();
        walk(&Cli::command(), "scn", &mut missing);
        assert!(missing.is_empty(), "arguments without help: {missing:?}");
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
        assert!(matches!(cli.command, Command::Commit { ref message, amend: false, .. } if message == &["msg"]));
        assert!(Cli::try_parse_from(["scn", "commit"]).is_ok(), "checked at run time");
        let cli = Cli::try_parse_from(["scn", "commit", "--amend"]).expect("parse");
        assert!(matches!(
            cli.command,
            Command::Commit {
                ref message,
                amend: true,
                ..
            } if message.is_empty()
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
        assert!(matches!(cli.command, Command::Commit { all: true, ref message, .. } if message == &["msg"]));
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
                ref args,
                token: Some(_),
                with_token: false
            } if args.is_empty()
        ));
        let cli = Cli::try_parse_from(["scn", "login", "https://h", "--token", "t"]).expect("parse");
        assert!(matches!(cli.command, Command::Login { ref args, .. } if args == &["https://h"]));
        let cli = Cli::try_parse_from(["scn", "login", "scn_pat_x", "https://h"]).expect("parse");
        assert!(matches!(cli.command, Command::Login { ref args, .. } if args.len() == 2));
        assert!(Cli::try_parse_from(["scn", "login", "a", "b", "c"]).is_err());
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            login_args(&v(&["scn_pat_x", "https://h"])).ok(),
            Some((Some("https://h"), Some("scn_pat_x")))
        );
        assert_eq!(login_args(&v(&["http://h"])).ok(), Some((Some("http://h"), None)));
        assert_eq!(login_args(&v(&[])).ok(), Some((None, None)));
        assert!(login_args(&v(&["http://a", "http://b"])).is_err());
        assert!(login_args(&v(&["t1", "t2"])).is_err());
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
