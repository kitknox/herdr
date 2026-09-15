#!/bin/sh
# Install a tagged Rootshell build without modifying package-manager files.
set -eu
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
tag=${1:-}
if [ -z "$tag" ]; then
    curl -fL --retry 3 -o "$tmp/rootshell.json" https://github.com/kitknox/herdr/releases/download/rootshell-channel/rootshell.json
    version=$(sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)"[,[:space:]]*$/\1/p' "$tmp/rootshell.json")
    tag=rootshell-v$version
fi
printf '%s\n' "$tag" | grep -Eq '^rootshell-v[0-9]+\.[0-9]+\.[0-9]+$' || {
    echo 'Expected rootshell-vMAJOR.MINOR.PATCH' >&2; exit 1;
}
case "$(uname -s)" in
    Darwin) os=macos ;;
    Linux) os=linux ;;
    *) echo 'This installer supports macOS and Linux.' >&2; exit 1 ;;
esac
case "$(uname -m)" in
    arm64|aarch64) arch=aarch64 ;;
    x86_64) arch=x86_64 ;;
    *) echo 'Unsupported architecture.' >&2; exit 1 ;;
esac
case "${SHELL:-}" in
    */zsh) profile=${ZDOTDIR:-$HOME}/.zshrc; line='export PATH="$HOME/.local/opt/herdr-rootshell/bin:$PATH" # rootshell-herdr' ;;
    */bash) profile=$HOME/.bashrc; line='export PATH="$HOME/.local/opt/herdr-rootshell/bin:$PATH" # rootshell-herdr' ;;
    */fish) profile=${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/rootshell-herdr.fish; line='fish_add_path --path $HOME/.local/opt/herdr-rootshell/bin # rootshell-herdr' ;;
    *) echo 'Use this installer from a bash, zsh, or fish login account.' >&2; exit 1 ;;
esac
asset=herdr-$os-$arch
base=https://github.com/kitknox/herdr/releases/download/$tag
curl -fL --retry 3 -o "$tmp/$asset" "$base/$asset"
curl -fL --retry 3 -o "$tmp/$asset.sha256" "$base/$asset.sha256"
(
    cd "$tmp"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "$asset.sha256"
    else
        shasum -a 256 -c "$asset.sha256"
    fi
)
dest=$HOME/.local/opt/herdr-rootshell/bin
mkdir -p "$dest"
install -m 755 "$tmp/$asset" "$dest/herdr.new"
mv -f "$dest/herdr.new" "$dest/herdr"
mkdir -p "$(dirname "$profile")"
if ! grep -Fqx "$line" "$profile" 2>/dev/null; then
    printf '\n%s\n' "$line" >> "$profile"
fi
# Bash login shells may not read .bashrc.
if [ "${SHELL##*/}" = bash ]; then
    login_profile=$HOME/.bash_profile
    if [ ! -e "$login_profile" ]; then
        if [ -e "$HOME/.bash_login" ]; then login_profile=$HOME/.bash_login
        elif [ -e "$HOME/.profile" ]; then login_profile=$HOME/.profile
        fi
    fi
    if ! grep -Fqx "$line" "$login_profile" 2>/dev/null; then
        printf '\n%s\n' "$line" >> "$login_profile"
    fi
fi
printf '\nInstalled %s. Open a new terminal to use it. Future upgrades: herdr update\n' "$tag"
printf 'Existing servers keep running. Save work before stopping them; stopping terminates pane processes.\n'
printf 'From outside Herdr: herdr server stop, then herdr (or stop/restart your named session).\n'
printf 'Rollback: remove the rootshell-herdr PATH lines from your shell startup files, open a new terminal, and restart the server with official Herdr.\n'
