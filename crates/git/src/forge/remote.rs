#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRepo {
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RemoteRepo {
    /// Parses the remote URL forms Git accepts. Requires exactly `owner/name` as the path.
    pub fn parse(url: &str) -> Option<Self> {
        let url = url.trim();
        let (authority, path) = if let Some((_, rest)) = url.split_once("://") {
            rest.split_once('/')?
        } else {
            // scp-like: [user@]host:path
            let (authority, path) = url.split_once(':')?;
            if authority.contains('/') || authority.contains('\\') || authority.len() < 2 {
                return None;
            }
            (authority, path)
        };
        let host = authority.rsplit('@').next()?;
        // Strip the port (not for bracketed IPv6, which is not a forge host anyway).
        let host = host.split(':').next()?.to_ascii_lowercase();
        if host.is_empty() || host.contains(char::is_whitespace) {
            return None;
        }
        let path = path.trim_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let path = path.trim_end_matches('/');
        let mut parts = path.split('/');
        let owner = parts.next()?;
        let name = parts.next()?;
        if parts.next().is_some() || owner.is_empty() || name.is_empty() {
            return None;
        }
        if owner.contains(char::is_whitespace) || name.contains(char::is_whitespace) {
            return None;
        }
        Some(Self {
            host,
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    pub fn is_github(&self) -> bool {
        self.host == "github.com" || self.host.ends_with(".github.com") || self.host.contains("github")
    }

    pub fn gh_repo_arg(&self) -> String {
        if self.host == "github.com" {
            format!("{}/{}", self.owner, self.name)
        } else {
            format!("{}/{}/{}", self.host, self.owner, self.name)
        }
    }

    pub fn web_url(&self) -> String {
        format!("https://{}/{}/{}", self.host, self.owner, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(host: &str, owner: &str, name: &str) -> Option<RemoteRepo> {
        Some(RemoteRepo {
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
        })
    }

    #[test]
    fn parses_all_url_forms() {
        for url in [
            "https://github.com/o/r",
            "https://github.com/o/r.git",
            "https://github.com/o/r/",
            "https://github.com/o/r.git/",
            "https://user@github.com/o/r.git",
            "https://user:token@GitHub.com/o/r.git",
            "git@github.com:o/r.git",
            "git@github.com:o/r",
            "ssh://git@github.com/o/r.git",
            "ssh://git@github.com:22/o/r.git",
            "git://github.com/o/r",
            "  https://github.com/o/r.git\n",
        ] {
            assert_eq!(RemoteRepo::parse(url), repo("github.com", "o", "r"), "{url}");
        }
    }

    #[test]
    fn keeps_dots_in_names_and_ghe_hosts() {
        assert_eq!(
            RemoteRepo::parse("git@git.corp.example.com:team/my.repo.git"),
            repo("git.corp.example.com", "team", "my.repo")
        );
        assert_eq!(
            RemoteRepo::parse("https://github.acme.com/o/r"),
            repo("github.acme.com", "o", "r")
        );
    }

    #[test]
    fn rejects_invalid_inputs() {
        for url in [
            "",
            "   ",
            "https://github.com",
            "https://github.com/",
            "https://github.com/o",
            "https://github.com/o/",
            "https://github.com/o/r/extra",
            "https://github.com//r",
            "file:///home/me/repo",
            "/home/me/repo",
            "C:/Users/me/repo",
            "C:\\Users\\me\\repo",
            "../relative/repo",
            "git@github.com:o",
            "git@github.com:",
            "https://github.com/o/.git",
        ] {
            assert_eq!(RemoteRepo::parse(url), None, "{url}");
        }
    }

    #[test]
    fn github_detection() {
        assert!(repo("github.com", "o", "r").unwrap().is_github());
        assert!(repo("gist.github.com", "o", "r").unwrap().is_github());
        assert!(repo("github.acme.com", "o", "r").unwrap().is_github());
        assert!(!repo("gitlab.com", "o", "r").unwrap().is_github());
        assert!(!repo("bitbucket.org", "o", "r").unwrap().is_github());
    }

    #[test]
    fn gh_arg_and_web_url() {
        let dotcom = repo("github.com", "o", "r").unwrap();
        assert_eq!(dotcom.gh_repo_arg(), "o/r");
        assert_eq!(dotcom.web_url(), "https://github.com/o/r");
        let ghe = repo("github.acme.com", "o", "r").unwrap();
        assert_eq!(ghe.gh_repo_arg(), "github.acme.com/o/r");
        assert_eq!(ghe.web_url(), "https://github.acme.com/o/r");
    }
}
