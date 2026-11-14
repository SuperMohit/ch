//! A run's workspace: the copy of the task's repo, its git baseline, the diff after the
//! run, and the check files copied over it.
//!
//! The host runs git only on the task's own files, before the agent starts: the
//! baseline commit. Its git directory is then copied to `baseline.git` in the run
//! directory, out of the agent's reach. Everything after the agent ran treats the
//! workspace as hostile: the diff runs [`DIFF_SCRIPT`] in a container with no network
//! and a time limit, against a scratch copy of `baseline.git` (the workspace's own
//! `.git` can name commands for git to run, and a gitfile can point at a FIFO or a host
//! path), and however the run ends, every `.git` (in any case) in the workspace and the
//! run's ch home is removed by a [`Scrub`], so git on the host never reads agent-written
//! config there.

use std::ffi::{CStr, CString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context as _, bail};
use nix::dir::{Dir, Type};
use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag};
use nix::sys::stat::{FchmodatFlags, Mode, SFlag, fchmod, fchmodat, fstat, fstatat};
use nix::unistd::{UnlinkatFlags, unlinkat};

/// The commit the agent starts from has a fixed author and date, so its id depends
/// only on the files.
const BASELINE_ENV: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "ch-eval"),
    ("GIT_AUTHOR_EMAIL", "ch-eval@localhost"),
    ("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z"),
    ("GIT_COMMITTER_NAME", "ch-eval"),
    ("GIT_COMMITTER_EMAIL", "ch-eval@localhost"),
    ("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z"),
];

/// Copies the directory `from` to `to`, which must not exist. Symlinks are copied as
/// links. A `.git` directory at the top is left out, so the baseline is the only commit.
pub fn copy_repo(from: &Path, to: &Path) -> anyhow::Result<()> {
    copy_tree(from, to, true)
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))
}

/// Copies the directory `from` to `to`, which must not exist, symlinks as links.
pub fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    copy_tree(from, to, false)
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))
}

fn copy_tree(from: &Path, to: &Path, skip_git: bool) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        if skip_git && entry.file_name() == ".git" {
            continue;
        }
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
        } else if kind.is_dir() {
            copy_tree(&src, &dst, false)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Copies the files under `from` over `to`, replacing what is there. Whatever the agent
/// left at a destination is removed first, so a symlink is replaced rather than
/// followed out of the workspace, and a file where a directory is needed gives way.
/// Directories the agent made unwritable are made writable first.
pub fn overlay(from: &Path, to: &Path) -> anyhow::Result<()> {
    overlay_dir(from, to)
        .with_context(|| format!("copying {} over {}", from.display(), to.display()))
}

/// Gives the owner access to the directory `dir` itself (not what is under it).
fn writable(dir: &Path) -> std::io::Result<()> {
    let mode = std::fs::symlink_metadata(dir)?.permissions().mode();
    if mode & 0o700 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o700))?;
    }
    Ok(())
}

fn overlay_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    writable(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let existing = std::fs::symlink_metadata(&dst).ok();
        if entry.file_type()?.is_dir() {
            match existing {
                Some(m) if m.is_dir() => {}
                Some(_) => {
                    std::fs::remove_file(&dst)?;
                    std::fs::create_dir(&dst)?;
                }
                None => std::fs::create_dir(&dst)?,
            }
            overlay_dir(&src, &dst)?;
        } else {
            match existing {
                Some(m) if m.is_dir() => {
                    walk(&dst, Walk::RemoveAll)?;
                    std::fs::remove_dir(&dst)?;
                }
                Some(_) => std::fs::remove_file(&dst)?,
                None => {}
            }
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Commits everything in `work` as the baseline, in a new repository at `work/.git`,
/// and keeps a copy of that git directory at `baseline`. The files are the task's, not
/// yet touched by the agent.
pub fn commit_baseline(work: &Path, baseline: &Path) -> anyhow::Result<()> {
    let git_dir = work.join(".git");
    git(work, &git_dir, &["init", "--quiet", "--template="])?;
    git(work, &git_dir, &["add", "--all"])?;
    git(
        work,
        &git_dir,
        &[
            "commit",
            "--quiet",
            "--allow-empty",
            "--no-verify",
            "-m",
            "baseline",
        ],
    )?;
    copy_tree(&git_dir, baseline, false)
        .with_context(|| format!("copying {} to {}", git_dir.display(), baseline.display()))
}

/// Takes the diff against the baseline. Runs with `GD` set to a scratch copy of the
/// baseline's git directory (writable: `git add` updates its index) and `WT` to the
/// workspace, with no global or system git config; stdout is the patch. It exits
/// non-zero when `git add` or `git diff` failed, after printing what it could: `add
/// --ignore-errors` goes on past files it can't read, and a nested repository without
/// a commit is left out with an error.
pub const DIFF_SCRIPT: &str = r#"g() {
  git --git-dir="$GD" --work-tree="$WT" -c safe.directory='*' -c core.hooksPath=/dev/null \
    -c core.fsmonitor=false -c core.quotePath=false "$@"
}
g add --all --ignore-errors >/dev/null
added=$?
g diff --cached --no-ext-diff --no-textconv
diffed=$?
[ "$added" -eq 0 ] && [ "$diffed" -eq 0 ]
"#;

/// The environment [`DIFF_SCRIPT`] needs besides `GD` and `WT`.
pub const DIFF_ENV: &[(&str, &str)] = &[
    ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ("GIT_CONFIG_NOSYSTEM", "1"),
];

/// Removes every entry named `.git`, in any case (`.Git` is `.git` on a case-insensitive
/// file system), under `dir`, at any depth and of any kind (directory, gitfile,
/// symlink, FIFO), without following symlinks. The top level goes first. Returns how
/// many.
pub fn remove_git_entries(dir: &Path) -> anyhow::Result<usize> {
    walk(dir, Walk::Scrub).with_context(|| format!("removing .git entries under {}", dir.display()))
}

/// Removes the `.git` entries under its directories when the run is over: at
/// [`Scrub::finish`], or when dropped on any other way out (an error, a cut-off run,
/// a panic, an interrupted matrix).
#[derive(Debug)]
pub struct Scrub {
    dirs: Vec<PathBuf>,
    done: bool,
}

impl Scrub {
    pub fn new(dirs: Vec<PathBuf>) -> Self {
        Self { dirs, done: false }
    }

    /// Removes them now, keeping the guard for later. Returns how many there were.
    pub fn now(&self) -> anyhow::Result<usize> {
        let mut removed = 0;
        for dir in &self.dirs {
            if dir.exists() {
                removed += remove_git_entries(dir)?;
            }
        }
        Ok(removed)
    }

    /// Removes them now, and returns how many there were.
    pub fn finish(mut self) -> anyhow::Result<usize> {
        self.done = true;
        self.now()
    }
}

impl Drop for Scrub {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.now();
        }
    }
}

/// Gives the owner read, write and search permission on `dir` and every directory under
/// it (symlinks aren't followed), so files the agent locked can be replaced or removed.
pub fn make_writable(dir: &Path) -> anyhow::Result<()> {
    walk(dir, Walk::Writable)
        .map(drop)
        .with_context(|| format!("making {} writable", dir.display()))
}

/// Removes `dir` and everything in it, whatever depth, path length or permissions the
/// agent left.
pub fn remove_dir(dir: &Path) -> anyhow::Result<()> {
    walk(dir, Walk::RemoveAll)
        .map_err(anyhow::Error::from)
        .and_then(|_| std::fs::remove_dir(dir).map_err(anyhow::Error::from))
        .with_context(|| format!("removing {}", dir.display()))
}

/// What [`walk`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walk {
    /// Remove the `.git` entries.
    Scrub,
    /// Only make every directory accessible.
    Writable,
    /// Remove everything under the root.
    RemoveAll,
}

/// A directory being walked: its name in its parent, what is left to look at in it
/// (each name, and whether it is a directory), and whether it goes too.
struct Frame {
    name: CString,
    todo: Vec<(CString, bool)>,
    remove: bool,
}

/// Opens directories without following symlinks.
fn dir_flags() -> OFlag {
    OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC
}

/// Walks the tree under `root` with fd-relative calls and one directory open at a time,
/// climbing back through `..`, so neither its depth nor its path length matters (macOS
/// refuses paths of 1024 bytes and more). Every directory is made accessible to the
/// owner on the way. The agent is no longer running when this is called, so nothing
/// moves underneath it. Returns how many `.git` entries it removed.
fn walk(root: &Path, mode: Walk) -> std::io::Result<usize> {
    let meta = std::fs::symlink_metadata(root)?;
    if !meta.is_dir() {
        return Ok(0);
    }
    if meta.permissions().mode() & 0o700 != 0o700 {
        std::fs::set_permissions(
            root,
            std::fs::Permissions::from_mode(meta.permissions().mode() | 0o700),
        )?;
    }
    let mut cur = Dir::open(root, dir_flags(), Mode::empty())?;
    let mut todo = list(&mut cur)?;
    // The top level's `.git` goes first, so a failure deeper down can't leave it.
    todo.sort_by_key(|(name, _)| is_git(name));
    let mut stack = vec![Frame {
        name: CString::default(),
        todo,
        remove: mode == Walk::RemoveAll,
    }];
    let mut removed = 0;
    while let Some(frame) = stack.last_mut() {
        match frame.todo.pop() {
            Some((name, is_dir)) => {
                let git = mode == Walk::Scrub && is_git(&name);
                let remove = frame.remove || git;
                if git {
                    removed += 1;
                }
                if is_dir {
                    cur = open_child(&cur, &name)?;
                    let todo = list(&mut cur)?;
                    stack.push(Frame { name, todo, remove });
                } else if remove {
                    unlinkat(&cur, name.as_c_str(), UnlinkatFlags::NoRemoveDir)?;
                }
            }
            None => {
                let done = stack.pop().unwrap_or(Frame {
                    name: CString::default(),
                    todo: Vec::new(),
                    remove: false,
                });
                if stack.is_empty() {
                    break;
                }
                cur = Dir::openat(&cur, "..", dir_flags(), Mode::empty())?;
                if done.remove {
                    unlinkat(&cur, done.name.as_c_str(), UnlinkatFlags::RemoveDir)?;
                }
            }
        }
    }
    Ok(removed)
}

fn is_git(name: &CStr) -> bool {
    name.to_bytes().eq_ignore_ascii_case(b".git")
}

/// The entries of `dir`, but `.` and `..`, each with whether it is a directory (a
/// symlink isn't).
fn list(dir: &mut Dir) -> std::io::Result<Vec<(CString, bool)>> {
    let mut names = Vec::new();
    for entry in dir.iter() {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        names.push((name.to_owned(), entry.file_type()));
    }
    let mut out = Vec::with_capacity(names.len());
    for (name, kind) in names {
        let is_dir = match kind {
            Some(t) => t == Type::Directory,
            None => {
                let st = fstatat(&*dir, name.as_c_str(), AtFlags::AT_SYMLINK_NOFOLLOW)?;
                SFlag::from_bits_truncate(st.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR
            }
        };
        out.push((name, is_dir));
    }
    Ok(out)
}

/// Opens the directory `name` in `parent`, first giving the owner access if it was
/// taken away, and leaves it readable, writable and searchable by the owner.
fn open_child(parent: &Dir, name: &CStr) -> std::io::Result<Dir> {
    let opened = match Dir::openat(parent, name, dir_flags(), Mode::empty()) {
        Err(Errno::EACCES) => {
            let st = fstatat(parent, name, AtFlags::AT_SYMLINK_NOFOLLOW)?;
            let mode = Mode::from_bits_truncate(st.st_mode) | Mode::S_IRWXU;
            // It is a directory, not a symlink, so following is safe.
            fchmodat(parent, name, mode, FchmodatFlags::FollowSymlink)?;
            Dir::openat(parent, name, dir_flags(), Mode::empty())?
        }
        other => other?,
    };
    let st = fstat(&opened)?;
    let mode = Mode::from_bits_truncate(st.st_mode);
    if !mode.contains(Mode::S_IRWXU) {
        fchmod(&opened, mode | Mode::S_IRWXU)?;
    }
    Ok(opened)
}

/// Runs git on `work` with the git directory `git_dir`, ignoring any global or system
/// config, hooks and fsmonitor. Returns stdout.
fn git(work: &Path, git_dir: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .arg("--work-tree")
        .arg(work)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(work)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .envs(BASELINE_ENV.iter().copied())
        .stdin(Stdio::null())
        .output()
        .context("running git (is it installed?)")?;
    if !out.status.success() {
        bail!(
            "git {} in {}: {}",
            args.join(" "),
            work.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn copies_the_repo_without_its_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("src/.git")).unwrap();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".git/HEAD"), "x").unwrap();
        fs::write(repo.join("src/a.txt"), "a").unwrap();
        std::os::unix::fs::symlink("src/a.txt", repo.join("link")).unwrap();
        let work = dir.path().join("work");
        copy_repo(&repo, &work).unwrap();
        assert!(!work.join(".git").exists());
        assert!(
            work.join("src/.git").is_dir(),
            "only the top .git is left out"
        );
        assert_eq!(fs::read_to_string(work.join("src/a.txt")).unwrap(), "a");
        assert_eq!(
            fs::read_link(work.join("link")).unwrap(),
            Path::new("src/a.txt")
        );
    }

    #[test]
    fn overlay_replaces_links_instead_of_following_them() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "keep").unwrap();
        let outside_dir = dir.path().join("outside-dir");
        fs::create_dir(&outside_dir).unwrap();

        let work = dir.path().join("work");
        fs::create_dir_all(work.join("tests")).unwrap();
        std::os::unix::fs::symlink(&outside, work.join("check.py")).unwrap();
        std::os::unix::fs::symlink(&outside_dir, work.join("hidden")).unwrap();
        fs::write(work.join("tests/data"), "a file where a directory goes").unwrap();
        fs::create_dir(work.join("conf")).unwrap();
        fs::write(work.join("conf/x"), "a directory where a file goes").unwrap();

        let verify = dir.path().join("verify");
        fs::create_dir_all(verify.join("hidden")).unwrap();
        fs::create_dir_all(verify.join("tests/data")).unwrap();
        fs::write(verify.join("check.py"), "check").unwrap();
        fs::write(verify.join("hidden/t.py"), "t").unwrap();
        fs::write(verify.join("tests/data/d.txt"), "d").unwrap();
        fs::write(verify.join("conf"), "c").unwrap();
        overlay(&verify, &work).unwrap();

        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        assert_eq!(fs::read_dir(&outside_dir).unwrap().count(), 0);
        assert_eq!(fs::read_to_string(work.join("check.py")).unwrap(), "check");
        assert!(
            !fs::symlink_metadata(work.join("hidden"))
                .unwrap()
                .is_symlink()
        );
        assert_eq!(fs::read_to_string(work.join("hidden/t.py")).unwrap(), "t");
        assert_eq!(
            fs::read_to_string(work.join("tests/data/d.txt")).unwrap(),
            "d"
        );
        assert_eq!(fs::read_to_string(work.join("conf")).unwrap(), "c");
    }

    /// Runs the diff script on the host against a scratch copy of `baseline`.
    fn run_diff(work: &Path, baseline: &Path) -> (String, bool) {
        let scratch = baseline.with_extension("scratch");
        let _ = std::fs::remove_dir_all(&scratch);
        copy_dir(baseline, &scratch).unwrap();
        let out = Command::new("sh")
            .args(["-c", DIFF_SCRIPT])
            .env("GD", &scratch)
            .env("WT", work)
            .envs(DIFF_ENV.iter().copied())
            .output()
            .unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            out.status.success(),
        )
    }

    #[test]
    fn diff_against_the_baseline_ignores_the_workspace_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        fs::create_dir(&work).unwrap();
        fs::write(work.join("a.txt"), "one\n").unwrap();
        fs::write(work.join("gone.txt"), "bye\n").unwrap();
        let baseline = dir.path().join("baseline.git");
        commit_baseline(&work, &baseline).unwrap();
        assert!(work.join(".git").is_dir());
        assert_eq!(run_diff(&work, &baseline), (String::new(), true));

        // The agent edits, adds, deletes, and plants a command in its own git config.
        fs::write(work.join("a.txt"), "two\n").unwrap();
        fs::write(work.join("new.txt"), "new\n").unwrap();
        fs::remove_file(work.join("gone.txt")).unwrap();
        let marker = dir.path().join("ran");
        fs::write(
            work.join(".git/config"),
            format!("[core]\n\tfsmonitor = touch {}\n", marker.display()),
        )
        .unwrap();
        let (patch, ok) = run_diff(&work, &baseline);
        assert!(ok);
        assert!(patch.contains("-one\n+two"), "{patch}");
        assert!(patch.contains("+++ b/new.txt"), "{patch}");
        assert!(patch.contains("--- a/gone.txt"), "{patch}");
        assert!(!patch.contains(".git/"), "{patch}");
        assert!(!marker.exists(), "the workspace's git config must not run");
    }

    #[test]
    fn the_diff_is_best_effort() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        fs::create_dir(&work).unwrap();
        fs::write(work.join("a.txt"), "one\n").unwrap();
        let baseline = dir.path().join("baseline.git");
        commit_baseline(&work, &baseline).unwrap();
        fs::write(work.join("a.txt"), "two\n").unwrap();
        // An unreadable file and a nested repository with no commit.
        fs::write(work.join("locked.txt"), "x").unwrap();
        fs::set_permissions(work.join("locked.txt"), fs::Permissions::from_mode(0o000)).unwrap();
        fs::create_dir_all(work.join("nested/.git")).unwrap();
        let (patch, ok) = run_diff(&work, &baseline);
        assert!(!ok);
        assert!(
            patch.contains("+two"),
            "what could be read is still there: {patch}"
        );
        fs::set_permissions(work.join("locked.txt"), fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn removes_every_git_entry_and_locked_directories() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        fs::create_dir_all(work.join(".git/objects")).unwrap();
        fs::create_dir_all(work.join("a/b")).unwrap();
        fs::write(work.join("a/.git"), "gitdir: /etc").unwrap();
        std::os::unix::fs::symlink("/tmp", work.join("a/b/.git")).unwrap();
        let fifo = work.join("a/fifo-holder");
        fs::create_dir(&fifo).unwrap();
        let made = Command::new("mkfifo")
            .arg(fifo.join(".git"))
            .status()
            .unwrap();
        assert!(made.success());
        fs::write(work.join("a/keep.txt"), "k").unwrap();
        // A directory the agent locked.
        fs::create_dir_all(work.join("locked/.git")).unwrap();
        fs::set_permissions(work.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

        // Any case: `.Git` is `.git` to git on a case-insensitive file system.
        fs::create_dir_all(work.join("c/.Git/hooks")).unwrap();
        fs::create_dir_all(work.join("d")).unwrap();
        fs::write(work.join("d/.GIT"), "gitdir: /tmp").unwrap();

        assert_eq!(remove_git_entries(&work).unwrap(), 7);
        assert!(work.join("a/keep.txt").is_file());
        assert!(fs::symlink_metadata(work.join("a/b/.git")).is_err());
        assert!(
            Path::new("/tmp").is_dir(),
            "a symlink is removed, not followed"
        );

        fs::set_permissions(work.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
        remove_dir(&work).unwrap();
        assert!(!work.exists());
    }

    #[test]
    fn scrub_cleans_up_on_drop_and_counts_on_finish() {
        let dir = tempfile::tempdir().unwrap();
        let (work, home) = (dir.path().join("work"), dir.path().join("ch-home"));
        let plant = || {
            fs::create_dir_all(work.join(".git")).unwrap();
            fs::create_dir_all(home.join("sessions/.Git")).unwrap();
        };
        plant();
        drop(Scrub::new(vec![work.clone(), home.clone()]));
        assert!(!work.join(".git").exists());
        assert!(!home.join("sessions/.Git").exists());
        plant();
        let removed = Scrub::new(vec![work.clone(), home.clone(), dir.path().join("missing")])
            .finish()
            .unwrap();
        assert_eq!(removed, 2);
    }

    /// Makes `levels` nested directories with 200-character names under `dir`, with
    /// fd-relative calls (their absolute paths are too long for macOS), and returns the
    /// deepest.
    fn deep(dir: &Path, levels: usize) -> Dir {
        let name = "d".repeat(200);
        let mut cur = Dir::open(dir, dir_flags(), Mode::empty()).unwrap();
        for _ in 0..levels {
            nix::sys::stat::mkdirat(&cur, name.as_str(), Mode::S_IRWXU).unwrap();
            cur = Dir::openat(&cur, name.as_str(), dir_flags(), Mode::empty()).unwrap();
        }
        cur
    }

    fn plant_git(at: &Dir) {
        nix::sys::stat::mkdirat(at, ".git", Mode::S_IRWXU).unwrap();
        let git = Dir::openat(at, ".git", dir_flags(), Mode::empty()).unwrap();
        let fd = nix::fcntl::openat(
            &git,
            "config",
            OFlag::O_CREAT | OFlag::O_WRONLY,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .unwrap();
        use std::io::Write as _;
        std::fs::File::from(fd)
            .write_all(b"[core]\n\tfsmonitor = touch /tmp/pwned\n")
            .unwrap();
    }

    #[test]
    fn deep_trees_are_scrubbed_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        fs::create_dir(&work).unwrap();
        fs::create_dir(work.join(".git")).unwrap();
        fs::write(
            work.join(".git/config"),
            "[core]\n\tfsmonitor = touch /tmp/pwned\n",
        )
        .unwrap();
        let bottom = deep(&work, 6);
        plant_git(&bottom);
        // A locked directory halfway down.
        let half = deep(&work.join("d".repeat(200)), 0);
        fchmodat(
            &half,
            "d".repeat(200).as_str(),
            Mode::empty(),
            FchmodatFlags::FollowSymlink,
        )
        .unwrap();
        drop((bottom, half));
        assert!(
            std::fs::remove_dir_all(&work).is_err(),
            "the tree defeats path-based removal"
        );

        assert_eq!(remove_git_entries(&work).unwrap(), 2);
        assert!(!work.join(".git").exists());
        // Down at the bottom, too.
        let mut cur = Dir::open(&work, dir_flags(), Mode::empty()).unwrap();
        for _ in 0..6 {
            cur = Dir::openat(&cur, "d".repeat(200).as_str(), dir_flags(), Mode::empty()).unwrap();
        }
        assert_eq!(
            fstatat(&cur, ".git", AtFlags::AT_SYMLINK_NOFOLLOW).err(),
            Some(Errno::ENOENT)
        );
        drop(cur);
        remove_dir(&work).unwrap();
        assert!(!work.exists());
    }
}
