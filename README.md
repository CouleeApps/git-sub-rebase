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

## Why

I have to edit a repo that has many tracking submodules that all need to be rebased every time you want to rebase the parent repo.
My options were:
1. Handle rebase conflicts on every commit rebased, updating the various submodule pointers for each
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
