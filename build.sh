#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")"

lang=""
user_choices=""
auto_run=""

for arg in "$@"; do
    case "$arg" in
        --pl|--polish) lang="pl" ;;
        --en|--english) lang="en" ;;
        --rpm) user_choices="$user_choices 1" ;;
        --deb) user_choices="$user_choices 2" ;;
        --zip|--windows) user_choices="$user_choices 3" ;;
        --tar|--tarball) user_choices="$user_choices 4" ;;
        --all) user_choices="5" ;;
        --run) auto_run="y" ;;
        --no-run) auto_run="n" ;;
    esac
done

if [[ -z "$lang" ]]; then
    printf '\nSelect language / Wybierz język:\n'
    printf '  1) Polski (PL)\n'
    printf '  2) English (EN) [default / domyślny]\n'
    read -r -p 'Choice / Wybór [1/2, default 2]: ' lang_choice
    case "${lang_choice:-2}" in
        1|pl|PL|polski) lang="pl" ;;
        *) lang="en" ;;
    esac
fi

if [[ "$lang" == "en" ]]; then
    txt_header="=== eafrei Package Builder ==="
    txt_menu_title="Select packages to build (space-separated or 5 for all):"
    txt_opt1="1) RPM (.rpm)"
    txt_opt2="2) DEB (.deb)"
    txt_opt3="3) Windows ZIP (.zip)"
    txt_opt4="4) Linux Tarball (.tar.gz)"
    txt_opt5="5) All"
    txt_prompt="Your choice [1-5, default 5]: "
    txt_building="Building: "
    txt_success="Packages built successfully!"
    txt_locations="All packages are located in:"
    txt_ask_run="Do you want to run the project now (cargo run --release)? [y/N]: "
    txt_running="Running project..."
else
    txt_header="=== Budowanie paczek eafrei ==="
    txt_menu_title="Wybierz pakiety do zbudowania (oddziel spacjami lub wpisz 5 dla wszystkich):"
    txt_opt1="1) RPM (.rpm)"
    txt_opt2="2) DEB (.deb)"
    txt_opt3="3) Windows ZIP (.zip)"
    txt_opt4="4) Linux Tarball (.tar.gz)"
    txt_opt5="5) Wszystkie"
    txt_prompt="Twój wybór [1-5, domyślnie 5]: "
    txt_building="Budowanie: "
    txt_success="Pomyślnie utworzono pakiety!"
    txt_locations="Wszystkie pakiety znajdują się w folderze:"
    txt_ask_run="Czy chcesz teraz uruchomić projekt (cargo run --release)? [t/N]: "
    txt_running="Uruchamianie projektu..."
fi

if [[ -z "$user_choices" ]]; then
    printf '\n%s\n\n' "$txt_header"
    printf '%s\n' "$txt_menu_title"
    printf '  %s\n' "$txt_opt1"
    printf '  %s\n' "$txt_opt2"
    printf '  %s\n' "$txt_opt3"
    printf '  %s\n' "$txt_opt4"
    printf '  %s\n\n' "$txt_opt5"

    read -r -p "$txt_prompt" user_choices
    user_choices="${user_choices:-5}"
fi

build_rpm=0
build_deb=0
build_zip=0
build_tar=0

for ch in $user_choices; do
    ch="${ch//,/}"
    case "$ch" in
        1) build_rpm=1 ;;
        2) build_deb=1 ;;
        3) build_zip=1 ;;
        4) build_tar=1 ;;
        5|all|All|wszystkie)
            build_rpm=1
            build_deb=1
            build_zip=1
            build_tar=1
            ;;
    esac
done

if (( build_rpm == 0 && build_deb == 0 && build_zip == 0 && build_tar == 0 )); then
    build_rpm=1
    build_deb=1
    build_zip=1
    build_tar=1
fi

dist_dir="target/packages"
mkdir -p "$dist_dir"
built_files=()

version=$(awk -F"[\"']" '
    /^[[:space:]]*\[/ { pkg = ($0 ~ /^[[:space:]]*\[package\][[:space:]]*(#.*)?$/) }
    pkg && /^[[:space:]]*version[[:space:]]*=/ { print $2; exit }
' Cargo.toml)

if (( build_rpm || build_deb )); then
    if [[ ! -f "target/release/eafrei" ]]; then
        printf '\n%s eafrei (release)\n' "$txt_building"
        cargo build --release
    fi
fi

if (( build_rpm )); then
    printf '\n%s RPM\n' "$txt_building"
    cargo-generate-rpm
    rpm_path=$(find target/generate-rpm -maxdepth 1 -name "*.rpm" 2>/dev/null | sort -V | tail -n 1)
    if [[ -n "$rpm_path" && -f "$rpm_path" ]]; then
        cp -f "$rpm_path" "$dist_dir/"
        built_files+=("$dist_dir/$(basename "$rpm_path")")
    fi
fi

if (( build_deb )); then
    printf '\n%s DEB\n' "$txt_building"
    cargo deb --no-build
    deb_path=$(find target/debian -maxdepth 1 -name "*.deb" 2>/dev/null | sort -V | tail -n 1)
    if [[ -n "$deb_path" && -f "$deb_path" ]]; then
        cp -f "$deb_path" "$dist_dir/"
        built_files+=("$dist_dir/$(basename "$deb_path")")
    fi
fi

if (( build_tar )); then
    printf '\n%s Linux Tarball\n' "$txt_building"
    target="x86_64-unknown-linux-gnu"
    name="eafrei-$version-$target"
    bin="target/$target/release/eafrei"
    if [[ ! -f "$bin" ]]; then
        cargo build --release --locked --bin eafrei --target "$target"
    fi
    stage=$(mktemp -d -- "target/.eafrei-tar.XXXXXXXX")
    pkg="$stage/$name"
    mkdir -p -- "$pkg/config" "$pkg/public" "$pkg/certs"
    install -m 0755 -- "$bin" "$pkg/eafrei"
    install -m 0644 -- config/mail.conf "$pkg/config/mail.conf"
    install -m 0644 -- config/php.conf "$pkg/config/php.conf"
    install -m 0644 -- config/web.conf "$pkg/config/web.conf"
    install -m 0644 -- packaging/eafrei.service "$pkg/eafrei.service"
    install -m 0644 -- public/index.html "$pkg/public/index.html"
    if [[ -f public/.htaccess ]]; then
        install -m 0644 -- public/.htaccess "$pkg/public/.htaccess"
    fi
    tar -czf "$dist_dir/$name.tar.gz" -C "$stage" "$name"
    rm -rf -- "$stage"
    built_files+=("$dist_dir/$name.tar.gz")
fi

if (( build_zip )); then
    printf '\n%s Windows ZIP\n' "$txt_building"
    target="x86_64-pc-windows-gnu"
    name="eafrei-$version-$target"
    bin="target/$target/release/eafrei.exe"
    if [[ ! -f "$bin" ]]; then
        cargo build --release --locked --bin eafrei --target "$target"
    fi
    stage=$(mktemp -d -- "target/.eafrei-zip.XXXXXXXX")
    pkg="$stage/$name"
    mkdir -p -- "$pkg/config" "$pkg/public" "$pkg/certs"
    install -m 0755 -- "$bin" "$pkg/eafrei.exe"
    install -m 0644 -- config/mail.conf "$pkg/config/mail.conf"
    install -m 0644 -- config/php.conf "$pkg/config/php.conf"
    install -m 0644 -- config/web.conf "$pkg/config/web.conf"
    install -m 0644 -- public/index.html "$pkg/public/index.html"
    if [[ -f public/.htaccess ]]; then
        install -m 0644 -- public/.htaccess "$pkg/public/.htaccess"
    fi
    (cd "$stage" && zip -rq "$name.zip" "$name")
    mv -f -- "$stage/$name.zip" "$dist_dir/$name.zip"
    rm -rf -- "$stage"
    built_files+=("$dist_dir/$name.zip")
fi

printf '\n\033[1;32m%s\033[0m\n' "$txt_success"
printf '%s %s\n' "$txt_locations" "$(realpath "$dist_dir")"
for file in "${built_files[@]}"; do
    printf '  - %s\n' "$(realpath "$file")"
done

printf '\n'
if [[ -z "$auto_run" ]]; then
    read -r -p "$txt_ask_run" auto_run
fi
case "$auto_run" in
    t|T|tak|Tak|TAK|y|Y|yes|Yes|YES)
        printf '%s\n' "$txt_running"
        cargo run --release
        ;;
    *)
        ;;
esac
