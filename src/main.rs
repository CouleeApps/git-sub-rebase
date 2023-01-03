use git2::{Repository, Commit, TreeWalkMode, TreeWalkResult, ObjectType, Submodule, Tree, Oid, RebaseOptions, ResetType, BranchType, Delta, Sort, Signature};
use anyhow::{Error, Result, anyhow};
use structopt::StructOpt;
use std::ffi::OsStr;
use std::borrow::{BorrowMut};
use std::collections::{BTreeMap, HashMap};
use git2::build::CheckoutBuilder;
use chrono::Local;
use std::io::stdin;
use std::path::PathBuf;
use std::process::Command;
use std::str::FromStr;
use std::sync::{atomic};
use std::sync::atomic::AtomicBool;
use git2::ErrorCode::{Applied, Conflict, NotFound};
use git2::ErrorClass::{Os, Rebase};

// TODO: GPG signing
// TODO: Continue/abort after a crash
// TODO: Interactive mode where you can pick/edit/squash/fixup/drop

#[derive(StructOpt)]
struct Config {
    #[structopt(name="ref")]
    ref_: String,
}
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

fn read_stdin() -> Result<String> {
    INTERRUPTED.store(false, atomic::Ordering::SeqCst);
    let mut choice = String::new();
    stdin().read_line(&mut choice)?;
    match INTERRUPTED.load(atomic::Ordering::SeqCst) {
        false => Ok(choice),
        _ => Err(anyhow!("Interrupted"))
    }
}

fn sub_path_to_string(path: &Vec<String>) -> String {
    if path.is_empty() {
        format!("*root*")
    } else {
        format!("{}", path.join("/"))
    }
}

fn branch_name_to_canonical(repo: &Repository, name: &String) -> Result<String> {
    let branch = repo.find_branch(name.as_str(), BranchType::Local);
    if let Ok(_branch) = branch {
        Ok(name.clone())
    } else {
        let branch = repo.find_branch(name.as_str(), BranchType::Remote);
        if let Ok(_branch) = branch {
            let slash = name.find("/").ok_or(anyhow!("Remote branch name is wacky"))? + 1;
            let b = &name[slash..];
            Ok(b.to_string())
        } else {
            Err(anyhow!("Not found"))
        }
    }
}

// Find which version of a submodule is checked out in a parent's tree
fn submodule_at_tree(submodule: &Submodule, tree: &Tree) -> Result<Option<Oid>> {
    let mut sub_object = None;

    // Find target commit in submodule for target in parent
    tree.walk(TreeWalkMode::PreOrder, |path, entry| {
        (|| -> Result<TreeWalkResult> {
            let combined = path.to_owned() + entry.name().ok_or(Error::msg("unknown name for object"))?;
            match entry.kind() {
                Some(ObjectType::Commit) => {

                    if <String as AsRef<OsStr>>::as_ref(&combined) == submodule.path().as_os_str() {
                        sub_object = Some(entry.id());
                    }
                }
                _ => {}
            }

            Ok(TreeWalkResult::Ok)
        })().unwrap_or_else(|err| {
            eprintln!("{}", err);
            TreeWalkResult::Abort
        })
    })?;

    Ok(sub_object)
}

// Postorder traverse submodules in a repository and apply a function to them, collecting results
// Parent repo will be provided a hashmap of the return values of the calls on its child submodules
fn recurse_subs<F, T>(repo: &Repository, target: &Commit, op: &F) -> Result<T>
    where F: Fn(&Repository, Option<&Submodule>, &Commit, &Vec<String>, HashMap<String, T>) -> Result<T>
{
    fn recurse<F, T>(repo: &Repository, submodule: Option<&Submodule>, target: &Commit, path: &mut Vec<String>, op: &F) -> Result<T>
        where F: Fn(&Repository, Option<&Submodule>, &Commit, &Vec<String>, HashMap<String, T>) -> Result<T>
    {
        let mut results = HashMap::new();

        // Iterate submodules
        let submodules = repo.submodules()?;
        for mut sub in submodules {
            let sub_repo = if let Ok(sub_repo) = sub.open() {
                sub_repo
            } else {
                println!("Submodule {} didn't load, trying to update...", sub.name().expect("Submodule neads name"));
                // Sometimes the sub can be empty and still exist, so nuke it if that happens
                if sub.path().exists() {
                    if !sub.path().read_dir()?.any(|_| true) {
                        // Whoops?
                        let cmd = Command::new("git")
                            .arg("submodule")
                            .arg("update")
                            .arg("--init")
                            .arg("--recursive")
                            .arg(sub.name().expect("Submodule should have name"))
                            .current_dir(repo.workdir().expect("Has workdir"))
                            .output()?;
                        eprintln!("{}", String::from_utf8(cmd.stdout)?);
                        eprintln!("{}", String::from_utf8(cmd.stderr)?);
                    }
                    sub.update(true, None)?;
                } else {
                    sub.update(true, None)?;
                }

                if let Ok(sub_repo) = sub.open() {
                    sub_repo
                } else {
                    println!("Submodule {} didn't load, was it deleted?", sub.name().expect("Submodule neads name"));

                    // Why did it fail to open? Could be deleted on the new commit
                    match submodule_at_tree(&sub, &target.tree()?) {
                        Ok(Some(_)) => {
                            println!("... No it wasn't? Ignoring it.");
                            continue;
                        },
                        Ok(None) => {
                            println!("... Yes it was");
                            continue;
                        },
                        e @ Err(_) => {
                            println!("... Git has no idea. Aborting...");
                            return e.map(|_| panic!())
                        },
                    }
                }
            };

            let sub_object = submodule_at_tree(&sub, &target.tree()?)?;
            if let Some(sub_object) = sub_object {
                let sub_target = sub_repo.find_commit(sub_object)?;

                let sub_name: String = sub.name().expect("Submodule needs name").into();

                path.push(sub_name.clone());
                results.insert(sub_name, recurse(&sub_repo, Some(&sub), &sub_target, path, op)?);
                path.remove(path.len() - 1);
            }
        }

        Ok(op(repo, submodule, target, path, results)?)
    }

    recurse(repo, None, target, &mut vec![], op)
}

fn update_submodules(repo: &Repository, target: &Commit) -> Result<()> {
    // Clean up submodules to point to real branches
    let need_checkouts = recurse_subs(&repo, &target, &|repo: &Repository, _submodule, _target, _path, child_results: HashMap<String, bool>| -> Result<bool> {
        // Only for repos with no checked out branch
        let head = repo.head()?;
        if head.name().expect("Ref expected name") == "HEAD" || head.name().expect("Ref expected name").contains("/multi_rebase_") {
            Ok(true)
        } else {
            Ok(child_results.len() > 0 || child_results.into_iter().map(|(_sub, needs)| needs).all(|needs| needs))
        }
    })?;

    if need_checkouts {
        // Find branch names we can checkout
        println!("Some of your submodules have no checked out branch. This will make rebasing fail! Trying to fix...");

        let checkout_names = recurse_subs(&repo, &target, &|repo: &Repository, _submodule, _target, path: &Vec<String>, child_results: HashMap<String, HashMap<Vec<String>, (String, String)>>| -> Result<HashMap<Vec<String>, (String, String)>> {
            // Only for repos with no checked out branch
            let head = repo.head()?;
            let format_path = sub_path_to_string(path);
            let head_name = head.name().expect("Ref expected name");

            let mut results = HashMap::new();
            for (_child, result) in child_results {
                results.extend(result.into_iter());
            }

            if head_name == "HEAD" || head_name.contains("/multi_rebase_") {
                // Find all local branches that are equal to HEAD of a named remote branch
                let matching_tracked_branches = repo.branches(Some(BranchType::Remote))?.map(|b| -> Result<Option<String>> {
                    let (branch, _branch_type) = b?;
                    let name: String = branch.name()?.expect("Branch has name").into();
                    if name.starts_with("backup/") || name.ends_with("HEAD") || name.contains("multi_rebase_") {
                        return Ok(None);
                    }
                    if branch.into_reference().peel_to_commit()?.id() == head.peel_to_commit()?.id() {
                        Ok(Some(name))
                    } else {
                        Ok(None)
                    }
                }).flat_map(|n| n).flat_map(|n| n).flat_map(|remote_branch_name| -> Result<Vec<(String, String)>> {
                    let branches = repo.branches(Some(BranchType::Local))?.map(|b| -> Result<Option<(String, String)>> {
                        let (branch, _type) = b?;
                        if let (Some(name), Some(upstream_name)) = (branch.name()?, branch.upstream()?.name()?) {
                            if upstream_name == remote_branch_name {
                                Ok(Some((name.to_string(), remote_branch_name.clone())))
                            } else {
                                Ok(None)
                            }
                        } else {
                            Ok(None)
                        }
                    }).flat_map(|n| n).flat_map(|n| n).collect::<Vec<_>>();

                    Ok(branches)
                }).flat_map(|r| r).collect::<Vec<_>>();

                // Find all branches that are equal to HEAD
                let matching_local_branches = repo.branches(Some(BranchType::Local))?.map(|b| -> Result<Option<(String, String)>> {
                    let (branch, _branch_type) = b?;
                    let name: String = branch.name()?.expect("Branch has name").into();
                    if name.starts_with("backup/") || name.ends_with("HEAD") || name.contains("multi_rebase_") {
                        return Ok(None);
                    }
                    if branch.into_reference().peel_to_commit()?.id() == head.peel_to_commit()?.id() {
                        Ok(Some((name.clone(), name)))
                    } else {
                        Ok(None)
                    }
                }).flat_map(|n| n).flat_map(|n| n).collect::<Vec<_>>();

                let matching_remote_branches = repo.branches(Some(BranchType::Remote))?.map(|b| -> Result<Option<(String, String)>> {
                    let (branch, _branch_type) = b?;
                    let name: String = branch.name()?.expect("Branch has name").into();
                    if name.starts_with("backup/") || name.ends_with("HEAD") || name.contains("multi_rebase_") {
                        return Ok(None);
                    }
                    if branch.into_reference().peel_to_commit()?.id() == head.peel_to_commit()?.id() {
                        Ok(Some((branch_name_to_canonical(repo, &name)?, name)))
                    } else {
                        Ok(None)
                    }
                }).flat_map(|n| n).flat_map(|n| n).collect::<Vec<_>>();

                let all_local_branches = repo.branches(Some(BranchType::Local))?.map(|b| -> Result<Option<(String, String)>> {
                    let (branch, _type) = b?;
                    let name: String = branch.name()?.expect("Branch has name").into();
                    if name.starts_with("backup/") || name.ends_with("HEAD") || name.contains("multi_rebase_") {
                        return Ok(None);
                    }
                    Ok(Some((name.clone(), name)))
                }).flat_map(|n| n).flat_map(|n| n).collect::<Vec<_>>();

                // See if we can line up any

                let branch_name =
                    if matching_tracked_branches.len() == 1 {
                        println!("Check out {} for {}? (same as HEAD) [Y/n]", matching_tracked_branches[0].0, format_path);
                        let choice = read_stdin()?;
                        if choice.starts_with("n") || choice.starts_with("N") {
                            return Err(anyhow!("Cancelling..."));
                        }

                        matching_tracked_branches[0].clone()
                    } else if matching_tracked_branches.len() > 1 {
                        println!("Need to check out a branch for {}: [pick one]", format_path);
                        for (i, (local, _remote)) in matching_tracked_branches.iter().enumerate() {
                            println!("[{}] {} (same as HEAD)", i + 1, local);
                        }

                        let choice = read_stdin()?;
                        let index = usize::from_str(choice.as_str().trim())?;
                        if index == 0 || index > matching_tracked_branches.len() {
                            return Err(anyhow!("Bad index, cancelling..."));
                        }

                        matching_tracked_branches[index - 1].clone()
                    } else if matching_local_branches.len() == 1 {
                        println!("Check out {} for {}? (same as HEAD) [Y/n]", matching_local_branches[0].0, format_path);
                        let choice = read_stdin()?;
                        if choice.starts_with("n") || choice.starts_with("N") {
                            return Err(anyhow!("Cancelling..."));
                        }

                        matching_local_branches[0].clone()
                    } else if matching_local_branches.len() > 1 {
                        println!("Need to check out a branch for {}: [pick one]", format_path);
                        for (i, (local, _remote)) in matching_local_branches.iter().enumerate() {
                            println!("[{}] {} (same as HEAD)", i + 1, local);
                        }

                        let choice = read_stdin()?;
                        let index = usize::from_str(choice.as_str().trim())?;
                        if index == 0 || index > matching_local_branches.len() {
                            return Err(anyhow!("Bad index, cancelling..."));
                        }

                        matching_local_branches[index - 1].clone()
                    } else if matching_remote_branches.len() == 1 {
                        println!("Check out {} for {}? (same as HEAD) [Y/n]", matching_remote_branches[0].0, format_path);
                        let choice = read_stdin()?;
                        if choice.starts_with("n") || choice.starts_with("N") {
                            return Err(anyhow!("Cancelling..."));
                        }

                        matching_remote_branches[0].clone()
                    } else if matching_remote_branches.len() > 1 {
                        println!("Need to check out a branch for {}: [pick one]", format_path);
                        for (i, (local, _remote)) in matching_remote_branches.iter().enumerate() {
                            println!("[{}] {} (same as HEAD)", i + 1, local);
                        }

                        let choice = read_stdin()?;
                        let index = usize::from_str(choice.as_str().trim())?;
                        if index == 0 || index > matching_remote_branches.len() {
                            return Err(anyhow!("Bad index, cancelling..."));
                        }

                        matching_remote_branches[index - 1].clone()
                    } else if all_local_branches.len() == 1 {
                        println!("Check out {} for {}? (not HEAD, will reset --hard) [Y/n]", all_local_branches[0].0, format_path);
                        let choice = read_stdin()?;
                        if choice.starts_with("n") || choice.starts_with("N") {
                            return Err(anyhow!("Cancelling..."));
                        }

                        all_local_branches[0].clone()
                    } else if all_local_branches.len() > 1 {
                        println!("Need to check out a branch for {}: [pick one]", format_path);
                        for (i, (local, _remote)) in all_local_branches.iter().enumerate() {
                            println!("[{}] {} (not HEAD, will reset --hard)", i + 1, local);
                        }

                        let choice = read_stdin()?;
                        let index = usize::from_str(choice.as_str().trim())?;
                        if index == 0 || index > all_local_branches.len() {
                            return Err(anyhow!("Bad index, cancelling..."));
                        }

                        all_local_branches[index - 1].clone()
                    } else {
                        return Err(anyhow!("No branches found for {}", format_path));
                    };

                results.insert(path.clone(), branch_name);
            }

            Ok(results)
        })?;

        // Pretty print
        let max_branch_len = checkout_names.iter().map(|(path, _)| sub_path_to_string(path).len()).max().unwrap_or(0);
        let max_local_len = checkout_names.iter().map(|(_, (local, _))| local.len()).max().unwrap_or(0);
        println!("\n");
        println!("Checking out branches for submodules: ");
        for (sub, (local, remote)) in checkout_names.iter().collect::<BTreeMap<_, _>>() {
            println!("{}:{} {}{} ==> {}", sub_path_to_string(sub), String::from_utf8(vec![b' '; max_branch_len - sub_path_to_string(sub).len()])?, local, String::from_utf8(vec![b' '; max_local_len - local.len()])?, remote);
        }

        println!("\n");
        println!("Running: ");
        recurse_subs(&repo, &target, &|repo: &Repository, _submodule, _target, path, _child_results| -> Result<()> {
            if let Some((local, remote)) = checkout_names.get(path) {
                println!("{}:{} {}{} ==> {}", sub_path_to_string(path), String::from_utf8(vec![b' '; max_branch_len - sub_path_to_string(path).len()])?, local, String::from_utf8(vec![b' '; max_local_len - local.len()])?, remote);

                // Make a backup branch because aaa my data
                let branch_name = format!("backup/HEAD_{}", Local::now().format("%H-%M-%S"));
                repo.branch(&branch_name, &repo.head()?.peel_to_commit()?, true)?;

                let current = repo.head()?.peel_to_commit()?;
                let branch =
                    if let Ok(branch) = repo.find_branch(local.as_str(), BranchType::Local) {
                        branch
                    } else {
                        // Create branch
                        let mut branch = repo.branch(local.as_str(), &current, false)?;
                        branch.set_upstream(repo.find_branch(remote.as_str(), BranchType::Remote)?.name()?)?;
                        branch
                    };
                repo.set_head(branch.into_reference().name().expect("Branch has name"))?;
                repo.reset(current.as_object(), ResetType::Mixed, None)?;
                repo.reset(current.as_object(), ResetType::Hard, None)?;
            }
            Ok(())
        })?;
    }

    let need_clean_old_rebase = recurse_subs(&repo, &target, &|repo, _submodule, _target, _path, child_results| {
        for (_child, result) in child_results {
            if result {
                return Ok(true);
            }
        }

        let multi_rebase_old = repo.find_branch("multi_rebase_old", BranchType::Local);
        let multi_rebase_cur = repo.find_branch("multi_rebase_cur", BranchType::Local);
        let multi_rebase_new = repo.find_branch("multi_rebase_new", BranchType::Local);
        let multi_rebase_track = repo.find_branch("multi_rebase_track", BranchType::Local);

        Ok(multi_rebase_old.is_ok() || multi_rebase_cur.is_ok() || multi_rebase_new.is_ok() || multi_rebase_track.is_ok())
    })?;
    if need_clean_old_rebase {
        eprintln!("Detected old multi-rebase operation that probably failed.");
        eprintln!("Press ENTER to clean it up and start over...");
        let _ = read_stdin()?;
        recurse_subs(&repo, &target, &|repo, _submodule, _target, _path, _child_results| {
            if let Ok(multi_rebase_old) = repo.find_branch("multi_rebase_old", BranchType::Local) {
                multi_rebase_old.into_reference().delete()?;
            }
            if let Ok(multi_rebase_cur) = repo.find_branch("multi_rebase_cur", BranchType::Local) {
                multi_rebase_cur.into_reference().delete()?;
            }
            if let Ok(multi_rebase_new) = repo.find_branch("multi_rebase_new", BranchType::Local) {
                multi_rebase_new.into_reference().delete()?;
            }
            if let Ok(multi_rebase_track) = repo.find_branch("multi_rebase_track", BranchType::Local) {
                multi_rebase_track.into_reference().delete()?;
            }

            Ok(())
        })?;
    }

    Ok(())
}

struct RebaseState {
    sign: dyn for<'a> Fn(Signature, Signature, Option<&'a str>, Tree, Vec<Commit>) -> Option<Commit<'a>>,
}

extern "C" fn sign_commit(
    out: *mut libgit2_sys::git_oid,
    author: *const libgit2_sys::git_signature,
    committer: *const libgit2_sys::git_signature,
    message_encoding: *const std::os::raw::c_char,
    message: *const std::os::raw::c_char,
    tree: *const libgit2_sys::git_tree,
    parent_count: usize,
    parents: *const libgit2_sys::git_commit,
    payload: *mut std::os::raw::c_void,
) -> std::os::raw::c_int {
    unsafe {
        // error = git_commit_create(&commit_id, rebase->repo, NULL,
        //                           author, committer, message_encoding, message,
        //                           tree, 1, (const git_commit **)&parent_commit);

        libgit2_sys::GIT_PASSTHROUGH
    }
}

fn multi_rebase_inner(repo: &Repository, _submodule: Option<&Submodule>, target: &Commit, path: &Vec<String>, mut child_results: HashMap<String, HashMap<Oid, Oid>>) -> Result<HashMap<Oid, Oid>> {
    let named_path = sub_path_to_string(path);
    println!("[{}] Now rebasing", named_path);
    if !child_results.is_empty() {
        println!("[{}] Child submodules commit map: {:?}", named_path, child_results);
    }

    let head = repo.head()?;
    let base = repo.merge_base(head.peel_to_commit()?.id(), target.id())?;

    // Make a backup branch because aaa my data
    let mut branch_name = head.name().expect("Head should have a name");
    if branch_name.contains('/') {
        branch_name = branch_name.split('/').last().expect("Split should have results");
    }
    let branch_name = format!("backup/{}_{}", branch_name, Local::now().format("%H-%M-%S"));
    repo.branch(&branch_name, &head.peel_to_commit()?, true)?;

    // Make four branches to keep track of state:
    // - multi_rebase_old:   the previous head commit, in case of failure
    // - multi_rebase_cur:   the rebase-head with all rebased commits so far
    // - multi_rebase_track: the commit on the pre-rebase branch that we are rebasing next
    // - multi_rebase_new:   the head branch used during the rebase
    repo.branch("multi_rebase_cur", &head.peel_to_commit()?, true)?;
    repo.branch("multi_rebase_old", &head.peel_to_commit()?, true)?;
    let mut track_branch = repo.branch("multi_rebase_track", &head.peel_to_commit()?, true)?.into_reference();
    let new_branch = repo.branch("multi_rebase_new", &head.peel_to_commit()?, true)?.into_reference();
    repo.set_head(new_branch.name().expect("Need refname"))?;

    let mut sub_heads = HashMap::new();
    for (sub, _) in &child_results {
        let res_submodule = repo.find_submodule(&sub)?;
        let sub_repo = res_submodule.open()?;
        sub_heads.insert(sub.clone(), sub_repo.head()?.name().expect("Head needs name").to_string());
    }

    // Map of old commit id -> new commit id
    let mut commit_map = HashMap::new();

    // If we have nothing to rebase, exit early
    println!("[{}] HEAD is at {}", named_path, head.peel_to_commit()?.id().to_string());
    println!("[{}] Target is  {}", named_path, target.id());
    if head.peel_to_commit()?.id() == target.id() {
        println!("[{}] {} --> {}", named_path, target.id(), target.id());
        commit_map.insert(target.id(), target.id());
        println!("[{}] Nothing to rebase", named_path);
        match head.name() {
            Some("HEAD") | None => {
                let id = head.peel_to_commit()?.id();
                println!("[{}] Set HEAD to {}", named_path, id.to_string());
                repo.set_head_detached(id)?;
            }
            Some(head_name) => {
                println!("[{}] Set HEAD to {}", named_path, head_name);
                repo.set_head(head_name)?;
            }
        }
        return Ok(commit_map);
    }

    // If we're rebasing onto the same commit as we've branched, there's no point in redoing all the commits
    println!("[{}] base is at {}", named_path, base.to_string());
    if base == target.id() {
        println!("[{}] Branched from base, using current tree.", named_path);

        // Add all the commits as themself -> themself
        let mut walk = repo.revwalk()?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)?;
        walk.push_head()?;
        walk.hide(base)?;
        for commit in walk.into_iter() {
            let commit = commit?;
            println!("[{}] {} --> {}", named_path, commit, commit);
            commit_map.insert(commit.clone(), commit);
        }
        println!("[{}] {} --> {}", named_path, base, base);
        commit_map.insert(base.clone(), base);
        match head.name() {
            Some("HEAD") | None => {
                let id = head.peel_to_commit()?.id();
                println!("[{}] Set HEAD to {}", named_path, id.to_string());
                repo.set_head_detached(id)?;
            }
            Some(head_name) => {
                println!("[{}] Set HEAD to {}", named_path, head_name);
                repo.set_head(head_name)?;
            }
        }
        return Ok(commit_map);
    }

    // Mark initial commit as pointing to the head where we're rebasing onto
    commit_map.insert(base, target.id());

    let mut rebase = loop {
        let copts = CheckoutBuilder::new();
        let mut ropts = RebaseOptions::new();
        ropts.checkout_options(copts);

        unsafe {
            (*std::mem::transmute::<_, *mut libgit2_sys::git_rebase_options>(ropts.raw())).commit_create_cb = Some(sign_commit);
        }

        match repo.rebase(Some(&repo.reference_to_annotated_commit(&new_branch)?), Some(&repo.find_annotated_commit(base)?), Some(&repo.find_annotated_commit(target.id())?), Some(ropts.borrow_mut())) {
            Ok(value) => break Ok(value),
            Err(e) if e.code() == Conflict => {
                eprintln!("[{}] {}", named_path, e);

                // Let user resolve and then continue
                eprintln!("[{}] Rebase conflict!", named_path);
                eprintln!("[{}] Please resolve then press enter when satisfied", named_path);

                let _ = read_stdin()?;
            }
            Err(e) => break Err(e)
        }
    }?;

    // Clean working copy before starting the rebase
    // Because the submodules are dumb and don't reset

    // Submodules like to mark themselves modified sometimes and that can cause the rebase to get unhappy
    for entry in repo.diff_index_to_workdir(None, None)?.deltas() {
        let diff_path = String::from_utf8_lossy(entry.new_file().path_bytes().expect("New file expected path")).into_owned();
        if entry.status() != Delta::Unmodified {
            println!("[{}] Modified: {:?}", named_path, entry.new_file().path());
            if child_results.contains_key(&diff_path) {
                println!("[{}] Unexpected submodule diff: {:?} {:?}", named_path, entry.new_file().path(), entry.status());

                // Submodule that was not updated
                let diff_submodule = repo.find_submodule(&diff_path)?;
                let sub_repo = diff_submodule.open()?;

                // What's its head? If it's in our results list then we shouldn't need to touch it, just stage it
                sub_repo.set_head(sub_repo.find_branch("multi_rebase_cur", BranchType::Local)?.into_reference().name().expect("Branch ref needs name"))?;
                sub_repo.reset(&sub_repo.find_object(entry.old_file().id(), Some(ObjectType::Commit))?, ResetType::Hard, Some(CheckoutBuilder::new().borrow_mut()))?;

                repo.index()?.update_all(&[&diff_path], None)?;
                repo.index()?.add_path((&diff_path).as_ref())?;
            }
        }
    }
    repo.index()?.write()?;

    while let Some(Ok(op)) = rebase.next() {
        track_branch.delete()?;
        track_branch = repo.branch("multi_rebase_track", &repo.find_commit(op.id())?, true)?.into_reference();

        //
        // THE IMPORTANT PART:
        //

        // Make sure the submodules updated (they don't on the first commit, and conflict on all later commits)
        let tree = repo.find_commit(op.id())?.tree()?;
        for mut submodule in repo.submodules()? {
            let sub_repo = match submodule.open() {
                Ok(sub_repo) => Ok(sub_repo),
                Err(e) if e.class() == Os && e.code() == NotFound => {
                    eprintln!("[{}] Submodule {} not found... maybe it needs init?", named_path, submodule.name().expect("Submodule should have name"));
                    let cmd = Command::new("git")
                        .arg("submodule")
                        .arg("update")
                        .arg("--init")
                        .arg("--recursive")
                        .arg(submodule.name().expect("Submodule should have name"))
                        .current_dir(repo.workdir().expect("Has workdir"))
                        .output()?;
                    eprintln!("{}", String::from_utf8(cmd.stdout)?);
                    eprintln!("{}", String::from_utf8(cmd.stderr)?);
                    submodule.sync()?;
                    submodule.update(true, None)?;
                    submodule.reload(true)?;
                    submodule.open()
                },
                Err(e) => Err(e)
            }?;
            let sub_name = submodule.name().expect("Submodule should have name").to_string();

            if let None = child_results.get(submodule.path().to_str().expect("Submodule should have path")) {
                // Need to initialize the new submodule for multi-rebase
                // This involves:
                // 1. Reset it to the state at this branch's head commit (final location of submodule)
                // 2. Sub_rebase it to the current commit
                // 3. This generates a mapping of all the commits from now until the final commit, which we can use
                // XXX: If the submodule is deleted during that span, may God help you.

                let final_head = submodule_at_tree(&submodule, &head.peel_to_commit()?.tree()?)?;
                let target_head = submodule_at_tree(&submodule, &tree)?;
                if let (Some(final_head), Some(target_head)) = (final_head, target_head) {
                    sub_repo.set_head_detached(final_head)?;

                    let mut sub_path = path.clone();
                    sub_path.push(sub_name.clone());
                    let sub_results = recurse_subs(&sub_repo, &sub_repo.find_commit(target_head)?, &multi_rebase_inner)?;
                    eprintln!("[{}] Rebased new submodule {} with results: {:?}", named_path, submodule.name().expect("Submodule should have name"), &sub_results);
                    child_results.insert(submodule.path().to_str().expect("Submodule should have path").to_string(), sub_results);
                } else {
                    return Err(anyhow!(format!("[{}] Cannot rebase newly added inner submodule {}", named_path, submodule.name().expect("Submodule should have name"))));
                }
            }

            let expected_commit = submodule_at_tree(&submodule, &tree)?;
            if let Some(expected_commit) = expected_commit {
                let default_results = HashMap::new();
                let converted_expected = child_results.get(submodule.path().to_str().expect("Submodule should have path")).unwrap_or_else(|| &default_results).get(&expected_commit);
                let sub_head = loop {
                    match sub_repo.head().and_then(|h| h.peel_to_commit()) {
                        Ok(commit) => break commit.id(),
                        _ => {
                            eprintln!("[{}] Submodule {} has no HEAD id, please check out a branch and press ENTER...", named_path, submodule.name().expect("Submodule should have name"));
                            let _ = read_stdin()?;

                            submodule.reload(true)?;
                        }
                    };
                };

                if sub_head != expected_commit {
                    if let Some(converted) = converted_expected {
                        if sub_head != *converted {
                            println!("[{}] Should expect {} to be at {}, it's at {}", named_path, sub_name, converted, sub_head);

                            sub_repo.set_head(sub_repo.find_branch("multi_rebase_cur", BranchType::Local)?.into_reference().name().expect("Branch ref needs name"))?;
                            sub_repo.reset(&sub_repo.find_object(*converted, Some(ObjectType::Commit))?, ResetType::Hard, Some(CheckoutBuilder::new().borrow_mut()))?;

                            repo.index()?.update_all(&[submodule.path()], None)?;
                            repo.index()?.add_path(submodule.path())?;
                            repo.index()?.write()?;
                            println!("[{}] Update submodule {} to {}", named_path, sub_name, *converted);
                        }
                    } else {
                        println!("[{}] Should expect {} to be at {}, it's at {}", named_path, sub_name, expected_commit, sub_head);

                        sub_repo.set_head(sub_repo.find_branch("multi_rebase_cur", BranchType::Local)?.into_reference().name().expect("Branch ref needs name"))?;
                        sub_repo.reset(&sub_repo.find_object(expected_commit, Some(ObjectType::Commit))?, ResetType::Hard, Some(CheckoutBuilder::new().borrow_mut()))?;

                        repo.index()?.update_all(&[submodule.path()], None)?;
                        repo.index()?.add_path(submodule.path())?;
                        repo.index()?.write()?;
                        println!("[{}] Update submodule {} to {}", named_path, sub_name, expected_commit);
                    }
                }
            } else {
                println!("[{}] Submodule {} revision", named_path, sub_name);
            }
        }

        // Then just try to commit and see if it works
        let new_id = loop {
            match rebase.commit(None, &repo.signature()?, None) {
                Ok(id) => break id,
                Err(e) if e.code() == Applied && e.class() == Rebase => {
                    // Whatever the last commit is, should be the new id
                    println!("[{}] Commit patch was already applied! Assuming that means we can ignore it.", named_path);
                    break repo.head()?.peel_to_commit()?.id()
                }
                Err(e) => {
                    eprintln!("[{}] {}", named_path, e);

                    // Let user resolve and then continue
                    eprintln!("[{}] Rebase conflict!", named_path);
                    eprintln!("[{}] Please resolve then press enter when satisfied", named_path);

                    let _ = read_stdin()?;
                }
            }
        };
        println!("[{}] Rebased commit {} --> {}", named_path, op.id(), new_id);
        commit_map.insert(op.id(), new_id);
    }
    rebase.finish(Some(&repo.signature()?))?;

    // Revert head for parent to rebase
    match head.name() {
        Some("HEAD") | None => {
            let id = head.peel_to_commit()?.id();
            println!("[{}] Set HEAD to {}", named_path, id.to_string());
            repo.set_head_detached(id)?;
        }
        Some(head_name) => {
            println!("[{}] Set HEAD to {}", named_path, head_name);
            repo.set_head(head_name)?;
        }
    }

    println!("[{}] Reset HEAD (hard) to finalized commit {}", named_path, head.peel_to_commit()?.id().to_string());
    repo.reset(&head.peel_to_commit()?.into_object(), ResetType::Hard, Some(CheckoutBuilder::new().borrow_mut()))?;

    // Reset subs
    for (sub, _) in &child_results {
        if let Some(sub_head_name) = sub_heads.get(sub) {
            let res_submodule = repo.find_submodule(sub)?;
            let sub_repo = res_submodule.open()?;
            let sub_head = sub_repo.find_reference(sub_head_name)?;
            if sub_head.name().expect("Head should have a name") != "HEAD" {
                println!("[{}] Set submodule {} HEAD to {}", named_path, sub, sub_head.name().expect("Need refname"));
                sub_repo.set_head(sub_head.name().expect("Sub head has name"))?;
            }
            println!("[{}] Reset submodule {} HEAD (hard) to finalized commit {}", named_path, sub, sub_head.peel_to_commit()?.id().to_string());
            sub_repo.reset(&sub_head.peel_to_commit()?.into_object(), ResetType::Hard, Some(CheckoutBuilder::new().borrow_mut()))?;
        }
    }

    Ok(commit_map)
}

fn do_rebase(repo: &Repository, target: &Commit) -> Result<()> {
    // ---------------------------------------------------------------------------------------------
    // The Real Part TM
    // ---------------------------------------------------------------------------------------------

    // Rebase!
    println!("REBASE!! START!!");
    recurse_subs(&repo, &target, &multi_rebase_inner)?;

    Ok(())
}

fn main() -> Result<()> {
    ctrlc::set_handler(move || {
        INTERRUPTED.store(true, atomic::Ordering::SeqCst);
    })?;

    let base = std::env::current_dir()?;
    let repo = Repository::open(&base)?;

    let config = Config::from_args();

    let stats = repo.diff_index_to_workdir(None, None)?.stats()?;
    if stats.files_changed() != 0 {
        eprintln!("Cannot run with a dirty working copy! Please stash first.");
        return Err(Error::msg("Dirty working copy"));
    }

    // I ~don't~ know where I'm going, but I'm on my way
    // The road goes on forever, but the party never ends
    // - Warriors
    let target = match repo.resolve_reference_from_short_name(config.ref_.as_str()) {
        Ok(obj) => obj.peel_to_commit()?,
        Err(e) => {
            eprintln!("Cannot find object {}: {}", config.ref_, e);
            return Err(Error::from(e));
        }
    };

    // Make sure nobody is locked
    recurse_subs(&repo, &target, &|repo: &Repository, _submodule, _target, path, child_results| -> Result<()> {
        let mut worktree = PathBuf::from(repo.path());
        worktree.push("index.lock");
        if worktree.exists() {
            return Err(anyhow!("Lockfile for {} exists, please finish your operations or delete it before starting.", sub_path_to_string(path)));
        }
        Ok(())
    })?;

    update_submodules(&repo, &target)?;

    // Find the named branches all the submodules were using so we can update them after the rebase
    let original_branch_names = recurse_subs(&repo, &target, &|repo: &Repository, _submodule, _target, _path, child_results: HashMap<String, HashMap<Vec<String>, String>>| -> Result<HashMap<Vec<String>, String>> {
        let head = repo.head()?;
        let mut results = HashMap::new();

        results.insert(vec![], head.name().expect("Ref expected name").into());
        for (path, c_results) in child_results {
            for (mut cpath, cvalue) in c_results.into_iter() {
                cpath.insert(0, path.clone());
                results.insert(cpath, cvalue);
            }
        }

        Ok(results)
    })?;

    println!("Submodule branches to restore after running:");
    let mut sorted_names = original_branch_names.iter().collect::<Vec<(_, _)>>();
    sorted_names.sort();
    let max_sub_len = sorted_names.iter().map(|(path, _)| sub_path_to_string(path).len()).max().unwrap_or(0);
    for (path, branch) in sorted_names {
        println!("{}:{} {}", sub_path_to_string(path), String::from_utf8(vec![b' '; max_sub_len - sub_path_to_string(path).len()])?, branch);
    }

    println!("Press ENTER to begin...");
    let _ = read_stdin()?;

    if let Err(e) = do_rebase(&repo, &target) {
        println!("Reverting branches...");

        // Revert branches
        recurse_subs(&repo, &target, &|repo: &Repository, _submodule, _target, path, _child_results| {
            let named_path = sub_path_to_string(path);
            let rebase_old = repo.find_branch("multi_rebase_old", BranchType::Local);
            if let Err(_) = rebase_old {
                // Not touched
                println!("[{}] Already done", named_path);
                return Ok(());
            }
            let old_head = rebase_old?.into_reference().peel_to_commit()?;
            let update_branch = original_branch_names.get(path);
            if let Some(branch_name) = update_branch {
                if branch_name != "HEAD" {
                    println!("[{}] Set HEAD to {}", named_path, branch_name);
                    repo.set_head(&branch_name)?;
                }
                println!("[{}] Reset HEAD (hard) to finalized commit {}", named_path, old_head.id());
                repo.reset(&old_head.into_object(), ResetType::Hard, Some(CheckoutBuilder::new().borrow_mut()))?;
            }

            // Clean up extra branches
            println!("[{}] Cleaning up branches", named_path);
            repo.find_branch("multi_rebase_old", BranchType::Local)?.into_reference().delete()?;
            repo.find_branch("multi_rebase_cur", BranchType::Local)?.into_reference().delete()?;
            repo.find_branch("multi_rebase_new", BranchType::Local)?.into_reference().delete()?;
            repo.find_branch("multi_rebase_track", BranchType::Local)?.into_reference().delete()?;

            Ok(())
        })?;

        println!("REBASE FAIL!");
        return Err(e);
    } else {
        // Switch branches to multi_rebase_new for all repos
        recurse_subs(&repo, &target, &|repo: &Repository, _submodule, _target, path, _child_results| {
            let named_path = sub_path_to_string(path);
            let rebase_new = repo.find_branch("multi_rebase_new", BranchType::Local);
            if let Err(_) = rebase_new {
                // Not touched
                println!("[{}] Already done", named_path);
                return Ok(());
            }
            let new_head = rebase_new?.into_reference().peel_to_commit()?;
            let update_branch = original_branch_names.get(path);
            if let Some(branch_name) = update_branch {
                if branch_name != "HEAD" {
                    println!("[{}] Set HEAD to {}", named_path, branch_name);
                    repo.set_head(&branch_name)?;
                }
                println!("[{}] Reset HEAD (hard) to finalized commit {}", named_path, new_head.id());
                repo.reset(&new_head.into_object(), ResetType::Hard, Some(CheckoutBuilder::new().borrow_mut()))?;
            }

            // Clean up extra branches
            println!("[{}] Cleaning up branches", named_path);
            repo.find_branch("multi_rebase_old", BranchType::Local)?.into_reference().delete()?;
            repo.find_branch("multi_rebase_cur", BranchType::Local)?.into_reference().delete()?;
            repo.find_branch("multi_rebase_new", BranchType::Local)?.into_reference().delete()?;
            repo.find_branch("multi_rebase_track", BranchType::Local)?.into_reference().delete()?;

            Ok(())
        })?;

        println!("REBASE!! DONE!!");
    }

    return Ok(());
}
