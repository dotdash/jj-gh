//! Wrapper around `gh` that resolves PR selectors through jj bookmarks.
//!
//! For the `gh pr <sub>` subcommands that take a `[<number>|<url>|<branch>]`
//! selector, the leading positional argument is handled as follows:
//!
//!   * a PR number or URL is passed on untouched, so an all-digit revset
//!     cannot be used here;
//!   * any other value is a jj revset, replaced by the local bookmark on the
//!     single revision it matches;
//!   * with no leading positional, `@` is used.
//!
//! Several matched revisions, no bookmark, or more than one bookmark is an
//! error. The selector has to come first: `gh` accepts it after flags, we
//! do not.
//!
//! `--ref`/`-r` of `gh workflow run` is resolved the same way.
//!
//! When the current directory is outside the work tree of the colocated
//! workspace backing the repo, `gh` is pointed at that git repo.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};
use std::sync::OnceLock;

const JJ: &str = "jj";

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The first `gh` on `PATH` that is not this binary, so that installing this
/// binary as `gh` does not make it recurse into itself.
fn gh_path() -> &'static Path {
    static PATH: OnceLock<PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let me = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .unwrap_or_else(|e| {
                eprintln!("gh: cannot determine our own path: {e}");
                exit(1);
            });
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .map(|dir| dir.join("gh"))
            .find(|cand| is_executable(cand) && std::fs::canonicalize(cand).is_ok_and(|c| c != me))
            .unwrap_or_else(|| {
                eprintln!("gh: no `gh` other than ourselves found on PATH.");
                exit(1);
            })
    })
}

/// The git dir and work tree of the colocated workspace backing this jj repo,
/// unless the current directory already lives in that work tree.
fn foreign_git_repo() -> Option<(PathBuf, PathBuf)> {
    let out = Command::new(JJ).args(["git", "root"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let git_dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    // A non-colocated repo keeps its git dir inside `.jj`, where there is no
    // work tree for `gh` to use.
    if git_dir.file_name() != Some(OsStr::new(".git")) {
        return None;
    }
    let work_tree = std::fs::canonicalize(git_dir.parent()?).ok()?;
    let inside = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .is_ok_and(|cwd| cwd.starts_with(&work_tree));
    (!inside).then_some((git_dir, work_tree))
}

/// `gh`, told where the git repo is when the current directory has none.
fn gh() -> Command {
    let mut cmd = Command::new(gh_path());
    if let Some((git_dir, work_tree)) = foreign_git_repo() {
        cmd.env("GIT_DIR", git_dir).env("GIT_WORK_TREE", work_tree);
    }
    cmd
}

/// Replace this process with `cmd`, or exit with an error if that fails.
fn exec(mut cmd: Command) -> ! {
    let err = cmd.exec();
    eprintln!("gh: failed to exec {}: {err}", gh_path().display());
    exit(1);
}

/// `gh pr` subcommands that take a PR selector as their first positional.
/// (`list`/`status`/`create` don't, and so are left untouched.)
fn is_selector_subcmd(sub: &str) -> bool {
    matches!(
        sub,
        "view"
            | "review"
            | "merge"
            | "comment"
            | "close"
            | "edit"
            | "diff"
            | "checks"
            | "ready"
            | "reopen"
            | "lock"
            | "unlock"
            | "checkout"
            | "update-branch"
    )
}

/// Whether `arg` is a selector `gh` resolves by itself: a PR number or URL.
fn is_gh_selector(arg: &str) -> bool {
    (!arg.is_empty() && arg.bytes().all(|b| b.is_ascii_digit())) || arg.contains("://")
}

/// Whether `rest` asks for the subcommand's help rather than naming a PR.
/// `-h` counts in leading position only, as it is otherwise indistinguishable
/// from the value of a preceding flag.
fn wants_help(rest: &[OsString]) -> bool {
    rest.first().is_some_and(|arg| arg == "-h") || rest.iter().any(|arg| arg == "--help")
}

/// The revset to resolve into a selector for `gh pr <sub>`, plus the arguments
/// to pass on. `None` leaves the selector to `gh`.
fn pr_selector(rest: &[OsString]) -> (Option<&str>, &[OsString]) {
    match rest.first().and_then(|arg| arg.to_str()) {
        Some(arg) if is_gh_selector(arg) => (None, rest),
        Some(arg) if !arg.starts_with('-') => (Some(arg), &rest[1..]),
        // Not UTF-8, so not a revset either; `gh` may still make sense of it.
        None if !rest.is_empty() => (None, rest),
        _ => (Some("@"), rest),
    }
}

/// One entry per line of `jj log` output, holding that revision's bookmarks.
fn parse_bookmarks(out: &str) -> Vec<Vec<String>> {
    out.lines()
        .map(|line| line.split_whitespace().map(str::to_owned).collect())
        .collect()
}

/// Local bookmark names on each revision matched by `revset`, one entry per
/// revision.
fn bookmarks_per_revision(revset: &str) -> Vec<Vec<String>> {
    let out = Command::new(JJ)
        .args([
            "log",
            "--no-graph",
            "-r",
            revset,
            "-T",
            r#"local_bookmarks.map(|b| b.name()).join(" ") ++ "\n""#,
        ])
        .output()
        .unwrap_or_else(|e| {
            eprintln!("gh: failed to run jj: {e}");
            exit(1);
        });
    if !out.status.success() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
        exit(1);
    }
    parse_bookmarks(&String::from_utf8_lossy(&out.stdout))
}

/// Why a revset does not name exactly one bookmark.
#[derive(Debug, PartialEq)]
enum Ambiguous {
    Revisions(usize),
    NoBookmark,
    Bookmarks(Vec<String>),
}

/// The single bookmark on the single revision in `revisions`.
fn pick_bookmark(mut revisions: Vec<Vec<String>>) -> Result<String, Ambiguous> {
    if revisions.len() != 1 {
        return Err(Ambiguous::Revisions(revisions.len()));
    }
    let mut names = revisions.pop().unwrap();
    match names.len() {
        0 => Err(Ambiguous::NoBookmark),
        1 => Ok(names.pop().unwrap()),
        _ => Err(Ambiguous::Bookmarks(names)),
    }
}

/// The unique bookmark on the single revision `revset` matches, or an error
/// exit.
fn resolve_bookmark(revset: &str) -> String {
    match pick_bookmark(bookmarks_per_revision(revset)) {
        Ok(name) => name,
        Err(Ambiguous::Revisions(n)) => {
            eprintln!("gh: `{revset}` matches {n} revisions, need exactly one.");
            exit(1);
        }
        Err(Ambiguous::NoBookmark) => {
            eprintln!("gh: no bookmark found on revision `{revset}`.");
            exit(1);
        }
        Err(Ambiguous::Bookmarks(names)) => {
            eprintln!("gh: revision `{revset}` has more than one bookmark:");
            for name in &names {
                eprintln!("  {name}");
            }
            exit(1);
        }
    }
}

/// Rewrite `--ref`/`-r` values through `resolve`, in the separate
/// (`--ref REV`) as well as the attached (`--ref=REV`, `-r=REV`, `-rREV`)
/// forms.
fn resolve_ref_args(rest: &[OsString], resolve: impl Fn(&str) -> String) -> Vec<OsString> {
    fn attached(arg: &str) -> Option<(&'static str, &str)> {
        ["--ref=", "-r=", "-r"]
            .into_iter()
            .find_map(|flag| Some((flag, arg.strip_prefix(flag).filter(|v| !v.is_empty())?)))
    }
    let mut out = Vec::with_capacity(rest.len());
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        if arg == "--ref" || arg == "-r" {
            out.push(arg.clone());
            match iter.next().map(|rev| rev.to_str()) {
                Some(Some(revset)) => out.push(resolve(revset).into()),
                Some(None) => {
                    eprintln!(
                        "gh: revision argument to `{}` is not valid UTF-8.",
                        arg.display()
                    );
                    exit(1);
                }
                None => {}
            }
        } else if let Some((flag, revset)) = arg.to_str().and_then(attached) {
            out.push(format!("{flag}{}", resolve(revset)).into());
        } else {
            out.push(arg.clone());
        }
    }
    out
}

fn main() {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();

    if let [pr, sub, rest @ ..] = args.as_slice()
        && pr == "pr"
        && sub.to_str().is_some_and(is_selector_subcmd)
        && !wants_help(rest)
    {
        let (revset, passthrough) = pr_selector(rest);
        let mut cmd = gh();
        cmd.arg("pr").arg(sub);
        if let Some(revset) = revset {
            cmd.arg(resolve_bookmark(revset));
        }
        cmd.args(passthrough);
        exec(cmd);
    }

    if let [workflow, run, rest @ ..] = args.as_slice()
        && workflow == "workflow"
        && run == "run"
        && !wants_help(rest)
    {
        let mut cmd = gh();
        cmd.arg("workflow")
            .arg("run")
            .args(resolve_ref_args(rest, resolve_bookmark));
        exec(cmd);
    }

    let mut cmd = gh();
    cmd.args(&args);
    exec(cmd);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn gh_selectors_are_numbers_and_urls() {
        assert!(is_gh_selector("123"));
        assert!(is_gh_selector("https://github.com/o/r/pull/1"));
        assert!(!is_gh_selector(""));
        assert!(!is_gh_selector("@"));
        assert!(!is_gh_selector("my-branch"));
        assert!(!is_gh_selector("trunk()..@"));
    }

    #[test]
    fn short_help_is_only_recognized_in_leading_position() {
        assert!(wants_help(&args(&["-h"])));
        assert!(wants_help(&args(&["--admin", "--help"])));
        assert!(!wants_help(&args(&["-b", "-h"])));
        assert!(!wants_help(&args(&[])));
    }

    #[test]
    fn a_bookmark_is_picked_only_when_unambiguous() {
        let one = |list: &[&str]| vec![names(list)];
        assert_eq!(pick_bookmark(one(&["a"])), Ok("a".to_string()));
        assert_eq!(pick_bookmark(one(&[])), Err(Ambiguous::NoBookmark));
        assert_eq!(
            pick_bookmark(one(&["a", "b"])),
            Err(Ambiguous::Bookmarks(names(&["a", "b"])))
        );
        assert_eq!(pick_bookmark(vec![]), Err(Ambiguous::Revisions(0)));
        assert_eq!(
            pick_bookmark(vec![names(&["a"]), names(&[])]),
            Err(Ambiguous::Revisions(2))
        );
    }

    #[test]
    fn selector_subcmds_are_the_ones_taking_a_pr() {
        assert!(is_selector_subcmd("view"));
        assert!(is_selector_subcmd("checks"));
        assert!(!is_selector_subcmd("list"));
        assert!(!is_selector_subcmd("create"));
    }

    #[test]
    fn pr_selector_picks_the_leading_positional() {
        let rest = args(&["my-branch", "--json", "state"]);
        assert_eq!(pr_selector(&rest), (Some("my-branch"), &rest[1..]));

        let rest = args(&["--json", "state"]);
        assert_eq!(pr_selector(&rest), (Some("@"), &rest[..]));

        assert_eq!(pr_selector(&[]), (Some("@"), &[][..]));

        let rest = args(&["123", "--web"]);
        assert_eq!(pr_selector(&rest), (None, &rest[..]));

        let rest = vec![OsString::from_vec(vec![0xff])];
        assert_eq!(pr_selector(&rest), (None, &rest[..]));
    }

    #[test]
    fn bookmarks_are_parsed_per_revision() {
        assert_eq!(
            parse_bookmarks("a b\n\nc\n"),
            vec![names(&["a", "b"]), names(&[]), names(&["c"])]
        );
        assert!(parse_bookmarks("").is_empty());
    }

    #[test]
    fn ref_values_are_resolved_in_every_form() {
        let resolved = resolve_ref_args(
            &args(&[
                "--ref", "@", "-r", "x", "--ref=y", "-r=z", "-rw", "-f", "k=v",
            ]),
            |r| format!("<{r}>"),
        );
        assert_eq!(
            resolved,
            args(&[
                "--ref",
                "<@>",
                "-r",
                "<x>",
                "--ref=<y>",
                "-r=<z>",
                "-r<w>",
                "-f",
                "k=v"
            ])
        );
    }

    #[test]
    fn trailing_ref_without_value_is_left_to_gh() {
        assert_eq!(
            resolve_ref_args(&args(&["--ref"]), |r| r.to_string()),
            args(&["--ref"])
        );
    }
}
