#!/bin/bash

# Add the new emails of the lore archive of the Git mailing list to the
# lore-git-md corpus, then summarize them and write the digests, as the
# retired "Daily digest" GitHub workflow did.
#
# The directory named by LORE_GIT_MD_WORK_DIR (default: the current
# directory) must hold the two bare repositories lore-git.git, a mirror of
# https://lore.kernel.org/git/1, and lore-git-md.git, the corpus.  The
# helper executables update-lore-git-md and digestive must be on the PATH,
# AZURE_OPENAI_ENDPOINT must be set, and `az login` must have been run:
# the Azure access tokens come from the Azure CLI, not from an API key.

set -eu

work_dir=${LORE_GIT_MD_WORK_DIR:-$PWD}
source_repo=$work_dir/lore-git.git
target_repo=$work_dir/lore-git-md.git

update-lore-git-md "$source_repo" "$target_repo"

# Once the time limit is reached, digestive stops summarizing emails and the
# next cycle resumes from what was committed.
digestive --azure-openai --azure-cli-token --since 2025/01/01 \
	--max-runtime 50m --target-repo "$target_repo"
