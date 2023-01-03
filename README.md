# git-sub-rebase
It's `git rebase` except it also follows submodules, and rebases all of them onto the respective commit from the parent repo.

## Usage

:warning: This tool is experimental and no support will be provided. Back up your git repository prior to trying this. I've melted mine a couple times during the course of developing this tool. :warning:

Link as `git-sub-rebase` in your PATH, and then:

```sh
# Rebase current HEAD onto <ref>
git sub-rebase <ref>
# E.g.:
git sub-rebase origin/dev
```

You will see a whole bunch of debug text printed. This is intentional (easier to debug when something inevitably goes wrong).

## What

Imagine you have two repos, structured like this, and you want to rebase `HEAD` onto `origin/master`

Before rebase:
```
Parent Repo              | Submodule
-------------------------|----------------------------
 c <-- origin/master     |
 |                       |
 |   e <-- HEAD          |
 |   |                   |
 |   d-------------------|-------->D <-- HEAD
 |   |                   |         |
 b--/--------------------|---->B  /  <-- origin/master
 | /                     |     | /   
 a-----------------------|---->A
```

Rebase the submodule and update pointers
```
Parent Repo              | Submodule
-------------------------|----------------------------
 c <-- origin/master     |
 |                       |
 |   e' <-- HEAD         |
 |   |                   |
 |   d'------------------|-----D' <-- HEAD
 |   |                   |     |
 b--/--------------------|---->B <-- origin/master
 | /                     |     |
 a-----------------------|---->A
```

Then rebase the main repo
```
Parent Repo              | Submodule
-------------------------|----------------------------
 e'' <-- HEAD            |
 |                       |
 d''---------------------|---->D' <-- HEAD
 |                       |     |
 c <-- origin/master     |     |
 |                       |     |
 b-----------------------|---->B <-- origin/master
 |                       |     |
 a-----------------------|---->A
```
You need to rebase `D` onto `B` in the submodule, then update `d` and `e` in the main repo to point to `D'`, THEN rebase the updated `d'` and `e'` onto `c` in the main repo. That way `d''` points to `D'` as `d` pointed to `D`, both repo histories are linear, and every commit in the parent repo points to a commit in the submodule that is in the branch from `HEAD`.

## Why

Look at how complicated this is, for just 3 commits and 2 submodules! Now imagine doing it across 4+ submodules with upwards of 50 commits.

I have to edit a repo that has many tracking submodules that all need to be rebased every time you want to rebase the parent repo.
My options were:
1. Manually handle rebase conflicts on every commit rebased, updating the various submodule pointers for each
2. Discard submodule history and just rebase all the commits to the tip of the rebased submodule
3. Write some rust code to do #1 for me :)

Codebase definitely not written during an extra long meeting while working on [Binary Ninja](https://github.com/Vector35/binaryninja-api), which has a notoriously moduley repo structure...

## How

Easy N step process:
1. Rebase all submodules according to these steps
2. Record all old -> new commit hashes for the rebased submodule's commits
3. Rebase parent repo's commits on new HEAD  
  a. For each commit in the parent repo that updates the submodule pointer, swap to the new submodule commit hash
4. Do some extra magic on the side for dealing with the fact that git submodules are a terrible system that break horribly when you try to do any git actions involving them

## Contributing

This repo is a ceritified :ok_hand: Spaghetti Masterpiece of recursion. If you wish to PR changes, you may do so at your own mental risk. The code is released under MIT License.
