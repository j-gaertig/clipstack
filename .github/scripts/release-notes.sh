#!/usr/bin/env bash
# Release notes for the commits since the previous tag: summary, a collapsed
# commit list and first-time contributors.
# Usage: release-notes.sh <tag>   (needs `gh` and GH_TOKEN for GitHub logins)
set -euo pipefail

TAG=$1
HEAD=${GITHUB_SHA:-HEAD}
REPO=${GITHUB_REPOSITORY:-$(gh repo view --json nameWithOwner -q .nameWithOwner)}
PREV=$(git describe --tags --abbrev=0 "$HEAD" 2>/dev/null || true)
RANGE=${PREV:+$PREV..}$HEAD

# GitHub login of a commit's author, or its git name if it has no account.
declare -A LOGIN
login() {
  local sha=$1 name=$2
  if [[ -z ${LOGIN[$sha]+x} ]]; then
    local l
    l=$(gh api "repos/$REPO/commits/$sha" -q '.author.login // empty' 2>/dev/null) || l=""
    LOGIN[$sha]=${l:+@$l}
    [[ -n ${LOGIN[$sha]} ]] || LOGIN[$sha]=$name
  fi
  echo "${LOGIN[$sha]}"
}

mapfile -t COMMITS < <(git log --reverse --format='%H%x09%ae%x09%an%x09%s' "$RANGE")
declare -A SEEN_BEFORE=()
if [[ -n $PREV ]]; then
  while read -r email; do SEEN_BEFORE[$email]=1; done < <(git log --format='%ae' "$PREV")
fi

declare -A AUTHORS=() NEW=()
LIST=""
for line in "${COMMITS[@]}"; do
  IFS=$'\t' read -r sha email name subject <<<"$line"
  who=$(login "$sha" "$name")
  AUTHORS[$who]=1
  LIST+="- [\`${sha:0:7}\`](https://github.com/$REPO/commit/$sha) $subject ($who)"$'\n'
  if [[ -z ${SEEN_BEFORE[$email]+x} && -z ${NEW[$who]+x} ]]; then
    NEW[$who]=$sha
  fi
done

n=${#COMMITS[@]}
c=${#AUTHORS[@]}
plural() { (( $1 == 1 )) && echo "$1 $2" || echo "$1 ${2}s"; }

echo "## New release: $TAG"
echo
echo "$(plural "$n" commit) by $(plural "$c" contributor)${PREV:+ since $PREV}."
echo
echo "### Downloads"
echo
echo "| System | File |"
echo "| --- | --- |"
echo "| Linux (any distribution) | \`Clipstack-x86_64.AppImage\`, \`Clipstack-aarch64.AppImage\` |"
echo "| Debian, Ubuntu, Mint, Pop!_OS | \`Clipstack-amd64.deb\`, \`Clipstack-arm64.deb\` |"
echo "| Fedora, openSUSE, RHEL | \`Clipstack-x86_64.rpm\`, \`Clipstack-aarch64.rpm\` |"
echo "| Linux (plain binary) | \`Clipstack-linux-x86_64.tar.gz\`, \`Clipstack-linux-aarch64.tar.gz\` |"
echo "| Windows 10/11 | \`Clipstack-Setup-x64.exe\` (installer), \`Clipstack-windows-x64.zip\` (portable) |"
echo "| macOS 11+ (Apple Silicon and Intel) | \`Clipstack-macos.dmg\` |"
echo
echo "<details>"
echo "<summary>Commits</summary>"
echo
printf '%s' "$LIST"
echo
echo "</details>"
if (( ${#NEW[@]} )); then
  echo
  echo "### New contributors"
  echo
  for who in "${!NEW[@]}"; do
    sha=${NEW[$who]}
    echo "- $who made their first contribution in [\`${sha:0:7}\`](https://github.com/$REPO/commit/$sha)"
  done
fi
if [[ -n $PREV ]]; then
  echo
  echo "**Full changelog:** https://github.com/$REPO/compare/$PREV...$TAG"
fi
