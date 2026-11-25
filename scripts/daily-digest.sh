#!/bin/bash

# Add the new emails of the lore archive of the Git mailing list to the
# lore-git-md corpus, then summarize them and write the digests, as the
# retired "Daily digest" GitHub workflow did.
#
# Usage: daily-digest.sh [--once | --register-task | --start-in-tmux]
#
# Without an option, the script runs one cycle after the other, forever; a
# cycle starts at least an hour after the previous one did.  --once runs a
# single cycle.  --start-in-tmux runs the script in a new window of the tmux
# session, unless it runs in one already.  --register-task registers a
# Windows scheduled task that does that every hour.
#
# LORE_GIT_MD_WORK_DIR names the work directory (default: the current
# directory), which is created if missing and which the script changes into.
# It holds the bare repositories lore-git.git, a mirror of
# https://lore.kernel.org/git/1, and lore-git-md.git, the corpus, which are
# cloned if missing, and helper/, the executables of the latest release of
# lore-git-md-helper.  AZURE_OPENAI_ENDPOINT must be set, and `az login` must
# have been run: the Azure access tokens come from the Azure CLI, not from an
# API key.

set -eu

# $0 may be relative, so this has to come before changing the directory.
script=$(cd "$(dirname "$0")" && pwd)/${0##*/}
work_dir=${LORE_GIT_MD_WORK_DIR:-.}
mkdir -p "$work_dir"
cd "$work_dir"
work_dir=$PWD
# Relative names: update-lore-git-md puts the name of the archive into the
# subject of its import commits, and the corpus is public.
source_repo=lore-git.git
target_repo=lore-git-md.git
source_url=https://lore.kernel.org/git/1
target_url=https://github.com/git-digestif/lore-git-md.git
task_name=lore-git-md-daily-digest
window_name=daily-digest

die () {
	echo "$*" >&2
	exit 1
}

target_git () {
	git --git-dir="$target_repo" "$@"
}

# Clone the repositories that do not exist yet.  The archive comes with all
# its contents, because update-lore-git-md reads every new email from it; the
# corpus comes without the contents of its files, which digestive fetches as
# it needs them.  Neither clone is shallow.
clone_repos () {
	test -d "$source_repo" ||
		git clone --bare "$source_url" "$source_repo"
	test -d "$target_repo" ||
		git clone --bare --filter=blob:none --single-branch \
			--branch main "$target_url" "$target_repo"
}

# Put the executables of the latest release of the helper for this system on
# the PATH, downloading them first if they are not in the cache yet.
install_helper () {
	case "$(uname -s)" in
	Linux) os=linux ext=tar.gz ;;
	MINGW* | MSYS* | CYGWIN*) os=windows ext=zip ;;
	*) die "Unsupported operating system: $(uname -s)" ;;
	esac
	arch=$(uname -m)
	# The bash of the Git SDK for ARM64 is an x64 build that runs
	# emulated, so uname -m says x86_64 there.  The CPU decides.
	case $os,${PROCESSOR_IDENTIFIER-} in
	windows,ARM*) arch=aarch64 ;;
	esac
	case $os-$arch in
	linux-x86_64 | windows-x86_64 | windows-aarch64) ;;
	*) die "No release of the helper for $os-$arch" ;;
	esac

	repo=git-digestif/lore-git-md-helper
	# The redirect of the latest release page names the tag.  The REST API
	# allows only 60 requests an hour per IP address without a token.
	tag=$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
		"https://github.com/$repo/releases/latest" || :)
	case $tag in
	*/releases/tag/*) tag=${tag##*/} ;;
	*) die "Could not find the latest release of $repo" ;;
	esac

	name=lore-git-md-helper-$tag-$os-$arch
	url=https://github.com/$repo/releases/download/$tag/$name.$ext
	cache_dir=$work_dir/helper
	if ! test -d "$cache_dir/$name"
	then
		echo "Downloading $name" >&2
		mkdir -p "$cache_dir"
		tmp=$(mktemp -d "$cache_dir/tmp.XXXXXX")
		(
			trap 'rm -rf "$tmp"' EXIT
			curl -fsSL -o "$tmp/$name.$ext" "$url"
			case $ext in
			zip) unzip -q "$tmp/$name.$ext" -d "$tmp" ;;
			*) tar xzf "$tmp/$name.$ext" -C "$tmp" ;;
			esac
			mv "$tmp/$name" "$cache_dir/$name"
		)
	fi
	PATH=$cache_dir/$name:$PATH
}

# Fast-forward both repositories to their origins.  A corpus ref that is
# ahead of its origin is kept, and one that has diverged from it is an error.
fetch_repos () {
	git --git-dir="$source_repo" fetch origin master:master

	target_git fetch origin \
		+refs/heads/main:refs/remotes/origin/main \
		+refs/notes/msgid:refs/remotes/origin/notes/msgid
	for pair in refs/heads/main:refs/remotes/origin/main \
		refs/notes/msgid:refs/remotes/origin/notes/msgid
	do
		ref=${pair%%:*} tracking=${pair#*:}
		if ! target_git rev-parse -q --verify "$ref" >/dev/null ||
			target_git merge-base --is-ancestor "$ref" "$tracking"
		then
			target_git update-ref "$ref" "$tracking"
		elif ! target_git merge-base --is-ancestor "$tracking" "$ref"
		then
			die "$ref has diverged from $tracking"
		fi
	done
}

# Push both refs at once: update-lore-git-md insists that they carry the same
# Source-Commit trailer.
push_corpus () {
	target_git push --atomic origin refs/heads/main refs/notes/msgid
}

# Run the script in a new window of the tmux session, unless a window of that
# name has a live pane.  The window gets the environment of the tmux server,
# not this script's (except for PATH), so the work directory is passed on.
start_in_tmux () {
	tmux has-session 2>/dev/null || die "There is no tmux session"
	if tmux list-panes -a -F '#{window_name} #{pane_dead}' |
		grep -qx "$window_name 0"
	then
		echo "Already running in the tmux window $window_name" >&2
		return
	fi
	tmux new-window -d -n "$window_name" -c "$work_dir" "$(printf '%q ' \
		env "LORE_GIT_MD_WORK_DIR=$work_dir" "$BASH" "$script")"
}

# Register a Windows scheduled task that runs this script with
# --start-in-tmux every hour, also after the computer was asleep.
# CHERE_INVOKING keeps the login shell in the task's working directory.
register_task () {
	case "$(uname -s)" in
	MINGW* | MSYS* | CYGWIN*) ;;
	*) die "--register-task works only on Windows" ;;
	esac

	env_exe=$(cygpath -w /usr/bin/env.exe)
	bash_exe=$(cygpath -w /usr/bin/bash.exe)
	win_dir=$(cygpath -w "$work_dir")
	args="CHERE_INVOKING=1 \"$bash_exe\" -l \"$script\" --start-in-tmux"

	ps=$(cat <<-\EOF
		$ErrorActionPreference = 'Stop'
		$action = New-ScheduledTaskAction -Execute $env:TASK_EXE `
		    -Argument $env:TASK_ARGS -WorkingDirectory $env:TASK_DIR
		$trigger = New-ScheduledTaskTrigger -Once -At (Get-Date) `
		    -RepetitionInterval (New-TimeSpan -Hours 1)
		$settings = New-ScheduledTaskSettingsSet -StartWhenAvailable `
		    -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
		$null = Register-ScheduledTask -TaskName $env:TASK_NAME `
		    -Action $action -Trigger $trigger -Settings $settings -Force
	EOF
	)
	TASK_NAME=$task_name TASK_EXE=$env_exe \
	TASK_ARGS=$args TASK_DIR=$win_dir \
	powershell.exe -NoProfile -NonInteractive -Command "$ps"
	echo "Registered the scheduled task $task_name"
}

cycle () {
	clone_repos
	install_helper
	fetch_repos

	update-lore-git-md "$source_repo" "$target_repo"

	# update-lore-git-md has just checked that the two refs agree, so what
	# an interrupted cycle left behind can go out now, not only after
	# digestive, which may be interrupted in turn.
	push_corpus

	# Once the time limit is reached, digestive stops summarizing emails and
	# the next cycle resumes from what was committed, which is pushed even
	# if digestive failed.
	status=0
	digestive --azure-openai --azure-cli-token --since 2025/01/01 \
		--max-runtime 50m --target-repo "$target_repo" || status=$?

	push_corpus
	return $status
}

# Run one cycle after the other.  A cycle that fails is reported, and the
# next one starts when it is due.  Every cycle is a process of its own:
# the shell ignores `set -e` in a function that is called like
# `cycle || ...`.  The work directory is passed on as an absolute path, as
# a relative one would be taken from inside the work directory itself.
run_forever () {
	while :
	do
		started=$SECONDS
		status=0
		LORE_GIT_MD_WORK_DIR=$work_dir "$BASH" "$script" --once ||
			status=$?
		test $status -eq 0 ||
			echo "$(date): cycle failed with status $status" >&2

		remaining=$((3600 - (SECONDS - started)))
		test $remaining -le 0 || sleep $remaining
	done
}

case "${1-}" in
'') run_forever ;;
--once) cycle ;;
--register-task) register_task ;;
--start-in-tmux) start_in_tmux ;;
*) die "Usage: $0 [--once | --register-task | --start-in-tmux]" ;;
esac
