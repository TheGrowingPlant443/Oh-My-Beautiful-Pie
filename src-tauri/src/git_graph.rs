use crate::code_diff::{default_branch, git_ok, git_optional, git_timed, repository_root, resolve_commit, text, valid_commit, validate_new_branch, WRITE_LOCK};
use serde::Serialize;
use std::path::Path;

pub type Result<T, E = String> = std::result::Result<T, E>;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphCommit {
    pub hash: String,
    pub parents: Vec<String>,
    pub author: String,
    pub date: String,
    pub subject: String,
    pub refs: Vec<String>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitGraph {
    pub root: String,
    pub head: Option<String>,
    /// Tip of the default branch; its first-parent chain is the main line pull requests are merged into.
    pub main_commit: Option<String>,
    pub current_branch: Option<String>,
    pub commits: Vec<GraphCommit>,
    pub truncated: bool,
}

pub fn graph(project: &Path, limit: u32) -> Result<GitGraph> {
    let root = repository_root(project)?;
    let limit = limit.clamp(1, 5000) as usize;
    let head = resolve_commit(&root, "HEAD")?;
    let count = format!("--max-count={}", limit + 1);
    let mut args = vec!["log", "--exclude=refs/stash", "--all", "--topo-order", &count, "-z", "--format=%H%x1f%P%x1f%an%x1f%aI%x1f%D%x1f%s"];
    // A detached HEAD may not be reachable from any ref included by --all.
    if head.is_some() { args.push("HEAD"); }
    let bytes = git_ok(&root, &args)?;
    let mut commits = Vec::new();
    for record in bytes.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let decoded = String::from_utf8_lossy(record);
        let fields: Vec<&str> = decoded.splitn(6, '\u{1f}').collect();
        if fields.len() != 6 { return Err("Git returned an invalid history record".into()); }
        commits.push(GraphCommit {
            hash: fields[0].into(), parents: fields[1].split_whitespace().map(str::to_string).collect(),
            author: fields[2].into(), date: fields[3].into(), subject: fields[5].into(),
            refs: fields[4].split(", ").filter(|r| !r.is_empty()).map(str::to_string).collect(),
        });
    }
    let truncated = commits.len() > limit;
    commits.truncate(limit);
    let main_commit = default_branch(&root)?.map(|(_, commit)| commit);
    Ok(GitGraph { root: root.display().to_string(), head, main_commit,
        current_branch: git_optional(&root, &["symbolic-ref", "--short", "-q", "HEAD"])?.map(text), commits, truncated })
}

// Never abort an operation started by the user. Git paths also work for linked worktrees.
fn ensure_no_operation(root: &Path) -> Result<()> {
    for marker in ["MERGE_HEAD", "REVERT_HEAD", "CHERRY_PICK_HEAD", "rebase-merge", "rebase-apply", "sequencer"] {
        let path = text(git_ok(root, &["rev-parse", "--path-format=absolute", "--git-path", marker])?);
        if Path::new(&path).exists() { return Err("A Git operation is already in progress; finish or abort it with Git first".into()); }
    }
    Ok(())
}

fn apply_commit(project: &Path, commit: &str, revert: bool) -> Result<String> {
    let _writer = WRITE_LOCK.lock();
    let root = repository_root(project)?;
    valid_commit(&root, commit)?;
    ensure_no_operation(&root)?;
    // Starting clean lets --abort reliably restore the exact state that preceded this action.
    if !git_ok(&root, &["status", "--porcelain", "-z", "--untracked-files=all"])?.is_empty() {
        return Err("Commit or stash your working changes before reverting or cherry-picking".into());
    }
    let parents = text(git_ok(&root, &["rev-list", "--parents", "-n", "1", commit])?);
    let command = if revert { "revert" } else { "cherry-pick" };
    let mut args = vec![command];
    if revert { args.push("--no-edit"); }
    if parents.split_whitespace().count() > 2 { args.extend(["-m", "1"]); }
    args.push(commit);
    if let Err(error) = git_timed(&root, &args, None, 120) {
        // On a failure before Git starts its sequencer there is nothing to abort.
        let active = ["REVERT_HEAD", "CHERRY_PICK_HEAD", "sequencer"].iter().try_fold(false, |active, marker| -> Result<bool> {
            let path = text(git_ok(&root, &["rev-parse", "--path-format=absolute", "--git-path", marker])?);
            Ok(active || Path::new(&path).exists())
        })?;
        let verb = if revert { "Reverting" } else { "Cherry-picking" };
        if active {
            if let Err(abort) = git_timed(&root, &[command, "--abort"], None, 30) {
                return Err(format!("{verb} {} failed: {error}\nAutomatic abort failed; resolve the repository with Git: {abort}", &commit[..7]));
            }
        }
        return Err(format!("{verb} {} failed and was undone: {error}", &commit[..7]));
    }
    let summary = text(git_ok(&root, &["log", "-1", "--format=%h: %s", "HEAD"])?);
    Ok(format!("{} {} as {summary}", if revert { "Reverted" } else { "Cherry-picked" }, &commit[..7]))
}

pub fn revert(project: &Path, commit: &str) -> Result<String> { apply_commit(project, commit, true) }
pub fn cherry_pick(project: &Path, commit: &str) -> Result<String> { apply_commit(project, commit, false) }

pub fn reset_last(project: &Path) -> Result<String> {
    let _writer = WRITE_LOCK.lock();
    let root = repository_root(project)?;
    ensure_no_operation(&root)?;
    let parent = resolve_commit(&root, "HEAD~1")?.ok_or("HEAD has no parent commit to reset to")?;
    git_timed(&root, &["reset", "HEAD~1"], None, 60)?;
    Ok(format!("Reset to {}; the undone changes are unstaged in your working tree", &parent[..7]))
}

pub fn create_branch(project: &Path, name: &str, commit: Option<&str>) -> Result<String> {
    let _writer = WRITE_LOCK.lock();
    let root = repository_root(project)?;
    ensure_no_operation(&root)?;
    let name = name.trim();
    if name.is_empty() { return Err("Enter a name for the new branch".into()); }
    validate_new_branch(&root, name)?;
    let mut args = vec!["checkout", "-b", name];
    if let Some(commit) = commit { valid_commit(&root, commit)?; args.push(commit); }
    git_timed(&root, &args, None, 60)?;
    Ok(format!("Created and switched to {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, process::Command};
    struct Repo(PathBuf);
    impl Repo {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("omp-git-tree-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            let repo = Self(path);
            repo.git(&["init", "-q", "-b", "master"]);
            for (key, value) in [("user.name", "Test"), ("user.email", "test@example.invalid"), ("commit.gpgsign", "false"), ("core.autocrlf", "false")] { repo.git(&["config", key, value]); }
            repo
        }
        fn git(&self, args: &[&str]) -> String {
            let out = Command::new("git").arg("-C").arg(&self.0).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        fn commit(&self, file: &str, contents: &str, message: &str) -> String {
            fs::write(self.0.join(file), contents).unwrap();
            self.git(&["add", "-A"]);self.git(&["commit", "-q", "-m", message]);self.git(&["rev-parse", "HEAD"])
        }
    }
    impl Drop for Repo { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }

    #[test]
    fn graph_lists_merges_refs_authors_and_truncation_without_stash() {
        let repo = Repo::new();
        assert!(graph(&repo.0, 300).unwrap().commits.is_empty());
        let first = repo.commit("a", "one", "c1");
        let second = repo.commit("a", "two", "c2");repo.git(&["tag", "v1"]);
        repo.git(&["switch", "-q", "-c", "feature"]);repo.commit("b", "b", "c3");
        repo.git(&["switch", "-q", "master"]);repo.commit("c", "c", "master side");
        repo.git(&["merge", "--no-ff", "-m", "merge feature", "feature"]);
        fs::write(repo.0.join("a"), "stash").unwrap();repo.git(&["stash", "push", "-q"]);
        let all = graph(&repo.0, 300).unwrap();
        assert_eq!(all.head.as_deref(), Some(repo.git(&["rev-parse", "HEAD"]).as_str()));
        assert_eq!(all.current_branch.as_deref(), Some("master"));
        assert_eq!(all.commits[0].parents.len(), 2);
        assert!(all.commits[0].refs.iter().any(|r| r == "HEAD -> master"));
        assert!(all.commits.iter().find(|c| c.hash == second).unwrap().refs.iter().any(|r| r == "tag: v1"));
        assert!(all.commits.iter().all(|c| c.author == "Test" && !c.subject.starts_with("WIP on")));
        for (i, commit) in all.commits.iter().enumerate() { for parent in &commit.parents { assert!(all.commits.iter().position(|c| &c.hash == parent).unwrap() > i); } }
        let limited = graph(&repo.0, 2).unwrap();assert_eq!(limited.commits.len(), 2);assert!(limited.truncated);
        repo.git(&["checkout", "--detach", &first]);let detached = repo.commit("d", "d", "detached only");
        let view = graph(&repo.0, 300).unwrap();assert!(view.commits.iter().any(|c| c.hash == detached));assert!(view.current_branch.is_none());
    }

    #[test]
    fn revert_creates_a_commit_and_handles_merge_first_parent() {
        let repo = Repo::new();repo.commit("a", "one", "base");let edit = repo.commit("a", "two", "edit");
        revert(&repo.0, &edit).unwrap();assert_eq!(fs::read_to_string(repo.0.join("a")).unwrap(), "one");assert!(repo.git(&["log", "-1", "--format=%s"]).starts_with("Revert \""));
        repo.git(&["switch", "-q", "-c", "feature"]);repo.commit("b", "new", "feature file");repo.git(&["switch", "-q", "master"]);
        repo.git(&["merge", "--no-ff", "-m", "merge", "feature"]);let merge = repo.git(&["rev-parse", "HEAD"]);
        revert(&repo.0, &merge).unwrap();assert!(!repo.0.join("b").exists());
    }

    #[test]
    fn conflicting_actions_are_aborted_and_dirty_or_existing_operations_are_preserved() {
        let repo = Repo::new();let first = repo.commit("a", "one\n", "base");let middle = repo.commit("a", "two\n", "middle");let head = repo.commit("a", "three\n", "last");
        let error = revert(&repo.0, &middle).unwrap_err();assert!(error.contains("failed and was undone"), "{error}");
        assert_eq!(repo.git(&["rev-parse", "HEAD"]), head);assert_eq!(repo.git(&["status", "--porcelain"]), "");assert!(!repo.0.join(".git/REVERT_HEAD").exists());
        repo.git(&["switch", "-q", "-c", "other", &first]);repo.commit("a", "different\n", "conflict");let current = repo.git(&["rev-parse", "HEAD"]);
        let error = cherry_pick(&repo.0, &middle).unwrap_err();assert!(error.contains("failed and was undone"), "{error}");assert_eq!(repo.git(&["rev-parse", "HEAD"]), current);assert_eq!(repo.git(&["status", "--porcelain"]), "");assert!(!repo.0.join(".git/CHERRY_PICK_HEAD").exists());
        fs::write(repo.0.join("a"), "unsaved").unwrap();assert!(revert(&repo.0, &middle).unwrap_err().contains("Commit or stash"));assert_eq!(fs::read_to_string(repo.0.join("a")).unwrap(), "unsaved");
        repo.git(&["restore", "a"]);
        let out = Command::new("git").arg("-C").arg(&repo.0).args(["cherry-pick", &middle]).output().unwrap();assert!(!out.status.success());
        assert!(revert(&repo.0, &middle).unwrap_err().contains("already in progress"));assert!(repo.0.join(".git/CHERRY_PICK_HEAD").exists());
        assert!(reset_last(&repo.0).unwrap_err().contains("already in progress"));assert!(create_branch(&repo.0, "blocked", None).unwrap_err().contains("already in progress"));
        repo.git(&["cherry-pick", "--abort"]);
    }

    #[test]
    fn cherry_pick_reset_and_branch_creation_work_on_selected_commits() {
        let repo = Repo::new();let first = repo.commit("a", "one", "base");assert_eq!(reset_last(&repo.0).unwrap_err(), "HEAD has no parent commit to reset to");
        repo.git(&["switch", "-q", "-c", "feature"]);let pick = repo.commit("b", "b", "pick this");
        repo.git(&["switch", "-q", "master"]);repo.commit("c", "c", "master diverges");
        cherry_pick(&repo.0, &pick).unwrap();assert_eq!(repo.git(&["log", "-1", "--format=%s"]), "pick this");assert_ne!(repo.git(&["rev-parse", "HEAD"]), pick);
        let parent = repo.git(&["rev-parse", "HEAD~1"]);reset_last(&repo.0).unwrap();assert_eq!(repo.git(&["rev-parse", "HEAD"]), parent);assert!(repo.0.join("b").exists());assert!(!repo.git(&["status", "--porcelain"]).is_empty());
        fs::remove_file(repo.0.join("b")).unwrap();
        create_branch(&repo.0, "topic", Some(&first)).unwrap();assert_eq!(repo.git(&["branch", "--show-current"]), "topic");assert_eq!(repo.git(&["rev-parse", "HEAD"]), first);
        create_branch(&repo.0, "topic-two", None).unwrap();assert_eq!(repo.git(&["branch", "--show-current"]), "topic-two");
        for bad in ["", "bad name", "-x", "a..b", "master"] { assert!(create_branch(&repo.0, bad, None).is_err()); }
        for bad in ["HEAD", "--all", "zzzz"] { assert!(create_branch(&repo.0, "bad-start", Some(bad)).unwrap_err().starts_with("Not a commit:")); }
    }
}
