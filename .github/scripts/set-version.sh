#!/usr/bin/env bash
# Nadaje wydaniu wersję z formularza workflow, tylko w kopii roboczej (bez commita):
# sprawdza postać X.Y.Z i to, że jest wyższa od ostatniego tagu tej części, wpisuje ją do
# Cargo.toml, odświeża wpisy workspace w Cargo.lock i podaje tag oraz wersję w GITHUB_OUTPUT.
# Użycie: set-version.sh server|client X.Y.Z
set -euo pipefail

component=$1
version=$2

if [[ ! $version =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
  echo "::error::Wersja musi mieć postać X.Y.Z (same cyfry, bez zer wiodących), podano: '$version'."
  exit 1
fi

prefix="$component-v"
tag="$prefix$version"
latest=$(git ls-remote --tags --refs origin "refs/tags/$prefix*" | sed "s|.*refs/tags/$prefix||" | sort -V | tail -n 1)
if [ -n "$latest" ]; then
  highest=$(printf '%s\n%s\n' "$latest" "$version" | sort -V | tail -n 1)
  if [ "$latest" = "$version" ] || [ "$highest" != "$version" ]; then
    echo "::error::Wersja $version musi być wyższa od ostatniego wydania $prefix$latest."
    exit 1
  fi
fi

case $component in
  server)
    package=server
    sed -i '0,/^version = ".*"/s//version = "'"$version"'"/' crates/server/Cargo.toml
    ;;
  client)
    package=client-win
    sed -i '/^\[workspace\.package\]/,/^\[/ s/^version = ".*"/version = "'"$version"'"/' Cargo.toml
    ;;
  *)
    echo "::error::Nieznana część: $component (server albo client)."
    exit 1
    ;;
esac
cargo update --workspace --quiet

actual=$(cargo metadata --no-deps --format-version 1 | jq -r --arg p "$package" '.packages[] | select(.name == $p) | .version')
if [ "$actual" != "$version" ]; then
  echo "::error::Nie udało się ustawić wersji: $package ma $actual zamiast $version."
  exit 1
fi

echo "Wersja $version, tag $tag (poprzednie wydanie: ${latest:-brak})."
echo "version=$version" >> "$GITHUB_OUTPUT"
echo "tag=$tag" >> "$GITHUB_OUTPUT"
