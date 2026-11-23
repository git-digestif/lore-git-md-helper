#!/bin/bash

# Add the new emails of the lore archive of the Git mailing list to the
# lore-git-md corpus, then summarize them and write the digests, as the
# retired "Daily digest" GitHub workflow did.
#
# The directory named by LORE_GIT_MD_WORK_DIR (default: the current
# directory) holds the two bare repositories lore-git.git, a mirror of
# https://lore.kernel.org/git/1, and lore-git-md.git, the corpus, which are
# cloned if missing.  The helper executables update-lore-git-md and digestive
# come from the latest release of lore-git-md-helper, and are cached in the
# subdirectory helper.  AZURE_OPENAI_ENDPOINT must be set, and `az login`
# must have been run: the Azure access tokens come from the Azure CLI, not
# from an API key.

set -eu

work_dir=${LORE_GIT_MD_WORK_DIR:-$PWD}
source_repo=$work_dir/lore-git.git
target_repo=$work_dir/lore-git-md.git
source_url=https://lore.kernel.org/git/1
target_url=https://github.com/git-digestif/lore-git-md.git

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

clone_repos
install_helper
fetch_repos

update-lore-git-md "$source_repo" "$target_repo"

# Once the time limit is reached, digestive stops summarizing emails and the
# next cycle resumes from what was committed, which is pushed even if
# digestive failed.
status=0
digestive --azure-openai --azure-cli-token --since 2025/01/01 \
	--max-runtime 50m --target-repo "$target_repo" || status=$?

push_corpus
exit $status
