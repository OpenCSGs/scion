//! `scn remote`: the server and repository a working copy belongs to, in git's spelling. A working
//! copy has exactly one, named `origin`.

use scion_client::Code;
use scion_proto::v1::ErrorReason;
use scion_wc::WorkingCopy;
use serde::Serialize;

use crate::commands::{Ctx, connect, split_repo_url};
use crate::output::{CliError, Render};

pub const ORIGIN: &str = "origin";

/// What the text shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Show {
    /// `scn remote`: the name.
    Name,
    /// `scn remote -v`: name and URL, as git prints them.
    Verbose,
    /// `scn remote get-url`.
    Url,
    /// `scn remote set-url`: nothing, like git.
    Nothing,
}

#[derive(Debug, Serialize)]
pub struct RemoteOut {
    pub schema: &'static str,
    pub name: &'static str,
    pub url: String,
    pub endpoint: String,
    pub repo: String,
    #[serde(skip)]
    pub show: Show,
}

impl Render for RemoteOut {
    fn text(&self) -> String {
        match self.show {
            Show::Name => self.name.to_owned(),
            Show::Verbose => format!("{0}\t{1} (fetch)\n{0}\t{1} (push)", self.name, self.url),
            Show::Url => self.url.clone(),
            Show::Nothing => String::new(),
        }
    }
}

/// The only remote name scn knows.
pub fn check_name(name: Option<&str>) -> Result<(), CliError> {
    match name {
        None | Some(ORIGIN) => Ok(()),
        Some(n) => Err(CliError::Usage(format!(
            "no such remote {n:?}: a working copy has one remote, origin"
        ))),
    }
}

fn out(wc: &WorkingCopy, show: Show) -> RemoteOut {
    let s = wc.state();
    RemoteOut {
        schema: "scion.cli.remote/v1",
        name: ORIGIN,
        url: format!("{}/{}", s.endpoint.trim_end_matches('/'), s.repo),
        endpoint: s.endpoint.clone(),
        repo: s.repo.clone(),
        show,
    }
}

pub fn show(ctx: &Ctx, show: Show) -> Result<RemoteOut, CliError> {
    Ok(out(&WorkingCopy::open(&ctx.cwd)?, show))
}

/// `scn remote set-url URL`: a new address for the server, or another repository. The new one
/// must have the commits the working copy is on (its base and any pending commit), so that sync
/// and land keep working.
pub async fn set_url(ctx: &Ctx, url: &str) -> Result<RemoteOut, CliError> {
    let (endpoint, repo) = split_repo_url(url)?
        .filter(|(_, repo)| !repo.is_empty())
        .ok_or_else(|| CliError::Usage(format!("{url}: expected http(s)://host[:port]/REPOSITORY")))?;
    let mut wc = WorkingCopy::open(&ctx.cwd)?;
    let client = connect(&endpoint, Some(&repo)).await?;
    for commit in [&wc.state().base, &wc.state().head].into_iter().flatten() {
        match client.root_tree(commit).await {
            Ok(_) => {}
            Err(e)
                if e.reason() != Some(ErrorReason::RepoNotFound)
                    && matches!(
                        &e,
                        scion_client::Error::Rpc {
                            code: Code::NotFound,
                            ..
                        }
                    ) =>
            {
                return Err(CliError::Usage(format!(
                    "{url} does not have commit {commit}, which this working copy is on; \
                     is it the same repository?"
                )));
            }
            Err(e) => return Err(e.into()),
        }
    }
    wc.set_remote(&endpoint, &repo)?;
    Ok(out(&wc, Show::Nothing))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_and_names() {
        let mut r = RemoteOut {
            schema: "s",
            name: ORIGIN,
            url: "http://h:7080/org/app".into(),
            endpoint: "http://h:7080".into(),
            repo: "org/app".into(),
            show: Show::Name,
        };
        assert_eq!(r.text(), "origin");
        r.show = Show::Verbose;
        assert_eq!(
            r.text(),
            "origin\thttp://h:7080/org/app (fetch)\norigin\thttp://h:7080/org/app (push)"
        );
        r.show = Show::Url;
        assert_eq!(r.text(), "http://h:7080/org/app");
        r.show = Show::Nothing;
        assert_eq!(r.text(), "");
        assert!(check_name(None).is_ok() && check_name(Some("origin")).is_ok());
        assert!(check_name(Some("upstream")).is_err());
    }
}
